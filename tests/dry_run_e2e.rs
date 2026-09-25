//! End-to-end verification of `--dry-run`: same idiom as
//! `tests/backend_headers_e2e.rs` — a temporary `.npu/` scope, the real
//! binary launched as a child process.

#![allow(clippy::expect_used)] // tolerated in tests (cf. Cargo.toml [lints.clippy]).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

fn fixture_scope(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-fixtures")
        .join(format!("e2e-dry-run-{name}-{n}"));
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

fn run_npu(scope: &Path, args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_npu"))
        .args(args)
        .current_dir(scope)
        .env("HOME", scope)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("APPDATA")
        // No `docker` on PATH: proves a dry run never shells out to it, even
        // for a `port = "auto"` backend.
        .env("PATH", scope)
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

fn write_scope(scope: &Path, header_secret_env: &str) {
    write(
        scope,
        ".npu/backends/stub.toml",
        &format!(
            r#"
            id = "stub"
            base_url = "http://127.0.0.1:{{{{ backend.port }}}}"
            type = "openai-compatible"
            port = "auto"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"

            [headers]
            Authorization = "Bearer {{{{ env.{header_secret_env} }}}}"

            [runtime]
            type = "docker"
            image = "does-not-matter:latest"
            options = ["-p", "{{{{ backend.port }}}}:9000"]
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

/// `--dry-run` on a `port = "auto"` backend, with `docker` absent from
/// `PATH`: succeeds (never resolves the runtime), and the printed URL keeps
/// the unresolved `{{ backend.port }}` placeholder verbatim.
#[test]
fn dry_run_never_resolves_the_runtime_and_leaves_the_port_placeholder() {
    let scope = fixture_scope("port-auto");
    write_scope(&scope, "NPU_TEST_DRY_RUN_SECRET");

    let output = Command::new(env!("CARGO_BIN_EXE_npu"))
        .args(["e2e-cmd", "--dry-run"])
        .current_dir(&scope)
        .env("HOME", &scope)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("APPDATA")
        .env("PATH", &scope)
        .env("NPU_TEST_DRY_RUN_SECRET", "s3cr3t-value")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launching npu");
    let mut child = output;
    {
        let stdin = child.stdin.as_mut().expect("stdin of the child process");
        stdin.write_all(b"hello").expect("writing to npu's stdin");
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("waiting for npu");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout must be UTF-8");
    let report: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("dry-run report must be valid JSON");

    assert_eq!(
        report["url"].as_str(),
        Some("http://127.0.0.1:{{ backend.port }}/v1/chat/completions"),
        "the port placeholder must be left unresolved"
    );
    assert_eq!(report["body"]["model"], "test-model");
    assert!(
        report["body"]["stream"].is_null(),
        "stdout is piped, not a terminal: a real invocation would not stream, and neither \
         must the dry-run report, got: {}",
        report["body"]
    );
    assert_eq!(
        report["body"]["messages"][0]["content"], "hello",
        "the request body is the same one `chat` would send"
    );
}

/// Header VALUES are redacted, header NAMES are not.
#[test]
fn dry_run_redacts_header_values_but_keeps_their_names() {
    let scope = fixture_scope("redact");
    write_scope(&scope, "NPU_TEST_DRY_RUN_SECRET_2");

    let output = Command::new(env!("CARGO_BIN_EXE_npu"))
        .args(["e2e-cmd", "--dry-run"])
        .current_dir(&scope)
        .env("HOME", &scope)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("APPDATA")
        .env("PATH", &scope)
        .env("NPU_TEST_DRY_RUN_SECRET_2", "s3cr3t-value")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launching npu")
        .wait_with_output()
        .expect("waiting for npu");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout must be UTF-8");
    assert!(
        !stdout.contains("s3cr3t-value"),
        "a header VALUE must never appear in a dry-run report"
    );
    let report: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("dry-run report must be valid JSON");
    assert!(report["headers"]["Authorization"].is_string());
    assert_ne!(report["headers"]["Authorization"], "s3cr3t-value");
}

/// `npu doctor --dry-run` and any built-in with `--dry-run`: usage error,
/// exit 2, empty stdout — the flag is declared only on business command
/// leaves, so clap itself rejects it elsewhere.
#[test]
fn dry_run_on_a_builtin_is_a_clap_usage_error() {
    let scope = fixture_scope("builtin");
    write_scope(&scope, "NPU_TEST_DRY_RUN_SECRET_3");

    let output = run_npu(&scope, &["doctor", "--dry-run"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}

/// `--dry-run` shows the prompt with its partial inserted, and `doctor`
/// names a command whose partial is missing.
#[test]
fn dry_run_shows_the_inserted_partial_and_doctor_flags_a_missing_one() {
    let scope = fixture_scope("partial");
    write_scope(&scope, "NPU_TEST_DRY_RUN_SECRET");
    // Without the header, whose variable this test does not set.
    write(
        &scope,
        ".npu/backends/stub.toml",
        "id = \"stub\"\nbase_url = \"http://127.0.0.1:9\"\ntype = \"openai-compatible\"\n\
         [operations.chat]\nmethod = \"POST\"\npath = \"/v1/chat/completions\"\n",
    );
    write(
        &scope,
        ".npu/commands/styled.md",
        "---\nmodel = \"test-model\"\n[partials]\nstyle = \"style\"\n---\n\
         {{ partials.style }} {{ input }}\n",
    );
    write(&scope, ".npu/partials/style.md", "Be terse.");
    write(
        &scope,
        ".npu/commands/orphan.md",
        "---\nmodel = \"test-model\"\n[partials]\nstyle = \"gone\"\n---\n\
         {{ partials.style }}\n",
    );

    let output = run_npu(&scope, &["styled", "--dry-run"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("dry-run report must be valid JSON");
    assert_eq!(report["body"]["messages"][0]["content"], "Be terse. hello");

    let output = run_npu(&scope, &["doctor"]);
    assert_eq!(output.status.code(), Some(2));
    let report = String::from_utf8(output.stdout).expect("stdout must be UTF-8");
    assert!(report.contains("orphan"), "got: {report}");
    assert!(report.contains("gone.md"), "got: {report}");
}
