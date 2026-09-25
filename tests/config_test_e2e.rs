#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn scope(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-fixtures");
    std::fs::create_dir_all(&fixtures).expect("fixture parent");
    loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = fixtures.join(format!("config-test-{name}-{n}"));
        match std::fs::create_dir(&path) {
            Ok(()) => {
                std::fs::create_dir(path.join(".npu")).expect("create scope");
                return path;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => panic!("create fixture: {e}"),
        }
    }
}

fn write(root: &Path, name: &str, text: &str) {
    let path = root.join(".npu").join(name);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create directory");
    std::fs::write(path, text).expect("write fixture");
}

fn configure(root: &Path, port: u16, format: &str) {
    write(
        root,
        "backends/stub.toml",
        &format!(
            "id = \"stub\"\nbase_url = \"http://127.0.0.1:{port}\"\ntype = \"openai-compatible\"\n[operations.chat]\nmethod = \"POST\"\npath = \"/v1/chat/completions\"\n"
        ),
    );
    write(
        root,
        "models/first.toml",
        "id = \"first\"\nbackend = \"stub\"\noperation = \"chat\"\nmodel = \"first\"\n",
    );
    write(
        root,
        "models/second.toml",
        "id = \"second\"\nbackend = \"stub\"\noperation = \"chat\"\nmodel = \"second\"\n",
    );
    write(
        root,
        "commands/git/review.md",
        &format!(
            "---\nmodel = \"first\"\n[args.kind]\nrequired = true\n[output]\nformat = \"{format}\"\n---\nClassify {{{{ input }}}} as {{{{ args.kind }}}}\n"
        ),
    );
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_npu"))
        .args(args)
        .current_dir(root)
        .env("HOME", root)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("APPDATA")
        .output()
        .expect("run npu")
}

fn stub(answers: Vec<&'static str>) -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local stub");
    let port = listener.local_addr().expect("local address").port();
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        for content in answers {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing test request");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            };
            stream.set_nonblocking(false).expect("blocking stream");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read request header");
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().expect("body length");
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("read request body");
            let response =
                serde_json::json!({"choices":[{"message":{"role":"assistant","content":content}}]})
                    .to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).expect("write response");
        }
    });
    (port, handle)
}

