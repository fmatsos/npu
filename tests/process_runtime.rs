//! End-to-end verification of the PROCESS runtime family, against the REAL
//! binary (`env!("CARGO_BIN_EXE_npu")`).
//!
//! **The controlled child is this test binary itself.** No test here needs
//! `llama-server`, Docker, or anything else installed: the backend fixtures
//! point their `[runtime].command` at `std::env::current_exe()` and run the
//! `fake_server` test below, which reads `NPU_FAKE_MODE` and behaves as a
//! server that binds, exits at once, never binds, or writes to both streams.
//! Every other test of this file reduces to "a server that does X" without
//! there being a server anywhere.
//!
//! `NPU_FAKE_MODE` is handed to the CHILD, through the backend's
//! `[runtime.env]` overlay — never set on this process: `std::env::set_var`
//! is `unsafe` under edition 2024 and forbidden here (`unsafe_code =
//! "forbid"`, cf. Cargo.toml). That also makes the overlay itself part of
//! what these tests exercise.
//!
//! The scope is mounted through `$XDG_CONFIG_HOME` and the state directory
//! through `$XDG_STATE_HOME`, both on the child `npu` process only (same
//! idiom as `tests/builtins_and_degraded_mode.rs`), so nothing here can see
//! the machine's real configuration or leave a record in the developer's
//! `~/.local/state`.

#![allow(clippy::expect_used)] // tolerated in tests (cf. Cargo.toml [lints.clippy]).

use std::io::Write as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// The variable the fake server reads, handed to it by the fixture's
/// `[runtime.env]` overlay.
const MODE_VAR: &str = "NPU_FAKE_MODE";

/// The port the fake server binds, substituted from `{{ backend.port }}` by
/// the same overlay.
const PORT_VAR: &str = "NPU_FAKE_PORT";

/// What a "noisy" fake server writes on stdout, and what `npu logs` must
/// hand back.
const STDOUT_MARKER: &str = "fake-server-on-stdout";

/// The same on stderr: a server logging there must not lose half its output.
const STDERR_MARKER: &str = "fake-server-on-stderr";

/// A variable set on the `npu` process itself — never on this one, where
/// `set_var` is `unsafe` and forbidden. `[runtime.env]` is documented as an
/// OVERLAY, so the spawned server must still see it: adding an
/// `.env_clear()` before the overlay would otherwise ship green and break
/// every real server that needs `HOME`, `PATH` or a proxy variable.
const INHERITED_VAR: &str = "NPU_INHERITED_MARKER";

/// Its value, echoed back by the fake server so the assertion is on a VALUE
/// and not on any wording.
const INHERITED_VALUE: &str = "inherited-from-the-npu-process";

/// What the fake server prefixes its own pid with, so a test that only has
/// the log can still find the process `serve` spawned.
const PID_MARKER: &str = "fake-server-pid=";

/// How long a bound fake server stays up if nothing stops it. Long enough
/// for any test here, short enough that a stray process from an interrupted
/// run is gone before anyone notices.
const FAKE_SERVER_LIFETIME: std::time::Duration = std::time::Duration::from_secs(60);

/// The controlled child of every test in this file.
///
/// Without `NPU_FAKE_MODE` this is an ordinary (empty) test, which is how it
/// behaves in a normal `cargo test` run. With it, this binary was spawned by
/// `npu serve` and must act as the server the fixture described.
#[test]
fn fake_server() {
    let Ok(mode) = std::env::var(MODE_VAR) else {
        return;
    };

    match mode.as_str() {
        // Exits before ever binding: `serve` must report the exit status,
        // not a readiness timeout.
        "exit" => {
            eprintln!("{STDERR_MARKER}");
            std::process::exit(3);
        }
        // Never binds and never exits: `serve` must give up on its own
        // budget and terminate it. Its pid goes into the log first, which
        // is kept on that path — the only way for a test to check that the
        // child was really terminated and not orphaned.
        "silent" => {
            println!("{PID_MARKER}{}", std::process::id());
            drop(std::io::stdout().flush());
            std::thread::sleep(FAKE_SERVER_LIFETIME);
        }
        // Binds, says something on both streams, then waits.
        _ => {
            let port = std::env::var(PORT_VAR).expect("the port comes from the overlay");
            let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
                .expect("the fake server must be able to bind its port");
            println!("{STDOUT_MARKER}");
            // The overlay is layered OVER the parent environment: this one
            // was set on `npu`, not here.
            println!(
                "{INHERITED_VAR}={}",
                std::env::var(INHERITED_VAR).unwrap_or_default()
            );
            eprintln!("{STDERR_MARKER}");
            drop(std::io::stdout().flush());
            drop(std::io::stderr().flush());
            std::thread::sleep(FAKE_SERVER_LIFETIME);
            drop(listener);
        }
    }
}

