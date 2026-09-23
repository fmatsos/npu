//! The process runtime: what `serve`, `stop`, `status` and `logs` do for a
//! backend declaring `type = "process"`.
//!
//! ZERO knowledge of any particular server. This module runs a command, and
//! WHICH command — llama.cpp, an MLX server, a shell script — comes entirely
//! from the backend's `[runtime]` table, exactly as the image does on the
//! Docker side. Nothing here mentions a model format, a weights directory or
//! an accelerator.
//!
//! Docker IS its own registry, so the Docker family persists nothing. A
//! process has no registry: once `npu serve` returns, the only thing that
//! remembers the pid is the record [`crate::runtime::state`] wrote. Every
//! function here therefore starts from that record, and the single most
//! dangerous question in the file — "is the process behind this pid still
//! the one we started?" — is answered by [`verdict`], never by the pid
//! alone.
//!
//! Everything that touches the outside world is injected through [`Host`]:
//! the environment, the state directory, process inspection, signalling and
//! the readiness probe. No test in the suite needs `llama-server`, or any
//! other server, installed — the integration tests spawn the TEST BINARY
//! itself as their controlled child.

use std::fmt;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::state;

/// This family's name: the `type` value a `[runtime]` table declares, and
/// what the `RUNTIME` column of `npu status` shows.
pub const NAME: &str = "process";

/// Interval between two readiness attempts while `serve` waits for the
/// spawned server to answer. Within the 100–250 ms band the contract asks
/// for: fast enough that a server ready in 300 ms is not made to look slow,
/// slow enough that a 30 s budget costs two hundred probes, not thirty
/// thousand.
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Interval between two reads of the log file in `logs --follow`.
///
/// ponytail: polling, not inotify — `--follow` is a human watching a log,
/// 200 ms is imperceptible to them, and a watcher would be a dependency plus
/// a platform matrix for exactly that.
const FOLLOW_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// How long a process is given to honour a graceful termination before it is
/// killed outright.
const TERMINATION_GRACE: Duration = Duration::from_secs(5);

/// Extension of the file `serve` redirects a spawned server's two streams
/// into, beside its state record in the state directory.
const LOG_EXTENSION: &str = "log";

/// Size of the buffer [`logs`] reads a log file through: one page, so a long
/// log is streamed rather than loaded whole.
const LOG_CHUNK: usize = 4096;

/// What [`status`](crate::builtin::status) reports for a runtime whose
/// recorded process is gone.
const EXITED: &str = "exited";

/// What it reports for a record whose pid is now somebody else's process:
/// it was recycled, so the record describes something that is no longer
/// there.
const STALE: &str = "stale state";

/// What it reports for a live process answering on its port.
const RUNNING: &str = "running";

/// What it reports for a live process NOT answering on its port: starting
/// up, wedged, or listening somewhere else than its `base_url` claims.
const UNREACHABLE: &str = "unreachable";

/// What it reports for a record another backend FILE wrote: the state
/// directory is machine-global, backend identifiers are not, so this row
/// describes somebody else's server and npu will not touch it.
const FOREIGN: &str = "foreign state";

/// The `INSTANCE` column's value when there is no pid to show.
const NO_INSTANCE: &str = "-";

/// The two signals this runtime sends, named independently of `sysinfo` so
/// that the dependency stays inside [`signal`] and a test can inject a
/// signaller without linking anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// "Please stop": the polite one, which a server may handle to flush.
    Term,
    /// "Stop now": the one nothing can decline.
    Kill,
}

/// What the operating system says about a pid, reduced to the ONE fact that
/// makes up a process IDENTITY.
///
/// A name is deliberately absent: on Linux it is the kernel's `comm` field,
/// truncated to 15 bytes, so `llama-server-v2` and `llama-server-v3` share
/// one. Comparing names would be a check that looks like one and is not.
///
/// The EXECUTABLE is absent for a stronger reason, and the reason is
/// empirical: `exec` replaces a task's image without changing its birth, so
/// a server reached through a wrapper script, a virtualenv shim or any
/// launcher that ends on `exec` runs under the pid `npu` spawned while
/// reporting a completely different executable. Comparing it made every such
/// runtime permanently "stale", which `stop` reports as a success while
/// leaving the server running and deleting the only record of it. Birth time
/// answers the question the check actually asks — see [`verdict`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessFacts {
    /// Seconds since the Unix epoch at which this pid was created — the
    /// token that makes PID REUSE detectable.
    ///
    /// Whole SECONDS, and that is the resolution of the check: `sysinfo`
    /// derives it from `/proc/stat`'s `btime` plus field 22 of
    /// `/proc/<pid>/stat` divided by the clock tick, an integer division.
    /// Two processes sharing a pid and born within the same epoch second
    /// are therefore indistinguishable here. Reaching that needs the pid
    /// space to wrap inside one second, which a stock `pid_max` of four
    /// million makes impractical but a container with a small pid
    /// namespace does not. There is no finer token without `unsafe` or a
    /// pidfd, so the resolution is stated rather than hidden.
    pub start_time: u64,
}

/// Everything this runtime needs from the outside world, in one place.
///
/// Bundled rather than passed as five parameters so that adding an eventual
/// sixth does not reopen every signature between here and `lib.rs`, and so
/// that a test writes its fakes once.
pub struct Host<'a> {
    /// Reads an environment variable of the CURRENT process — the parent
    /// environment a spawned child inherits, and what `{{ env.NAME }}`
    /// resolves against.
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// Where the state records live.
    pub state: state::StateEnv,
    /// What the system says about a pid (the real one is [`inspect`]).
    pub inspect: &'a dyn Fn(u32) -> Option<ProcessFacts>,
    /// Sends a signal, `true` only when it was actually delivered (the real
    /// one is [`signal`]).
    pub signal: &'a dyn Fn(u32, Signal) -> bool,
    /// Is something answering on this base URL? The same shape as
    /// `doctor`'s probe, and `builtin::tcp_probe` is the real one.
    pub probe: &'a dyn Fn(&str) -> Result<(), String>,
}

// `missing_debug_implementations` is a warning, and warnings are errors: a
// struct of `&dyn Fn` cannot derive `Debug`, so it gets one by hand. The
// closures have nothing to print; the state environment has.
impl fmt::Debug for Host<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Host").field("state", &self.state).finish()
    }
}

/// The real [`Host::inspect`]: what the system says about `pid`.
///
/// `System::new()` and a refresh restricted to that ONE pid: `new_all()`
/// would walk every process on the machine to answer a question about one.
/// The refresh's return value IS the liveness test — `0` means the pid is
/// not there — and `ProcessRefreshKind::nothing()` asks for no optional
/// datum at all, `start_time()` being part of the entry the refresh creates
/// rather than something a refresh kind turns on.
#[must_use]
pub fn inspect(pid: u32) -> Option<ProcessFacts> {
    let pid = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();

    if system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[pid]),
        true,
        sysinfo::ProcessRefreshKind::nothing(),
    ) == 0
    {
        return None;
    }

    let process = system.process(pid)?;
    Some(ProcessFacts {
        start_time: process.start_time(),
    })
}

/// The real [`Host::signal`]: sends `signal` to `pid`, and answers whether
/// it was DELIVERED.
///
/// `sysinfo` distinguishes three outcomes and only one of them is a success:
/// `Some(true)` delivered, `Some(false)` supported but failed, `None`
/// unsupported on this platform. An `.is_some()` here would report a failed
/// kill as a success and let `stop` clear the state of a process still
/// running.
#[must_use]
pub fn signal(pid: u32, signal: Signal) -> bool {
    let pid = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();

    if system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true) == 0 {
        return false;
    }
    let Some(process) = system.process(pid) else {
        return false;
    };

    let signal = match signal {
        Signal::Term => sysinfo::Signal::Term,
        Signal::Kill => sysinfo::Signal::Kill,
    };
    matches!(process.kill_with(signal), Some(true))
}

/// Where a backend's state record says its runtime is, from `npu`'s point of
/// view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence {
    /// No record at all: this backend was never served, or was stopped.
    NotStarted,
    /// The recorded process is there and is the one `npu` started.
    Alive(state::State),
    /// The recorded pid no longer exists: the server exited on its own.
    Exited(state::State),
    /// The pid exists but was born at another moment, so it is NOT the
    /// process `npu` started: it was recycled. The record is stale, and the
    /// pid it holds belongs to somebody else — to report and forget, never
    /// to signal.
    Reused(state::State),
    /// A LIVE process recorded by another backend file.
    ///
    /// The state directory is machine-global while backend identifiers are
    /// per-scope, so two projects each declaring `llamacpp` in their own
    /// `./.npu` would land on one record. What keeps them apart is the file
    /// NAME, which carries a digest of the backend file (see
    /// `state::SOURCE_DIGEST_HEX`): the digest ISOLATES.
    ///
    /// This variant is what catches the case the digest cannot. Eight hex
    /// characters are 32 bits, so two distinct source paths CAN meet on one
    /// name, and a record found there is then another project's: its pid is
    /// a genuine npu-started process with a matching birth, which
    /// [`verdict`] is structurally unable to see anything wrong with. The
    /// origin comparison is the COLLISION GUARD — on the one code path
    /// where being wrong is a SIGTERM, then a SIGKILL, against an unrelated
    /// project's server, and therefore unrecoverable. To report, never to
    /// signal and never to overwrite.
    Foreign(state::State),
}

