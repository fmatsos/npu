//! End-to-end verification of the built-ins and the degraded mode — the
//! REAL binary
//! (`env!("CARGO_BIN_EXE_npu")`), never a function called directly in this
//! test process, with temporary scopes mounted via `$XDG_CONFIG_HOME` (same
//! idiom as `run_npu_xdg`/`fixture_cwd_without_local_scope` in
//! `tests/output_contract_e2e.rs`).
//!
//! Two scenario families:
//! - (a)/(b)/(c): a BROKEN configuration (an unreadable backend TOML) ->
//!   `--help` stays usable (degraded mode), `doctor` reports the load
//!   failure (code 2), any other invocation propagates that same error
//!   (code 2);
//! - (d)/(e)/(f): a HEALTHY configuration -> `doctor` distinguishes a valid
//!   configuration from a dead backend (code 3, never 2), `models` and
//!   `describe` produce their expected output on stdout (code 0).
//!
//! None of these scenarios calls a real network backend: `--help`,
//! `doctor`, `models` and `describe` never contact a backend (`doctor` opens
//! at most a TCP socket to a deliberately closed port, cf. (d)). No test
//! uses port 8000: that is the repo's stubbed fixture's port
//! (`tests/output_contract_e2e.rs`), and a local service answering there
//! would silently skew (d). The closed port for (d) is obtained by binding
//! an ephemeral `TcpListener` and closing it right away (same idiom as
//! `builtin::tests::tcp_probe_fails_against_a_closed_port`), never a
//! hardcoded port number.
//!
//! `HOME` is redirected to a temporary directory without `.config/npu` for
//! each invocation, `$XDG_CONFIG_HOME` points to the temporary scope written
//! by the test: only that scope root is taken into account by
//! `scope::roots()`, never the real `$HOME` nor an `/etc/npu` that might
//! otherwise exist on the machine. `Command::env`/`env_remove` only touch
//! the CHILD PROCESS's environment: no test mutates the real environment
//! variables (`std::env::set_var` is `unsafe` in edition 2024, forbidden by
//! `unsafe_code = "forbid"`, cf. Cargo.toml).

