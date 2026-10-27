#[cfg(test)]
mod tests {
    use aiexandria::{
        auth::AuthConfig,
        cache::RepoCache,
        command::{run_command, CommandPolicyChain},
        generate::{generate_index, GenerateOptions},
        index::{CommandPolicy, GitRef},
        resolver::{descend, Resolver},
    };
    use git2::{Repository, Signature};
    use std::{
        path::Path,
        time::{Duration, Instant},
    };
    use tempfile::TempDir;
    fn write(root: &Path, path: &str, body: &str) {
        let p = root.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    fn commit(repo: &Repository) -> git2::Oid {
        let mut i = repo.index().unwrap();
        i.add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        i.write().unwrap();
        let id = i.write_tree().unwrap();
        let tree = repo.find_tree(id).unwrap();
        let sig = Signature::now("review", "review@example.invalid").unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<_> = parent.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, "fixture", &tree, &parents)
            .unwrap()
    }
    fn fixture(root: &Path) -> (Repository, GitRef) {
        let path = root.join("repo");
        std::fs::create_dir_all(&path).unwrap();
        let repo = Repository::init(&path).unwrap();
        write(&path, "README.md", "hello");
        commit(&repo);
        (
            repo,
            GitRef {
                url: format!("file://{}", path.display()),
                submodules: false,
                ..Default::default()
            },
        )
    }
    fn policy(pattern: &str) -> CommandPolicyChain {
        CommandPolicyChain::default()
            .extend(Some(&CommandPolicy {
                allow: Some(pattern.into()),
                timeout_secs: Some(1),
            }))
            .unwrap()
    }
    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
    #[cfg(unix)]
    #[test]
    fn merged_descendant_preserves_restriction() {
        let t = TempDir::new().unwrap();
        let (repo, origin) = fixture(t.path());
        let root = repo.workdir().unwrap();
        write(root,"pinakes.yaml","version: 1\nname: root\ncommand_policy: {allow: '^echo '}\nentries:\n- kind: merge\n  path: merged.yaml\n");
        write(root,"merged.yaml","version: 1\nname: merged\ncommand_policy: {allow: '^echo safe$'}\nentries:\n- {kind: index, name: child, path: child.yaml}\n");
        write(root,"child.yaml","version: 1\nname: child\nentries:\n- {kind: command, name: run, argv: [echo, unsafe]}\n");
        commit(&repo);
        let cache = RepoCache::new(t.path().join("cache"), AuthConfig::default());
        let resolver = Resolver::new(&cache);
        let loaded = resolver
            .load_index(&origin, "pinakes.yaml", &CommandPolicyChain::default())
            .unwrap();
        let child = descend(&resolver, &loaded, "child").unwrap();
        assert!(resolver.run(child.find("run").unwrap(), "").is_err());
    }
    #[test]
    fn index_cycle_is_rejected() {
        let t = TempDir::new().unwrap();
        let (repo, origin) = fixture(t.path());
        write(
            repo.workdir().unwrap(),
            "pinakes.yaml",
            "version: 1\nname: root\nentries:\n- {kind: index, name: again, path: pinakes.yaml}\n",
        );
        commit(&repo);
        let cache = RepoCache::new(t.path().join("cache"), AuthConfig::default());
        let resolver = Resolver::new(&cache);
        let loaded = resolver
            .load_index(&origin, "pinakes.yaml", &CommandPolicyChain::default())
            .unwrap();
        assert!(descend(&resolver, &loaded, "again").is_err());
    }
    #[test]
    fn branch_cache_collision() {
        let t = TempDir::new().unwrap();
        let (repo, origin) = fixture(t.path());
        let first = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature/a", &first, false).unwrap();
        write(repo.workdir().unwrap(), "README.md", "second");
        let second = commit(&repo);
        repo.branch("feature_a", &repo.find_commit(second).unwrap(), false)
            .unwrap();
        let cache = RepoCache::new(t.path().join("cache"), AuthConfig::default());
        let a = GitRef {
            branch: Some("feature/a".into()),
            ..origin.clone()
        };
        let b = GitRef {
            branch: Some("feature_a".into()),
            ..origin
        };
        let pa = cache.ensure(&a).unwrap();
        let pb = cache.ensure(&b).unwrap();
        assert_ne!(pa, pb);
        assert_eq!(cache.read_file(&pb, "README.md").unwrap(), "second");
    }
    #[test]
    fn failed_pinned_clone_still_fails_on_retry() {
        let t = TempDir::new().unwrap();
        let (_repo, origin) = fixture(t.path());
        let cache = RepoCache::new(t.path().join("cache"), AuthConfig::default());
        let bad = GitRef {
            commit: Some("1111111111111111111111111111111111111111".into()),
            ..origin
        };
        assert!(cache.ensure(&bad).is_err());
        assert!(cache.ensure(&bad).is_err());
    }
    #[test]
    fn missing_explicit_auth_is_an_error() {
        let key = "AIEX_REVIEW_UNSET_TOKEN_923047";
        std::env::remove_var(key);
        assert!(AuthConfig::default()
            .resolve("https://example.invalid/repo.git", Some(key))
            .is_err());
    }
    #[test]
    fn build_supports_ssh() {
        assert!(git2::Version::get().ssh());
    }
    #[cfg(unix)]
    #[test]
    fn argv_join_matches_spec() {
        let out = run_command(
            &policy("^echo hello world$"),
            &args(&["echo", "{query}"]),
            "hello world",
            Path::new("/tmp"),
            &[],
        );
        assert_eq!(out.unwrap().trim(), "hello world");
    }
    #[cfg(unix)]
    #[test]
    fn unicode_output_is_truncated_safely() {
        let result = std::panic::catch_unwind(|| {
            run_command(
                &policy("^/usr/bin/python3 "),
                &args(&["/usr/bin/python3", "-c", "print('€'*90000)"]),
                "",
                Path::new("/tmp"),
                &[],
            )
        });
        assert!(result.unwrap().unwrap().ends_with("...[truncated]"));
    }
    #[cfg(unix)]
    #[test]
    fn pipe_descendant_obeys_timeout() {
        let start = Instant::now();
        let result = run_command(
            &policy("^/usr/bin/python3 "),
            &args(&[
                "/usr/bin/python3",
                "-c",
                "import subprocess; subprocess.Popen(['/bin/sleep','2'])",
            ]),
            "",
            Path::new("/tmp"),
            &[],
        );
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_millis(1900));
    }
    #[test]
    fn generator_allocates_unique_ids() {
        let t = TempDir::new().unwrap();
        write(t.path(), "README.md", "a");
        write(t.path(), "README.rst", "b");
        write(t.path(), "search.md", "c");
        let yaml = generate_index(t.path(), &GenerateOptions::default()).unwrap();
        let parsed: aiexandria::index::Index =
            aiexandria::index::Index::parse(&yaml, "generated").unwrap();
        let ids: Vec<_> = parsed
            .entries
            .iter()
            .map(|e| e.id.as_deref().unwrap())
            .collect();
        assert_eq!(
            ids.iter()
                .copied()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            ids.len()
        );
        assert_eq!(ids.iter().filter(|&&id| id == "search").count(), 1);
    }
    #[cfg(unix)]
    #[test]
    fn generator_skips_symlinks_outside_root() {
        use std::os::unix::fs::symlink;
        let t = TempDir::new().unwrap();
        let root = t.path().join("repo");
        let outside = t.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        write(&outside, "README.md", "outside");
        symlink(&outside, root.join("linked")).unwrap();
        generate_index(&root, &GenerateOptions::default()).unwrap();
        assert!(!outside.join("pinakes.yaml").exists());
    }
    #[cfg(unix)]
    #[test]
    fn generated_search_treats_preprocessor_option_as_pattern() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempDir::new().unwrap();
        let root = t.path().join("repo");
        write(&root, "README.md", "hello");
        write(
            t.path(),
            "pre.sh",
            "#!/bin/sh\nprintf 'PREPROCESSOR_EXECUTED\\n'\n",
        );
        let pre = t.path().join("pre.sh");
        std::fs::set_permissions(&pre, std::fs::Permissions::from_mode(0o755)).unwrap();
        let yaml = generate_index(&root, &GenerateOptions::default()).unwrap();
        let index = aiexandria::index::Index::parse(&yaml, "generated").unwrap();
        let p = CommandPolicyChain::default()
            .extend(index.command_policy.as_ref())
            .unwrap();
        let command = index
            .entries
            .iter()
            .find(|e| e.id.as_deref() == Some("search"))
            .unwrap();
        let aiexandria::index::EntryKind::Command {
            argv,
            success_exit_codes,
            ..
        } = &command.kind
        else {
            panic!()
        };
        let out = run_command(
            &p,
            argv,
            &format!("--pre={}", pre.display()),
            &root,
            success_exit_codes,
        )
        .unwrap();
        assert!(!out.contains("PREPROCESSOR_EXECUTED"));
    }
    #[test]
    fn root_never_refresh_applies_to_doc_read() {
        use aiexandria::cache::RefreshPolicy;
        let t = TempDir::new().unwrap();
        let (repo, origin) = fixture(t.path());
        write(
            repo.workdir().unwrap(),
            "pinakes.yaml",
            "version: 1\nname: root\nentries:\n- {kind: doc, name: readme, path: README.md}\n",
        );
        commit(&repo);
        let cache = RepoCache::new(t.path().join("cache"), AuthConfig::default())
            .with_default_refresh(RefreshPolicy::Every(Duration::ZERO));
        let resolver = Resolver::new(&cache);
        let loaded = resolver
            .load_index_with_refresh(
                &origin,
                "pinakes.yaml",
                &CommandPolicyChain::default(),
                RefreshPolicy::Never,
            )
            .unwrap();
        write(repo.workdir().unwrap(), "README.md", "updated");
        commit(&repo);
        assert_eq!(
            resolver.read_doc(loaded.find("readme").unwrap()).unwrap(),
            "hello"
        );
    }

    #[test]
    fn mixed_merge_and_index_cycle_is_rejected() {
        let t = TempDir::new().unwrap();
        let (repo, origin) = fixture(t.path());
        write(
            repo.workdir().unwrap(),
            "pinakes.yaml",
            "version: 1\nname: root\nentries:\n- {kind: merge, path: merged.yaml}\n",
        );
        write(repo.workdir().unwrap(),"merged.yaml","version: 1\nname: merged\nentries:\n- {kind: index, name: again, path: pinakes.yaml}\n");
        commit(&repo);
        let cache = RepoCache::new(t.path().join("cache"), AuthConfig::default());
        let resolver = Resolver::new(&cache);
        let root = resolver
            .load_index(&origin, "pinakes.yaml", &CommandPolicyChain::default())
            .unwrap();
        assert!(descend(&resolver, &root, "again").is_err());
    }
    #[test]
    fn branch_and_tag_with_the_same_name_have_distinct_checkouts() {
        let t = TempDir::new().unwrap();
        let (repo, origin) = fixture(t.path());
        let first = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("release", &first, false).unwrap();
        write(repo.workdir().unwrap(), "README.md", "tagged");
        let second = commit(&repo);
        repo.tag_lightweight(
            "release",
            repo.find_commit(second).unwrap().as_object(),
            false,
        )
        .unwrap();
        let cache = RepoCache::new(t.path().join("cache"), AuthConfig::default());
        let branch = GitRef {
            branch: Some("release".into()),
            ..origin.clone()
        };
        let tag = GitRef {
            tag: Some("release".into()),
            ..origin
        };
        let a = cache.ensure(&branch).unwrap();
        let b = cache.ensure(&tag).unwrap();
        assert_ne!(a, b);
        assert_eq!(cache.read_file(&a, "README.md").unwrap(), "hello");
        assert_eq!(cache.read_file(&b, "README.md").unwrap(), "tagged");
    }
    #[test]
    fn abbreviated_pin_is_resolved_and_warm_pinned_head_is_verified() {
        let t = TempDir::new().unwrap();
        let (repo, origin) = fixture(t.path());
        let first = repo.head().unwrap().target().unwrap();
        write(repo.workdir().unwrap(), "README.md", "new");
        let second = commit(&repo);
        let cache = RepoCache::new(t.path().join("cache"), AuthConfig::default());
        let pin = GitRef {
            commit: Some(first.to_string()[..10].to_string()),
            ..origin
        };
        let dir = cache.ensure(&pin).unwrap();
        assert_eq!(cache.read_file(&dir, "README.md").unwrap(), "hello");
        let cached = Repository::open(&dir).unwrap();
        cached
            .checkout_tree(cached.find_commit(second).unwrap().as_object(), None)
            .unwrap();
        cached.set_head_detached(second).unwrap();
        drop(cached);
        let repaired = cache.ensure(&pin).unwrap();
        assert_eq!(cache.read_file(&repaired, "README.md").unwrap(), "hello");
    }
    #[cfg(unix)]
    #[test]
    fn generator_refuses_symlinked_output_files() {
        let t = TempDir::new().unwrap();
        let root = t.path().join("repo");
        write(&root, "docs/README.md", "hello");
        write(t.path(), "outside.yaml", "keep me");
        std::os::unix::fs::symlink(
            t.path().join("outside.yaml"),
            root.join("docs/pinakes.yaml"),
        )
        .unwrap();
        assert!(generate_index(&root, &GenerateOptions::default()).is_err());
        assert_eq!(
            std::fs::read_to_string(t.path().join("outside.yaml")).unwrap(),
            "keep me"
        );
    }
    #[cfg(unix)]
    #[test]
    fn command_stderr_is_bounded() {
        let error = run_command(
            &policy("^/usr/bin/python3 "),
            &args(&[
                "/usr/bin/python3",
                "-c",
                "import sys; sys.stderr.write('x'*1000000); sys.exit(1)",
            ]),
            "",
            Path::new("/tmp"),
            &[],
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.ends_with("...[truncated]"));
        assert!(message.len() < 263000);
    }
    #[cfg(unix)]
    #[test]
    fn network_clone_can_checkout_a_historical_pin() {
        use std::{
            net::{TcpListener, TcpStream},
            process::{Command, Stdio},
        };
        let t = TempDir::new().unwrap();
        let (repo, origin) = fixture(t.path());
        let old = repo.head().unwrap().target().unwrap();
        write(repo.workdir().unwrap(), "README.md", "latest");
        commit(&repo);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        struct Daemon(std::process::Child);
        impl Drop for Daemon {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let _daemon = Daemon(
            Command::new("git")
                .args([
                    "daemon",
                    "--reuseaddr",
                    "--export-all",
                    "--listen=127.0.0.1",
                    &format!("--port={port}"),
                    &format!("--base-path={}", t.path().display()),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let start = Instant::now();
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "git daemon did not start"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let remote = GitRef {
            url: format!("git://127.0.0.1:{port}/repo"),
            commit: Some(old.to_string()),
            ..origin
        };
        let cache = RepoCache::new(t.path().join("cache"), AuthConfig::default());
        let dir = cache.ensure(&remote).unwrap();
        assert_eq!(cache.read_file(&dir, "README.md").unwrap(), "hello");
    }
    fn submodule_fixture(temp: &TempDir) -> GitRef {
        let child_path = temp.path().join("child");
        let child = Repository::init(&child_path).unwrap();
        write(&child_path, "README.md", "submodule contents");
        commit(&child);
        let (parent, mut reference) = fixture(temp.path());
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "../child",
                "docs",
            ])
            .current_dir(parent.workdir().unwrap())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut index = parent.index().unwrap();
        index.read(true).unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = parent.find_tree(tree_id).unwrap();
        let head = parent.head().unwrap().peel_to_commit().unwrap();
        let signature = Signature::now("review", "review@example.invalid").unwrap();
        parent
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Add submodule",
                &tree,
                &[&head],
            )
            .unwrap();
        reference.submodules = true;
        reference
    }

    #[test]
    fn relative_submodule_urls_are_resolved_before_authentication() {
        let temp = TempDir::new().unwrap();
        let mut reference = submodule_fixture(&temp);
        let cache = RepoCache::new(temp.path().join("cache"), AuthConfig::default());
        let checkout = cache.ensure(&reference).unwrap();
        assert_eq!(
            cache.read_file(&checkout, "docs/README.md").unwrap(),
            "submodule contents"
        );
        Repository::open(checkout.join("docs")).unwrap();
        cache
            .ensure_with(
                &reference,
                aiexandria::cache::RefreshPolicy::Every(Duration::from_nanos(1)),
            )
            .unwrap();
        reference.submodules = false;
        let without_submodules = cache.ensure(&reference).unwrap();
        assert_ne!(checkout, without_submodules);
        assert!(!without_submodules.join("docs/README.md").exists());
    }

    #[test]
    fn empty_central_token_is_an_error() {
        let mut auth = AuthConfig::default();
        auth.hosts.insert(
            "example.invalid".into(),
            aiexandria::auth::HostAuth {
                source: aiexandria::auth::TokenSource::Inline {
                    token: String::new(),
                },
                username: None,
            },
        );
        assert!(auth
            .resolve("https://example.invalid/docs.git", None)
            .is_err());
    }
    #[test]
    fn failed_submodule_fetch_preserves_process_working_directory() {
        const CHILD: &str = "AIEX_TEST_SUBMODULE_FAILURE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let temp = TempDir::new().unwrap();
            let reference = submodule_fixture(&temp);
            std::fs::rename(temp.path().join("child"), temp.path().join("unavailable")).unwrap();
            let cache = RepoCache::new(temp.path().join("cache"), AuthConfig::default());
            assert!(cache.ensure(&reference).is_err());
            return;
        }
        // Run in a separate process: changing CWD in a threaded test is unsafe.
        let cwd = TempDir::new().unwrap();
        write(
            cwd.path(),
            "docs/sentinel.md",
            "must survive a failed clone",
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::failed_submodule_fetch_preserves_process_working_directory",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .current_dir(cwd.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(cwd.path().join("docs/sentinel.md")).unwrap(),
            "must survive a failed clone"
        );
    }
}