impl Presence {
    /// The record this presence was derived from, if any.
    #[must_use]
    fn state(&self) -> Option<&state::State> {
        match self {
            Presence::NotStarted => None,
            Presence::Alive(state)
            | Presence::Exited(state)
            | Presence::Reused(state)
            | Presence::Foreign(state) => Some(state),
        }
    }
}

/// Decides whether `facts` describe the very process `state` was written
/// for.
///
/// **This is the function that keeps `npu stop` from killing an innocent
/// process.** Two conditions, both required:
///
/// 1. the pid exists at all (`facts` is `Some`);
/// 2. its creation time equals the recorded one — pids are recycled, and
///    after a reboot or enough churn the pid in the record names somebody
///    else's process, which `start_time` is what detects.
///
/// Birth time is the WHOLE check of a pid's identity, and deliberately so.
/// `start_time` is an absolute epoch SECOND read from the system, so a
/// recycled pid — after a reboot, or after the pid counter wrapped — carries
/// a different one and is caught, EXCEPT when the reuse happened inside the
/// same second as the recorded birth (see [`ProcessFacts::start_time`] for
/// the resolution and for when that is reachable). Nothing else adds to
/// that:
///
/// - the EXECUTABLE cannot: `exec` swaps a task's image while leaving its
///   birth untouched, so a mismatching executable under a matching birth is
///   always this very child after an exec (a wrapper script, a virtualenv or
///   `uv`/`conda` shim, anything ending on `exec`), never a stranger.
///   Requiring it to match made every such runtime report itself `stale`,
///   and `stop` then answered success while leaving the server running and
///   erasing the only record that could still find it;
/// - the COMMAND LINE cannot either: a server that rewrites its own `argv`
///   (several do) would be declared an impostor by it;
/// - a process NAME cannot: the Linux kernel truncates `comm` to 15 bytes.
#[must_use]
fn verdict(state: state::State, facts: Option<&ProcessFacts>) -> Presence {
    let Some(facts) = facts else {
        return Presence::Exited(state);
    };

    if facts.start_time == state.process_start_time {
        Presence::Alive(state)
    } else {
        Presence::Reused(state)
    }
}

/// Reads `backend`'s record and confronts it with the system.
///
/// Takes the whole backend and not just its identifier because the record's
/// ORIGIN is part of the question, twice over: it is half the state file's
/// NAME (the digest, which is what keeps two projects' `llamacpp` apart),
/// and it is compared again in full once the record is read. The second
/// comparison is not redundant: the digest is 32 bits, two source paths can
/// meet on one name, and the record found there is then somebody else's.
///
/// Such a record is [`Presence::Foreign`] as long as its pid is alive —
/// never `Alive`, which `stop` would signal — and merely
/// [`Presence::Exited`] once nothing is behind that pid, since a record that
/// describes no process can harm nobody by being forgotten.
///
/// # Errors
///
/// Whatever [`state::load`] returns: an unreadable or unintelligible record
/// is an error naming its path, never a silent "not started" — which would
/// let `serve` start a second server beside the one already running.
pub fn presence(backend: &crate::config::Backend, host: &Host<'_>) -> crate::Result<Presence> {
    let Some(state) = state::load(&host.state, &backend.id, &backend.source)? else {
        return Ok(Presence::NotStarted);
    };
    let facts = (host.inspect)(state.pid);

    if state.source != backend.source {
        return Ok(if facts.is_some() {
            Presence::Foreign(state)
        } else {
            Presence::Exited(state)
        });
    }

    Ok(verdict(state, facts.as_ref()))
}

/// The message a `Foreign` record produces, for `serve` and for `stop`.
///
/// Names BOTH files — the one being acted on and the one the record was
/// written from — and the record itself: the reader has to be able to tell
/// which of their projects owns that server, and where the evidence is.
fn foreign(
    backend: &crate::config::Backend,
    state: &state::State,
    host: &Host<'_>,
) -> crate::Error {
    let record = state::state_path(&host.state, &backend.id, &backend.source)
        .map_or_else(|_| state.backend.clone(), |path| path.display().to_string());

    crate::Error::Backend(format!(
        "backend \"{}\" ({}): its state record {record} describes process {}, served from {} — \
         npu will not act on another configuration's runtime; stop it where it was started, or \
         rename this backend",
        backend.id,
        backend.source.display(),
        state.pid,
        state.source.display()
    ))
}

/// The file `serve` redirects a spawned server's two streams into: the state
/// record's own path with a `.log` extension, so both live in the state
/// directory and the identifier is validated once, by [`state::state_path`].
///
/// Derived from the record and not from the identifier alone, which is the
/// half of the isolation `npu logs` needs: the record's name carries a
/// digest of the backend FILE, so a second project asking for the logs of
/// its own `llamacpp` is answered "nothing was served" rather than handed
/// the first project's output.
fn log_path(backend: &crate::config::Backend, host: &Host<'_>) -> crate::Result<PathBuf> {
    Ok(state::state_path(&host.state, &backend.id, &backend.source)?.with_extension(LOG_EXTENSION))
}

/// Can this file be RUN, as opposed to merely existing?
///
/// A regular file with no execute bit makes `doctor` green about a command
/// `serve` then fails to spawn (`EACCES`), and — worse — shadows the real
/// binary sitting further down `PATH`, since the scan returns the first
/// match. Requiring the bit makes the scan continue, which is what `which`
/// and every shell do.
///
/// `#[cfg]` and not the `cfg!` macro used for the state directory, because
/// the Unix body needs a Unix-only trait in scope; the non-Unix body is the
/// honest answer where there is no such bit to read.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

/// Is `command` a path (`./server`, `/usr/bin/server`) rather than a bare
/// name to look up on `PATH`?
fn is_path(command: &str) -> bool {
    Path::new(command)
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty())
}

/// Locates `command`, either as the path it already is or by scanning the
/// injected `PATH`.
///
/// `None` when nothing executable answers to that name. The result is
/// CANONICALIZED when the filesystem allows it, so that every message about
/// this runtime — the spawn failure, the readiness timeout — names one
/// absolute path rather than whichever relative spelling the file happened
/// to use, and so that the path recorded in the state file still means the
/// same thing from another working directory.
fn find_command(command: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let resolve = |path: PathBuf| {
        (path.is_file() && is_executable(&path))
            .then(|| std::fs::canonicalize(&path).unwrap_or(path))
    };

    if is_path(command) {
        return resolve(PathBuf::from(command));
    }

    let path_var = env("PATH")?;
    std::env::split_paths(&path_var)
        .filter(|dir| !dir.as_os_str().is_empty())
        .find_map(|dir| resolve(dir.join(command)))
}

/// Locates the executable `backend` declares, or says what it looked for.
///
/// # Errors
///
/// `Error::Backend` (exit `3`) naming BOTH the command and the backend: a
/// command absent from this machine is something to install, not a file to
/// fix — the configuration may be perfectly correct on the machine it was
/// written for.
pub fn resolve_command(
    command: &str,
    backend_id: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> crate::Result<PathBuf> {
    find_command(command, env).ok_or_else(|| {
        crate::Error::Backend(format!(
            "backend \"{backend_id}\": command \"{command}\" not found (an absolute or relative \
             path is used as-is, a bare name is looked up on PATH)"
        ))
    })
}

/// Probes one runtime command for `doctor`'s check, against the REAL
/// environment.
///
/// # Errors
///
/// Returns `Err` naming the command when nothing executable answers to it.
pub fn command_probe(command: &str) -> Result<(), String> {
    let env = |name: &str| std::env::var(name).ok();
    if find_command(command, &env).is_some() {
        return Ok(());
    }
    Err(format!(
        "command \"{command}\" not found on PATH — install it, or point the backend's \
         [runtime].command at it"
    ))
}

/// Renders the `{{ args.model }}` / `{{ env.NAME }}` of ONE template.
///
/// Goes through `prompt::render`, the engine command files use, so a
/// placeholder behaves and fails identically in both places.
fn render_one(
    template: &str,
    model: &crate::config::Model,
    env: &dyn Fn(&str) -> Option<String>,
) -> crate::Result<String> {
    let mut args = std::collections::BTreeMap::new();
    args.insert("model".to_string(), model.model.clone());
    crate::prompt::render(template, "", &args, env)
}

/// The same, over a list.
fn render_all<'a>(
    templates: impl Iterator<Item = &'a String>,
    model: &crate::config::Model,
    env: &dyn Fn(&str) -> Option<String>,
) -> crate::Result<Vec<String>> {
    templates
        .map(|template| render_one(template, model, env))
        .collect()
}

