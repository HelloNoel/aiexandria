use super::*;
pub(super) struct Target<'a> {
    pub revision: Option<&'a str>,
    pub due: bool,
}
impl RepoCache {
    pub(super) fn build(
        &self,
        origin: &GitRef,
        previous: Option<PathBuf>,
        selection: &Selection,
        target: Target<'_>,
        base: &Path,
        deadline: Instant,
    ) -> Result<Snapshot> {
        let Target { revision, due } = target;
        let staging = tempfile::Builder::new()
            .prefix("snapshot-")
            .tempdir_in(base)?;
        let dir = staging.path();
        let run = |dir: Option<&Path>, args: Vec<String>, input: Option<&[u8]>| {
            self.git
                .run(dir, &args, Some(origin), &self.auth, input, deadline)
        };
        for (namespace, name) in [("heads", &origin.branch), ("tags", &origin.tag)] {
            if let Some(name) = name {
                run(
                    None,
                    vec![
                        "check-ref-format".into(),
                        format!("refs/{namespace}/{name}"),
                    ],
                    None,
                )?;
            }
        }
        let mut args: Vec<String> = vec!["clone".into(), "--no-checkout".into()];
        let diagnostics = if let Some(previous) = &previous {
            // Git ignores --local for shallow sources and upload-pack cannot
            // serve their missing promised objects. Copy private Git metadata;
            // only immutable object files are hardlinked into the new stage.
            copy_repository(previous, dir, deadline)?;
            Vec::new()
        } else {
            args.push("--filter=blob:none".into());
            if origin.commit.is_none() && origin.tag.is_none() {
                args.push("--depth=1".into());
            }
            if let Some(branch) = origin.branch.as_ref().or(origin.tag.as_ref()) {
                args.extend(["--branch".into(), branch.clone()]);
            }
            args.extend([
                "--".into(),
                origin.url.clone(),
                dir.to_string_lossy().into_owned(),
            ]);
            self.git
                .run_output(None, &args, Some(origin), &self.auth, None, deadline)?
                .1
        };
        let filtered = if let Some(previous) = &previous {
            std::fs::read_to_string(previous.join(".git/aiexandria-filter"))
                .map(|s| s == "supported")
                .unwrap_or(false)
        } else {
            !String::from_utf8_lossy(&diagnostics)
                .contains("filtering not recognized by server, ignoring")
        };
        run(
            Some(dir),
            vec![
                "remote".into(),
                "set-url".into(),
                "origin".into(),
                origin.url.clone(),
            ],
            None,
        )?;
        run(
            Some(dir),
            vec![
                "config".into(),
                "remote.origin.promisor".into(),
                filtered.to_string(),
            ],
            None,
        )?;
        if filtered {
            run(
                Some(dir),
                vec![
                    "config".into(),
                    "remote.origin.partialclonefilter".into(),
                    "blob:none".into(),
                ],
                None,
            )?;
        } else {
            let _ = run(
                Some(dir),
                vec![
                    "config".into(),
                    "--unset-all".into(),
                    "remote.origin.partialclonefilter".into(),
                ],
                None,
            );
        }
        std::fs::write(
            dir.join(".git/aiexandria-filter"),
            if filtered { "supported" } else { "unsupported" },
        )?;
        let format = run(
            Some(dir),
            vec!["rev-parse".into(), "--show-object-format".into()],
            None,
        )?;
        let oid_length = match format.trim() {
            "sha1" => 40,
            "sha256" => 64,
            other => {
                return Err(AIexError::Other(format!(
                    "unsupported Git object format: {other}"
                )))
            }
        };
        if origin
            .commit
            .as_ref()
            .is_some_and(|pin| pin.len() > oid_length)
        {
            return Err(AIexError::Other(format!(
                "commit pin exceeds the {oid_length}-character {} object ID length",
                format.trim()
            )));
        }
        std::fs::create_dir_all(dir.join(".git/info"))?;
        // Cache materialization must not execute arbitrary configured smudge filters.
        std::fs::write(dir.join(".git/info/attributes"), b"* -filter\n")?;
        let mut target = origin
            .tag
            .as_ref()
            .map(|tag| format!("refs/tags/{tag}"))
            .or_else(|| {
                origin
                    .branch
                    .as_ref()
                    .map(|branch| format!("refs/heads/{branch}"))
            })
            .unwrap_or_else(|| "HEAD".into());
        if previous.is_some() {
            target = "HEAD".into();
        }
        if let Some(revision) = revision {
            target = revision.into();
        } else if let Some(commit) = &origin.commit {
            target = commit.clone();
        } else if previous.is_some() && due {
            let reference = origin
                .branch
                .as_ref()
                .map(|v| format!("refs/heads/{v}"))
                .or_else(|| origin.tag.as_ref().map(|v| format!("refs/tags/{v}")))
                .unwrap_or_else(|| "HEAD".into());
            run(
                Some(dir),
                vec![
                    "fetch".into(),
                    "--depth=1".into(),
                    "origin".into(),
                    reference,
                ],
                None,
            )?;
            target = "FETCH_HEAD".into();
        }
        let resolve = |target: &str| {
            run(
                Some(dir),
                vec![
                    "rev-parse".into(),
                    "--verify".into(),
                    "--end-of-options".into(),
                    format!("{target}^{{commit}}"),
                ],
                None,
            )
        };
        let oid = if origin.commit.is_some() {
            // Disambiguate object IDs directly: a branch/tag with a hexadecimal
            // name must never override a commit prefix, and tags must not peel.
            let target = target.to_ascii_lowercase();
            let candidates = || {
                run(
                    Some(dir),
                    vec!["rev-parse".into(), format!("--disambiguate={target}")],
                    None,
                )
            };
            let mut matches = candidates()?;
            if matches.trim().is_empty() && target.len() == oid_length {
                run(
                    Some(dir),
                    vec!["fetch".into(), "origin".into(), target.clone()],
                    None,
                )?;
                matches = candidates()?;
            }
            let ids: Vec<_> = matches.lines().filter(|s| !s.is_empty()).collect();
            let oid = match ids.as_slice() {
                [oid] => (*oid).to_owned(),
                [] => {
                    return Err(AIexError::Other(
                        "commit pin was not found in the repository".into(),
                    ))
                }
                _ => {
                    return Err(AIexError::Other(
                        "commit pin prefix is ambiguous; use a full commit ID".into(),
                    ))
                }
            };
            let kind = run(
                Some(dir),
                vec!["cat-file".into(), "-t".into(), oid.clone()],
                None,
            )?;
            if kind.trim() != "commit" {
                return Err(AIexError::Other("pin must identify a commit object".into()));
            }
            oid
        } else {
            resolve(&target)?.trim().to_owned()
        };
        if let Some(previous) = &previous {
            if let Some(snapshot) = read_state(base).and_then(|s| {
                s.snapshots.into_iter().find(|s| {
                    base.join(&s.name) == *previous
                        && s.revision == oid
                        && s.selection.covers(selection)
                        && intact(base, s)
                })
            }) {
                return Ok(snapshot);
            }
        }
        run(
            Some(dir),
            vec![
                "update-ref".into(),
                "--no-deref".into(),
                "HEAD".into(),
                oid.clone(),
            ],
            None,
        )?;
        if selection.all || origin.submodules {
            run(
                Some(dir),
                vec!["sparse-checkout".into(), "disable".into()],
                None,
            )?;
        } else {
            let patterns = selection
                .paths
                .iter()
                .map(|p| format!("/{}", escape_pattern(p)))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            run(
                Some(dir),
                vec![
                    "sparse-checkout".into(),
                    "set".into(),
                    "--no-cone".into(),
                    "--stdin".into(),
                ],
                Some(patterns.as_bytes()),
            )?;
        }
        run(
            Some(dir),
            vec![
                "checkout".into(),
                "--force".into(),
                "--detach".into(),
                oid.clone(),
            ],
            None,
        )?;
        if origin.submodules {
            let mut args = vec!["-c".into(), "submodule.recurse=false".into()];
            let keys = run(
                Some(dir),
                vec![
                    "config".into(),
                    "--null".into(),
                    "--name-only".into(),
                    "--list".into(),
                ],
                None,
            )?;
            let drivers: BTreeSet<_> = keys
                .split('\0')
                .filter_map(|key| {
                    let (prefix, _) = key.rsplit_once('.')?;
                    prefix.starts_with("filter.").then_some(prefix)
                })
                .collect();
            for driver in drivers {
                for suffix in ["smudge=", "clean=", "process=", "required=false"] {
                    args.extend(["-c".into(), format!("{driver}.{suffix}")]);
                }
            }
            if origin.url.starts_with("file://") {
                args.extend(["-c".into(), "protocol.file.allow=always".into()]);
            }
            args.extend([
                "submodule".into(),
                "update".into(),
                "--init".into(),
                "--recursive".into(),
                "--force".into(),
                "--jobs=1".into(),
            ]);
            run(Some(dir), args, None)?;
        }
        std::fs::write(dir.join(".git/aiexandria-ready"), b"1")?;
        std::fs::write(dir.join(".pinakes-fetched"), b"")?;
        let snapshot = Snapshot {
            name: dir.file_name().unwrap().to_string_lossy().into_owned(),
            revision: oid,
            selection: if origin.submodules {
                Selection::all()
            } else {
                selection.clone()
            },
            created_ms: now_ms(),
        };
        let _ = staging.keep();
        Ok(snapshot)
    }
}

