//! End-to-end verification of `COMPLETE=<shell> npu` (`clap_complete::CompleteEnv`,
//! see `src/lib.rs::complete_env`): no built-in, no subcommand -- the
//! completion request is entirely environment-driven.

#![allow(clippy::expect_used)] // tolerated in tests (cf. Cargo.toml [lints.clippy]).

use std::process::{Command, Stdio};

/// `COMPLETE=bash npu` prints the shell registration script on stdout and
/// exits 0 -- it must run before anything else in `run()`, so this must
/// work even with a completely broken configuration.
#[test]
fn complete_bash_prints_a_registration_script() {
    let output = Command::new(env!("CARGO_BIN_EXE_npu"))
        .env("COMPLETE", "bash")
        .env("HOME", "/does/not/exist")
        .env_remove("XDG_CONFIG_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launching npu")
        .wait_with_output()
        .expect("waiting for npu");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("npu"),
        "the registration script must name the binary, got: {stdout}"
    );
}

/// A dynamic completion request for the root command lists the global
/// flags `run`'s own tree declares (`--config-dir`, added by this very
/// stack) -- proves the tree built for completion carries the same global
/// flags as `run`'s own. Built-ins are declared `hidden` ONLY in the
/// separate tree `cli::sectioned_help` builds for `--help`'s rendering
/// (a presentation concern); `complete_env` builds its own tree straight
/// from `cli::builtins::add_builtins`, so a built-in GROUP (`backend`,
/// `config`, ...) must still be offered here.
#[test]
fn complete_bash_dynamic_request_lists_global_flags() {
    let output = Command::new(env!("CARGO_BIN_EXE_npu"))
        .args(["--", "npu", ""])
        .env("COMPLETE", "bash")
        .env("_CLAP_COMPLETE_INDEX", "1")
        .env("HOME", "/does/not/exist")
        .env_remove("XDG_CONFIG_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launching npu")
        .wait_with_output()
        .expect("waiting for npu");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.lines().any(|line| line == "--config-dir"),
        "got: {stdout}"
    );
}

/// Root completion candidates must include built-in GROUPS (`backend`,
/// `config`), not only the global flags: `sectioned_help` hides them for
/// `--help`'s own rendering, but the tree `complete_env` builds for
/// completion must not inherit that hide.
#[test]
fn complete_bash_dynamic_request_offers_builtin_groups() {
    let output = Command::new(env!("CARGO_BIN_EXE_npu"))
        .args(["--", "npu", ""])
        .env("COMPLETE", "bash")
        .env("_CLAP_COMPLETE_INDEX", "1")
        .env("HOME", "/does/not/exist")
        .env_remove("XDG_CONFIG_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launching npu")
        .wait_with_output()
        .expect("waiting for npu");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let candidates: Vec<&str> = stdout.lines().collect();
    assert!(
        candidates.iter().any(|line| line.starts_with("backend")),
        "got: {candidates:?}"
    );
    assert!(
        candidates.iter().any(|line| line.starts_with("config")),
        "got: {candidates:?}"
    );
}
