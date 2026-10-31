use super::*;

impl RepoCache {
    // Called with job ownership held. Never unlink lock files: another process
    // may already have their inode open while waiting for ownership.
    pub(super) fn cleanup(&self, base: &Path, state: &mut CacheState) -> Result<()> {
        let previous = state
            .snapshots
            .iter()
            .rev()
            .find(|s| Some(&s.name) != state.current.as_ref())
            .map(|s| s.name.clone());
        let mut kept = Vec::new();
        for snapshot in &state.snapshots {
            let keep = Some(&snapshot.name) == state.current.as_ref()
                || Some(&snapshot.name) == previous.as_ref()
                || now_ms().saturating_sub(snapshot.created_ms) < 900_000;
            if keep {
                kept.push(snapshot.clone());
                continue;
            }
            let Some(_lease) = try_lock(&base.join(format!("{}.lease", snapshot.name)))? else {
                kept.push(snapshot.clone());
                continue;
            };
            // Scan after taking the lease. Token creation holds a shared lease,
            // so publication of a follow-up token cannot race deletion here.
            if self.retained_revisions(base).contains(&snapshot.revision) {
                kept.push(snapshot.clone());
                continue;
            }
            let _ = std::fs::remove_dir_all(base.join(format!("view-ready-{}", snapshot.name)));
            if let Err(error) = std::fs::remove_dir_all(base.join(&snapshot.name)) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    kept.push(snapshot.clone());
                }
            }
        }
        state.snapshots = kept;
        atomic_json(&base.join("state.json"), state)?;
        // Remove abandoned staging directories only while owning the job lock.
        // A live descendant holds that lock on Unix; Windows jobs kill children.
        for entry in std::fs::read_dir(base)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("command-")
                && entry.file_type()?.is_dir()
                && entry
                    .metadata()?
                    .modified()
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > Duration::from_secs(900))
            {
                if let Some(_lease) = try_lock(&entry.path().with_extension("lease"))? {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
            if name.starts_with("snapshot-")
                && entry.file_type()?.is_dir()
                && !state.snapshots.iter().any(|s| s.name == name)
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        Ok(())
    }
    fn retained_revisions(&self, base: &Path) -> BTreeSet<String> {
        let key = base.file_name().unwrap().to_string_lossy();
        let mut revisions = BTreeSet::new();
        for file in std::fs::read_dir(self.root.join("sessions"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let value = std::fs::read(file.path())
                .ok()
                .and_then(|v| serde_json::from_slice::<serde_json::Value>(&v).ok());
            if let Some(value) = value {
                if value["expires_ms"].as_u64().unwrap_or(0) > now_ms() {
                    if let Some(revision) = value["revisions"][key.as_ref()].as_str() {
                        revisions.insert(revision.into());
                    }
                } else {
                    let _ = std::fs::remove_file(file.path());
                }
            }
        }
        revisions
    }
    pub(super) fn prune_tokens(&self) {
        // Also clean tokens on reads when repositories never refresh.
        let _ = self.retained_revisions(&self.root.join("unused"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cleanup_preserves_leases_tokens_and_the_previous_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let cache = RepoCache::new(temp.path().into(), AuthConfig::default());
        let origin = GitRef {
            url: "file:///unused".into(),
            ..Default::default()
        };
        let base = cache.path_for(&origin);
        let mut state = CacheState::new(&origin);
        for n in 0..5 {
            let name = format!("snapshot-{n}");
            std::fs::create_dir_all(base.join(&name)).unwrap();
            state.snapshots.push(Snapshot {
                name,
                revision: n.to_string(),
                selection: Selection::all(),
                created_ms: 0,
            });
        }
        state.current = Some("snapshot-4".into());
        let lease = lock_file(&base.join("snapshot-1.lease")).unwrap();
        lease.lock_shared().unwrap();
        let key = base.file_name().unwrap().to_string_lossy();
        let token = cache.root.join("sessions/read-token");
        std::fs::create_dir_all(token.parent().unwrap()).unwrap();
        atomic_json(
            &token,
            &serde_json::json!({"expires_ms":now_ms()+900_000,"revisions":{key.as_ref():"0"}}),
        )
        .unwrap();
        cache.cleanup(&base, &mut state).unwrap();
        assert_eq!(
            state
                .snapshots
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["snapshot-0", "snapshot-1", "snapshot-3", "snapshot-4"]
        );
        drop(lease);
        atomic_json(&token, &serde_json::json!({"expires_ms":0})).unwrap();
        cache.cleanup(&base, &mut state).unwrap();
        assert_eq!(state.snapshots.len(), 2);
        assert!(!token.exists());
        assert!(base.join("snapshot-3").is_dir());
        assert!(base.join("snapshot-4").is_dir());
    }
}
