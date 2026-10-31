//! Real Git and independent-process regressions for the published cache.
use aiexandria::auth::AuthConfig;
use aiexandria::cache::{Freshness, RefreshPolicy, RepoCache};
use aiexandria::index::GitRef;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn git(dir: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().into()
}
struct Fixture {
    temp: tempfile::TempDir,
    repo: PathBuf,
    origin: GitRef,
    cache: RepoCache,
}
impl Fixture {
    fn new(format: &str, filter: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("remote");
        std::fs::create_dir(&repo).unwrap();
        git(
            &repo,
            &["init", "-b", "main", &format!("--object-format={format}")],
        );
        git(
            &repo,
            &[
                "config",
                "uploadpack.allowFilter",
                if filter { "true" } else { "false" },
            ],
        );
        std::fs::write(repo.join("pinakes.yaml"), "version: 1\nname: Fixture\nentries:\n  - {id: doc, name: Doc, kind: doc, path: doc.md}\n").unwrap();
        std::fs::write(repo.join("doc.md"), "old evidence\n").unwrap();
        std::fs::write(repo.join("extra.md"), "old extra\n").unwrap();
        std::fs::write(repo.join("blob.bin"), vec![123; 1_000_000]).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "fixture"]);
        let origin = GitRef {
            url: url::Url::from_directory_path(&repo).unwrap().into(),
            branch: Some("main".into()),
            submodules: false,
            ..Default::default()
        };
        let cache = RepoCache::new(temp.path().join("cache"), AuthConfig::default());
        Self {
            temp,
            repo,
            origin,
            cache,
        }
    }
    fn request(&self) -> RepoCache {
        self.cache.session(
            Freshness::CachedOk,
            Instant::now() + Duration::from_secs(20),
        )
    }
    fn change(&self) {
        std::fs::write(self.repo.join("doc.md"), "new evidence\n").unwrap();
        std::fs::write(self.repo.join("extra.md"), "new extra\n").unwrap();
        git(&self.repo, &["add", "."]);
        git(&self.repo, &["commit", "-m", "change"]);
    }
    fn force(&self) -> PathBuf {
        self.cache
            .update(
                &self.origin,
                RefreshPolicy::Never,
                aiexandria::cache::Selection::path("doc.md").unwrap(),
                Some(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64,
                ),
                Instant::now() + Duration::from_secs(20),
            )
            .unwrap()
    }
}
#[test]
fn sha256_branches_and_tags() {
    let f = Fixture::new("sha256", true);
    let dir = f
        .request()
        .ensure_path(&f.origin, RefreshPolicy::Never, "doc.md")
        .unwrap()
        .0;
    assert_eq!(git(&dir, &["rev-parse", "--show-object-format"]), "sha256");
    git(&f.repo, &["tag", "release"]);
    let tag = GitRef {
        branch: None,
        tag: Some("release".into()),
        ..f.origin.clone()
    };
    f.request()
        .ensure_path(&tag, RefreshPolicy::Never, "doc.md")
        .unwrap();
    f.change();
    let newer = f.force();
    assert_eq!(
        f.cache.read_file(&newer, "doc.md").unwrap(),
        "new evidence\n"
    );
    assert_eq!(f.cache.read_file(&dir, "doc.md").unwrap(), "old evidence\n");
}
#[test]
fn partial_download_and_followup_expansion_keep_the_selected_revision() {
    let f = Fixture::new("sha1", true);
    let request = f.request();
    let dir = request
        .ensure_path(&f.origin, RefreshPolicy::Never, "doc.md")
        .unwrap()
        .0;
    assert!(!dir.join("blob.bin").exists());
    let blob = git(&f.repo, &["rev-parse", "HEAD:blob.bin"]);
    assert!(
        git(&dir, &["rev-list", "--objects", "--missing=print", "HEAD"])
            .contains(&format!("?{blob}"))
    );
    let token = request.response_metadata()["snapshot"]
        .as_str()
        .unwrap()
        .to_owned();
    f.change();
    let current = f.force();
    let followup = f.request();
    followup.apply_snapshot(&f.origin, &token).unwrap();
    let old = followup
        .ensure_path(&f.origin, RefreshPolicy::Never, "extra.md")
        .unwrap()
        .0;
    assert_eq!(f.cache.read_file(&old, "extra.md").unwrap(), "old extra\n");
    assert_eq!(
        f.cache.read_file(&current, "doc.md").unwrap(),
        "new evidence\n"
    );
    assert_eq!(f.force(), current, "no-change refresh rebuilt a snapshot");
}
fn lock(path: &Path) -> File {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    file.lock().unwrap();
    file
}
#[test]
fn cached_reads_remain_available_during_an_owned_refresh() {
    let f = Fixture::new("sha1", false);
    let dir = f
        .request()
        .ensure_path(&f.origin, RefreshPolicy::Never, "doc.md")
        .unwrap()
        .0;
    assert_eq!(
        std::fs::read_to_string(dir.join(".git/aiexandria-filter")).unwrap(),
        "unsupported"
    );
    let owner = lock(&dir.parent().unwrap().join("job.lock"));
    let start = Instant::now();
    let cached = f
        .request()
        .ensure_path(
            &f.origin,
            RefreshPolicy::Every(Duration::from_nanos(1)),
            "doc.md",
        )
        .unwrap();
    assert!(start.elapsed() < Duration::from_millis(250));
    assert!(cached.1);
    let fresh = f.cache.session(
        Freshness::RequireFresh,
        Instant::now() + Duration::from_millis(80),
    );
    assert!(matches!(
        fresh.ensure_path(
            &f.origin,
            RefreshPolicy::Every(Duration::from_nanos(1)),
            "doc.md"
        ),
        Err(aiexandria::error::AIexError::RefreshPending)
    ));
    drop(owner);
}
#[test]
fn bulk_updates_coalesce_across_processes_and_wait_rechecks_force() {
    let f = Fixture::new("sha1", true);
    let dir = f
        .request()
        .ensure_path(&f.origin, RefreshPolicy::Never, "pinakes.yaml")
        .unwrap()
        .0;
    let registry = f.temp.path().join("registry.yaml");
    std::fs::write(&registry, format!("indexes:\n  docs:\n    repo: {{url: '{}', branch: main, submodules: false}}\n    path: pinakes.yaml\n    refresh: never\n", f.origin.url)).unwrap();
    let owner = lock(&dir.parent().unwrap().join("job.lock"));
    let spawn = |wait: bool| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_aiexandria"));
        cmd.args(["--registry"])
            .arg(&registry)
            .arg("--cache")
            .arg(f.cache.root())
            .args([
                "update",
                "--index",
                "docs",
                "--force",
                "--json",
                "--timeout",
                "15s",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if wait {
            cmd.arg("--wait");
        }
        cmd.spawn().unwrap()
    };
    let first = spawn(false);
    let limit = Instant::now() + Duration::from_secs(5);
    while !f.cache.root().join("update-scope.json").exists() {
        assert!(Instant::now() < limit);
        std::thread::sleep(Duration::from_millis(10));
    }
    let duplicate = spawn(false).wait_with_output().unwrap();
    assert!(duplicate.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&duplicate.stdout).unwrap()["status"],
        "already_running"
    );
    let waiting = spawn(true);
    std::thread::sleep(Duration::from_millis(100));
    drop(owner);
    for child in [first, waiting] {
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap()["status"],
            "completed"
        );
    }
}

