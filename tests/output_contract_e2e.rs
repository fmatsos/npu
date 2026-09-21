//! End-to-end verification of the output contract — the COMPLETE
//! pipeline, not the units of
//! `output.rs` or `command.rs` taken in isolation.
//!
//! Each test:
//! 1. mounts a fake local HTTP backend (`std::net::TcpListener` in a thread,
//!    same idiom as
//!    `backend::tests::chat_end_to_end_against_stubbed_http_server`) that
//!    answers with a fixed `chat/completions` response;
//! 2. writes a temporary `.npu/` scope under `target/` (backend, model and
//!    command) pointing at that fake backend;
//! 3. runs the REAL `npu` binary (`env!("CARGO_BIN_EXE_npu")`, never a
//!    function called directly in this test process) with that scope as
//!    the current directory, and checks the exit code, stdout and stderr.
//!
//! `HOME` is redirected to the temporary scope itself (which never contains
//! `.config/npu`) and `XDG_CONFIG_HOME` is removed from the child's
//! environment: only the temporary scope root (`<scope>/.npu`, via the
//! current directory) must be taken into account by `scope::roots()`, never
//! the real `$HOME` nor an `/etc/npu` that might otherwise exist on the
//! machine. `Command::env`/`env_remove` only touch the CHILD PROCESS's
//! environment: no test mutates the real environment variables
//! (`std::env::set_var` is `unsafe` in edition 2024, forbidden by
//! `unsafe_code = "forbid"`, cf. Cargo.toml).
//!
//! No test calls a real network backend: the only network touched is the
//! stubbed, local listener created by the test itself.

#![allow(clippy::expect_used)] // tolerated in tests (cf. Cargo.toml [lints.clippy]).

use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// Creates a unique temporary scope directory under `target/`, distinct
/// from the versioned `.npu/` fixture — same idiom as the
/// `command::tests`/`config::tests`/`tests/cli.rs` fixtures.
fn fixture_scope(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-fixtures")
        .join(format!("e2e-output-contract-{name}-{n}"));
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

/// Writes a complete `.npu` scope (backend + model) pointing at the fake
/// HTTP backend `addr`, plus the `e2e-cmd` fixture command whose `[output]`
/// section is `output_section` (its TOML content, WITHOUT the `[output]`
/// brackets themselves — every caller of this module supplies a non-empty
/// one, cf. the four scenarios below).
///
/// `scope` is the current directory the binary receives (`run_npu`), NOT
/// the scope root itself: `scope::roots()` (`src/scope.rs`) computes the
/// local root as `<cwd>/.npu`, never `<cwd>` directly — every path written
/// here must therefore be prefixed with `.npu/`, or the binary will find
/// neither backend, model, nor command (an exact bug reproduced and fixed
/// while writing this test: an earlier version wrote directly under
/// `<scope>/backends/...`, which `scope::roots()` ignores).
fn write_scope(scope: &Path, addr: std::net::SocketAddr, output_section: &str) {
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
        &format!(
            "+++\nmodel = \"test-model\"\n\n[output]\n{output_section}\n+++\n{{{{ input }}}}\n"
        ),
    );
}