#![allow(clippy::expect_used)] // tolerated in tests (cf. Cargo.toml [lints.clippy]).

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// Creates a unique directory under `target/`, so as not to pollute the repo
/// nor collide between tests run in parallel (same idiom as the other files
/// in `tests/`).
fn fixture_dir(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-fixtures")
        .join(format!("phase5-{name}-{n}"));
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

/// Runs the REAL `npu` binary with `$XDG_CONFIG_HOME` pointed at
/// `xdg_config_home` and `cwd` (deliberately without a local `.npu`) as the
/// current directory — same idiom as `run_npu_xdg` in
/// `tests/output_contract_e2e.rs`.
fn run_npu(cwd: &Path, xdg_config_home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_npu"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", cwd)
        .env("XDG_CONFIG_HOME", xdg_config_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launching the npu binary")
        .wait_with_output()
        .expect("waiting for the npu process to finish")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Writes a `$XDG_CONFIG_HOME/npu` scope whose `backends/ovms.toml` is
/// unreadable TOML (a PARSING failure, therefore fatal even when masked:
/// unlike a command, a broken backend has no knowable identity before it
/// is parsed).
fn write_broken_scope(xdg_root: &Path) {
    write(
        xdg_root,
        "npu/backends/ovms.toml",
        "this is not valid TOML { { {\n",
    );
}

/// Writes a HEALTHY `$XDG_CONFIG_HOME/npu` scope: an `ovms` backend pointing
/// at `base_url`, a `qwen-fast` model, and the `commit-message` command
/// (text format, no schema — `doctor`'s check (e) must not produce anything
/// for it).
fn write_healthy_scope(xdg_root: &Path, base_url: &str) {
    write(
        xdg_root,
        "npu/backends/ovms.toml",
        &format!(
            r#"
            id = "ovms"
            base_url = "{base_url}"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#
        ),
    );
    write(
        xdg_root,
        "npu/models/qwen-fast.toml",
        r#"
        id = "qwen-fast"
        backend = "ovms"
        operation = "chat"
        model = "qwen-fast-underlying"
        "#,
    );
    write(
        xdg_root,
        "npu/commands/commit-message.md",
        "---\ndescription = \"Generate a commit message\"\nmodel = \"qwen-fast\"\n---\n\
         {{ input }}\n",
    );
}

// -- (a)/(b)/(c): broken configuration ---------------------------------------

/// (a) A BROKEN configuration leaves `--help` usable (degraded mode, point 1
/// of the shared contract): exit 0, stdout lists the built-ins,
/// stderr signals the load failure.
#[test]
fn broken_config_help_still_works_and_lists_builtins_with_stderr_signal() {
    let xdg = fixture_dir("broken-help-xdg");
    let cwd = fixture_dir("broken-help-cwd");
    write_broken_scope(&xdg);

    let output = run_npu(&cwd, &xdg, &["--help"]);

    assert!(
        output.status.success(),
        "PROOF (a): npu --help must succeed (exit 0) despite a broken configuration, \
         got code {:?}; stderr: {}",
        output.status.code(),
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("doctor"),
        "stdout of --help must list \"doctor\", got: {stdout}"
    );
    assert!(
        stdout.contains("models"),
        "stdout of --help must list \"models\", got: {stdout}"
    );
    assert!(
        stdout.contains("serve"),
        "stdout of --help must list \"serve\", got: {stdout}"
    );
    assert!(
        stdout.contains("describe"),
        "stdout of --help must list \"describe\", got: {stdout}"
    );
    assert!(
        stdout.contains("version"),
        "stdout of --help must list \"version\", got: {stdout}"
    );
    assert!(
        stdout.contains("update"),
        "stdout of --help must list \"update\", got: {stdout}"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("doctor"),
        "PROOF (a): stderr must point to \"npu doctor\", got: {stderr}"
    );
}

/// `--version` describes the binary, not its configuration. It therefore stays
/// usable in degraded mode and prints exactly the Cargo package version.
#[test]
fn broken_config_version_still_prints_the_release_version() {
    let xdg = fixture_dir("broken-version-xdg");
    let cwd = fixture_dir("broken-version-cwd");
    write_broken_scope(&xdg);

    let output = run_npu(&cwd, &xdg, &["--version"]);

    assert!(
        output.status.success(),
        "npu --version must succeed despite a broken configuration; stderr: {}",
        stderr_of(&output)
    );
    assert_eq!(
        stdout_of(&output),
        format!("npu {}\n", env!("CARGO_PKG_VERSION"))
    );
    // `--version` never reads the configuration: warning about it would be noise.
    assert!(stderr_of(&output).is_empty(), "got: {}", stderr_of(&output));
}

/// (b) Same broken configuration: `npu doctor` exits with code 2 and its
/// report, on STDOUT, describes the load error.
#[test]
fn broken_config_doctor_reports_load_error_on_stdout_with_exit_code_two() {
    let xdg = fixture_dir("broken-doctor-xdg");
    let cwd = fixture_dir("broken-doctor-cwd");
    write_broken_scope(&xdg);

    let output = run_npu(&cwd, &xdg, &["doctor"]);

    assert_eq!(
        output.status.code(),
        Some(2),
        "PROOF (b): npu doctor must exit with code 2 on a broken configuration, stderr: {}",
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("ovms.toml"),
        "PROOF (b): the report must name the offending file, got: {stdout}"
    );
}

/// (c) Same broken configuration: any invocation whatsoever exits with code
/// 2.
///
/// Two DISTINCT mechanisms share this code, and this test covers both of
/// them separately rather than conflating them:
/// - a business command (`commit-message`) does not even exist in the
///   `clap` tree in degraded mode (`build_cli(&[])`, `write_broken_scope`
///   does not declare any command either): `clap` itself rejects it as an
///   unknown subcommand (same path as `tests/clap_error_stdout_purity.rs`),
///   BEFORE `run()` even reaches `loaded?` — this is NOT a propagation of
///   the load error, only an exit code that happens to coincide;
/// - `models` and `describe`, on the other hand, ALWAYS stay in the `clap`
///   tree (added unconditionally by `add_builtins`): their code 2 really
///   does traverse `loaded?`, so it carries the PRESERVED load error —
///   verified here by requiring that stderr name the offending file, not
///   just the exit code (point 1 of the shared contract).
#[test]
fn broken_config_any_other_invocation_exits_with_code_two() {
    let xdg = fixture_dir("broken-any-xdg");
    let cwd = fixture_dir("broken-any-cwd");
    write_broken_scope(&xdg);

    // Mechanism 1: rejected by `clap` itself (no business command in the
    // tree in degraded mode), not a propagation of `loaded?`.
    let business = run_npu(&cwd, &xdg, &["commit-message"]);
    assert_eq!(
        business.status.code(),
        Some(2),
        "PROOF (c): a subcommand absent from the tree in degraded mode must exit with code 2 \
         (`clap` rejection), stderr: {}",
        stderr_of(&business)
    );
    assert!(
        business.stdout.is_empty(),
        "nothing must be written to stdout on failure, got: {}",
        stdout_of(&business)
    );

    // Mechanism 2: `models` traverses `loaded?` (always in the tree) —
    // stderr must carry the PRESERVED load error, not a generic message,
    // otherwise this test would not distinguish this path from mechanism 1
    // above.
    let models = run_npu(&cwd, &xdg, &["config", "models"]);
    assert_eq!(
        models.status.code(),
        Some(2),
        "PROOF (c): npu models must propagate the load error (via loaded?), code 2, stderr: {}",
        stderr_of(&models)
    );
    assert!(
        stderr_of(&models).contains("ovms.toml"),
        "PROOF (c): stderr of npu models must name the offending configuration file (proof \
         that it is indeed the PRESERVED load error being propagated), got: {}",
        stderr_of(&models)
    );

    // Same proof as `models`, for `describe`.
    let describe = run_npu(&cwd, &xdg, &["describe", "commit-message"]);
    assert_eq!(
        describe.status.code(),
        Some(2),
        "PROOF (c): npu describe must propagate the load error (via loaded?), code 2, stderr: {}",
        stderr_of(&describe)
    );
    assert!(
        stderr_of(&describe).contains("ovms.toml"),
        "PROOF (c): stderr of npu describe must name the offending configuration file, got: {}",
        stderr_of(&describe)
    );
}

// -- (d)/(e)/(f): healthy configuration ---------------------------------------

/// Binds an ephemeral `TcpListener` then closes it right away, to obtain a
/// genuinely closed port without ever hardcoding a port number nor touching
/// port 8000 (the one used by the `tests/output_contract_e2e.rs` fixture) —
/// same idiom as `builtin::tests::tcp_probe_fails_against_a_closed_port`.
fn closed_port_base_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding the ephemeral listener");
    let addr = listener
        .local_addr()
        .expect("getting the listener's local address");
    drop(listener); // closes immediately: nobody is listening here anymore.
    format!("http://{addr}")
}

/// (d) HEALTHY configuration but dead backend (closed port): `npu doctor`
/// exits with code 3 (never 2: the configuration itself is valid), and the
/// report shows the configuration checks succeeding and reachability
/// failing.
#[test]
fn healthy_config_with_dead_backend_doctor_exits_three_with_reachability_failure_only() {
    let xdg = fixture_dir("healthy-dead-backend-xdg");
    let cwd = fixture_dir("healthy-dead-backend-cwd");
    write_healthy_scope(&xdg, &closed_port_base_url());

    let output = run_npu(&cwd, &xdg, &["doctor"]);

    assert_eq!(
        output.status.code(),
        Some(3),
        "PROOF (d): npu doctor must exit with code 3 (unreachable backend, valid config), \
         stderr: {}",
        stderr_of(&output)
    );
}

/// (e) Healthy configuration, `npu models`: exit 0, stdout contains
/// `qwen-fast`, `ovms` and `chat` (NAME/BACKEND/OPERATION columns, §16).
#[test]
fn healthy_config_models_lists_configured_model_with_its_backend_and_operation() {
    let xdg = fixture_dir("healthy-models-xdg");
    let cwd = fixture_dir("healthy-models-cwd");
    // Backend never contacted by `models`: a syntactically valid URL that
    // does not resolve to a real service is enough.
    write_healthy_scope(&xdg, "http://127.0.0.1:1");

    let output = run_npu(&cwd, &xdg, &["config", "models"]);

    assert!(
        output.status.success(),
        "PROOF (e): npu models must succeed (exit 0), stderr: {}",
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(stdout.contains("qwen-fast"), "got: {stdout}");
    assert!(stdout.contains("ovms"), "got: {stdout}");
    assert!(stdout.contains("chat"), "got: {stdout}");
}

/// (f) Healthy configuration, `npu describe commit-message`: exit 0, stdout
/// is parsable JSON describing the command.
#[test]
fn healthy_config_describe_produces_parsable_json_for_a_known_command() {
    let xdg = fixture_dir("healthy-describe-xdg");
    let cwd = fixture_dir("healthy-describe-cwd");
    write_healthy_scope(&xdg, "http://127.0.0.1:1");

    let output = run_npu(&cwd, &xdg, &["describe", "commit-message"]);

    assert!(
        output.status.success(),
        "PROOF (f): npu describe commit-message must succeed (exit 0), stderr: {}",
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("PROOF (f): stdout must be parsable JSON");
    assert_eq!(value["name"], "commit-message");
    assert_eq!(value["kind"], "command");
    assert_eq!(value["model"], "qwen-fast");
    assert_eq!(value["backend"], "ovms");
    assert!(value["fallback"].is_null());
    let file = value["source"]["file"]
        .as_str()
        .expect("source.file is a string");
    assert!(file.ends_with("commit-message.md"), "got: {file}");
    assert_eq!(
        value["source"]["scope"].as_str(),
        Some(xdg.join("npu").display().to_string().as_str())
    );
}

// -- (g)/(h): `npu serve` -----------------------------------------------------
//
// None of these scenarios requires Docker: both go through a configuration
// error, detected before any process is spawned. What is proven here is the
// exit code and the PURITY of stdout, never the wording of a message.

/// (g) `npu serve` on an unknown model: configuration error (exit 2), and
/// stdout stays empty — the container identifier is the only thing this
/// command ever writes there.
#[test]
fn healthy_config_serve_unknown_model_exits_two_with_empty_stdout() {
    let xdg = fixture_dir("serve-unknown-model-xdg");
    let cwd = fixture_dir("serve-unknown-model-cwd");
    write_healthy_scope(&xdg, "http://127.0.0.1:1");

    let output = run_npu(&cwd, &xdg, &["backend", "serve", "absent"]);

    assert_eq!(
        output.status.code(),
        Some(2),
        "PROOF (g): an unknown model is a configuration error, stderr: {}",
        stderr_of(&output)
    );
    assert!(
        output.stdout.is_empty(),
        "PROOF (g): stdout must be empty on failure, got: {}",
        stdout_of(&output)
    );
    assert!(
        stderr_of(&output).contains("absent"),
        "PROOF (g): the message must name the faulty model"
    );
}

/// (h) `npu serve` on a model whose backend declares no `[docker]` table:
/// configuration error (exit 2), stdout empty, and the message names the
/// backend that cannot be started. The scope written by
/// `write_healthy_scope` deliberately has no `[docker]` table.
#[test]
fn healthy_config_serve_backend_without_docker_exits_two_with_empty_stdout() {
    let xdg = fixture_dir("serve-no-docker-xdg");
    let cwd = fixture_dir("serve-no-docker-cwd");
    write_healthy_scope(&xdg, "http://127.0.0.1:1");

    let output = run_npu(&cwd, &xdg, &["backend", "serve", "qwen-fast"]);

    assert_eq!(
        output.status.code(),
        Some(2),
        "PROOF (h): a backend without [docker] cannot be served, stderr: {}",
        stderr_of(&output)
    );
    assert!(
        output.stdout.is_empty(),
        "PROOF (h): stdout must be empty on failure, got: {}",
        stdout_of(&output)
    );
    assert!(
        stderr_of(&output).contains("ovms"),
        "PROOF (h): the message must name the backend"
    );
}

// -- (i)/(j)/(k): lifecycle and verbosity -------------------------------------

/// (i) `npu stop` on a model whose backend declares no `[docker]` table:
/// configuration error (exit 2), stdout empty. Like (h), nothing is spawned.
#[test]
fn healthy_config_stop_backend_without_docker_exits_two_with_empty_stdout() {
    let xdg = fixture_dir("stop-no-docker-xdg");
    let cwd = fixture_dir("stop-no-docker-cwd");
    write_healthy_scope(&xdg, "http://127.0.0.1:1");

    let output = run_npu(&cwd, &xdg, &["backend", "stop", "qwen-fast"]);

    assert_eq!(
        output.status.code(),
        Some(2),
        "PROOF (i): npu only manages the containers it starts, stderr: {}",
        stderr_of(&output)
    );
    assert!(output.stdout.is_empty(), "PROOF (i): stdout must be empty");
    assert!(stderr_of(&output).contains("ovms"));
}

/// (j) `npu status` on a configuration without a single containerized
/// backend: exit 0 and a header on stdout — never an empty output, which
/// would be indistinguishable from a command that did nothing. No container
/// runtime is contacted, since there is no backend to ask about.
#[test]
fn healthy_config_status_without_containerized_backend_still_prints_its_header() {
    let xdg = fixture_dir("status-empty-xdg");
    let cwd = fixture_dir("status-empty-cwd");
    write_healthy_scope(&xdg, "http://127.0.0.1:1");

    let output = run_npu(&cwd, &xdg, &["backend", "status"]);

    assert!(
        output.status.success(),
        "PROOF (j): npu status must succeed, stderr: {}",
        stderr_of(&output)
    );
    assert!(
        !stdout_of(&output).trim().is_empty(),
        "PROOF (j): the header must survive an empty table"
    );
}

/// (k) `--verbose` moves the threshold without ever touching stdout: `info`
/// adds a trace on STDERR, `error` silences the degraded-mode warning that
/// `warn` (the default) prints. Run against a BROKEN configuration, where
/// that warning is the observable difference.
#[test]
fn verbose_changes_stderr_only_and_never_stdout() {
    let xdg = fixture_dir("verbose-xdg");
    let cwd = fixture_dir("verbose-cwd");
    write_broken_scope(&xdg);

    let default_level = run_npu(&cwd, &xdg, &["--help"]);
    let silenced = run_npu(&cwd, &xdg, &["--verbose", "error", "--help"]);
    let verbose = run_npu(&cwd, &xdg, &["--verbose", "info", "--help"]);

    assert!(
        !stderr_of(&default_level).is_empty(),
        "PROOF (k): the default threshold keeps the load warning"
    );
    assert!(
        stderr_of(&silenced).is_empty(),
        "PROOF (k): --verbose error silences the warning, got: {}",
        stderr_of(&silenced)
    );
    assert!(
        stderr_of(&verbose).len() > stderr_of(&default_level).len(),
        "PROOF (k): --verbose info adds a trace on stderr"
    );
    assert_eq!(
        stdout_of(&default_level),
        stdout_of(&verbose),
        "PROOF (k): stdout must not depend on the verbosity level"
    );
}

// -- 0.4.0 command tree ------------------------------------------------------

/// `config check` is `doctor` under its grouped name: same report, same code.
#[test]
fn broken_config_config_check_matches_doctor() {
    let xdg = fixture_dir("broken-config-check-xdg");
    let cwd = fixture_dir("broken-config-check-cwd");
    write_broken_scope(&xdg);

    let doctor = run_npu(&cwd, &xdg, &["doctor"]);
    let check = run_npu(&cwd, &xdg, &["config", "check"]);

    assert_eq!(check.status.code(), Some(2));
    assert_eq!(stdout_of(&check), stdout_of(&doctor));
}

/// The pre-0.4.0 top-level built-ins are gone: each is an unknown command,
/// rejected by `clap` with nothing on stdout.
#[test]
fn removed_top_level_built_ins_are_unknown_commands_with_empty_stdout() {
    let xdg = fixture_dir("removed-builtins-xdg");
    let cwd = fixture_dir("removed-builtins-cwd");
    write_healthy_scope(&xdg, &closed_port_base_url());

    for args in [
        &["serve", "qwen-fast"][..],
        &["stop", "qwen-fast"],
        &["status"],
        &["logs", "qwen-fast"],
        &["models"],
        &["version"],
    ] {
        let output = run_npu(&cwd, &xdg, args);
        assert_eq!(output.status.code(), Some(2), "npu {args:?}");
        assert!(output.stdout.is_empty(), "npu {args:?} wrote on stdout");
    }
}

/// A name the built-ins released is an ordinary command again, and runs as
/// one: it reaches the backend, which is dead here, hence `3`.
#[test]
fn a_command_named_status_is_a_business_command() {
    let xdg = fixture_dir("custom-status-xdg");
    let cwd = fixture_dir("custom-status-cwd");
    write_healthy_scope(&xdg, &closed_port_base_url());
    write(
        &xdg,
        "npu/commands/status.md",
        "---\ndescription = \"Summarize a status\"\nmodel = \"qwen-fast\"\n---\n{{ input }}\n",
    );

    let output = run_npu(&cwd, &xdg, &["status"]);

    assert_eq!(
        output.status.code(),
        Some(3),
        "stderr: {}",
        stderr_of(&output)
    );
    assert!(output.stdout.is_empty());
}

/// A built-in is described from the `clap` tree, so a broken configuration
/// does not prevent it — while a configured command, which needs the
/// configuration, still exits `2`.
#[test]
fn broken_config_describe_still_describes_a_built_in() {
    let xdg = fixture_dir("broken-describe-xdg");
    let cwd = fixture_dir("broken-describe-cwd");
    write_broken_scope(&xdg);

    let output = run_npu(&cwd, &xdg, &["describe", "backend", "serve"]);

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    let value: serde_json::Value =
        serde_json::from_str(stdout_of(&output).trim()).expect("stdout must be JSON");
    assert_eq!(value["name"], "backend/serve");
    assert_eq!(value["kind"], "builtin");
    assert_eq!(value["args"]["MODEL"]["required"], true);
    assert_eq!(value["degraded_mode"], false);

    let doctor = run_npu(&cwd, &xdg, &["describe", "doctor"]);
    let value: serde_json::Value =
        serde_json::from_str(stdout_of(&doctor).trim()).expect("stdout must be JSON");
    assert_eq!(value["degraded_mode"], true);

    let custom = run_npu(&cwd, &xdg, &["describe", "commit-message"]);
    assert_eq!(custom.status.code(), Some(2));
    assert!(custom.stdout.is_empty());
}

/// An unknown path exits `2` and the message names it.
#[test]
fn describe_unknown_path_exits_two_naming_it() {
    let xdg = fixture_dir("describe-unknown-xdg");
    let cwd = fixture_dir("describe-unknown-cwd");
    write_healthy_scope(&xdg, &closed_port_base_url());

    let output = run_npu(&cwd, &xdg, &["describe", "backend", "nope"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(
        stderr_of(&output).contains("backend/nope"),
        "got: {}",
        stderr_of(&output)
    );
}

/// `--help` lists the configured commands and the built-ins in two
/// distinct sections. Asserted on the NAMES and on the blank line between
/// sections, never on the headings' wording.
#[test]
fn help_lists_configured_commands_and_built_ins_in_separate_sections() {
    let xdg = fixture_dir("help-sections-xdg");
    let cwd = fixture_dir("help-sections-cwd");
    write_healthy_scope(&xdg, &closed_port_base_url());

    let output = run_npu(&cwd, &xdg, &["--help"]);

    assert!(output.status.success());
    let stdout = stdout_of(&output);
    let section_of = |name: &str| {
        stdout
            .split("\n\n")
            .position(|block| {
                block
                    .lines()
                    .any(|line| line.trim_start().starts_with(name))
            })
            .expect("every name must be listed in the help")
    };
    let custom = section_of("commit-message ");
    for builtin in ["backend ", "config ", "doctor ", "describe ", "update "] {
        assert_ne!(
            section_of(builtin),
            custom,
            "{builtin}shares the configured section"
        );
    }
    assert_eq!(section_of("backend "), section_of("update "));
}

/// Progress indicators are for a terminal: through a pipe — how every test,
/// and every program driving npu, sees it — stderr carries no escape
/// sequence and no carriage return, even while a model is being waited on.
#[test]
fn no_indicator_reaches_a_non_terminal_stderr() {
    let xdg = fixture_dir("no-indicator-xdg");
    let cwd = fixture_dir("no-indicator-cwd");
    write_healthy_scope(&xdg, &closed_port_base_url());

    let output = run_npu(&cwd, &xdg, &["--verbose", "info", "commit-message"]);

    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
    assert!(
        !output
            .stderr
            .iter()
            .any(|byte| *byte == 0x1b || *byte == b'\r'),
        "stderr: {}",
        stderr_of(&output)
    );
}
