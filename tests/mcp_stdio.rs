//! MCP wire-protocol smoke test: launch the compiled `aiexandria` binary and
//! drive it through `initialize` -> `tools/list` -> a few `tools/call` requests
//! over stdio. Validates that JSON-RPC framing, schema, and tool dispatch all
//! work end-to-end.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use git2::{Repository, Signature};
use serde_json::{json, Value};
use tempfile::TempDir;

fn write(path: &Path, body: &str) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

fn make_repo(dir: &Path) -> String {
    write(
        &dir.join("pinakes.yaml"),
        r#"
version: 1
name: smoke
command_policy:
  allow: '^echo '
entries:
  - id: readme
    name: README
    kind: doc
    path: README.md
  - id: greet
    name: greet
    kind: command
    argv: ["echo", "hi {query}"]
"#,
    );
    write(&dir.join("README.md"), "smoke readme\n");

    let repo = Repository::init_opts(dir, git2::RepositoryInitOptions::new().initial_head("main"))
        .unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let sig = Signature::now("t", "t@t").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();
    format!("file://{}", dir.display())
}

fn read_response(reader: &mut BufReader<std::process::ChildStdout>, deadline: Instant) -> Value {
    let mut line = String::new();
    while Instant::now() < deadline {
        line.clear();
        let n = reader.read_line(&mut line).unwrap();
        if n == 0 {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        return serde_json::from_str(&line).unwrap();
    }
    panic!("timed out waiting for response");
}

#[test]
fn mcp_stdio_round_trip() {
    let tmp = TempDir::new().unwrap();
    let url = make_repo(&tmp.path().join("repo"));

    let registry = tmp.path().join("registry.yaml");
    write(
        &registry,
        &format!(
            "indexes:\n  smoke:\n    repo:\n      url: {url}\n      branch: main\n    path: pinakes.yaml\n"
        ),
    );

    let bin = env!("CARGO_BIN_EXE_aiexandria");
    let mut child = Command::new(bin)
        .arg("--registry")
        .arg(&registry)
        .arg("--cache")
        .arg(tmp.path().join("cache"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    let send = |stdin: &mut std::process::ChildStdin, v: Value| {
        let s = serde_json::to_string(&v).unwrap();
        writeln!(stdin, "{s}").unwrap();
        stdin.flush().unwrap();
    };

    send(
        &mut stdin,
        json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}
        }),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let init = read_response(&mut stdout, deadline);
    assert_eq!(init["id"], 1);
    assert!(init["result"]["serverInfo"].is_object());

    send(
        &mut stdin,
        json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
    );

    send(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let list = read_response(&mut stdout, deadline);
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in ["list_indexes", "list_docs", "read_doc", "search_docs"] {
        assert!(names.contains(&expected), "missing tool {expected}");
    }

    send(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
               "params":{"name":"read_doc","arguments":{"index":"smoke","path":[],"entry_id":"readme"}}}),
    );
    let read = read_response(&mut stdout, deadline);
    let text = read["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("smoke readme"), "got: {text}");

    send(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
               "params":{"name":"search_docs","arguments":{"index":"smoke","path":[],"query":"world","command_id":"greet"}}}),
    );
    let cmd = read_response(&mut stdout, deadline);
    let cmd_text = cmd["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(cmd_text.trim(), "hi world");

    drop(stdin);
    let _ = child.wait();
}
