use super::*;

struct Ticket(PathBuf);
impl Drop for Ticket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
impl RepoCache {
    pub(super) fn admit(&self, base: &Path, deadline: Instant) -> Result<File> {
        let queued = now_ms();
        let ticket = base.join("queued.json");
        atomic_json(&ticket, &queued)?;
        let _ticket = Ticket(ticket);
        loop {
            if Instant::now() >= deadline
                || self.git.cancelled.load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(AIexError::RefreshPending);
            }
            // Admission is brief; no Git operation runs under this lock.
            if let Some(_admission) = try_lock(&self.root.join("v3-admission.lock"))? {
                let interactive = try_lock(&base.join("interactive.lock"))?.is_none();
                let mut oldest = (queued, base.to_path_buf());
                let mut oldest_interactive = oldest.clone();
                for entry in std::fs::read_dir(self.root.join("v3"))? {
                    let path = entry?.path();
                    if path == base {
                        continue;
                    }
                    let time = std::fs::read(path.join("queued.json"))
                        .ok()
                        .and_then(|b| serde_json::from_slice::<u64>(&b).ok());
                    let Some(time) = time else {
                        continue;
                    };
                    // Ignore tickets abandoned by dead owners.
                    if try_lock(&path.join("job.lock"))?.is_some() {
                        continue;
                    }
                    let candidate = (time, path.clone());
                    if candidate < oldest {
                        oldest = candidate.clone();
                    }
                    if candidate < oldest_interactive
                        && try_lock(&path.join("interactive.lock"))?.is_none()
                    {
                        oldest_interactive = candidate;
                    }
                }
                let slots: &[usize] = if interactive { &[2, 3, 0, 1] } else { &[0, 1] };
                for slot in slots {
                    let first = if *slot >= 2 {
                        &oldest_interactive.1
                    } else {
                        &oldest.1
                    };
                    if first != base {
                        continue;
                    }
                    if let Some(lock) = try_lock(&self.root.join(format!("v3-slot-{slot}.lock")))? {
                        return Ok(lock);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