/// The child's environment: the parent's, with the `[runtime.env]` table
/// layered over it.
///
/// An overlay rather than a replacement, and the reason nothing is cleared
/// here: a server that needs `HOME`, `PATH` or a proxy setting must not have
/// to redeclare them to gain one variable.
fn rendered_env(
    process: &crate::config::Process,
    model: &crate::config::Model,
    env: &dyn Fn(&str) -> Option<String>,
) -> crate::Result<Vec<(String, String)>> {
    let values = render_all(process.env.values(), model, env)?;
    Ok(process.env.keys().cloned().zip(values).collect())
}

/// Starts `backend`'s server and returns its pid — which IS `npu serve`'s
/// result, hence what the caller writes to stdout.
///
/// The order of the steps below is part of the contract, because it is the
/// order in which failures are reported and each one of them leaves stdout
/// at zero bytes:
///
/// 1. refuse if a managed process is already ALIVE (a stale record is
///    cleared, never killed);
/// 2. refuse if the fixed port is taken;
/// 3. render the arguments and the environment;
/// 4. create/truncate the log file;
/// 5. spawn, both streams into it;
/// 6. write the record atomically;
/// 7. wait for readiness, bounded by `startup_timeout_secs`;
/// 8. only then, the pid.
///
/// After step 5, any failure terminates the child, CLEARS the record and
/// KEEPS the log — which is the only evidence of why the server refused to
/// start.
///
/// # Errors
///
/// `Error::Backend` (exit `3`) for everything about the runtime: already
/// served, port taken, command absent, spawn refused, early exit, readiness
/// timeout. `Error::Io` for a state directory that cannot be written, and
/// `Error::Config` for a template referencing an undefined environment
/// variable.
pub fn serve(
    backend: &crate::config::Backend,
    process: &crate::config::Process,
    model: &crate::config::Model,
    host: &Host<'_>,
) -> crate::Result<String> {
    // 1. Asked BEFORE the port, exactly as on the Docker side: a backend
    //    already served holds its own port, and diagnosing that as a port
    //    conflict sends its user to fix a `port` key that is correct.
    match presence(backend, host)? {
        Presence::Alive(state) => {
            return Err(crate::Error::Backend(format!(
                "backend \"{}\" is already served by process {} — `npu status` to see it, \
                 `npu stop {}` to end it",
                backend.id, state.pid, model.id
            )));
        }
        // Refused BEFORE the log is truncated and before anything is
        // spawned: the record, the log and the port all belong to another
        // project's server, and proceeding would take all three from it.
        Presence::Foreign(state) => return Err(foreign(backend, &state, host)),
        // A record whose process is gone is not an error and not a
        // corpse to shoot at: it is forgotten, and the start proceeds.
        Presence::Exited(_) | Presence::Reused(_) => {
            state::clear(&host.state, &backend.id, &backend.source)?;
        }
        Presence::NotStarted => {}
    }

    // 2. ponytail: TOCTOU accepted — between this check and the spawn,
    //    another process can take the port. The alternative is binding the
    //    socket here and handing it to the child, which needs `unsafe`
    //    fd inheritance. The Docker side accepts the same race, and this is
    //    exactly why `port = "auto"` is refused for this family: npu has no
    //    way to ask a process which port it ended up on.
    if let Some(crate::config::Port::Fixed(port)) = &backend.port
        && !super::port_is_free(*port)
    {
        return Err(crate::Error::Backend(format!(
            "backend \"{}\": port {port} is already in use by something else — change its \
             \"port\" key, or stop what is listening on it",
            backend.id
        )));
    }

    // 3. `command` is rendered like everything else in the table.
    //    `config::validate_process` runs it through the same whitelist as
    //    the arguments — `{{ env.NAME }}` and `{{ args.model }}` are
    //    explicitly accepted there — so handing the raw string to the
    //    executable lookup would be a key validated as a template and then
    //    not honoured: exit `3` reporting a placeholder as a missing binary.
    let rendered_command = render_one(&process.command, model, host.env)?;
    let executable = resolve_command(&rendered_command, &backend.id, host.env)?;
    let arguments = render_all(process.arguments.iter(), model, host.env)?;
    let environment = rendered_env(process, model, host.env)?;

    // 4. The log file is created BEFORE the spawn and truncated: a previous
    //    run's log must not be read as this one's evidence.
    let log = log_path(backend, host)?;
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent).map_err(|err| io_at(parent, &err))?;
    }
    let out = std::fs::File::create(&log).map_err(|err| io_at(&log, &err))?;
    let errors = out.try_clone().map_err(|err| io_at(&log, &err))?;

    // 5. Both streams into the same file, in the order the server wrote
    //    them: a server that logs to stderr (most do) would otherwise leave
    //    half its diagnosis nowhere.
    // ponytail: no setsid (needs unsafe pre_exec, forbidden crate-wide); the
    // child dies on terminal SIGHUP. Session-detach shim if that bites.
    let mut child = Command::new(&executable)
        .args(&arguments)
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(errors))
        .spawn()
        .map_err(|err| {
            // Nothing was spawned, so this run produced no log — and step 4
            // just emptied the previous one. Removing it puts the state back
            // to "nothing was ever served", which is what `npu logs` then
            // says; leaving a zero-byte file behind would make it exit `0`
            // printing nothing, indistinguishable from a silent server.
            drop(std::fs::remove_file(&log));
            crate::Error::Backend(format!(
                "backend \"{}\": cannot run \"{}\": {err}",
                backend.id,
                executable.display()
            ))
        })?;

    // 6. The record is written BEFORE the readiness wait: a `npu serve`
    //    interrupted halfway must still leave something that knows which
    //    pid to stop.
    let pid = child.id();
    let Some(facts) = (host.inspect)(pid) else {
        return Err(abandon(
            &mut child,
            backend,
            host,
            &executable,
            &log,
            "the spawned process could not be inspected, so npu cannot record an identity for \
             it and would never be able to tell it apart from a reused pid",
        ));
    };
    let record = state::State {
        version: state::SCHEMA_VERSION,
        backend: backend.id.clone(),
        pid,
        executable: executable.clone(),
        arguments: arguments.clone(),
        started_at: unix_now(),
        process_start_time: facts.start_time,
        base_url: backend.base_url.clone(),
        source: backend.source.clone(),
    };
    if let Err(err) = state::save(&host.state, &record) {
        // The record could not be written, so there is nothing to clear —
        // only a child nobody would ever be able to find again. Its own
        // error is the one that reaches the caller, and the log stays.
        terminate_child(&mut child, host);
        return Err(err);
    }

    // 7.
    wait_until_ready(&mut child, backend, process, host, &executable, &log)?;

    // 8.
    Ok(pid.to_string())
}

/// Seconds since the Unix epoch, for [`state::State::started_at`].
///
/// A clock set before 1970 yields `0` rather than an error: this value is
/// reported, never compared — the anti-reuse token is
/// `process_start_time`, which comes from the system, not from here.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Polls the spawned child until it answers on its `base_url`, dies, or runs
/// out of budget.
///
/// The child's death is checked BEFORE the probe: a server that exited in
/// 20 ms must be reported as "it exited with this status", not as "it did
/// not answer within 30 s".
///
/// The LAST probe failure is carried into the timeout message. The budget is
/// the only thing a bare timeout blames, and it is very often not what is
/// wrong: a `base_url` whose port is not the one the server was told to
/// listen on, or a host nothing resolves, produces a probe that can never
/// succeed, and the reader has to be able to tell that from "connection
/// refused".
fn wait_until_ready(
    child: &mut Child,
    backend: &crate::config::Backend,
    process: &crate::config::Process,
    host: &Host<'_>,
    executable: &Path,
    log: &Path,
) -> crate::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(process.startup_timeout_secs);
    let _spinner =
        crate::progress::Indicator::spinner(&format!("starting backend \"{}\"", backend.id));

    loop {
        match child.try_wait() {
            Err(err) => {
                return Err(abandon(
                    child,
                    backend,
                    host,
                    executable,
                    log,
                    &format!("it could not be waited on: {err}"),
                ));
            }
            Ok(Some(status)) => {
                // Already gone: nothing to terminate, only to forget — and
                // only OUR record, never one another `serve` wrote in the
                // meantime (cf. `state::clear_of`).
                drop(state::clear_of(
                    &host.state,
                    &backend.id,
                    &backend.source,
                    child.id(),
                ));
                return Err(start_failure(
                    backend,
                    executable,
                    log,
                    &format!("it exited during startup ({status})"),
                ));
            }
            Ok(None) => {}
        }

        let probe_error = match (host.probe)(&backend.base_url) {
            Ok(()) => return Ok(()),
            Err(message) => message,
        };

        if Instant::now() >= deadline {
            return Err(abandon(
                child,
                backend,
                host,
                executable,
                log,
                &format!(
                    "it did not answer on {} within {} s ([runtime].startup_timeout_secs; last \
                     attempt: {probe_error})",
                    backend.base_url, process.startup_timeout_secs
                ),
            ));
        }

        std::thread::sleep(READINESS_POLL_INTERVAL);
    }
}

