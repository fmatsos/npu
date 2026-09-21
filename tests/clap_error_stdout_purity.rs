//! Locks down stdout purity (npu-cli-spec.md §14) on the error path
//! of `clap` ITSELF (unknown subcommand, missing required
//! argument), which exits via `get_matches()` — i.e. via the `std::process::exit`
//! internal to `clap` — WITHOUT going through `main()`'s error handler
//! (`src/main.rs`).
//!
//! Evidence requested by the L1+L2 review (gap not covered by
//! `output_contract_e2e.rs`, which only checks stdout purity on the
//! application error path — `Error::Config`/`Error::Output`, handled by
//! `main()` — never on `clap`'s own path): §23 makes `npu` a CLI invoked
//! by an agent, for whom non-empty output on stdout on failure is
//! just as dangerous here as on any other error path — an
//! agent piping stdout must never receive a `clap` usage message
//! mixed with command output.
//!
//! Uses the REAL compiled `npu` binary (`env!("CARGO_BIN_EXE_npu")`),
//! run with the repo's versioned `.npu/` fixture (current directory =
//! crate root, cf. `CARGO_MANIFEST_DIR`) as the local scope — the same
//! fixture as `tests/cli.rs`. `HOME` is redirected to a
//! temporary directory without `.config/npu` and `XDG_CONFIG_HOME` is removed, so that
//! only this local fixture is taken into account (same isolation
//! idiom as `tests/output_contract_e2e.rs`); no network is
//! ever contacted by these three scenarios, `clap` failing (or `--help`
//! printing) before any backend call.

#![allow(clippy::expect_used)] // allowed in tests (see Cargo.toml [lints.clippy]).

use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

fn isolated_home() -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-fixtures")
        .join(format!("clap-stdout-purity-home-{n}"));
    std::fs::create_dir_all(&dir).expect("failed to create the isolated HOME directory");
    dir
}

fn run_npu(args: &[&str]) -> Output {
    let home = isolated_home();
    Command::new(env!("CARGO_BIN_EXE_npu"))
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to launch the npu binary")
        .wait_with_output()
        .expect("failed to wait for the npu process to finish")
}

/// Nonexistent subcommand: `clap` fails BEFORE `main()`
/// regains control (`get_matches()` calls `std::process::exit`
/// directly, cf. `lib.rs::run`). stdout must remain strictly empty.
#[test]
fn unknown_subcommand_writes_nothing_to_stdout() {
    let output = run_npu(&["nonexistent-subcommand"]);

    assert!(!output.status.success(), "an unknown subcommand must fail");
    assert!(
        output.stdout.is_empty(),
        "stdout must remain empty on a `clap` error (unknown subcommand), got: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr must be valid UTF-8");
    assert!(
        stderr.contains("nonexistent-subcommand"),
        "stderr must name the offending subcommand, got: {stderr}"
    );
}

/// Missing required argument (`translate` without `--language`): same `clap`
/// error path, a different form (argument validation rather than
/// subcommand resolution). stdout must remain strictly empty.
#[test]
fn missing_required_arg_writes_nothing_to_stdout() {
    let output = run_npu(&["translate"]);

    assert!(
        !output.status.success(),
        "translate without --language must fail (missing required argument)"
    );
    assert!(
        output.stdout.is_empty(),
        "stdout must remain empty on a `clap` error (missing required argument), got: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr must be valid UTF-8");
    assert!(
        stderr.to_lowercase().contains("language"),
        "stderr must name the missing required argument, got: {stderr}"
    );
}

/// Nominal case, for explicit contrast: `--help` MUST write to stdout
/// (this is not an error path, cf. `clap`'s standard behavior) —
/// locks down that this test does not confuse "stdout empty" with "`clap`
/// must never write anything to stdout", which would be false.
#[test]
fn help_writes_to_stdout_not_stderr() {
    let output = run_npu(&["--help"]);

    assert!(output.status.success(), "npu --help must succeed");
    assert!(
        !output.stdout.is_empty(),
        "npu --help MUST write the help to stdout, this is not an error path"
    );
    assert!(
        output.stderr.is_empty(),
        "npu --help must not write anything to stderr, got: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