fn copy_repository(source: &Path, target: &Path, deadline: Instant) -> Result<()> {
    fn copy(source: &Path, target: &Path, objects: bool, deadline: Instant) -> Result<()> {
        if Instant::now() >= deadline {
            return Err(AIexError::RefreshPending);
        }
        let meta = std::fs::symlink_metadata(source)?;
        if meta.is_symlink() {
            return Err(AIexError::Other(
                "unexpected symlink in Git metadata".into(),
            ));
        }
        if meta.is_dir() {
            std::fs::create_dir_all(target)?;
            for entry in std::fs::read_dir(source)? {
                let entry = entry?;
                copy(
                    &entry.path(),
                    &target.join(entry.file_name()),
                    objects || entry.file_name() == "objects",
                    deadline,
                )?;
            }
        } else {
            let name = source.file_name().unwrap().to_string_lossy();
            let immutable = objects
                && (name.ends_with(".pack")
                    || name.ends_with(".idx")
                    || name.bytes().all(|b| b.is_ascii_hexdigit()));
            if !immutable || std::fs::hard_link(source, target).is_err() {
                std::fs::copy(source, target)?;
            }
        }
        Ok(())
    }
    std::fs::create_dir_all(target.join(".git"))?;
    for name in [
        "HEAD",
        "config",
        "shallow",
        "objects",
        "refs",
        "packed-refs",
        "modules",
    ] {
        let from = source.join(".git").join(name);
        if from.exists() {
            copy(
                &from,
                &target.join(".git").join(name),
                name == "objects",
                deadline,
            )?;
        }
    }
    Ok(())
}
