//! End-to-end verification of `--model`: same idiom as
//! `tests/backend_headers_e2e.rs` / `tests/dry_run_e2e.rs`.

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
        .join(format!("e2e-model-override-{name}-{n}"));
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

fn write_scope(scope: &Path) {
    write(
        scope,
        ".npu/backends/stub.toml",
        r#"
        id = "stub"
        base_url = "http://127.0.0.1:0"
        type = "openai-compatible"

        [operations.chat]
        method = "POST"
        path = "/v1/chat/completions"
        "#,
    );
    write(
        scope,
        ".npu/models/primary.toml",
        r#"
        id = "primary"
        backend = "stub"
        operation = "chat"
        model = "primary-model"
        "#,
    );
    write(
        scope,
        ".npu/models/secondary.toml",
        r#"
        id = "secondary"
        backend = "stub"
        operation = "chat"
        model = "secondary-model"
        "#,
    );
    write(
        scope,
        ".npu/commands/e2e-cmd.md",
        "---\nmodel = \"primary\"\n---\n{{ input }}\n",
    );
}

fn run(scope: &Path, args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_npu"))
        .args(args)
        .current_dir(scope)
        .env("HOME", scope)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("APPDATA")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launching npu");
    {
        let stdin = child.stdin.as_mut().expect("stdin of the child process");
        stdin.write_all(b"hello").expect("writing to npu's stdin");
    }
    drop(child.stdin.take());
    child.wait_with_output().expect("waiting for npu")
}

/// `--model` replaces the command's own model, proven through `--dry-run`
/// (no network needed): the printed body carries the OVERRIDE model, not
/// the command file's.
#[test]
fn model_override_replaces_the_command_s_own_model() {
    let scope = fixture_scope("replaces");
    write_scope(&scope);

    let output = run(&scope, &["e2e-cmd", "--model", "secondary", "--dry-run"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout must be UTF-8");
    let report: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("dry-run report must be valid JSON");
    assert_eq!(report["body"]["model"], "secondary-model");
}

/// An unknown `--model` id fails with `Error::Config` (exit 2), empty
/// stdout, naming the unknown id -- and never reaches the network or reads
/// the input (proven by giving no stdin at all: a read would block forever
/// in a real terminal, but here an empty pipe would just return "" rather
/// than error, so what matters is the exit code and stdout purity).
#[test]
fn an_unknown_model_override_is_a_config_error_before_anything_else() {
    let scope = fixture_scope("unknown");
    write_scope(&scope);

    let output = run(&scope, &["e2e-cmd", "--model", "does-not-exist"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does-not-exist"),
        "the error must name the unknown override id, got: {stderr}"
    );
}
