//! End-to-end verification of the project scope walk-up and
//! `--config-dir`/`NPU_CONFIG_DIR` (see `src/scope.rs::project_root`).
//! Same idiom as `tests/builtins_and_degraded_mode.rs`: the REAL binary,
//! temporary directories, `$XDG_CONFIG_HOME` pointed at an EMPTY scope so
//! only the project scope under test is ever picked up.

#![allow(clippy::expect_used)] // tolerated in tests (cf. Cargo.toml [lints.clippy]).

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

fn fixture_dir(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-fixtures")
        .join(format!("e2e-config-dir-{name}-{n}"));
    std::fs::create_dir_all(&dir).expect("creating the fixture directory");
    dir
}

fn write(dir: &Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("creating the parent directory");
    }
    std::fs::write(path, contents).expect("writing the fixture");
}

/// A project scope with just the `qwen-fast` model configured (no backend,
/// no command needed: `config models` is enough to prove which scope won).
fn write_scope(npu_dir: &Path) {
    write(
        npu_dir,
        "backends/ovms.toml",
        r#"
        id = "ovms"
        base_url = "http://127.0.0.1:1"
        type = "openai-compatible"

        [operations.chat]
        method = "POST"
        path = "/v1/chat/completions"
        "#,
    );
    write(
        npu_dir,
        "models/qwen-fast.toml",
        r#"
        id = "qwen-fast"
        backend = "ovms"
        operation = "chat"
        model = "qwen-fast-underlying"
        "#,
    );
}

fn run(cwd: &Path, empty_xdg: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
    // `HOME` deliberately UNRELATED to `cwd`'s ancestry: `HOME == cwd` (or
    // an ancestor of it) would make the walk-up's home boundary stop the
    // search before it ever reaches the fixture's own `.npu` — these tests
    // are about the walk-up itself, not about the home boundary.
    Command::new(env!("CARGO_BIN_EXE_npu"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", empty_xdg)
        .env("XDG_CONFIG_HOME", empty_xdg)
        .envs(extra_env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launching npu")
        .wait_with_output()
        .expect("waiting for npu")
}

/// A `.npu` found by walking up from a NESTED cwd (no `.npu` at cwd
/// itself) is picked up: `config models` lists the model it declares.
#[test]
fn a_npu_directory_in_a_parent_of_cwd_is_found_by_walking_up() {
    let root = fixture_dir("walkup-root");
    write_scope(&root.join(".npu"));
    let nested = root.join("src").join("deep");
    std::fs::create_dir_all(&nested).expect("nested dir");
    let empty_xdg = fixture_dir("walkup-empty-xdg");

    let output = run(&nested, &empty_xdg, &["config", "models"], &[]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("qwen-fast"), "got: {stdout}");
}

/// A `.git` boundary (as a FILE, the worktree case) stops the walk-up: a
/// `.npu` further up must stay invisible, and `config models` then lists
/// no model at all.
#[test]
fn a_git_boundary_stops_the_walk_up_even_when_git_is_a_file() {
    let outer = fixture_dir("git-boundary-outer");
    write_scope(&outer.join(".npu"));
    let project = outer.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    std::fs::write(project.join(".git"), "gitdir: /elsewhere\n").expect(".git file");
    let nested = project.join("src");
    std::fs::create_dir_all(&nested).expect("nested dir");
    let empty_xdg = fixture_dir("git-boundary-empty-xdg");

    let output = run(&nested, &empty_xdg, &["config", "models"], &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("qwen-fast"),
        "the outer .npu must stay invisible past the .git boundary, got: {stdout}"
    );
}

/// `NPU_CONFIG_DIR` overrides the walk-up entirely.
#[test]
fn npu_config_dir_env_var_overrides_the_walk_up() {
    let elsewhere = fixture_dir("env-override-elsewhere");
    write_scope(&elsewhere.join("the-npu-dir"));
    let cwd = fixture_dir("env-override-cwd");
    let empty_xdg = fixture_dir("env-override-empty-xdg");

    let output = run(
        &cwd,
        &empty_xdg,
        &["config", "models"],
        &[(
            "NPU_CONFIG_DIR",
            elsewhere.join("the-npu-dir").to_str().expect("utf-8 path"),
        )],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("qwen-fast"), "got: {stdout}");
}

/// `--config-dir` wins over `NPU_CONFIG_DIR` when both are set.
#[test]
fn config_dir_flag_wins_over_the_environment_variable() {
    let env_target = fixture_dir("flag-wins-env-target");
    write_scope(&env_target.join(".npu"));
    let flag_target = fixture_dir("flag-wins-flag-target");
    write_scope(&flag_target.join(".npu"));
    let cwd = fixture_dir("flag-wins-cwd");
    let empty_xdg = fixture_dir("flag-wins-empty-xdg");

    let output = run(
        &cwd,
        &empty_xdg,
        &[
            "--config-dir",
            flag_target.join(".npu").to_str().expect("utf-8 path"),
            "config",
            "models",
        ],
        &[(
            "NPU_CONFIG_DIR",
            env_target.join(".npu").to_str().expect("utf-8 path"),
        )],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("qwen-fast"), "got: {stdout}");
}

/// `npu doctor` names the project scope it resolved, as an `Ok` line.
#[test]
fn doctor_names_the_resolved_project_scope() {
    let root = fixture_dir("doctor-names-scope");
    write_scope(&root.join(".npu"));
    let empty_xdg = fixture_dir("doctor-names-scope-empty-xdg");

    let output = run(&root, &empty_xdg, &["doctor", "--json"], &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let checks: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("doctor --json must be valid JSON");
    let checks = checks.as_array().expect("array");
    let scope_line = checks.iter().find(|c| {
        c["label"]
            .as_str()
            .is_some_and(|l| l.starts_with("project scope"))
    });
    assert!(
        scope_line.is_some(),
        "doctor must report the resolved project scope, got: {stdout}"
    );
    assert_eq!(scope_line.expect("checked above")["status"], "ok");
}