/// Maximum delay granted to the stubbed server to receive the `npu`
/// binary's connection when launched by `run_npu`. Bounds the `accept()`
/// call (see below) rather than letting it block indefinitely: without this
/// bound, a future change that made `npu` fail BEFORE it contacts the
/// backend (a validation regression, for example) would not produce a red
/// test but a `cargo test` run that never finishes — a defect observed
/// empirically while writing this file (the scope resolution bug above,
/// diagnosed via `/proc/<pid>/task/*/wchan` after several minutes of
/// hanging).
const ACCEPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Starts a stubbed HTTP server that answers once with the
/// `chat/completions` body carrying `content` as the message, then
/// terminates. Same idiom as
/// `backend::tests::chat_end_to_end_against_stubbed_http_server`,
/// generalized to accept an arbitrary response content AND bound the wait
/// for a connection (`ACCEPT_TIMEOUT`, see its doc) rather than block
/// indefinitely.
fn spawn_stub_server(content: String) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
    use std::io::{BufRead, BufReader, Read};
    use std::time::Instant;

    let listener = TcpListener::bind("127.0.0.1:0").expect("binding the stubbed listener");
    let addr = listener
        .local_addr()
        .expect("getting the listener's local address");
    listener
        .set_nonblocking(true)
        .expect("switching the listener to non-blocking");

    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + ACCEPT_TIMEOUT;
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "no connection received on the stubbed listener within the \
                         {ACCEPT_TIMEOUT:?} deadline: the npu binary never contacted the \
                         backend (did it fail earlier in the pipeline?)"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                // Direct `panic!`: an accept error other than a plain `WouldBlock` (e.g.
                // the listener was closed) is a defect in the test itself, never a case to
                // propagate cleanly — same tolerance as for a `#[test]` (cf. Cargo.toml
                // `[lints.clippy]`), here necessarily local (`#[allow]` below) since this
                // code runs in the server thread, not in the `#[test]` function itself.
                #[allow(clippy::panic)]
                Err(err) => panic!("accepting the connection: {err}"),
            }
        };
        // The accepted stream may inherit the listener's non-blocking mode
        // depending on the platform: switch it back to blocking explicitly,
        // otherwise the header read below would fail immediately with
        // `WouldBlock` instead of waiting for the request.
        stream
            .set_nonblocking(false)
            .expect("switching the accepted stream back to blocking");
        let mut reader = BufReader::new(stream.try_clone().expect("cloning the TCP stream"));

        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("reading a header line");
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        reader
            .read_exact(&mut body)
            .expect("reading the request body");

        let response_body = serde_json::json!({
            "choices": [
                { "message": { "role": "assistant", "content": content } }
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

    (addr, handle)
}

/// Runs the REAL `npu` binary (compiled by cargo for this test run, never a
/// function called directly in this test process) with `scope` as the
/// current directory and `stdin_data` sent on its standard input, then
/// waits for it to finish and returns its complete output (code, stdout,
/// stderr).
fn run_npu(scope: &Path, args: &[&str], stdin_data: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_npu"))
        .args(args)
        .current_dir(scope)
        // Configuration scope isolation: only
        // <scope>/.npu (via the current directory) must be visible. `HOME`
        // is redirected to `scope` itself (which never contains
        // `.config/npu`) and `XDG_CONFIG_HOME` is removed, so that neither
        // the real $HOME nor an XDG_CONFIG_HOME inherited from the test's
        // environment introduce a stray scope root. This only touches the
        // CHILD PROCESS's environment, never the real environment
        // variables of this test process (`std::env::set_var` is `unsafe`,
        // forbidden here).
        .env("HOME", scope)
        .env_remove("XDG_CONFIG_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launching the npu binary");

    // Close stdin (drop) after writing: the fixture command's `stdin` input
    // mode reads until EOF (`read_to_string`), which never happens while
    // the descriptor stays open on the parent side.
    {
        let stdin = child.stdin.as_mut().expect("stdin of the child process");
        stdin
            .write_all(stdin_data.as_bytes())
            .expect("writing to npu's stdin");
    }
    drop(child.stdin.take());

    child
        .wait_with_output()
        .expect("waiting for the npu process to finish")
}

/// Writes a GENERAL scope (via `$XDG_CONFIG_HOME/npu`, never `<cwd>/.npu`)
/// containing a `never-invoked` command whose `[output].schema` points at
/// `schemas/broken-or-missing.json` — cf. `src/scope.rs::candidate_roots`:
/// `$XDG_CONFIG_HOME/npu` is a scope root in its own right, more general
/// than `<cwd>/.npu`, exactly the level targeted by the L3 review, fix 1
/// ("broken schema belonging to a command nobody invokes").
///
/// If `schema_body` is `Some`, the schema file is written with that content
/// (used to simulate syntactically broken JSON); if `None`, it is never
/// written at all (missing schema).
fn write_general_scope_with_never_invoked_command(
    xdg_root: &Path,
    base_url: &str,
    schema_body: Option<&str>,
) {
    write(
        xdg_root,
        "npu/backends/stub.toml",
        &format!(
            r#"
            id = "stub"
            base_url = "{base_url}"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"
            "#
        ),
    );
    write(
        xdg_root,
        "npu/models/test-model.toml",
        r#"
        id = "test-model"
        backend = "stub"
        operation = "chat"
        model = "test-model"
        "#,
    );
    write(
        xdg_root,
        "npu/commands/never-invoked.md",
        "+++\nmodel = \"test-model\"\n\n[output]\nformat = \"json\"\n\
         schema = \"schemas/broken-or-missing.json\"\n+++\n{{ input }}\n",
    );
    if let Some(body) = schema_body {
        write(xdg_root, "npu/schemas/broken-or-missing.json", body);
    }
}

/// Runs the REAL `npu` binary with `$XDG_CONFIG_HOME` pointed at
/// `xdg_config_home` (the GENERAL scope written by
/// `write_general_scope_with_never_invoked_command`) and `cwd` as the
/// current directory — a directory deliberately WITHOUT a local `.npu`, so
/// that the only scope root taken into account is `$XDG_CONFIG_HOME/npu`
/// (cf. `src/scope.rs::candidate_roots`). `HOME` is redirected to `cwd`
/// (which never contains `.config/npu`) for the same isolation reason as
/// `run_npu`.
fn run_npu_xdg(cwd: &Path, xdg_config_home: &Path, args: &[&str], stdin_data: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_npu"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", cwd)
        .env("XDG_CONFIG_HOME", xdg_config_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launching the npu binary");

    {
        let stdin = child.stdin.as_mut().expect("stdin of the child process");
        stdin
            .write_all(stdin_data.as_bytes())
            .expect("writing to npu's stdin");
    }
    drop(child.stdin.take());

    child
        .wait_with_output()
        .expect("waiting for the npu process to finish")
}

/// Creates a temporary working directory without a local `.npu`, distinct
/// from the general `$XDG_CONFIG_HOME/npu` scope — same idiom as
/// `fixture_scope`.
fn fixture_cwd_without_local_scope(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-fixtures")
        .join(format!("e2e-lazy-schema-cwd-{name}-{n}"));
    std::fs::create_dir_all(&dir).expect("creating the temporary working directory");
    dir
}

/// L3 review, fix 1, proof (a): a MISSING schema, declared by a command in
/// a general scope that nobody invokes, must no longer disable
/// `npu --help` for the whole CLI (lazy existence resolution, aligned with
/// lazy compilation — cf. `command::resolve_schema_path`).
#[test]
fn help_survives_a_missing_schema_declared_by_an_uninvoked_command_in_the_general_scope() {
    // The backend URL here is only an infrastructure detail required by
    // `write_general_scope_with_never_invoked_command` (the model needs a
    // valid backend for `config::load_scopes` to succeed): it is NEVER
    // contacted, `--help` never contacts any backend — no stubbed server to
    // mount here, unlike (c)/(d).
    let xdg = fixture_scope("xdg-missing-schema");
    let cwd = fixture_cwd_without_local_scope("missing-schema");
    // Schema never written: `schemas/broken-or-missing.json` is absent from
    // disk.
    write_general_scope_with_never_invoked_command(&xdg, "http://127.0.0.1:1", None);

    let output = run_npu_xdg(&cwd, &xdg, &["--help"], "");

    assert!(
        output.status.success(),
        "PROOF (a): npu --help must succeed (exit 0) even with a missing schema in a general \
         scope, got code {:?}; stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// L3 review, fix 1, proof (b): a schema that IS PRESENT but syntactically
/// BROKEN, declared by a command in a general scope that nobody invokes,
/// must not disable `npu --help` either — schema compilation stays lazy
/// (rule 4 of the shared contract), unchanged by this fix.
#[test]
fn help_survives_a_syntactically_broken_schema_declared_by_an_uninvoked_command_in_the_general_scope()
 {
    let xdg = fixture_scope("xdg-broken-schema");
    let cwd = fixture_cwd_without_local_scope("broken-schema");
    write_general_scope_with_never_invoked_command(
        &xdg,
        "http://127.0.0.1:1",
        Some("{ this is not JSON"),
    );

    let output = run_npu_xdg(&cwd, &xdg, &["--help"], "");

    assert!(
        output.status.success(),
        "PROOF (b): npu --help must succeed (exit 0) even with a syntactically broken schema \
         in a general scope, got code {:?}; stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// L3 review, fix 1, proof (c): actually invoking the command whose schema
/// is missing must fail with `Error::Config` (exit 2), naming both the
/// schema's resolved path AND the command file that requires it.
#[test]
fn invoking_the_command_with_a_missing_schema_fails_with_exit_code_two_naming_both_paths() {
    let (addr, server) = spawn_stub_server("{\"a\": 1}".to_string());

    let xdg = fixture_scope("xdg-missing-schema-invoked");
    let cwd = fixture_cwd_without_local_scope("missing-schema-invoked");
    write_general_scope_with_never_invoked_command(&xdg, &format!("http://{addr}"), None);

    let output = run_npu_xdg(&cwd, &xdg, &["never-invoked"], "whatever");
    server.join().expect("the server thread must not panic");

    assert_eq!(
        output.status.code(),
        Some(2),
        "PROOF (c): a missing schema discovered AT USE TIME must fail with code 2 \
         (Error::Config — the configuration is broken, not the model's response); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "nothing must be written to stdout when the output contract fails, got: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr must be valid UTF-8");
    assert!(
        stderr.contains("broken-or-missing.json"),
        "PROOF (c): stderr must name the schema's resolved path, got: {stderr}"
    );
    assert!(
        stderr.contains("never-invoked.md"),
        "PROOF (c): stderr must also name the offending command file, got: {stderr}"
    );
}

/// L3 review, fix 1, proof (d): same requirement as (c), for a schema that
/// IS PRESENT but syntactically BROKEN.
#[test]
fn invoking_the_command_with_a_broken_schema_fails_with_exit_code_two_naming_both_paths() {
    let (addr, server) = spawn_stub_server("{\"a\": 1}".to_string());

    let xdg = fixture_scope("xdg-broken-schema-invoked");
    let cwd = fixture_cwd_without_local_scope("broken-schema-invoked");
    write_general_scope_with_never_invoked_command(
        &xdg,
        &format!("http://{addr}"),
        Some("{ this is not JSON"),
    );

    let output = run_npu_xdg(&cwd, &xdg, &["never-invoked"], "whatever");
    server.join().expect("the server thread must not panic");

    assert_eq!(
        output.status.code(),
        Some(2),
        "PROOF (d): a broken schema discovered AT USE TIME must fail with code 2 \
         (Error::Config); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "nothing must be written to stdout when the output contract fails, got: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr must be valid UTF-8");
    assert!(
        stderr.contains("broken-or-missing.json"),
        "PROOF (d): stderr must name the schema's resolved path, got: {stderr}"
    );
    assert!(
        stderr.contains("never-invoked.md"),
        "PROOF (d): stderr must also name the offending command file, got: {stderr}"
    );
}

/// a) the model answers with JSON wrapped in a Markdown fence -> stdout
/// receives valid compact JSON, exit 0.
#[test]
fn json_wrapped_in_fence_and_schema_satisfied_succeeds_end_to_end() {
    let (addr, server) =
        spawn_stub_server("```json\n{\"category\": \"bug\", \"confidence\": 0.9}\n```".to_string());

    let scope = fixture_scope("fenced-json-ok");
    let schema = r#"{
        "type": "object",
        "required": ["category", "confidence"],
        "properties": {
            "category": { "type": "string" },
            "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
        },
        "additionalProperties": false
    }"#;
    write(&scope, ".npu/schemas/classification.json", schema);
    write_scope(
        &scope,
        addr,
        "format = \"json\"\nschema = \"schemas/classification.json\"",
    );

    let output = run_npu(&scope, &["e2e-cmd"], "whatever");
    server.join().expect("the server thread must not panic");

    assert!(
        output.status.success(),
        "expected exit code 0, got {:?}; stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout must be valid UTF-8");
    assert_eq!(
        stdout, "{\"category\":\"bug\",\"confidence\":0.9}\n",
        "stdout must contain EXACTLY the COMPACT JSON serialization followed by a single \
         trailing newline (added by `run()`, §14/§22: npu classify | jq .), regardless of \
         the Markdown wrapping returned by the model — nothing before, nothing after"
    );
}

/// b) the model answers with JSON that violates the schema -> exit 4,
/// message on stderr, nothing on stdout.
#[test]
fn json_violating_schema_fails_with_exit_code_four_end_to_end() {
    // Missing "confidence": violates `required`.
    let (addr, server) = spawn_stub_server("{\"category\": \"bug\"}".to_string());

    let scope = fixture_scope("schema-violation");
    let schema = r#"{
        "type": "object",
        "required": ["category", "confidence"],
        "properties": {
            "category": { "type": "string" },
            "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
        },
        "additionalProperties": false
    }"#;
    write(&scope, ".npu/schemas/classification.json", schema);
    write_scope(
        &scope,
        addr,
        "format = \"json\"\nschema = \"schemas/classification.json\"",
    );

    let output = run_npu(&scope, &["e2e-cmd"], "whatever");
    server.join().expect("the server thread must not panic");

    assert_eq!(
        output.status.code(),
        Some(4),
        "a JSON output that violates the declared schema must fail with code 4 \
         (Error::Output — the config is valid, it's the model that answered badly); \
         stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "nothing must be written to stdout when the output contract fails, got: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr must be valid UTF-8");
    assert!(
        stderr.contains("confidence"),
        "stderr must name the violation (missing required property), got: {stderr}"
    );
}

/// c) the model answers with text that is not JSON while format = "json"
/// -> exit 4.
#[test]
fn non_json_response_with_json_format_fails_with_exit_code_four_end_to_end() {
    let (addr, server) = spawn_stub_server("this is not JSON at all".to_string());

    let scope = fixture_scope("non-json-response");
    // No schema declared: rule 2 of the shared contract — format = "json"
    // without a schema is allowed, we then only validate that the output is
    // well-formed JSON.
    write_scope(&scope, addr, "format = \"json\"");

    let output = run_npu(&scope, &["e2e-cmd"], "whatever");
    server.join().expect("the server thread must not panic");

    assert_eq!(
        output.status.code(),
        Some(4),
        "a response that is not JSON while format = \"json\" must fail with code 4; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}

/// d) a format = "text" command with `max_lines` = 1 whose model returns
/// three lines -> exit 4.
#[test]
fn text_exceeding_max_lines_fails_with_exit_code_four_end_to_end() {
    let (addr, server) = spawn_stub_server("line 1\nline 2\nline 3".to_string());

    let scope = fixture_scope("max-lines-exceeded");
    write_scope(&scope, addr, "format = \"text\"\nmax_lines = 1");

    let output = run_npu(&scope, &["e2e-cmd"], "whatever");
    server.join().expect("the server thread must not panic");

    assert_eq!(
        output.status.code(),
        Some(4),
        "a text response exceeding max_lines must fail with code 4, never be silently \
         truncated (§15); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("stderr must be valid UTF-8");
    assert!(
        stderr.contains('1') && stderr.contains('3'),
        "stderr must cite the expected and the received number of lines, got: {stderr}"
    );
}