/// Terminates the child `serve` just spawned, forgets its record, KEEPS its
/// log, and builds the error that says why.
///
/// The log survives on purpose: it is the only place the server explained
/// itself, and deleting it would leave the operator with an exit status and
/// nothing to read.
fn abandon(
    child: &mut Child,
    backend: &crate::config::Backend,
    host: &Host<'_>,
    executable: &Path,
    log: &Path,
    detail: &str,
) -> crate::Error {
    let pid = child.id();
    terminate_child(child, host);
    // Only the record naming THIS child: `serve` holds no lock, so a second
    // `serve` on the same backend may have written its own record in
    // between, and an unconditional removal would delete the record of a
    // server that is running — leaving it with nothing that can stop it.
    drop(state::clear_of(
        &host.state,
        &backend.id,
        &backend.source,
        pid,
    ));

    start_failure(backend, executable, log, detail)
}

/// Ends the child `serve` spawned and reaps it, with the same escalation as
/// [`stop`]: ask first, insist second.
///
/// Watches the CHILD (`try_wait`) rather than the process table, because
/// this is one process `npu` owns: a reaped child leaves no trace to poll
/// for, and a zombie would still be listed as a live pid.
///
/// Only `Ok(Some(status))` means "already gone". An `Err` from `try_wait` —
/// `ECHILD`, which a parent that set `SIGCHLD` to `SIG_IGN` before `exec`ing
/// `npu` produces for every wait — says the WAIT CHANNEL is unusable, not
/// that the process is. Reading it as "gone" skipped both the signal and the
/// kill while the caller went on to delete the state record, leaking one
/// running server per `serve` under such a parent; `npu` is explicitly meant
/// to be driven by other programs, so that parent is not hypothetical.
fn terminate_child(child: &mut Child, host: &Host<'_>) {
    if !matches!(child.try_wait(), Ok(Some(_)))
        && (!(host.signal)(child.id(), Signal::Term) || !exits_within(child, TERMINATION_GRACE))
    {
        // Best effort: a child that cannot be killed must not replace the
        // failure being reported with one of its own.
        drop(child.kill());
    }
    drop(child.wait());
}

/// The message every post-spawn failure produces: it names the backend, the
/// command, what happened, and the LOG PATH — the one thing that can still
/// be read afterwards.
fn start_failure(
    backend: &crate::config::Backend,
    executable: &Path,
    log: &Path,
    detail: &str,
) -> crate::Error {
    crate::Error::Backend(format!(
        "backend \"{}\": \"{}\" failed to serve: {detail} — its output was kept in {}",
        backend.id,
        executable.display(),
        log.display()
    ))
}

/// Waits up to `budget` for an already-signalled child to exit.
///
/// Only an observed exit status counts, for the same reason as in
/// [`terminate_child`]: a `try_wait` that ERRS proves nothing about the
/// process, and treating it as an exit skipped the escalation to
/// `SIGKILL`.
fn exits_within(child: &mut Child, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(super::PROBE_POLL_INTERVAL);
    }
}

/// Waits up to `budget` for a process `npu` does NOT own to stop being the
/// one described by `state`.
///
/// Watches the identity, not the pid: a pid that reappears as somebody
/// else's process within the grace period must count as gone, or `stop`
/// would escalate to `SIGKILL` against an innocent process.
fn ceases_within(state: &state::State, host: &Host<'_>, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        let facts = (host.inspect)(state.pid);
        if !matches!(verdict(state.clone(), facts.as_ref()), Presence::Alive(_)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(super::PROBE_POLL_INTERVAL);
    }
}

/// Ends what [`serve`] started for `backend`, and returns the backend
/// identifier — this command's result.
///
/// The identifier, and not the pid [`serve`] returned: by the time `stop`
/// answers, that pid names nothing, and a command that printed a pid when it
/// killed one and something else when there was nothing to kill would force
/// its caller to branch on which. The pid while it exists is `npu status`'s
/// INSTANCE column.
///
/// Escalates: `SIGTERM`, a bounded wait, then `SIGKILL`. A signal reported
/// as UNSUPPORTED by the platform is a failure of graceful termination like
/// any other and falls through to the kill; treating it as a success would
/// leave a running server behind a cleared record.
///
/// Idempotent, like `npu stop` on the Docker side: nothing to stop is a
/// success. A stale record is forgotten, never signalled — the pid it holds
/// may belong to anybody by now.
///
/// # Errors
///
/// `Error::Io` for an unreadable record, and `Error::Backend` naming the
/// backend and the pid when the process survived `SIGKILL` — in which case
/// the record is deliberately KEPT, since forgetting it would orphan a
/// running server. `Error::Backend` too for a record another backend FILE
/// wrote while its process is still alive: it is reported, never signalled
/// and never cleared.
pub fn stop(backend: &crate::config::Backend, host: &Host<'_>) -> crate::Result<String> {
    match presence(backend, host)? {
        Presence::NotStarted => {}
        // Neither signalled nor forgotten: the pid is a live server of
        // another configuration, and both actions would be taken on it.
        Presence::Foreign(state) => return Err(foreign(backend, &state, host)),
        Presence::Exited(_) | Presence::Reused(_) => {
            state::clear(&host.state, &backend.id, &backend.source)?;
        }
        Presence::Alive(state) => {
            let stopped = (host.signal)(state.pid, Signal::Term)
                && ceases_within(&state, host, TERMINATION_GRACE);

            if !stopped {
                (host.signal)(state.pid, Signal::Kill);
                if !ceases_within(&state, host, TERMINATION_GRACE) {
                    return Err(crate::Error::Backend(format!(
                        "backend \"{}\": process {} survived both signals — its state is kept, \
                         since forgetting a running server would leave it unreachable to npu",
                        backend.id, state.pid
                    )));
                }
            }

            state::clear(&host.state, &backend.id, &backend.source)?;
        }
    }

    Ok(backend.id.clone())
}

/// What `npu status` shows for `backend`: the `INSTANCE` column (the pid,
/// for this family), the `URL` column and the `STATE` column.
///
/// The three come from ONE source. A backend that has been served answers
/// with the URL its record holds — the one it was actually started on — and
/// `running`/`unreachable` is decided by probing that very URL. A backend
/// with no record answers with `configured`, the URL the files declare
/// today. Splitting the two (probing one URL and printing another) is how a
/// row comes to say `running` next to an address nothing is listening on,
/// the day someone edits `port` without restarting.
///
/// `None` as a state means "not started", whose wording belongs to the
/// report, exactly as on the Docker side.
///
/// A record that cannot be read becomes the state of THAT line — never an
/// error: one backend whose state file is corrupt must not suppress the rows
/// of every other backend.
///
/// Reports without repairing: a stale record is named as such and left
/// alone, because a report that silently deleted what it describes could
/// never be run twice. `serve` and `stop` are what clear it.
#[must_use]
pub fn report(
    backend: &crate::config::Backend,
    configured: &str,
    host: &Host<'_>,
) -> (String, String, Option<String>) {
    let presence = match presence(backend, host) {
        Ok(presence) => presence,
        Err(err) => {
            return (
                NO_INSTANCE.to_string(),
                configured.to_string(),
                Some(err.to_string()),
            );
        }
    };

    let instance = presence
        .state()
        .map_or_else(|| NO_INSTANCE.to_string(), |state| state.pid.to_string());
    let url = presence
        .state()
        .map_or_else(|| configured.to_string(), |state| state.base_url.clone());

    let state = match &presence {
        Presence::NotStarted => None,
        Presence::Exited(_) => Some(EXITED.to_string()),
        Presence::Reused(_) => Some(STALE.to_string()),
        Presence::Foreign(_) => Some(FOREIGN.to_string()),
        Presence::Alive(_) => Some(
            if (host.probe)(&url).is_ok() {
                RUNNING
            } else {
                UNREACHABLE
            }
            .to_string(),
        ),
    };

    (instance, url, state)
}