/// Creates a unique directory under `target/`, so as not to pollute the repo
/// nor collide between tests run in parallel (same idiom as the other files
/// in `tests/`).
fn fixture_dir(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-fixtures")
        .join(format!("process-e2e-{name}-{n}"));
    std::fs::create_dir_all(&dir).expect("creating the fixture directory");
    dir
}

/// A port nothing is listening on: bound to learn its number, then released.
///
/// Never a hardcoded number — the suite runs in parallel, and a fixed port
/// would make two tests fight over it.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral listener");
    listener.local_addr().expect("local address").port()
}

/// Everything one scenario needs: a scope, a state directory, a home with no
/// local `.npu`, and the port its fake server was told to bind.
struct Fixture {
    scope: PathBuf,
    state: PathBuf,
    home: PathBuf,
    port: u16,
}

impl Fixture {
    /// Writes a scope with one backend served by `mode` and one model
    /// pointing at it.
    fn new(name: &str, mode: &str, startup_timeout_secs: u64) -> Fixture {
        Fixture::build(name, mode, startup_timeout_secs, false)
    }

    /// The same fixture, but reached through a `/bin/sh` wrapper that ends
    /// on `exec` — the shape of every launcher people actually put in
    /// `command`: a shell script, a virtualenv or `uv`/`conda` shim, a
    /// `Makefile` recipe.
    ///
    /// `exec` swaps the image and keeps the birth, so the running process is
    /// still the pid `npu serve` spawned while reporting another executable.
    /// Unix-only because it is about `/bin/sh`.
    #[cfg(unix)]
    fn through_a_wrapper(name: &str, mode: &str, startup_timeout_secs: u64) -> Fixture {
        Fixture::build(name, mode, startup_timeout_secs, true)
    }

    fn build(name: &str, mode: &str, startup_timeout_secs: u64, wrapped: bool) -> Fixture {
        let root = fixture_dir(name);
        let scope = root.join("scope");
        let state = root.join("state");
        let home = root.join("home");
        std::fs::create_dir_all(&home).expect("the home directory");
        let port = free_port();

        // `command` is THIS binary, and the arguments make it run the
        // `fake_server` test alone: the controlled child is the test suite
        // itself, which is what keeps this file free of any dependency on
        // an installed server.
        let executable = std::env::current_exe().expect("the test binary's own path");
        let command = if wrapped {
            wrapper(&root, &executable)
        } else {
            executable.clone()
        };

        write(
            &scope,
            "npu/backends/local.toml",
            &format!(
                r#"
id = "local"
base_url = "http://127.0.0.1:{{{{ backend.port }}}}"
type = "openai-compatible"
port = {port}

[operations.chat]
method = "POST"
path = "/v1/chat/completions"

[runtime]
type = "process"
command = "{}"
arguments = ["--exact", "fake_server", "--nocapture"]
startup_timeout_secs = {startup_timeout_secs}

[runtime.env]
{MODE_VAR} = "{mode}"
{PORT_VAR} = "{{{{ backend.port }}}}"
"#,
                command.display()
            ),
        );

        write(
            &scope,
            "npu/models/qwen.toml",
            r#"
id = "qwen"
backend = "local"
operation = "chat"
model = "qwen3-4b"
"#,
        );

        Fixture {
            scope,
            state,
            home,
            port,
        }
    }