#[test]
fn interactive_demand_promotes_an_updater_queued_behind_background_slots() {
    let f = Fixture::new("sha1", true);
    let dir = f.request().ensure(&f.origin).unwrap();
    let registry = f.temp.path().join("registry.yaml");
    std::fs::write(&registry, format!("indexes:\n  docs:\n    repo: {{url: '{}', branch: main, submodules: false}}\n    path: pinakes.yaml\n    refresh: never\n", f.origin.url)).unwrap();
    let _slot0 = lock(&f.cache.root().join("v3-slot-0.lock"));
    let _slot1 = lock(&f.cache.root().join("v3-slot-1.lock"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aiexandria"));
    let update = cmd
        .arg("--registry")
        .arg(&registry)
        .arg("--cache")
        .arg(f.cache.root())
        .args(["update", "--force", "--json", "--timeout", "15s"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let limit = Instant::now() + Duration::from_secs(5);
    while !dir.parent().unwrap().join("queued.json").exists() {
        assert!(Instant::now() < limit);
        std::thread::sleep(Duration::from_millis(10));
    }
    let foreground = f.cache.session(
        Freshness::RequireFresh,
        Instant::now() + Duration::from_secs(10),
    );
    foreground
        .ensure_path(
            &f.origin,
            RefreshPolicy::Every(Duration::from_millis(1)),
            "doc.md",
        )
        .unwrap();
    let output = update.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn credential_helper_scopes_explicit_tokens_and_uses_host_credentials_for_other_repositories() {
    use std::io::Write;
    let invoke = |path: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_aiexandria"))
            .args(["__git-credential", "get"])
            .env("AIEX_TEST_TOKEN", "explicit-fixture-secret")
            .env(
                "AIEXANDRIA_CREDENTIAL_ORIGIN",
                r#"{"url":"https://example.invalid/team/docs","token_env":"AIEX_TEST_TOKEN"}"#,
            )
            .env(
                "AIEXANDRIA_CREDENTIAL_AUTH",
                r#"{"hosts":{"example.invalid":{"token":"host-fixture-secret"}}}"#,
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        write!(
            child.stdin.take().unwrap(),
            "protocol=https\nhost=example.invalid\npath={path}\n\n"
        )
        .unwrap();
        let result = child.wait_with_output().unwrap();
        assert!(result.status.success());
        String::from_utf8(result.stdout).unwrap()
    };
    assert!(invoke("team/docs").contains("explicit-fixture-secret"));
    let other = invoke("team/submodule");
    assert!(other.contains("host-fixture-secret"));
    assert!(!other.contains("explicit-fixture-secret"));
}

#[cfg(unix)]
#[test]
fn git_deadline_terminates_descendants_and_redacts_explicit_credentials() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("fake-git");
    let pid = temp.path().join("child.pid");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nsleep 20 &\nprintf '%s' $! > '{}'\nwait\n",
            pid.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut backend = aiexandria::git_cli::GitCli::default();
    backend.executable = script.clone();
    let begin = Instant::now();
    assert!(backend
        .run(
            None,
            &[],
            None,
            &AuthConfig::default(),
            None,
            begin + Duration::from_millis(250)
        )
        .is_err());
    assert!(begin.elapsed() < Duration::from_secs(2));
    let child = std::fs::read_to_string(pid).unwrap();
    // A killed orphan can briefly be a zombie until init reaps it.
    let state = Command::new("ps")
        .args(["-o", "stat=", "-p", child.trim()])
        .output()
        .unwrap();
    let state = String::from_utf8(state.stdout).unwrap();
    assert!(
        state.trim().is_empty() || state.trim().starts_with('Z'),
        "child still running: {state}"
    );
    std::fs::write(&script, "#!/bin/sh\necho fixture-secret >&2\nexit 1\n").unwrap();
    let auth = AuthConfig {
        hosts: [(
            "example.invalid".into(),
            aiexandria::auth::HostAuth {
                source: aiexandria::auth::TokenSource::Inline {
                    token: "fixture-secret".into(),
                },
                username: None,
            },
        )]
        .into_iter()
        .collect(),
    };
    let origin = GitRef {
        url: "https://example.invalid/docs".into(),
        ..Default::default()
    };
    let error = backend
        .run(
            None,
            &[],
            Some(&origin),
            &auth,
            None,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap_err()
        .to_string();
    assert!(!error.contains("fixture-secret"));
    assert!(error.contains("[redacted]"));
}

#[test]
fn submodule_objects_are_reused_without_modifying_published_content() {
    let mut f = Fixture::new("sha1", true);
    let child = Fixture::new("sha1", false);
    git(
        &f.repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            child.repo.to_str().unwrap(),
            "deps/docs",
        ],
    );
    git(&f.repo, &["commit", "-am", "submodule"]);
    f.origin.submodules = true;
    let old = f.request().ensure(&f.origin).unwrap();
    child.change();
    git(
        &f.repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--remote",
            "deps/docs",
        ],
    );
    git(&f.repo, &["commit", "-am", "advance submodule"]);
    let new = f.force();
    assert_eq!(
        f.cache.read_file(&new, "deps/docs/doc.md").unwrap(),
        "new evidence\n"
    );
    assert_eq!(
        f.cache.read_file(&old, "deps/docs/doc.md").unwrap(),
        "old evidence\n"
    );
}

#[cfg(unix)]
#[test]
fn killed_updater_keeps_ownership_until_its_git_descendant_exits() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new("sha1", true);
    let dir = f.request().ensure(&f.origin).unwrap();
    let registry = f.temp.path().join("registry.yaml");
    std::fs::write(&registry, format!("indexes:\n  docs:\n    repo: {{url: '{}', branch: main, submodules: false}}\n    path: pinakes.yaml\n    refresh: never\n", f.origin.url)).unwrap();
    let marker = f.temp.path().join("fetch-started");
    let script = f.temp.path().join("delayed-git");
    std::fs::write(&script, format!("#!/bin/sh\ncase \" $* \" in\n  *' fetch '*) touch '{}'; sleep 2; exit 1 ;;\nesac\nexec git \"$@\"\n", marker.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_aiexandria"))
        .arg("--registry")
        .arg(&registry)
        .arg("--cache")
        .arg(f.cache.root())
        .arg("--git")
        .arg(&script)
        .args(["update", "--force", "--timeout", "15s"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let limit = Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        assert!(Instant::now() < limit);
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let ownership = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.parent().unwrap().join("job.lock"))
        .unwrap();
    assert!(
        matches!(ownership.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
        "parent crash released ownership while Git was alive"
    );
    let limit = Instant::now() + Duration::from_secs(5);
    while ownership.try_lock().is_err() {
        assert!(Instant::now() < limit);
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(ownership);
    f.change();
    let refreshed = f.force();
    assert_eq!(
        f.cache.read_file(&refreshed, "doc.md").unwrap(),
        "new evidence\n"
    );
}

#[test]
fn remote_generation_uses_an_isolated_copy() {
    let f = Fixture::new("sha1", true);
    let dir = f.request().ensure(&f.origin).unwrap();
    let before = std::fs::read(dir.join("pinakes.yaml")).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_aiexandria"))
        .arg("--cache")
        .arg(f.cache.root())
        .args([
            "generate",
            "--repo",
            &f.origin.url,
            "--branch",
            "main",
            "--no-submodules",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!result.stdout.is_empty());
    assert_eq!(std::fs::read(dir.join("pinakes.yaml")).unwrap(), before);
}

#[test]
fn submodule_host_credentials_partition_shared_cache_profiles() {
    let f = Fixture::new("sha1", true);
    let registry = f.temp.path().join("registry.yaml");
    let auth = f.temp.path().join("auth.yaml");
    std::fs::write(&registry, format!("indexes:\n  docs:\n    repo: {{url: '{}', branch: main, submodules: false}}\n    path: pinakes.yaml\n    refresh: never\n", f.origin.url)).unwrap();
    // The root has no credential, but a referenced submodule may use this host.
    std::fs::write(
        &auth,
        "hosts:\n  elsewhere.invalid:\n    token_env: AIEX_PROFILE_TEST_TOKEN\n",
    )
    .unwrap();
    for token in ["profile-one", "profile-two", "profile-two"] {
        let result = Command::new(env!("CARGO_BIN_EXE_aiexandria"))
            .arg("--registry")
            .arg(&registry)
            .arg("--auth")
            .arg(&auth)
            .arg("--cache")
            .arg(f.cache.root())
            .args(["update", "--json"])
            .env("AIEX_PROFILE_TEST_TOKEN", token)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let profiles = std::fs::read_dir(f.cache.root().join("v3"))
        .unwrap()
        .filter(|e| e.as_ref().unwrap().path().join("state.json").exists())
        .count();
    assert_eq!(
        profiles, 2,
        "different environment credentials reused one cache profile"
    );
}

#[path = "cache_cli/pins.rs"]
mod pins;