/// Streams the log file [`serve`] redirected a server's two streams into.
///
/// ponytail: `--follow` is a read-to-EOF then a poll loop, no `tail`, no
/// watcher, no dependency — the same idiom as every other poll in this
/// module. Ctrl-C needs no handler: this function mutates nothing.
///
/// `sink` is injected for the same reason `streamer` is on the Docker side:
/// the logs ARE this command's result, and a test must be able to read them
/// without a pipe.
///
/// # Errors
///
/// `Error::Backend` naming the backend and the expected path when there is
/// no log to read — nothing was ever served. `Error::Io` naming the path for
/// a read that fails, and whatever `sink` returns (a closed stdout is
/// `Error::Io`, exit `1`).
pub fn logs(
    backend: &crate::config::Backend,
    follow: bool,
    host: &Host<'_>,
    sink: &dyn Fn(&[u8]) -> crate::Result<()>,
) -> crate::Result<()> {
    let path = log_path(backend, host)?;

    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(crate::Error::Backend(format!(
                "backend \"{}\": no log to read at {} — nothing was served from this npu",
                backend.id,
                path.display()
            )));
        }
        Err(err) => return Err(io_at(&path, &err)),
    };

    drain(&mut file, &path, sink)?;

    if !follow {
        return Ok(());
    }

    loop {
        std::thread::sleep(FOLLOW_POLL_INTERVAL);
        // The handle keeps its own offset, so each pass resumes exactly
        // where the previous one stopped — no seek, no re-read. The loop
        // ends with the process: `--follow` is Ctrl-C's to interrupt, and
        // it mutates nothing, so it needs no handler to be interrupted
        // safely.
        drain(&mut file, &path, sink)?;
    }
}

/// Copies everything readable from `file`'s current offset into `sink`, and
/// returns how many bytes that was.
fn drain(
    file: &mut std::fs::File,
    path: &Path,
    sink: &dyn Fn(&[u8]) -> crate::Result<()>,
) -> crate::Result<usize> {
    let mut buffer = [0_u8; LOG_CHUNK];
    let mut total = 0;

    loop {
        let read = file.read(&mut buffer).map_err(|err| io_at(path, &err))?;
        if read == 0 {
            return Ok(total);
        }
        sink(&buffer[..read])?;
        total += read;
    }
}

/// The real sink behind [`logs`]: the process's own stdout, unbuffered and
/// byte for byte — a log is the command's RESULT, and reformatting it would
/// be adding to stdout.
///
/// # Errors
///
/// `Error::Io` (exit `1`) on a closed or broken stdout, which is the pipe
/// case the exit-code contract names.
pub fn stdout_sink(bytes: &[u8]) -> crate::Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(bytes)?;
    out.flush()?;
    Ok(())
}

/// Wraps an I/O failure so the message NAMES the file it is about (same
/// reason as `state::io_at`: `std::fs` errors carry no path).
fn io_at(path: &Path, err: &std::io::Error) -> crate::Error {
    crate::Error::Io(std::io::Error::new(
        err.kind(),
        format!("{}: {err}", path.display()),
    ))
}