#[test]
fn passing_case_and_model_override_have_one_request_and_json_report() {
    let (port, server) = stub(vec![r#"{"category":"hardware","confidence":0.9}"#]);
    let root = scope("pass");
    configure(&root, port, "json");
    write(
        &root,
        "tests/git/review/hardware.toml",
        "args = { kind = \"ticket\" }\ninput = \"printer\"\n[expect]\n\"/category\" = \"hardware\"\n\"/confidence\" = { min = 0.8 }\n",
    );
    let result = run(
        &root,
        &[
            "config", "test", "git", "review", "--model", "second", "--json",
        ],
    );
    assert_eq!(
        result.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&result.stdout).expect("json report");
    assert_eq!(rows[0]["command"], "git/review");
    assert_eq!(rows[0]["result"], "pass");
    assert_eq!(rows[0]["distinct_outputs"], 1);
    server.join().expect("stub completed");
}

#[test]
fn failed_expectation_continues_and_repeat_counts_distinct_outputs() {
    let (port, server) = stub(vec![
        "feat: one",
        "feat: two",
        "feat: one",
        "feat: two",
        "feat: one",
        "feat: two",
    ]);
    let root = scope("repeat");
    configure(&root, port, "text");
    write(
        &root,
        "tests/git/review/a.toml",
        "args = { kind = \"ticket\" }\ninput = \"printer\"\n[expect]\ncontains = [\"missing\"]\n",
    );
    write(
        &root,
        "tests/git/review/b.toml",
        "args = { kind = \"ticket\" }\ninput = \"printer\"\n[expect]\ncontains = [\"feat:\"]\n",
    );
    let result = run(&root, &["config", "test", "--repeat", "3", "--json"]);
    assert_eq!(
        result.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&result.stdout).expect("json report");
    assert_eq!(rows[0]["result"], "fail");
    assert_eq!(rows[1]["result"], "pass");
    assert_eq!(rows[0]["distinct_outputs"], 2);
    assert_eq!(rows[1]["distinct_outputs"], 2);
    server.join().expect("stub completed");
}

#[test]
fn malformed_case_stops_before_network_and_names_its_file() {
    let root = scope("malformed");
    configure(&root, 9, "text");
    write(
        &root,
        "tests/git/review/bad.toml",
        "args = { kind = \"ticket\" }\ninput = \"printer\"\n[expect]\n\"/category\" = \"hardware\"\n",
    );
    let result = run(&root, &["config", "test"]);
    assert_eq!(result.status.code(), Some(2));
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("bad.toml"));
}

#[test]
fn dry_run_renders_prompt_without_contacting_backend() {
    let root = scope("dry-run");
    configure(&root, 9, "text");
    write(
        &root,
        "tests/git/review/a.toml",
        "args = { kind = \"ticket\" }\ninput = \"printer\"\n[expect]\ncontains = [\"feat:\"]\n",
    );
    let result = run(&root, &["config", "test", "--dry-run", "--json"]);
    assert_eq!(
        result.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&result.stdout).expect("json report");
    assert_eq!(
        rows[0]["messages"][0]["content"],
        "Classify printer as ticket"
    );
}

#[test]
fn missing_cases_and_unreachable_backend_keep_stdout_empty() {
    let root = scope("missing");
    configure(&root, 9, "text");
    let missing = run(&root, &["config", "test"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(missing.stdout.is_empty());
    write(
        &root,
        "tests/git/review/a.toml",
        "args = { kind = \"ticket\" }\ninput = \"printer\"\n[expect]\ncontains = [\"feat:\"]\n",
    );
    let unreachable = run(&root, &["config", "test"]);
    assert_eq!(unreachable.status.code(), Some(3));
    assert!(unreachable.stdout.is_empty());
}

#[test]
fn expected_output_contract_failure_is_a_passing_case() {
    let (port, server) = stub(vec!["one\ntwo"]);
    let root = scope("expected-output-error");
    configure(&root, port, "text");
    write(
        &root,
        "commands/git/review.md",
        "---\nmodel = \"first\"\n[args.kind]\nrequired = true\n[output]\nformat = \"text\"\nmax_lines = 1\n---\n{{ input }} {{ args.kind }}\n",
    );
    write(
        &root,
        "tests/git/review/a.toml",
        "args = { kind = \"ticket\" }\ninput = \"printer\"\n[expect]\nexit_code = 4\n",
    );
    let result = run(&root, &["config", "test", "--json"]);
    assert_eq!(
        result.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&result.stdout).expect("json report");
    assert_eq!(rows[0]["result"], "pass");
    assert_eq!(rows[0]["distinct_outputs"], 0);
    server.join().expect("stub completed");
}

#[test]
fn failing_json_pointer_returns_four_and_names_the_case() {
    let (port, server) = stub(vec![r#"{"category":"software"}"#]);
    let root = scope("pointer-failure");
    configure(&root, port, "json");
    write(
        &root,
        "tests/git/review/hardware.toml",
        "args = { kind = \"ticket\" }\ninput = \"printer\"\n[expect]\n\"/category\" = \"hardware\"\n",
    );
    let result = run(&root, &["config", "test", "--json"]);
    assert_eq!(result.status.code(), Some(4));
    let rows: serde_json::Value = serde_json::from_slice(&result.stdout).expect("json report");
    assert_eq!(rows[0]["result"], "fail");
    assert_eq!(rows[0]["case"], "hardware");
    assert!(
        rows[0]["message"]
            .as_str()
            .expect("failure reason")
            .contains("/category")
    );
    server.join().expect("stub completed");
}

#[test]
fn local_case_replaces_same_named_user_case() {
    let root = scope("scopes");
    let user = scope("user");
    configure(&root, 9, "text");
    write(
        &user,
        "tests/git/review/a.toml",
        "input = \"user\"\n[expect]\ncontains = [\"user\"]\n",
    );
    write(
        &root,
        "tests/git/review/a.toml",
        "args = { kind = \"ticket\" }\ninput = \"local\"\n[expect]\ncontains = [\"local\"]\n",
    );
    write(
        &user,
        "tests/git/review/b.toml",
        "args = { kind = \"ticket\" }\ninput = \"other\"\n[expect]\ncontains = [\"other\"]\n",
    );
    std::fs::rename(user.join(".npu"), user.join("npu")).expect("make XDG user scope");
    let mut command = Command::new(env!("CARGO_BIN_EXE_npu"));
    command
        .args(["config", "test", "--dry-run", "--json"])
        .current_dir(&root)
        .env("HOME", &root);
    if cfg!(windows) {
        command.env("APPDATA", &user).env_remove("XDG_CONFIG_HOME");
    } else {
        command.env("XDG_CONFIG_HOME", &user).env_remove("APPDATA");
    }
    let result = command.output().expect("run npu");
    assert_eq!(
        result.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&result.stdout).expect("json report");
    assert_eq!(rows.as_array().expect("rows").len(), 2);
    assert_eq!(
        rows[0]["messages"][0]["content"],
        "Classify local as ticket"
    );
    assert_eq!(
        rows[1]["messages"][0]["content"],
        "Classify other as ticket"
    );
}

#[test]
fn file_input_is_relative_to_case_and_unknown_case_key_is_rejected() {
    let root = scope("fixture-input");
    configure(&root, 9, "text");
    write(&root, "tests/git/review/fixture.txt", "from fixture");
    write(
        &root,
        "tests/git/review/a.toml",
        "args = { kind = \"ticket\" }\ninput = { file = \"fixture.txt\" }\n[expect]\ncontains = [\"feat:\"]\n",
    );
    let dry = run(&root, &["config", "test", "--dry-run", "--json"]);
    assert_eq!(dry.status.code(), Some(0));
    let rows: serde_json::Value = serde_json::from_slice(&dry.stdout).expect("json report");
    assert_eq!(
        rows[0]["messages"][0]["content"],
        "Classify from fixture as ticket"
    );
    write(
        &root,
        "tests/git/review/a.toml",
        "args = { kind = \"ticket\" }\ninput = \"inline\"\nunknwon = true\n[expect]\n",
    );
    let invalid = run(&root, &["config", "test"]);
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("a.toml"));
}
