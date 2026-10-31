use super::*;

#[derive(Debug)]
pub struct CommandView {
    dir: tempfile::TempDir,
    _lease: File,
}
impl Drop for CommandView {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.dir.path().with_extension("lease"));
    }
}
impl CommandView {
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl RepoCache {
    pub(crate) fn warm_execution_view(&self, dir: &Path) -> Result<()> {
        let deadline = self
            .deadline
            .unwrap_or_else(|| Instant::now() + self.git.timeout);
        loop {
            let (ready, lock) = view_paths(dir);
            let failure = std::fs::read(lock.with_extension("failure"))
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
            if let Some(failure) = failure {
                if failure["retry_ms"].as_u64().unwrap_or(0) > now_ms() {
                    return Err(AIexError::RefreshFailed(
                        failure["error"]
                            .as_str()
                            .unwrap_or("command copy failed")
                            .into(),
                    ));
                }
            }
            if ready.is_dir() {
                return Ok(());
            }
            if Instant::now() >= deadline
                || self.git.cancelled.load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(AIexError::RefreshPending);
            }
            self.start_view(dir);
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    pub fn execution_view(&self, dir: &Path) -> Result<CommandView> {
        let (ready, lock) = view_paths(dir);
        loop {
            self.view_deadline(
                self.deadline
                    .unwrap_or_else(|| Instant::now() + self.git.timeout),
            )?;
            self.warm_execution_view(dir)?;
            if let Some(_owner) = try_lock(&lock)? {
                if !ready.is_dir() {
                    continue;
                }
                let view = tempfile::Builder::new()
                    .prefix("command-")
                    .tempdir_in(dir.parent().unwrap())?;
                // The lock makes taking a prepared copy exclusive across processes.
                // Remove the empty destination for Windows rename semantics.
                let lease = lock_file(&view.path().with_extension("lease"))?;
                lease.lock_shared()?;
                std::fs::remove_dir(view.path())?;
                std::fs::rename(&ready, view.path())?;
                drop(_owner);
                // Prepare the next independent copy while this command executes.
                self.start_view(dir);
                return Ok(CommandView {
                    dir: view,
                    _lease: lease,
                });
            }
        }
    }
    fn start_view(&self, dir: &Path) {
        let (_, key) = view_paths(dir);
        {
            let mut jobs = jobs().lock().unwrap();
            if jobs.len() >= 128 || !jobs.insert(key.clone()) {
                return;
            }
        }
        let cache = self.clone();
        let dir = dir.to_path_buf();
        let worker = self.git.worker();
        std::thread::spawn(move || {
            let _worker = worker;
            let result = cache.build_view(&dir);
            if let Err(error) = &result {
                if !cache
                    .git
                    .cancelled
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    let _ = atomic_json(
                        &key.with_extension("failure"),
                        &serde_json::json!({"retry_ms":now_ms()+5000,"error":error.to_string()}),
                    );
                }
            }
            jobs().lock().unwrap().remove(&key);
            if matches!(result, Ok(true))
                && !view_paths(&dir).0.is_dir()
                && !cache
                    .git
                    .cancelled
                    .load(std::sync::atomic::Ordering::SeqCst)
            {
                cache.start_view(&dir);
            }
        });
    }
    fn build_view(&self, dir: &Path) -> Result<bool> {
        let (ready, lock) = view_paths(dir);
        let Some(_owner) = try_lock(&lock)? else {
            return Ok(false);
        };
        if ready.is_dir() {
            return Ok(false);
        }
        let base = dir.parent().unwrap();
        let lease = lock_file(&base.join(format!(
            "{}.lease",
            dir.file_name().unwrap().to_string_lossy()
        )))?;
        lease.lock_shared()?;
        if !dir.is_dir() {
            return Err(AIexError::RefreshPending);
        }
        let deadline = Instant::now() + self.git.timeout;
        // Copying is disk work, not a Git acquisition; bound it separately.
        let _slot = loop {
            self.view_deadline(deadline)?;
            let mut slot = None;
            for n in 0..2 {
                if let Some(file) = try_lock(&self.root.join(format!("v3-copy-{n}.lock")))? {
                    slot = Some(file);
                    break;
                }
            }
            if let Some(slot) = slot {
                break slot;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let stage_prefix = format!("view-stage-{}-", dir.file_name().unwrap().to_string_lossy());
        for entry in std::fs::read_dir(base)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(&stage_prefix)
                && entry.file_type()?.is_dir()
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        let stage = tempfile::Builder::new()
            .prefix(&stage_prefix)
            .tempdir_in(base)?;
        self.copy_view(dir, stage.path(), deadline)?;
        self.view_deadline(deadline)?;
        std::fs::rename(stage.path(), ready)?;
        let _ = std::fs::remove_file(lock.with_extension("failure"));
        Ok(true)
    }
    fn view_deadline(&self, deadline: Instant) -> Result<()> {
        if Instant::now() >= deadline
            || self.git.cancelled.load(std::sync::atomic::Ordering::SeqCst)
        {
            Err(AIexError::RefreshPending)
        } else {
            Ok(())
        }
    }
    fn copy_view(&self, source: &Path, target: &Path, deadline: Instant) -> Result<()> {
        self.view_deadline(deadline)?;
        std::fs::create_dir_all(target)?;
        for entry in std::fs::read_dir(source)? {
            self.view_deadline(deadline)?;
            let entry = entry?;
            let path = entry.path();
            let dest = target.join(entry.file_name());
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                let link = std::fs::read_link(&path)?;
                #[cfg(unix)]
                std::os::unix::fs::symlink(link, dest)?;
                #[cfg(windows)]
                {
                    if path.is_dir() {
                        std::os::windows::fs::symlink_dir(link, dest)?;
                    } else {
                        std::os::windows::fs::symlink_file(link, dest)?;
                    }
                }
            } else if kind.is_dir() {
                self.copy_view(&path, &dest, deadline)?;
            } else {
                std::fs::copy(path, dest)?;
            }
        }
        Ok(())
    }
}
fn view_paths(dir: &Path) -> (PathBuf, PathBuf) {
    let base = dir.parent().unwrap();
    let name = dir.file_name().unwrap().to_string_lossy();
    (
        base.join(format!("view-ready-{name}")),
        base.join(format!("{name}.view.lock")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expired_wait_preserves_preparation_and_command_writes_are_isolated() {
        let temp = tempfile::tempdir().unwrap();
        let cache = RepoCache::new(temp.path().join("cache"), AuthConfig::default());
        let dir = cache.root.join("v3/fixture/snapshot-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("doc"), "evidence").unwrap();
        let a = try_lock(&cache.root.join("v3-copy-0.lock"))
            .unwrap()
            .unwrap();
        let b = try_lock(&cache.root.join("v3-copy-1.lock"))
            .unwrap()
            .unwrap();
        let request = cache.session(
            Freshness::CachedOk,
            Instant::now() + Duration::from_millis(80),
        );
        assert!(matches!(
            request.execution_view(&dir),
            Err(AIexError::RefreshPending)
        ));
        drop(a);
        drop(b);
        let request = cache.session(Freshness::CachedOk, Instant::now() + Duration::from_secs(5));
        request.warm_execution_view(&dir).unwrap();
        let view = request.execution_view(&dir).unwrap();
        std::fs::write(view.path().join("doc"), "command output").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("doc")).unwrap(),
            "evidence"
        );
        let next = request.execution_view(&dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(next.path().join("doc")).unwrap(),
            "evidence"
        );
        cache.git.cancel();
    }
}