#[cfg(test)]
#[allow(clippy::expect_used)] // tolerated in tests (cf. Cargo.toml [lints.clippy]).
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Unique fixture directory: tests run in parallel and a fixed path
    /// collides (same idiom as every other module's).
    fn fixture_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!("process-{name}-{n}"));
        std::fs::create_dir_all(&dir).expect("fixture directory creation");
        dir
    }

    fn state_env(name: &str) -> state::StateEnv {
        let dir = fixture_dir(name);
        state::StateEnv {
            xdg_state_home: Some(dir.clone()),
            home: Some(dir),
        }
    }

    /// The backend FILE every fixture of this module pretends to come from.
    /// `record` and `backend` share it, so a record is OURS unless a test
    /// deliberately moves one of the two (cf. the `Foreign` tests).
    const SOURCE: &str = "/projA/.npu/backends/qwen-fast.toml";

    fn record(backend: &str, pid: u32) -> state::State {
        state::State {
            version: state::SCHEMA_VERSION,
            backend: backend.to_string(),
            pid,
            executable: PathBuf::from("/opt/server"),
            arguments: vec!["--port".to_string(), "8080".to_string()],
            started_at: 1_790_084_826,
            process_start_time: 1_790_084_820,
            base_url: "http://127.0.0.1:8080".to_string(),
            source: PathBuf::from(SOURCE),
        }
    }

    fn facts(start_time: u64) -> ProcessFacts {
        ProcessFacts { start_time }
    }

    fn model(id: &str, underlying: &str) -> crate::config::Model {
        crate::config::Model {
            id: id.to_string(),
            backend: "b".to_string(),
            operation: "chat".to_string(),
            model: underlying.to_string(),
            fallback: None,
            generation: crate::config::Generation::default(),
        }
    }

    fn process(command: &str, arguments: &[&str]) -> crate::config::Process {
        crate::config::Process {
            command: command.to_string(),
            arguments: arguments.iter().map(ToString::to_string).collect(),
            env: std::collections::BTreeMap::new(),
            startup_timeout_secs: 30,
        }
    }

    // -- identity ------------------------------------------------------------

    #[test]
    fn a_pid_with_the_recorded_birth_is_alive() {
        let state = record("qwen-fast", 4242);
        let alive = verdict(state.clone(), Some(&facts(1_790_084_820)));
        assert_eq!(alive, Presence::Alive(state));
    }

    /// The case the whole identity check exists for: the pid is there, but
    /// it was recycled. Signalling it would kill somebody else's process.
    #[test]
    fn a_reused_pid_is_stale_never_alive() {
        let state = record("qwen-fast", 4242);
        let presence = verdict(state, Some(&facts(1_790_099_999)));
        assert!(matches!(presence, Presence::Reused(_)));
    }

    /// A launcher that ends on `exec` — a wrapper script, a virtualenv or
    /// `uv`/`conda` shim — leaves the birth intact and swaps the image, so
    /// the record's `executable` no longer names what is running. That must
    /// still be OUR process: declaring it stale made `stop` answer success
    /// while leaving the server up and deleting the record that could still
    /// find it.
    #[test]
    fn a_pid_that_exec_ed_another_executable_is_still_alive() {
        let mut state = record("qwen-fast", 4242);
        state.executable = PathBuf::from("/opt/wrapper.sh");
        let alive = verdict(state.clone(), Some(&facts(1_790_084_820)));
        assert_eq!(alive, Presence::Alive(state));
    }

    #[test]
    fn an_absent_pid_has_exited() {
        let state = record("qwen-fast", 4242);
        assert!(matches!(verdict(state, None), Presence::Exited(_)));
    }

    #[test]
    fn a_backend_that_was_never_served_is_not_started() {
        let host = Host {
            env: &|_| None,
            state: state_env("never"),
            inspect: &|_| None,
            signal: &|_, _| false,
            probe: &|_| Ok(()),
        };
        assert_eq!(
            presence(&backend("qwen-fast"), &host).expect("an absent record is normal"),
            Presence::NotStarted
        );
    }

    /// The blocker this variant exists for: the state directory is
    /// machine-global while backend identifiers are per-scope, so two
    /// projects each declaring `qwen-fast` in their own `./.npu` land on the
    /// same record. The pid is a genuine npu-started process with a matching
    /// birth — `verdict` sees nothing wrong with it — and it belongs to the
    /// other project.
    #[test]
    fn a_live_record_written_by_another_backend_file_is_foreign() {
        let env = state_env("foreign");
        let mut other = record("qwen-fast", 4242);
        other.source = PathBuf::from("/projB/.npu/backends/qwen-fast.toml");
        plant(&env, &backend("qwen-fast"), &other);

        let host = Host {
            env: &|_| None,
            state: env,
            inspect: &|_| Some(facts(1_790_084_820)),
            signal: &|_, _| false,
            probe: &|_| Ok(()),
        };

        let presence = presence(&backend("qwen-fast"), &host).expect("readable");
        assert!(matches!(presence, Presence::Foreign(_)), "{presence:?}");
    }

    /// A foreign record whose pid is GONE describes nothing and can harm
    /// nobody by being forgotten: refusing there would leave the second
    /// project permanently unable to serve.
    #[test]
    fn a_dead_record_written_by_another_backend_file_has_merely_exited() {
        let env = state_env("foreign-dead");
        let mut other = record("qwen-fast", 4242);
        other.source = PathBuf::from("/projB/.npu/backends/qwen-fast.toml");
        plant(&env, &backend("qwen-fast"), &other);

        let host = Host {
            env: &|_| None,
            state: env,
            inspect: &|_| None,
            signal: &|_, _| false,
            probe: &|_| Ok(()),
        };

        let presence = presence(&backend("qwen-fast"), &host).expect("readable");
        assert!(matches!(presence, Presence::Exited(_)), "{presence:?}");
    }

    /// The kill the whole check exists to prevent: another project's `npu
    /// stop` must signal NOTHING, and must not delete the record either —
    /// that record is the only thing that can still find that server.
    #[test]
    fn stopping_a_foreign_record_signals_nothing_and_keeps_it() {
        let env = state_env("stop-foreign");
        let mut other = record("qwen-fast", 4242);
        other.source = PathBuf::from("/projB/.npu/backends/qwen-fast.toml");
        plant(&env, &backend("qwen-fast"), &other);
        let sent: std::sync::Mutex<Vec<Signal>> = std::sync::Mutex::new(Vec::new());
        let host = Host {
            env: &|_| None,
            state: env.clone(),
            inspect: &|_| Some(facts(1_790_084_820)),
            signal: &|_, signal| {
                sent.lock().expect("the signal log").push(signal);
                true
            },
            probe: &|_| Ok(()),
        };

        let err = stop(&backend("qwen-fast"), &host).expect_err("another project is not ours");

        assert_eq!(err.exit_code(), 3);
        assert!(sent.lock().expect("the signal log").is_empty());
        assert!(
            state::load(&env, "qwen-fast", Path::new(SOURCE))
                .expect("readable")
                .is_some()
        );
    }

    /// Both files, because the reader has to know WHICH of their projects
    /// owns that server — identifiers, never wording.
    #[test]
    fn the_foreign_refusal_names_both_backend_files() {
        let env = state_env("foreign-named");
        let mut other = record("qwen-fast", 4242);
        other.source = PathBuf::from("/projB/.npu/backends/qwen-fast.toml");
        plant(&env, &backend("qwen-fast"), &other);
        let host = Host {
            env: &|_| None,
            state: env,
            inspect: &|_| Some(facts(1_790_084_820)),
            signal: &|_, _| true,
            probe: &|_| Ok(()),
        };

        let err = stop(&backend("qwen-fast"), &host).expect_err("another project is not ours");
        let message = err.to_string();
        assert!(message.contains(SOURCE), "{message}");
        assert!(
            message.contains("/projB/.npu/backends/qwen-fast.toml"),
            "{message}"
        );
        assert!(message.contains("qwen-fast"), "{message}");
    }

    /// `serve` must refuse BEFORE the log is truncated and before anything
    /// is spawned: the record, the log and the port all belong to the other
    /// project's server.
    #[test]
    fn serving_over_a_foreign_record_is_refused() {
        let env = state_env("serve-foreign");
        let mut other = record("qwen-fast", 4242);
        other.source = PathBuf::from("/projB/.npu/backends/qwen-fast.toml");
        plant(&env, &backend("qwen-fast"), &other);
        let host = Host {
            env: &|_| None,
            state: env.clone(),
            inspect: &|_| Some(facts(1_790_084_820)),
            signal: &|_, _| true,
            probe: &|_| Ok(()),
        };

        let err = serve(
            &backend("qwen-fast"),
            &process("/nothing-is-spawned", &[]),
            &model("m", "qwen3"),
            &host,
        )
        .expect_err("another project's runtime is not ours to replace");

        assert_eq!(err.exit_code(), 3);
        assert!(
            state::load(&env, "qwen-fast", Path::new(SOURCE))
                .expect("readable")
                .is_some()
        );
    }

    /// The `foreign state` row: reported, never repaired, like every other
    /// record `status` describes.
    #[test]
    fn a_foreign_record_is_reported_as_such() {
        let env = state_env("report-foreign");
        let mut other = record("qwen-fast", 4242);
        other.source = PathBuf::from("/projB/.npu/backends/qwen-fast.toml");
        plant(&env, &backend("qwen-fast"), &other);
        let host = Host {
            env: &|_| None,
            state: env,
            inspect: &|_| Some(facts(1_790_084_820)),
            signal: &|_, _| false,
            probe: &|_| Ok(()),
        };

        assert_eq!(
            report(&backend("qwen-fast"), "http://127.0.0.1:8080", &host).2,
            Some(FOREIGN.to_string())
        );
    }

    /// An unreadable record must reach the caller as an error naming its
    /// path: a silent "not started" would let `serve` start a second server.
    #[test]
    fn an_unreadable_record_is_an_error_naming_its_path() {
        let env = state_env("unreadable");
        let dir = state::state_dir(&env).expect("a home is set");
        std::fs::create_dir_all(&dir).expect("the state directory");
        let path =
            state::state_path(&env, "qwen-fast", Path::new(SOURCE)).expect("a valid identifier");
        std::fs::write(&path, "{ not json").expect("the planted record");

        let host = Host {
            env: &|_| None,
            state: env,
            inspect: &|_| None,
            signal: &|_, _| false,
            probe: &|_| Ok(()),
        };

        let err =
            presence(&backend("qwen-fast"), &host).expect_err("a corrupt record must be reported");
        assert_eq!(err.exit_code(), 1);
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
    }

    // -- the real inspector --------------------------------------------------

    /// The one test that exercises `sysinfo` itself, against the only
    /// process every run is guaranteed to have: this one. It pins the
    /// assumption the whole identity check rests on — that a refresh asking
    /// for no optional datum still yields a `start_time()` in SECONDS SINCE
    /// THE UNIX EPOCH, and not an uptime-relative figure. If that ever
    /// changed, every `Presence` in this module would silently become
    /// `Reused`, and `stop` would never stop anything again.
    #[test]
    fn inspecting_this_very_process_yields_an_epoch_birth() {
        let facts = inspect(std::process::id()).expect("this process exists");

        let now = unix_now();
        assert!(facts.start_time > 1_600_000_000, "{}", facts.start_time);
        assert!(facts.start_time <= now, "{} > {now}", facts.start_time);
    }

    /// A pid nothing owns must be absent, never "identity unknown but
    /// present": `serve` refuses to start when the record looks alive.
    #[test]
    fn inspecting_a_pid_that_cannot_exist_yields_nothing() {
        // Above every documented pid_max, so no scheduler can have handed
        // it out.
        assert_eq!(inspect(u32::MAX), None);
    }

    // -- resolving the executable -------------------------------------------

    /// Writes a file at `path` and makes it runnable where that is a
    /// question the filesystem answers.
    fn executable_file(path: &Path) {
        std::fs::write(path, "#!/bin/sh\n").expect("the fake executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
                .expect("making it executable");
        }
    }

    #[test]
    fn a_bare_name_is_found_on_the_injected_path() {
        let dir = fixture_dir("path-lookup");
        executable_file(&dir.join("fake-server"));
        let path = dir.display().to_string();
        let env = |name: &str| (name == "PATH").then(|| path.clone());

        let resolved = resolve_command("fake-server", "b", &env).expect("it is on PATH");

        assert!(resolved.ends_with("fake-server"), "{}", resolved.display());
    }

    #[test]
    fn a_path_is_used_as_is_without_consulting_path() {
        let dir = fixture_dir("explicit-path");
        let executable = dir.join("fake-server");
        executable_file(&executable);
        let env = |_: &str| None;

        let resolved =
            resolve_command(&executable.display().to_string(), "b", &env).expect("it exists");

        assert!(resolved.ends_with("fake-server"), "{}", resolved.display());
    }

    /// `command` is validated as a TEMPLATE (`config::validate_process`
    /// accepts `{{ env.NAME }}` and `{{ args.model }}` in it), so it has to
    /// be rendered like every other entry of the table. Handed raw to the
    /// executable lookup it produced exit `3` naming a placeholder as a
    /// missing binary — a key read, validated, and then not honoured. The
    /// assertion is on the RENDERED value appearing in the failure.
    #[test]
    fn the_command_itself_is_rendered_before_it_is_looked_up() {
        let host = Host {
            env: &|name: &str| (name == "NPU_TEST_BIN").then(|| "npu-rendered-command".to_string()),
            state: state_env("render-command"),
            inspect: &|_| None,
            signal: &|_, _| false,
            probe: &|_| Ok(()),
        };

        let err = serve(
            &backend("qwen-fast"),
            &process("{{ env.NPU_TEST_BIN }}", &[]),
            &model("m", "qwen3"),
            &host,
        )
        .expect_err("no such command exists");

        let message = err.to_string();
        assert!(message.contains("npu-rendered-command"), "{message}");
    }

    /// The rejection has to name BOTH, or its reader cannot tell which
    /// backend asked for a command this machine does not have.
    #[test]
    fn an_absent_command_names_the_command_and_the_backend() {
        let env = |_: &str| None;
        let err = resolve_command("does-not-exist-anywhere", "qwen-fast", &env)
            .expect_err("nothing must resolve");

        assert_eq!(err.exit_code(), 3);
        let message = err.to_string();
        assert!(message.contains("does-not-exist-anywhere"), "{message}");
        assert!(message.contains("qwen-fast"), "{message}");
    }

    /// A file with no execute bit is not a command: `doctor` reported it
    /// as available while `serve` then failed to spawn it, and — worse — it
    /// SHADOWED the real binary further down `PATH`, since the scan returns
    /// the first match. Unix-only, because the bit is.
    #[cfg(unix)]
    #[test]
    fn a_non_executable_file_is_skipped_and_the_scan_continues() {
        let shadow = fixture_dir("shadow");
        let real = fixture_dir("real");
        std::fs::write(shadow.join("fake-server"), "#!/bin/sh\n").expect("the non-executable");
        executable_file(&real.join("fake-server"));
        let path = format!("{}:{}", shadow.display(), real.display());
        let env = |name: &str| (name == "PATH").then(|| path.clone());

        let resolved = resolve_command("fake-server", "b", &env).expect("the real one is found");

        assert!(resolved.starts_with(&real), "{}", resolved.display());
    }

    #[test]
    fn an_empty_path_variable_resolves_nothing() {
        let env = |name: &str| (name == "PATH").then(String::new);
        assert!(resolve_command("fake-server", "b", &env).is_err());
    }

    // -- rendering -----------------------------------------------------------

    #[test]
    fn arguments_render_the_model_and_the_environment() {
        let process = process(
            "server",
            &[
                "--model",
                "{{ args.model }}",
                "--home",
                "{{ env.NPU_TEST_HOME }}",
            ],
        );
        let env = |name: &str| (name == "NPU_TEST_HOME").then(|| "/tmp/models".to_string());

        let rendered = render_all(process.arguments.iter(), &model("m", "qwen3"), &env)
            .expect("both placeholders resolve");

        assert_eq!(
            rendered,
            vec!["--model", "qwen3", "--home", "/tmp/models"]
                .into_iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
    }

    /// An undefined variable is a CONFIGURATION error naming it, never an
    /// empty string silently handed to a server.
    #[test]
    fn an_undefined_environment_variable_is_rejected_by_name() {
        let process = process("server", &["{{ env.NPU_TEST_ABSENT }}"]);
        let err = render_all(process.arguments.iter(), &model("m", "qwen3"), &|_| None)
            .expect_err("an undefined variable must fail");

        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("NPU_TEST_ABSENT"), "{err}");
    }

    #[test]
    fn the_environment_overlay_is_rendered_as_a_pair_list() {
        let mut process = process("server", &[]);
        process
            .env
            .insert("NPU_MODEL".to_string(), "{{ args.model }}".to_string());

        let rendered = rendered_env(&process, &model("m", "qwen3"), &|_| None).expect("it renders");

        assert_eq!(
            rendered,
            vec![("NPU_MODEL".to_string(), "qwen3".to_string())]
        );
    }

    // -- status --------------------------------------------------------------

    #[test]
    fn status_of_a_never_served_backend_has_no_instance_and_no_state() {
        let host = Host {
            env: &|_| None,
            state: state_env("status-absent"),
            inspect: &|_| None,
            signal: &|_, _| false,
            probe: &|_| Ok(()),
        };
        assert_eq!(
            report(&backend("qwen-fast"), "http://127.0.0.1:8080", &host),
            (
                NO_INSTANCE.to_string(),
                "http://127.0.0.1:8080".to_string(),
                None
            )
        );
    }

    #[test]
    fn status_reports_the_pid_as_the_instance_and_running_when_reachable() {
        let env = state_env("status-running");
        state::save(&env, &record("qwen-fast", 4242)).expect("the record");
        let host = Host {
            env: &|_| None,
            state: env,
            inspect: &|_| Some(facts(1_790_084_820)),
            signal: &|_, _| false,
            probe: &|_| Ok(()),
        };

        // The URL is the RECORDED one, not the configured one: the row
        // must not claim `running` next to an address nothing answers on.
        assert_eq!(
            report(&backend("qwen-fast"), "http://127.0.0.1:9999", &host),
            (
                "4242".to_string(),
                "http://127.0.0.1:8080".to_string(),
                Some(RUNNING.to_string())
            )
        );
    }

    #[test]
    fn status_of_a_live_process_that_does_not_answer_is_unreachable() {
        let env = state_env("status-unreachable");
        state::save(&env, &record("qwen-fast", 4242)).expect("the record");
        let host = Host {
            env: &|_| None,
            state: env,
            inspect: &|_| Some(facts(1_790_084_820)),
            signal: &|_, _| false,
            probe: &|_| Err("connection refused".to_string()),
        };

        assert_eq!(
            report(&backend("qwen-fast"), "http://127.0.0.1:8080", &host).2,
            Some(UNREACHABLE.to_string())
        );
    }

    #[test]
    fn status_tells_an_exited_process_apart_from_a_stale_record() {
        let env = state_env("status-exited");
        state::save(&env, &record("qwen-fast", 4242)).expect("the record");
        let gone = Host {
            env: &|_| None,
            state: env.clone(),
            inspect: &|_| None,
            signal: &|_, _| false,
            probe: &|_| Ok(()),
        };
        assert_eq!(
            report(&backend("qwen-fast"), "http://127.0.0.1:8080", &gone).2,
            Some(EXITED.to_string())
        );

        let reused = Host {
            env: &|_| None,
            state: env,
            inspect: &|_| Some(facts(1_790_099_999)),
            signal: &|_, _| false,
            probe: &|_| Ok(()),
        };
        assert_eq!(
            report(&backend("qwen-fast"), "http://127.0.0.1:8080", &reused).2,
            Some(STALE.to_string())
        );
    }

    /// A report that dies on its first unreadable line is not a report: the
    /// broken record becomes the state of ITS row and nothing else.
    #[test]
    fn a_corrupt_record_becomes_that_row_s_state_rather_than_an_error() {
        let env = state_env("status-corrupt");
        let dir = state::state_dir(&env).expect("a home is set");
        std::fs::create_dir_all(&dir).expect("the state directory");
        let path =
            state::state_path(&env, "qwen-fast", Path::new(SOURCE)).expect("a valid identifier");
        std::fs::write(&path, "{ not json").expect("the planted record");

        let host = Host {
            env: &|_| None,
            state: env,
            inspect: &|_| None,
            signal: &|_, _| false,
            probe: &|_| Ok(()),
        };

        let (instance, url, state) = report(&backend("qwen-fast"), "http://127.0.0.1:8080", &host);
        assert_eq!(instance, NO_INSTANCE);
        assert_eq!(url, "http://127.0.0.1:8080");
        let state = state.expect("a broken record must still produce a state");
        assert!(state.contains(&path.display().to_string()), "{state}");
    }

    // -- stop ----------------------------------------------------------------

    #[test]
    fn stopping_a_backend_that_was_never_served_succeeds() {
        let host = Host {
            env: &|_| None,
            state: state_env("stop-absent"),
            inspect: &|_| None,
            signal: &|_, _| false,
            probe: &|_| Ok(()),
        };
        let backend = backend("qwen-fast");

        assert_eq!(
            stop(&backend, &host).expect("stopping nothing is a success"),
            "qwen-fast"
        );
    }

    /// The single most dangerous confusion in this file: a stale record must
    /// be FORGOTTEN, never signalled — its pid may belong to anybody.
    #[test]
    fn stopping_a_stale_record_signals_nothing_and_forgets_it() {
        let env = state_env("stop-stale");
        state::save(&env, &record("qwen-fast", 4242)).expect("the record");
        let signalled = std::sync::atomic::AtomicU64::new(0);
        let host = Host {
            env: &|_| None,
            state: env.clone(),
            inspect: &|_| Some(facts(1_790_099_999)),
            signal: &|_, _| {
                signalled.fetch_add(1, Ordering::Relaxed);
                true
            },
            probe: &|_| Ok(()),
        };

        stop(&backend("qwen-fast"), &host).expect("a stale record is stopped by forgetting it");

        assert_eq!(signalled.load(Ordering::Relaxed), 0);
        assert_eq!(
            state::load(&env, "qwen-fast", Path::new(SOURCE)).expect("readable"),
            None
        );
    }

    #[test]
    fn stopping_a_live_process_terminates_it_gracefully_and_forgets_it() {
        let env = state_env("stop-live");
        state::save(&env, &record("qwen-fast", 4242)).expect("the record");
        let sent: std::sync::Mutex<Vec<Signal>> = std::sync::Mutex::new(Vec::new());
        let alive = std::sync::atomic::AtomicBool::new(true);
        let host = Host {
            env: &|_| None,
            state: env.clone(),
            inspect: &|_| alive.load(Ordering::Relaxed).then(|| facts(1_790_084_820)),
            signal: &|_, signal| {
                sent.lock().expect("the signal log").push(signal);
                alive.store(false, Ordering::Relaxed);
                true
            },
            probe: &|_| Ok(()),
        };

        stop(&backend("qwen-fast"), &host).expect("a live process must be stopped");

        assert_eq!(*sent.lock().expect("the signal log"), vec![Signal::Term]);
        assert_eq!(
            state::load(&env, "qwen-fast", Path::new(SOURCE)).expect("readable"),
            None
        );
    }

    /// A platform that does not support `SIGTERM` reports `false`, which is
    /// a FAILED graceful termination: treating it as a success would leave a
    /// server running behind a cleared record.
    #[test]
    fn an_undeliverable_term_escalates_to_kill() {
        let env = state_env("stop-escalate");
        state::save(&env, &record("qwen-fast", 4242)).expect("the record");
        let sent: std::sync::Mutex<Vec<Signal>> = std::sync::Mutex::new(Vec::new());
        let alive = std::sync::atomic::AtomicBool::new(true);
        let host = Host {
            env: &|_| None,
            state: env.clone(),
            inspect: &|_| alive.load(Ordering::Relaxed).then(|| facts(1_790_084_820)),
            signal: &|_, signal| {
                sent.lock().expect("the signal log").push(signal);
                if signal == Signal::Kill {
                    alive.store(false, Ordering::Relaxed);
                    return true;
                }
                false
            },
            probe: &|_| Ok(()),
        };

        stop(&backend("qwen-fast"), &host).expect("the kill must end it");

        assert_eq!(
            *sent.lock().expect("the signal log"),
            vec![Signal::Term, Signal::Kill]
        );
        assert_eq!(
            state::load(&env, "qwen-fast", Path::new(SOURCE)).expect("readable"),
            None
        );
    }

    /// A process that survives everything keeps its record: forgetting it
    /// would orphan a running server npu could never reach again.
    #[test]
    fn a_process_surviving_both_signals_keeps_its_record_and_fails() {
        let env = state_env("stop-immortal");
        state::save(&env, &record("qwen-fast", 4242)).expect("the record");
        let host = Host {
            env: &|_| None,
            state: env.clone(),
            inspect: &|_| Some(facts(1_790_084_820)),
            signal: &|_, _| true,
            probe: &|_| Ok(()),
        };

        let err = stop(&backend("qwen-fast"), &host).expect_err("an immortal process must fail");

        assert_eq!(err.exit_code(), 3);
        let message = err.to_string();
        assert!(message.contains("qwen-fast"), "{message}");
        assert!(message.contains("4242"), "{message}");
        assert!(
            state::load(&env, "qwen-fast", Path::new(SOURCE))
                .expect("readable")
                .is_some()
        );
    }

    // -- serve ---------------------------------------------------------------

    fn backend(id: &str) -> crate::config::Backend {
        crate::config::Backend {
            id: id.to_string(),
            base_url: "http://127.0.0.1:8080".to_string(),
            kind: "openai-compatible".to_string(),
            operations: std::collections::HashMap::new(),
            port: None,
            runtime: None,
            docker: None,
            timeouts: None,
            source: PathBuf::from(SOURCE),
        }
    }

    /// Writes `state` where `backend`'s OWN record belongs, whatever origin
    /// the record itself claims.
    ///
    /// This is the shape of a DIGEST COLLISION, and the only way to reach
    /// [`Presence::Foreign`] now that a record's file name carries a digest
    /// of the backend file: two distinct source paths meeting on one name is
    /// precisely "a record found at OUR path, written by somebody else's
    /// file". `state::save` cannot express it — it files a record under the
    /// origin the record itself claims, which is what makes the isolation
    /// hold in production — so the test writes the file itself.
    fn plant(env: &state::StateEnv, backend: &crate::config::Backend, state: &state::State) {
        let path =
            state::state_path(env, &backend.id, &backend.source).expect("a valid identifier");
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the state directory");
        std::fs::write(&path, serde_json::to_vec(state).expect("json"))
            .expect("the planted record");
    }

    /// Nothing is spawned here: the refusal comes before any of it.
    #[test]
    fn serving_an_already_served_backend_names_the_pid_and_the_model() {
        let env = state_env("serve-twice");
        state::save(&env, &record("qwen-fast", 4242)).expect("the record");
        let host = Host {
            env: &|_| None,
            state: env,
            inspect: &|_| Some(facts(1_790_084_820)),
            signal: &|_, _| false,
            probe: &|_| Ok(()),
        };

        let err = serve(
            &backend("qwen-fast"),
            &process("does-not-exist", &[]),
            &model("m", "qwen3"),
            &host,
        )
        .expect_err("a second serve must be refused");

        assert_eq!(err.exit_code(), 3);
        let message = err.to_string();
        assert!(message.contains("qwen-fast"), "{message}");
        assert!(message.contains("4242"), "{message}");
        assert!(message.contains('m'), "{message}");
    }

    /// A record left by a process that is gone must not block a restart.
    #[test]
    fn a_stale_record_does_not_prevent_a_restart() {
        let env = state_env("serve-stale");
        state::save(&env, &record("qwen-fast", 4242)).expect("the record");
        let host = Host {
            env: &|_| None,
            state: env.clone(),
            inspect: &|_| None,
            signal: &|_, _| false,
            probe: &|_| Ok(()),
        };

        // The command does not exist, so the start fails AFTER the stale
        // record was cleared: what this asserts is that the failure is
        // about the command, not about a runtime believed to be running.
        let err = serve(
            &backend("qwen-fast"),
            &process("does-not-exist-anywhere", &[]),
            &model("m", "qwen3"),
            &host,
        )
        .expect_err("the command does not exist");

        assert!(err.to_string().contains("does-not-exist-anywhere"), "{err}");
        assert_eq!(
            state::load(&env, "qwen-fast", Path::new(SOURCE)).expect("readable"),
            None
        );
    }

    #[test]
    fn serving_on_a_taken_fixed_port_names_the_backend_and_the_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("the occupying listener");
        let port = listener.local_addr().expect("local address").port();

        let mut backend = backend("qwen-fast");
        backend.port = Some(crate::config::Port::Fixed(port));
        let host = Host {
            env: &|_| None,
            state: state_env("serve-port"),
            inspect: &|_| None,
            signal: &|_, _| false,
            probe: &|_| Ok(()),
        };

        let err = serve(
            &backend,
            &process("does-not-exist", &[]),
            &model("m", "qwen3"),
            &host,
        )
        .expect_err("an occupied port must fail");

        assert_eq!(err.exit_code(), 3);
        let message = err.to_string();
        assert!(message.contains("qwen-fast"), "{message}");
        assert!(message.contains(&port.to_string()), "{message}");
    }

    // -- logs ----------------------------------------------------------------

    #[test]
    fn logs_of_a_backend_that_was_never_served_name_the_expected_path() {
        let env = state_env("logs-absent");
        let host = Host {
            env: &|_| None,
            state: env.clone(),
            inspect: &|_| None,
            signal: &|_, _| false,
            probe: &|_| Ok(()),
        };

        let err = logs(&backend("qwen-fast"), false, &host, &|_| Ok(()))
            .expect_err("there is nothing to read");

        assert_eq!(err.exit_code(), 3);
        let expected = log_path(&backend("qwen-fast"), &host).expect("a valid identifier");
        assert!(
            err.to_string().contains(&expected.display().to_string()),
            "{err}"
        );
    }

    /// The READ half of the isolation, and the defect the digest exists
    /// for: `npu logs` must never hand a second project the first one's
    /// output. Same identifier, two backend FILES, two logs.
    #[test]
    fn two_projects_declaring_the_same_backend_have_different_logs() {
        let host = Host {
            env: &|_| None,
            state: state_env("logs-per-project"),
            inspect: &|_| None,
            signal: &|_, _| false,
            probe: &|_| Ok(()),
        };
        let mut other = backend("qwen-fast");
        other.source = PathBuf::from("/projB/.npu/backends/qwen-fast.toml");

        let ours = log_path(&backend("qwen-fast"), &host).expect("a valid identifier");
        let theirs = log_path(&other, &host).expect("a valid identifier");

        assert_ne!(ours, theirs);
    }

    #[test]
    fn logs_hand_the_file_to_the_sink_byte_for_byte() {
        let env = state_env("logs-read");
        let host = Host {
            env: &|_| None,
            state: env,
            inspect: &|_| None,
            signal: &|_, _| false,
            probe: &|_| Ok(()),
        };
        let path = log_path(&backend("qwen-fast"), &host).expect("a valid identifier");
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the state directory");
        // Longer than one read buffer: a log is streamed, not slurped.
        let contents = "café ".repeat(2000);
        std::fs::write(&path, &contents).expect("the log file");

        let collected = std::sync::Mutex::new(Vec::new());
        logs(&backend("qwen-fast"), false, &host, &|bytes| {
            collected
                .lock()
                .expect("the collector")
                .extend_from_slice(bytes);
            Ok(())
        })
        .expect("the log must be readable");

        assert_eq!(
            collected.into_inner().expect("the collector"),
            contents.as_bytes()
        );
    }
}