    /// Runs the real `npu` binary against this fixture.
    fn npu(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_npu"))
            .args(args)
            .current_dir(&self.home)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.scope)
            .env("XDG_STATE_HOME", &self.state)
            .env(INHERITED_VAR, INHERITED_VALUE)
            .env_remove(MODE_VAR)
            .env_remove(PORT_VAR)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("launching the npu binary")
            .wait_with_output()
            .expect("waiting for the npu binary")
    }

    /// The same backend identifier in ANOTHER project: its own scope, the
    /// machine's ONE state directory and one home.
    ///
    /// Backend identifiers are per-scope and `./.npu` is a documented scope,
    /// so `llamacpp` declared by two projects is ordinary, not pathological
    /// — while the state directory they share is machine-global. The home is
    /// shared with the state directory on purpose: macOS derives the state
    /// path from `$HOME` and ignores `XDG_STATE_HOME`, so splitting them
    /// would make this scenario stop being one on that platform.
    fn beside(&self, name: &str) -> Fixture {
        let mut other = Fixture::new(name, "bind", 20);
        other.state.clone_from(&self.state);
        other.home.clone_from(&self.home);
        other
    }

    /// The environment `npu` resolves its state directory from, built from
    /// the very variables [`Fixture::npu`] hands the child.
    fn state_env(&self) -> npu::runtime::state::StateEnv {
        npu::runtime::state::StateEnv {
            xdg_state_home: Some(self.state.clone()),
            home: Some(self.home.clone()),
        }
    }

    /// This fixture's backend FILE, as `npu` records it: canonicalized when
    /// the filesystem allows it, exactly as `config.rs` does.
    ///
    /// Half the state record's name, so a test cannot find that record
    /// without it — which is the whole point: the same identifier in
    /// another project is another file and another record.
    fn backend_source(&self) -> PathBuf {
        let declared = self.scope.join("npu").join("backends").join("local.toml");
        std::fs::canonicalize(&declared).unwrap_or(declared)
    }

    /// The state record `npu serve` writes.
    ///
    /// Derived through the library's own resolver rather than by restating
    /// the Linux convention: the directory is `$XDG_STATE_HOME/npu` on Linux
    /// and `$HOME/Library/Application Support/npu/state` on macOS, and a
    /// hard-coded path here would make every assertion below fail on the
    /// second platform for a reason that has nothing to do with the runtime.
    /// The file NAME is resolved the same way, and for the same reason: it
    /// carries a digest of the backend file this suite must not restate.
    fn record(&self) -> PathBuf {
        npu::runtime::state::state_path(&self.state_env(), "local", &self.backend_source())
            .expect("a valid backend identifier")
    }

    /// The log file `npu serve` redirects the child's two streams into: the
    /// record's own path with a `.log` extension, exactly as `serve` derives
    /// it.
    fn log(&self) -> PathBuf {
        self.record().with_extension("log")
    }
}

