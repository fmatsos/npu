//! End-to-end verification of the backend `[headers]` feature (see
//! `docs/configuration.md`): resolved at preflight, never leaked on stderr.
//!
//! Same idiom as `tests/output_contract_e2e.rs`: a fake local HTTP backend,
//! a temporary `.npu/` scope, the real binary launched as a child process.

#![allow(clippy::expect_used)] // tolerated in tests (cf. Cargo.toml [lints.clippy]).

use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

fn fixture_scope(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-fixtures")
        .join(format!("e2e-headers-{name}-{n}"));
    std::fs::create_dir_all(&dir).expect("creating the temporary scope directory");
    dir
}

fn write(dir: &Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("creating the parent directory");
    }
    std::fs::write(path, contents).expect("writing the fixture");
}

fn write_scope(scope: &Path, addr: std::net::SocketAddr, headers_table: &str) {
    write(
        scope,
        ".npu/backends/stub.toml",
        &format!(
            r#"
            id = "stub"
            base_url = "http://{addr}"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"

            [headers]
            {headers_table}
            "#
        ),
    );
    write(
        scope,
        ".npu/models/test-model.toml",
        r#"
        id = "test-model"
        backend = "stub"
        operation = "chat"
        model = "test-model"
        "#,
    );
    write(
        scope,
        ".npu/commands/e2e-cmd.md",
        "---\nmodel = \"test-model\"\n---\n{{ input }}\n",
    );
}

const ACCEPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Starts a stubbed HTTP server that answers once, after capturing the
/// `Authorization` header it received (if any).
fn spawn_stub_server() -> (
    std::net::SocketAddr,
    std::thread::JoinHandle<()>,
    std::sync::mpsc::Receiver<Option<String>>,
) {
    use std::io::{BufRead, BufReader, Read};
    use std::time::Instant;

    let listener = TcpListener::bind("127.0.0.1:0").expect("binding the stubbed listener");
    let addr = listener
        .local_addr()
        .expect("getting the listener's local address");
    listener
        .set_nonblocking(true)
        .expect("switching the listener to non-blocking");
    let (tx, rx) = std::sync::mpsc::channel();

    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + ACCEPT_TIMEOUT;
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        // No connection ever came (e.g. the binary failed
                        // before reaching the network, which some tests
                        // below expect): nothing to capture.
                        let _ = tx.send(None);
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                #[allow(clippy::panic)]
                Err(err) => panic!("accepting the connection: {err}"),
            }
        };
        stream
            .set_nonblocking(false)
            .expect("switching the accepted stream back to blocking");
        let mut reader = BufReader::new(stream.try_clone().expect("cloning the TCP stream"));

        let mut content_length = 0usize;
        let mut authorization = None;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("reading a header line");
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or(0);
            }
            if let Some(value) = line
                .strip_prefix("Authorization:")
                .or_else(|| line.strip_prefix("authorization:"))
            {
                authorization = Some(value.trim().to_string());
            }
        }
        let mut body = vec![0u8; content_length];
        reader
            .read_exact(&mut body)
            .expect("reading the request body");
        tx.send(authorization).expect("sending captured header");

        let response_body = serde_json::json!({
            "choices": [
                { "message": { "role": "assistant", "content": "hi" } }
            ]
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        let mut stream = stream;
        stream
            .write_all(response.as_bytes())
            .expect("writing the stubbed response");
    });

    (addr, handle, rx)
}

fn run_npu(scope: &Path, args: &[&str], env_vars: &[(&str, &str)]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_npu"))
        .args(args)
        .current_dir(scope)
        .env("HOME", scope)
        .env_remove("XDG_CONFIG_HOME")
        .envs(env_vars.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launching the npu binary");

    {
        let stdin = child.stdin.as_mut().expect("stdin of the child process");
        stdin.write_all(b"hello").expect("writing to npu's stdin");
    }
    drop(child.stdin.take());

    child.wait_with_output().expect("waiting for npu to exit")
}

#[test]
fn a_resolved_header_reaches_the_backend() {
    let scope = fixture_scope("resolved");
    let (addr, server, rx) = spawn_stub_server();
    write_scope(
        &scope,
        addr,
        r#"Authorization = "Bearer {{ env.NPU_TEST_SECRET }}""#,
    );

    let output = run_npu(&scope, &["e2e-cmd"], &[("NPU_TEST_SECRET", "s3cr3t-value")]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let captured = rx.recv().expect("the stub must report what it captured");
    assert_eq!(captured.as_deref(), Some("Bearer s3cr3t-value"));
    server.join().expect("server thread must not panic");
}

#[test]
fn a_missing_env_var_fails_at_preflight_exit_two_empty_stdout_no_network() {
    let scope = fixture_scope("missing-var");
    let (addr, server, rx) = spawn_stub_server();
    write_scope(
        &scope,
        addr,
        r#"Authorization = "Bearer {{ env.NPU_TEST_UNDEFINED_SECRET }}""#,
    );

    let output = run_npu(&scope, &["e2e-cmd"], &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("NPU_TEST_UNDEFINED_SECRET"));

    // The stub must never have received a connection: preflight rejects
    // the missing variable before the network is ever reached.
    assert_eq!(rx.recv().expect("the stub thread must report"), None);
    server.join().expect("server thread must not panic");
}

#[test]
fn a_header_value_never_appears_on_stderr_even_at_verbose_info() {
    let scope = fixture_scope("no-leak");
    let (addr, server, rx) = spawn_stub_server();
    write_scope(
        &scope,
        addr,
        r#"Authorization = "Bearer {{ env.NPU_TEST_SECRET }}""#,
    );

    let output = run_npu(
        &scope,
        &["--verbose", "info", "e2e-cmd"],
        &[("NPU_TEST_SECRET", "s3cr3t-value")],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("s3cr3t-value"),
        "the resolved header value must never appear on stderr, got: {stderr}"
    );
    assert!(
        stderr.contains("Authorization"),
        "the trace line must still name the header, got: {stderr}"
    );

    let captured = rx.recv().expect("the stub must report what it captured");
    assert_eq!(captured.as_deref(), Some("Bearer s3cr3t-value"));
    server.join().expect("server thread must not panic");
}