/// Writes an executable `/bin/sh` script that `exec`s `target` with whatever
/// arguments it was given, and returns its path.
///
/// Deliberately ends on `exec` and not on a plain call: that is what every
/// real launcher does, and what makes the spawned pid report an executable
/// other than the one `npu` recorded.
#[cfg(unix)]
fn wrapper(root: &Path, target: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;

    let path = root.join("wrapper.sh");
    std::fs::write(
        &path,
        format!("#!/bin/sh\nexec \"{}\" \"$@\"\n", target.display()),
    )
    .expect("writing the wrapper script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("making the wrapper executable");
    path
}

fn write(dir: &Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("creating the parent directory");
    }
    std::fs::write(path, contents).expect("writing the fixture");
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// The pid `npu serve` wrote on stdout — its RESULT, and the only thing on
/// that stream.
fn served_pid(output: &Output) -> u32 {
    stdout_of(output)
        .trim()
        .parse()
        .expect("serve's result on stdout is the pid and nothing else")
}

/// Is that pid still a live process? Uses the library's own inspector, so
/// the assertion rests on the same source `stop` does.
fn is_live(pid: u32) -> bool {
    npu::runtime::process::inspect(pid).is_some()
}

/// Waits for a pid to disappear, with a bound: signal delivery and process
/// teardown are asynchronous, and a bare assertion would be flaky.
fn wait_until_gone(pid: u32) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if !is_live(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

/// The whole lifecycle in one pass: start, report, stop, report again.
///
/// Written as one test rather than four because the four share one served
/// process, and splitting them would mean four servers and four chances to
/// leave one behind.
#[test]
fn serve_then_status_then_stop_then_status() {
    let fixture = Fixture::new("lifecycle", "bind", 20);

    let served = fixture.npu(&["serve", "qwen"]);
    assert_eq!(
        served.status.code(),
        Some(0),
        "stderr: {}",
        stderr_of(&served)
    );
    let pid = served_pid(&served);
    assert!(is_live(pid), "the served process must be running");

    let status = fixture.npu(&["status"]);
    assert_eq!(status.status.code(), Some(0));
    let report = stdout_of(&status);
    let line = report
        .lines()
        .find(|line| line.starts_with("local"))
        .expect("the backend must have a line");
    // Identifiers, never prose: the family, the instance (a pid, for this
    // family) and the port the report claims it is reachable on.
    assert!(line.contains("process"), "{report}");
    assert!(line.contains(&pid.to_string()), "{report}");
    assert!(line.contains(&fixture.port.to_string()), "{report}");

    let stopped = fixture.npu(&["stop", "qwen"]);
    assert_eq!(
        stopped.status.code(),
        Some(0),
        "stderr: {}",
        stderr_of(&stopped)
    );
    assert_eq!(stdout_of(&stopped).trim(), "local");
    assert!(wait_until_gone(pid), "stop must end the served process");

    // And the record is gone with it: a second `status` must not claim a
    // runtime that no longer exists. Asserted on the RECORD and not on the
    // state word the row prints — a state word is prose, and `status` still
    // has to produce a line either way.
    assert!(!fixture.record().exists(), "{}", fixture.record().display());
    let after = fixture.npu(&["status"]);
    assert_eq!(after.status.code(), Some(0));
    assert!(
        stdout_of(&after)
            .lines()
            .any(|line| line.starts_with("local")),
        "the backend must still have a line"
    );
}

/// The same lifecycle, through a launcher that `exec`s — a shell wrapper, a
/// virtualenv or `uv`/`conda` shim, the shape most `command` values really
/// have.
///
/// The regression this pins: while process identity also required the
/// EXECUTABLE to match, such a runtime reported `stale state` the moment it
/// was up, and `npu stop` answered success (exit `0`, the backend id on
/// stdout) while leaving the server running, holding its port, and deleting
/// the only record that could still find it. The assertion that pins it is
/// the last one — the process is actually GONE after `stop` — since under
/// the regression `stop` cleared the record WITHOUT signalling anything. No
/// state word is asserted: prose can be reformulated, and the fact that was
/// false is the kill, not the label.
#[cfg(unix)]
#[test]
fn a_server_reached_through_an_exec_ing_wrapper_is_still_ours_to_stop() {
    let fixture = Fixture::through_a_wrapper("exec-wrapper", "bind", 20);

    let served = fixture.npu(&["serve", "qwen"]);
    assert_eq!(
        served.status.code(),
        Some(0),
        "stderr: {}",
        stderr_of(&served)
    );
    let pid = served_pid(&served);
    assert!(is_live(pid), "the served process must be running");

    let report = stdout_of(&fixture.npu(&["status"]));
    let line = report
        .lines()
        .find(|line| line.starts_with("local"))
        .expect("the backend must have a line");
    assert!(line.contains(&pid.to_string()), "{report}");

    let stopped = fixture.npu(&["stop", "qwen"]);
    assert_eq!(
        stopped.status.code(),
        Some(0),
        "stderr: {}",
        stderr_of(&stopped)
    );
    assert!(
        wait_until_gone(pid),
        "stop reported success, so the server must be gone — not orphaned"
    );
}

/// Another project's runtime is neither stopped nor taken over.
///
/// Two projects each declaring a backend `local` share the state DIRECTORY,
/// which is machine-global while identifiers are per-scope. They no longer
/// share a state FILE: its name carries a digest of the backend file it was
/// served from, so each project gets its own record, its own log and its own
/// server.
///
/// Before that, the second project's `npu stop` found a pid that was
/// genuinely an npu-started process with a genuinely matching birth, and
/// SIGTERM/SIGKILLed the first project's server — then cleared the record,
/// leaving `npu status` in the first project reporting a runtime that had
/// never been stopped by anyone who meant to.
///
/// What is asserted here is the OUTCOME, not the mechanism: B never reaches
/// A's server, and A keeps its record and its ability to stop it. The
/// mechanism moved — B used to be REFUSED with exit `3`, and is now simply
/// looking somewhere else — and pinning the refusal would pin the old
/// design. The refusal itself is still reachable through a digest collision
/// and is covered where a collision can be constructed
/// (`runtime::process::tests`).
#[test]
fn another_project_s_runtime_is_neither_stopped_nor_taken_over() {
    let project_a = Fixture::new("two-projects-a", "bind", 20);
    let served = project_a.npu(&["serve", "qwen"]);
    assert_eq!(
        served.status.code(),
        Some(0),
        "stderr: {}",
        stderr_of(&served)
    );
    let pid = served_pid(&served);

    let project_b = project_a.beside("two-projects-b");

    // B's backend was never served, so B's `stop` is the idempotent
    // success it is for anything never started — and it reaches nothing of
    // A's.
    let stopped = project_b.npu(&["stop", "qwen"]);
    assert_eq!(stopped.status.code(), Some(0), "{}", stderr_of(&stopped));
    assert!(
        is_live(pid),
        "the other project's server must not have been signalled"
    );
    assert!(
        project_a.record().exists(),
        "the record of a running server must not be deleted by another project"
    );
    assert_ne!(
        project_b.record(),
        project_a.record(),
        "two backend files must not share one record"
    );

    // And B's `serve` starts B's OWN server, on its own port, without
    // truncating A's log or overwriting A's record.
    let again = project_b.npu(&["serve", "qwen"]);
    assert_eq!(again.status.code(), Some(0), "{}", stderr_of(&again));
    let other_pid = served_pid(&again);
    assert_ne!(other_pid, pid);
    assert!(is_live(pid), "A's server must still be up");
    assert!(project_a.record().exists());

    // Each project stops its own, and only its own.
    let by_b = project_b.npu(&["stop", "qwen"]);
    assert_eq!(by_b.status.code(), Some(0), "{}", stderr_of(&by_b));
    assert!(wait_until_gone(other_pid), "B must stop B's server");
    assert!(is_live(pid), "B's stop must not reach A's server");

    let by_owner = project_a.npu(&["stop", "qwen"]);
    assert_eq!(by_owner.status.code(), Some(0), "{}", stderr_of(&by_owner));
    assert!(wait_until_gone(pid), "its own project must still stop it");
}

/// The READ half of the same isolation, and the defect this keying exists
/// for: project B's `npu logs` used to print project A's server output on
/// stdout, with exit `0` — `logs` reads the log file without ever consulting
/// a record, so nothing compared the two origins.
#[test]
fn another_project_s_logs_are_never_handed_over() {
    let project_a = Fixture::new("two-projects-logs-a", "bind", 20);
    let served = project_a.npu(&["serve", "qwen"]);
    assert_eq!(
        served.status.code(),
        Some(0),
        "stderr: {}",
        stderr_of(&served)
    );
    let pid = served_pid(&served);

    // A has a log, and it holds what A's server wrote.
    let mine = project_a.npu(&["logs", "qwen"]);
    assert_eq!(mine.status.code(), Some(0), "{}", stderr_of(&mine));
    assert!(
        stdout_of(&mine).contains(STDOUT_MARKER),
        "{}",
        stdout_of(&mine)
    );

    let project_b = project_a.beside("two-projects-logs-b");
    let theirs = project_b.npu(&["logs", "qwen"]);

    // B served nothing: exit `3`, and stdout is ZERO bytes — never a single
    // byte of A's log.
    assert_eq!(theirs.status.code(), Some(3), "{}", stderr_of(&theirs));
    assert!(theirs.stdout.is_empty(), "{:?}", stdout_of(&theirs));
    assert!(
        !stdout_of(&theirs).contains(STDOUT_MARKER),
        "{}",
        stdout_of(&theirs)
    );
    assert_ne!(
        project_b.log(),
        project_a.log(),
        "two backend files must not share one log"
    );

    drop(project_a.npu(&["stop", "qwen"]));
    assert!(wait_until_gone(pid));
}

/// `stop` is idempotent, exactly like on the Docker side: stopping what was
/// never started is a success, not an error to script around.
#[test]
fn stopping_a_backend_that_was_never_served_succeeds() {
    let fixture = Fixture::new("stop-idempotent", "bind", 20);

    let stopped = fixture.npu(&["stop", "qwen"]);

    assert_eq!(stopped.status.code(), Some(0), "{}", stderr_of(&stopped));
    assert_eq!(stdout_of(&stopped).trim(), "local");
}

/// A second `serve` must refuse, naming the backend and the pid already
/// holding it — never start a second server fighting for the same port.
#[test]
fn serving_twice_is_refused_and_writes_nothing_to_stdout() {
    let fixture = Fixture::new("serve-twice", "bind", 20);

    let served = fixture.npu(&["serve", "qwen"]);
    assert_eq!(served.status.code(), Some(0), "{}", stderr_of(&served));
    let pid = served_pid(&served);

    let again = fixture.npu(&["serve", "qwen"]);

    assert_eq!(again.status.code(), Some(3));
    assert!(again.stdout.is_empty(), "{:?}", stdout_of(&again));
    let message = stderr_of(&again);
    assert!(message.contains("local"), "{message}");
    assert!(message.contains(&pid.to_string()), "{message}");

    drop(fixture.npu(&["stop", "qwen"]));
    assert!(wait_until_gone(pid));
}

/// A server that dies during startup is reported as having EXITED, with its
/// status — not as a readiness timeout, which would blame the budget for a
/// crash.
#[test]
fn a_server_that_exits_at_once_fails_with_three_and_keeps_its_log() {
    let fixture = Fixture::new("early-exit", "exit", 20);

    let served = fixture.npu(&["serve", "qwen"]);

    assert_eq!(served.status.code(), Some(3));
    assert!(served.stdout.is_empty(), "{:?}", stdout_of(&served));
    let message = stderr_of(&served);
    // The backend, the command and the log path: the three things its
    // reader needs to go look at something.
    assert!(message.contains("local"), "{message}");
    assert!(
        message.contains(
            &std::env::current_exe()
                .expect("the test binary's own path")
                .display()
                .to_string()
        ),
        "{message}"
    );
    let log = fixture.log();
    assert!(
        message.contains(&log.display().to_string()),
        "{message}\nexpected: {}",
        log.display()
    );

    // The log is the only evidence of WHY it refused to start: it survives
    // the failure.
    let kept = std::fs::read_to_string(&log).expect("the log must be kept");
    assert!(kept.contains(STDERR_MARKER), "{kept}");

    // And the record does not: a failed start must leave nothing behind
    // that `status` would report as a runtime.
    assert!(!fixture.record().exists(), "{}", fixture.record().display());
}

/// A server that never answers is given its budget, then terminated: the
/// failure must not leave an orphan behind.
#[test]
fn a_server_that_never_binds_times_out_and_is_terminated() {
    let fixture = Fixture::new("never-binds", "silent", 1);

    let served = fixture.npu(&["serve", "qwen"]);

    assert_eq!(served.status.code(), Some(3), "{}", stderr_of(&served));
    assert!(served.stdout.is_empty(), "{:?}", stdout_of(&served));
    let message = stderr_of(&served);
    assert!(message.contains("local"), "{message}");
    assert!(
        message.contains(&fixture.log().display().to_string()),
        "{message}"
    );

    // Nothing left recorded...
    assert!(!fixture.record().exists(), "{}", fixture.record().display());

    // ...and, above all, nothing left RUNNING. The state file says nothing
    // about the process, so the pid comes out of the log this path keeps:
    // an orphaned server would hold its port while every npu command
    // reported the backend as never started.
    let kept = std::fs::read_to_string(fixture.log()).expect("the log must be kept");
    let pid: u32 = kept
        .lines()
        .find_map(|line| line.trim().strip_prefix(PID_MARKER))
        .expect("the silent server announces its pid before sleeping")
        .parse()
        .expect("a pid");
    assert!(
        wait_until_gone(pid),
        "the server that never answered must have been terminated, not orphaned"
    );
}

/// Both of a server's streams reach `npu logs`: a server that logs to stderr
/// (most do) would otherwise lose half its output.
#[test]
fn logs_hand_back_what_the_server_wrote_on_both_streams() {
    let fixture = Fixture::new("logs", "bind", 20);

    let served = fixture.npu(&["serve", "qwen"]);
    assert_eq!(served.status.code(), Some(0), "{}", stderr_of(&served));
    let pid = served_pid(&served);

    let logs = fixture.npu(&["logs", "qwen"]);

    assert_eq!(logs.status.code(), Some(0), "{}", stderr_of(&logs));
    let out = stdout_of(&logs);
    assert!(out.contains(STDOUT_MARKER), "{out}");
    assert!(out.contains(STDERR_MARKER), "{out}");
    // `[runtime.env]` is an overlay: the child still sees what `npu` itself
    // was given. A regression to `.env_clear().envs(...)` would break every
    // server needing `HOME`, `PATH` or a proxy variable, and nothing else
    // here would notice.
    assert!(out.contains(INHERITED_VALUE), "{out}");

    drop(fixture.npu(&["stop", "qwen"]));
    assert!(wait_until_gone(pid));
}

/// `npu logs` on a backend that was never served says so, naming it, and
/// writes nothing on stdout — which carries the log and nothing else.
#[test]
fn logs_of_a_never_served_backend_fail_with_three_and_an_empty_stdout() {
    let fixture = Fixture::new("logs-absent", "bind", 20);

    let logs = fixture.npu(&["logs", "qwen"]);

    assert_eq!(logs.status.code(), Some(3));
    assert!(logs.stdout.is_empty(), "{:?}", stdout_of(&logs));
    assert!(stderr_of(&logs).contains("local"), "{}", stderr_of(&logs));
}

/// A command this machine does not have is something to INSTALL: `doctor`
/// says so with exit `3`, never `2`, and names the command.
#[test]
fn doctor_reports_a_missing_runtime_command_as_unreachable() {
    let root = fixture_dir("doctor-missing-command");
    let scope = root.join("scope");
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("the home directory");
    let port = free_port();

    write(
        &scope,
        "npu/backends/local.toml",
        &format!(
            r#"
id = "local"
base_url = "http://127.0.0.1:{port}"
type = "openai-compatible"

[operations.chat]
method = "POST"
path = "/v1/chat/completions"

[runtime]
type = "process"
command = "npu-no-such-command-anywhere"
"#
        ),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_npu"))
        .arg("doctor")
        .current_dir(&home)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &scope)
        .env("XDG_STATE_HOME", root.join("state"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launching the npu binary")
        .wait_with_output()
        .expect("waiting for the npu binary");

    // `3`, never `2`: nothing in the files is wrong.
    assert_eq!(output.status.code(), Some(3), "{}", stdout_of(&output));
    let report = stdout_of(&output);
    assert!(report.contains("npu-no-such-command-anywhere"), "{report}");
}

/// `port = "auto"` cannot be honoured for a process runtime, and that is a
/// CONFIGURATION error: exit `2`, naming the file to fix.
#[test]
fn port_auto_with_a_process_runtime_is_rejected_end_to_end() {
    let root = fixture_dir("auto-port");
    let scope = root.join("scope");
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("the home directory");

    write(
        &scope,
        "npu/backends/local.toml",
        r#"
id = "local"
base_url = "http://127.0.0.1:{{ backend.port }}"
type = "openai-compatible"
port = "auto"

[operations.chat]
method = "POST"
path = "/v1/chat/completions"

[runtime]
type = "process"
command = "llama-server"
arguments = ["--port", "{{ backend.port }}"]
"#,
    );
    write(
        &scope,
        "npu/models/qwen.toml",
        r#"
id = "qwen"
backend = "local"
operation = "chat"
model = "qwen3-4b"
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_npu"))
        .args(["serve", "qwen"])
        .current_dir(&home)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &scope)
        .env("XDG_STATE_HOME", root.join("state"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launching the npu binary")
        .wait_with_output()
        .expect("waiting for the npu binary");

    assert_eq!(output.status.code(), Some(2), "{}", stderr_of(&output));
    assert!(output.stdout.is_empty(), "{:?}", stdout_of(&output));
    assert!(
        stderr_of(&output).contains("local.toml"),
        "{}",
        stderr_of(&output)
    );
}
