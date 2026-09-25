//! Where `npu` remembers what it started, and how that memory is written.
//!
//! Docker IS its own registry: `docker ps` answers "is it still up?" and
//! `docker port` answers "on which port?", which is why the Docker runtime
//! persists nothing (see the rationale on the ephemeral port allocation in
//! `config::port`). A process runtime has no such
//! registry — once `npu serve` exits, nothing but a file remembers the pid
//! it spawned — so `npu` keeps its own.
//!
//! This module is that persistence and NOTHING else: it locates the state
//! directory, and it writes, reads and removes one record per backend FILE.
//! It spawns nothing, signals nothing and probes nothing; deciding whether the
//! recorded pid is still the process we started belongs to the caller, which
//! is why [`State`] carries `process_start_time` rather than a verdict.
//!
//! Nothing here prints: a state file that cannot be understood comes back as
//! an `Error` naming its path, because stdout carries the command result and
//! nothing else.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Schema version written into every record, and the only one [`load`]
/// accepts.
///
/// Bumped when the SHAPE of [`State`] changes. A file carrying any other
/// value is reported, never guessed at and never deleted: the alternative is
/// reading a record written by a different `npu` and killing whatever pid it
/// happens to hold.
pub const SCHEMA_VERSION: u32 = 1;

/// Directory name `npu` owns under the platform's state root.
const STATE_LEAF: &str = "npu";

/// The environment inputs the state directory depends on, isolated so the
/// resolution stays a pure, testable function.
///
/// Same shape and same reason as [`crate::scope::ScopeEnv`]: `set_var` is
/// `unsafe` under edition 2024 and therefore forbidden here, so a test
/// cannot move the real variables — the environment has to be a parameter
/// for any of this to be covered at all.
#[derive(Debug, Clone)]
pub struct StateEnv {
    /// `$XDG_STATE_HOME`, if set and non-empty. Linux only.
    pub xdg_state_home: Option<PathBuf>,
    /// `$HOME`, if set and non-empty.
    pub home: Option<PathBuf>,
}

impl StateEnv {
    /// Builds a `StateEnv` from the real environment variables.
    ///
    /// Reuses `scope.rs`'s reader, so a variable that is set but EMPTY
    /// (`XDG_STATE_HOME=`) counts as absent here exactly as it does for the
    /// configuration scopes — one rule, tested once.
    #[must_use]
    pub fn from_env() -> Self {
        StateEnv {
            xdg_state_home: crate::scope::non_empty_env_var("XDG_STATE_HOME"),
            home: crate::scope::non_empty_env_var("HOME"),
        }
    }
}

impl StateEnv {
    /// Builds a `StateEnv` through an injected environment reader, with the
    /// same "set but empty is absent" rule as [`StateEnv::from_env`].
    #[must_use]
    pub fn from_vars(env: &dyn Fn(&str) -> Option<String>) -> Self {
        let var = |name: &str| {
            env(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        StateEnv {
            xdg_state_home: var("XDG_STATE_HOME"),
            home: var("HOME"),
        }
    }
}

/// The Linux state directory: `$XDG_STATE_HOME/npu`, else
/// `$HOME/.local/state/npu`.
///
/// `None` when neither variable is set — there is no third place to try, and
/// inventing one (`/tmp`, the cwd) would put a file that outlives the
/// process somewhere nobody looks for it.
fn linux_state_dir(env: &StateEnv) -> Option<PathBuf> {
    if let Some(xdg) = &env.xdg_state_home {
        return Some(xdg.join(STATE_LEAF));
    }
    Some(
        env.home
            .as_ref()?
            .join(".local")
            .join("state")
            .join(STATE_LEAF),
    )
}

/// The macOS state directory: `$HOME/Library/Application Support/npu/state`.
///
/// No `XDG_STATE_HOME` branch: the variable has no meaning on macOS, and
/// honouring it there would scatter the state of one machine over two
/// locations depending on which shell exported what.
fn macos_state_dir(env: &StateEnv) -> Option<PathBuf> {
    Some(
        env.home
            .as_ref()?
            .join("Library")
            .join("Application Support")
            .join(STATE_LEAF)
            .join("state"),
    )
}

/// The directory holding one state file per backend, for this platform.
///
/// Selected with the `cfg!` MACRO rather than a `#[cfg]` attribute, so both
/// branches are compiled and unit-tested on every target: an attribute would
/// leave the macOS body unbuilt on Linux, which is how a path convention
/// ships broken.
///
/// # Errors
///
/// `Error::Io` (exit `1`) when the environment names no home at all. Not
/// `Error::Config` (exit `2`): nothing in the user's `.npu` files is wrong,
/// so sending a calling program to go fix a configuration file would be a
/// lie about the remedy.
pub fn state_dir(env: &StateEnv) -> crate::Result<PathBuf> {
    let resolved = if cfg!(target_os = "macos") {
        macos_state_dir(env)
    } else {
        linux_state_dir(env)
    };

    resolved.ok_or_else(|| {
        crate::Error::Io(std::io::Error::other(
            "no state directory: neither $XDG_STATE_HOME nor $HOME is set, so npu has nowhere \
             to remember what it started",
        ))
    })
}

/// The state file of the backend `backend_id` declared by the file
/// `source`, inside [`state_dir`].
///
/// Takes the ORIGIN as well as the identifier because this directory is
/// machine-global while backend identifiers are per-scope: two projects
/// each declaring `llamacpp` in their own `./.npu` would otherwise share
/// one record and one log, and the second project's `npu logs` would hand
/// back the first one's output.
///
/// `source` is the backend file as `config.rs` recorded it — canonicalized
/// when the filesystem allowed it, so two spellings of one file land on
/// one record.
///
/// # Errors
///
/// `Error::Io` from [`state_dir`], or `Error::Config` naming the backend
/// when its identifier could not be used as a file name.
pub fn state_path(env: &StateEnv, backend_id: &str, source: &Path) -> crate::Result<PathBuf> {
    Ok(state_dir(env)?.join(file_name(backend_id, source)?))
}

/// Waits for the right to send a request to `backend`, when it declares
/// `max_concurrent = 1`: an exclusive advisory lock on
/// `<state dir>/<backend_id>-<digest>.lock`, released when the returned
/// file is dropped (or the process exits). `None` for a backend without a
/// limit, which never touches the state directory.
///
/// `on_busy` is called once, before blocking, when another process holds
/// the lock. With `wait` false, a held lock is an `Error::Backend` naming
/// the backend instead.
///
/// ponytail: keyed like the process records, by backend id and file, so
/// two scopes describing the same device do not serialize each other; a
/// device-level key would need a new configuration key naming the device.
///
/// # Errors
///
/// `Error::Io` when the state directory or the lock file cannot be used,
/// `Error::Backend` when the lock is held and `wait` is false.
pub fn acquire_request_slot(
    env: &StateEnv,
    backend: &crate::config::Backend,
    wait: bool,
    on_busy: &dyn Fn(),
) -> crate::Result<Option<std::fs::File>> {
    if backend.max_concurrent.is_none() {
        return Ok(None);
    }
    let path = state_path(env, &backend.id, &backend.source)?.with_extension("lock");
    let io_error = |err: std::io::Error| {
        crate::Error::Io(std::io::Error::new(
            err.kind(),
            format!("{}: {err}", path.display()),
        ))
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io_error)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(io_error)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) if wait => {
            on_busy();
            file.lock().map_err(io_error)?;
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(crate::Error::Backend(crate::error::BackendError::at(
                &backend.id,
                format!(
                    "backend \"{}\" is busy with another request (max_concurrent = 1) and \
                     --no-wait was given",
                    backend.id
                ),
            )));
        }
        Err(std::fs::TryLockError::Error(err)) => return Err(io_error(err)),
    }
    Ok(Some(file))
}

/// `<backend_id>-<digest>.json`, once the identifier is known to be safe as
/// a file name.
///
/// The check is [`crate::config::is_valid_runtime_id`], the crate's single
/// identifier predicate, reused rather than reinvented: its character set
/// (`[a-zA-Z0-9][a-zA-Z0-9_.-]*`) already excludes the empty string, `/`,
/// `.` and `..` by construction, so a traversal cannot be spelled at all.
/// The digest adds nothing to defend against — it is hex — but it is
/// appended to an ALREADY validated identifier, never a reason to stop
/// validating it.
///
/// Checked HERE and not left to `config.rs` alone, although that is where
/// the same predicate runs at load time: `validate_docker` only applies it
/// to a backend that declares a Docker runtime, so the guarantee is
/// conditional on a family. A function that turns an identifier into a path
/// may not depend on someone else having validated it.
fn file_name(backend_id: &str, source: &Path) -> crate::Result<String> {
    if !crate::config::is_valid_runtime_id(backend_id) {
        return Err(crate::Error::Config(crate::error::ConfigError::bare(
            Some(backend_id),
            format!(
                "backend \"{backend_id}\": identifier unusable as a state file name (ASCII \
                 letters, digits, \"_\", \".\" and \"-\", starting with a letter or a digit)"
            ),
        )));
    }
    Ok(format!("{backend_id}-{}.json", source_digest(source)))
}

/// How many hex characters of the source digest the file name carries.
///
/// Eight — 32 bits — and the length is a trade, not a default:
///
/// - the identifier has to stay READABLE in the name. The record is
///   documented as plain JSON a human can open when a runtime was
///   forgotten, and `llamacpp-3f2a19c4.json` still says which backend it
///   is, where a 64-character digest buries it;
/// - the population is tiny. A machine has a handful of backend files, not
///   a million: at 100 distinct sources the birthday bound is about
///   100² / 2 / 2³² ≈ one chance in 900 000;
/// - and the residual collision is not silent. Two sources landing on one
///   name still carry different [`State::source`] values, which
///   `runtime::process::presence` compares before acting — the collision is
///   reported as a foreign record, never signalled.
const SOURCE_DIGEST_HEX: usize = 8;

/// The first [`SOURCE_DIGEST_HEX`] hex characters of the SHA-256 of
/// `source`'s bytes.
///
/// SHA-256 and not `std::collections::hash_map::DefaultHasher`: that hasher
/// is explicitly not stable across Rust releases, and a state file whose
/// name moves when the toolchain moves orphans a running server — the
/// record `stop` needs is suddenly somewhere else. `sha2` is already in the
/// graph for the updater, so this costs no dependency.
///
/// Hashes the OS bytes rather than a lossy UTF-8 conversion: a path this
/// platform accepts but Unicode cannot spell must still map to one name.
fn source_digest(source: &Path) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let digest = Sha256::digest(source.as_os_str().as_encoded_bytes());
    let mut encoded = String::with_capacity(SOURCE_DIGEST_HEX);
    for byte in digest.iter().take(SOURCE_DIGEST_HEX / 2) {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

/// What `npu` persisted about a runtime it started.
///
/// `(pid, process_start_time)` is the pair that survives PID reuse, and the
/// WHOLE of the identity check: a pid alone can, after a reboot or enough
/// process churn, name somebody else's process, and the remedy for a stale
/// state file must never be a kill. `executable` and `arguments` are not
/// part of it — they are what an operator reading this file needs in order
/// to know what was started (see `runtime::process::verdict` for why
/// comparing either would reject `npu`'s own child after an `exec`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    /// Schema version.
    ///
    /// Stamped by [`save`] with [`SCHEMA_VERSION`] whatever a caller put
    /// here, and checked by [`load`]: the writer owns the schema, so a
    /// record built in another module cannot carry a version this one does
    /// not write. There is no constructor for the same reason there is no
    /// ceremony around a plain data record — the fields are the API.
    pub version: u32,
    /// Identifier of the backend this record belongs to, also its file name.
    pub backend: String,
    /// Pid of the process `npu serve` spawned.
    pub pid: u32,
    /// Executable that pid was started from — what `npu` launched, for
    /// whoever reads this file. NOT an identity token: after an `exec` the
    /// running image is another one, and the process is still ours.
    pub executable: PathBuf,
    /// Arguments it was started with, recorded for the same reason and with
    /// the same status: they say what was launched, and the configuration
    /// they were rendered from may have changed since.
    pub arguments: Vec<String>,
    /// When `npu` started it, in seconds since the Unix epoch.
    pub started_at: u64,
    /// `sysinfo`'s `Process::start_time()`, also in seconds since the Unix
    /// epoch: the anti-PID-reuse token.
    pub process_start_time: u64,
    /// The base URL the backend answers on, resolved — a runtime that was
    /// given an ephemeral port has no other place to record which one it
    /// got.
    pub base_url: String,
    /// The backend FILE this record was served from.
    ///
    /// Part of the identity, and for a reason `pid` cannot cover: this
    /// directory is machine-global while backend identifiers are per-scope,
    /// so two projects each declaring `.npu/backends/llamacpp.toml` would
    /// otherwise land on the same record.
    ///
    /// What keeps them apart is the file NAME, which carries a digest of
    /// this very path (see [`SOURCE_DIGEST_HEX`]). The field itself is what
    /// catches the residual case that digest cannot: 32 bits can collide,
    /// two sources can meet on one name, and the pid behind such a record is
    /// a genuine npu-started process with a genuinely matching birth. Only
    /// the origin, compared in full by `runtime::process::presence`, tells
    /// that it is the wrong BACKEND — on the one path where being wrong is a
    /// SIGKILL against another project's server.
    pub source: PathBuf,
}

/// Wraps an I/O failure so the message NAMES the file it is about.
///
/// `std::fs` errors carry no path in their `Display` ("No such file or
/// directory" and nothing else), and a diagnostic that does not name the
/// offending file is a regression for a calling agent. The original
/// `ErrorKind` is preserved rather than flattened to `other`, so a caller
/// can still tell a permission problem from a missing directory.
fn io_at(path: &Path, err: &std::io::Error) -> crate::Error {
    crate::Error::Io(std::io::Error::new(
        err.kind(),
        format!("{}: {err}", path.display()),
    ))
}

/// Same, for a file that was read fine but cannot be understood.
fn invalid_at(path: &Path, detail: &str) -> crate::Error {
    crate::Error::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("{}: {detail}", path.display()),
    ))
}

/// Writes `state` for its backend, atomically, and returns the file written.
///
/// `<id>-<digest>.json.tmp` -> write -> `flush` -> `sync_all` -> close ->
/// `rename`.
/// Every step earns its place: a truncate-in-place would leave a half
/// written record readable by the `npu stop` running in another process, and
/// deciding which process to kill from a truncated file is how the wrong one
/// dies. The temporary file is named after the BACKEND, not shared: two
/// `npu serve` on two backends run concurrently and a single `state.json.tmp`
/// would have them overwrite each other. The handle is dropped before the
/// rename, which Windows requires to replace an existing file.
///
/// The version written is [`SCHEMA_VERSION`], whatever the record carries:
/// the writer owns the schema. The path comes from the record too — its own
/// `backend` and its own `source` — rather than from a second parameter: a
/// record filed under an origin it does not claim is a record `load` could
/// never be made to agree with.
///
/// # Errors
///
/// `Error::Config` naming the backend for an identifier unusable as a file
/// name; otherwise `Error::Io` naming the path that failed.
pub fn save(env: &StateEnv, state: &State) -> crate::Result<PathBuf> {
    let dir = state_dir(env)?;
    let name = file_name(&state.backend, &state.source)?;

    std::fs::create_dir_all(&dir).map_err(|err| io_at(&dir, &err))?;

    let path = dir.join(&name);
    let staging = dir.join(format!("{name}.tmp"));

    let record = State {
        version: SCHEMA_VERSION,
        ..state.clone()
    };
    // Can only fail on a non-UTF-8 `executable`, which JSON cannot spell.
    let bytes = serde_json::to_vec_pretty(&record)
        .map_err(|err| invalid_at(&path, &format!("cannot be serialized: {err}")))?;

    {
        let mut file = std::fs::File::create(&staging).map_err(|err| io_at(&staging, &err))?;
        file.write_all(&bytes)
            .map_err(|err| io_at(&staging, &err))?;
        file.flush().map_err(|err| io_at(&staging, &err))?;
        file.sync_all().map_err(|err| io_at(&staging, &err))?;
    }

    std::fs::rename(&staging, &path).map_err(|err| io_at(&path, &err))?;

    Ok(path)
}

/// The version field alone, parsed WITHOUT `deny_unknown_fields`.
///
/// The whole point of a schema version is to diagnose a file this binary
/// does not understand — and a newer `npu` adding a field is exactly that
/// file. Parsing it straight into [`State`] would fail on the unknown key
/// FIRST, reporting a malformed file instead of a version mismatch and
/// sending its reader looking for a typo in something `npu` itself wrote.
#[derive(Debug, Deserialize)]
struct VersionProbe {
    version: u32,
    /// The pid the record holds, if a record of that shape ever holds one.
    ///
    /// Optional because this probe must survive ANY shape: it exists to
    /// diagnose a file this binary does not understand. Read only so the
    /// message can name the process the file describes — the remedy
    /// ("remove the file") is otherwise an instruction to delete the only
    /// thing that can still find a running server.
    #[serde(default)]
    pid: Option<u32>,
}

/// Reads the state of the backend `backend_id` declared by `source`, if any.
///
/// # Errors
///
/// An absent file is `Ok(None)`, not an error: "never started" is a normal
/// state of a lifecycle. Unreadable, malformed or carrying an unknown
/// version is `Error::Io` NAMING the path — never a silent `None`, which
/// would let a `serve` start a second process next to the one already
/// running, and never a deletion.
pub fn load(env: &StateEnv, backend_id: &str, source: &Path) -> crate::Result<Option<State>> {
    let path = state_path(env, backend_id, source)?;

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io_at(&path, &err)),
    };

    let probe: VersionProbe = serde_json::from_slice(&bytes)
        .map_err(|err| invalid_at(&path, &format!("is not a readable state file: {err}")))?;

    if probe.version != SCHEMA_VERSION {
        // The pid, when the file still spells one: this record may be the
        // only thing that knows about a RUNNING server (an `npu update`
        // while something was served produces exactly this file), and
        // "remove the file" alone is an instruction to delete that.
        let describes = probe.pid.map_or_else(
            || " — remove the file to forget the runtime it describes".to_string(),
            |pid| {
                format!(
                    " — it describes process {pid}, which must be stopped before the file is \
                     removed"
                )
            },
        );
        return Err(invalid_at(
            &path,
            &format!(
                "state schema version {} is unknown to this npu (which writes \
                 {SCHEMA_VERSION}){describes}",
                probe.version
            ),
        ));
    }

    let state: State = serde_json::from_slice(&bytes)
        .map_err(|err| invalid_at(&path, &format!("is not a readable state file: {err}")))?;

    Ok(Some(state))
}

/// Forgets the state of the backend `backend_id` declared by `source`.
///
/// # Errors
///
/// `Error::Io` naming the path, except for an already-absent file: removal
/// is idempotent because `stop` is, and a `stop` that fails on a runtime
/// already gone would make the lifecycle a one-way trip (same reason
/// `docker rm --force` is used over `docker stop`).
pub fn clear(env: &StateEnv, backend_id: &str, source: &Path) -> crate::Result<()> {
    let path = state_path(env, backend_id, source)?;

    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(io_at(&path, &err)),
    }
}

/// Forgets that state, but ONLY if the record still names `pid`.
///
/// `serve` holds no lock: between the moment it reads the record and the
/// moment one of its failure paths gives up, a second `serve` on the same
/// backend may have replaced that record with its own. An unconditional
/// removal there deletes the record of a process that is running — leaving a
/// server holding its port with nothing left that can stop it, and `npu
/// status` reporting `not started`. A caller abandoning its own child says
/// which pid it is abandoning, and a record naming another one is left alone.
///
/// A record that cannot be read is left alone too, and reported: this
/// function removes, it does not repair.
///
/// # Errors
///
/// Whatever [`load`] and [`clear`] return.
pub fn clear_of(env: &StateEnv, backend_id: &str, source: &Path, pid: u32) -> crate::Result<()> {
    match load(env, backend_id, source)? {
        Some(state) if state.pid != pid => Ok(()),
        _ => clear(env, backend_id, source),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // tolerated in tests (see Cargo.toml [lints.clippy]).
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Unique fixture directory, never a fixed path: tests run in parallel
    /// (same idiom as `scope::tests::fixture_dir`).
    fn fixture_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!("state-{name}-{n}"));
        std::fs::create_dir_all(&dir).expect("fixture directory creation");
        dir
    }

    /// An environment pointing BOTH variables at the same fixture, so the
    /// same test exercises the real code path on Linux and on macOS: the
    /// two platforms derive different subdirectories under it, and both are
    /// created on demand.
    fn fixture_env(name: &str) -> StateEnv {
        let dir = fixture_dir(name);
        StateEnv {
            xdg_state_home: Some(dir.clone()),
            home: Some(dir),
        }
    }

    fn env(xdg_state_home: Option<&str>, home: Option<&str>) -> StateEnv {
        StateEnv {
            xdg_state_home: xdg_state_home.map(PathBuf::from),
            home: home.map(PathBuf::from),
        }
    }

    /// The backend FILE every record of this module pretends to come from,
    /// and the second key of every state file name.
    const SOURCE: &str = "/home/alice/projA/.npu/backends/llamacpp.toml";

    fn source() -> PathBuf {
        PathBuf::from(SOURCE)
    }

    fn sample(backend: &str) -> State {
        State {
            version: SCHEMA_VERSION,
            backend: backend.to_string(),
            pid: 4242,
            executable: PathBuf::from("/usr/local/bin/llama-server"),
            arguments: vec!["--port".to_string(), "8080".to_string()],
            started_at: 1_790_084_826,
            process_start_time: 1_790_084_820,
            base_url: "http://127.0.0.1:8080/v1".to_string(),
            source: source(),
        }
    }

    #[test]
    fn linux_state_dir_uses_xdg_state_home_verbatim() {
        let e = env(Some("/xdg-state"), Some("/home/alice"));
        assert_eq!(linux_state_dir(&e), Some(PathBuf::from("/xdg-state/npu")));
    }

    #[test]
    fn linux_state_dir_falls_back_to_home_local_state() {
        let e = env(None, Some("/home/alice"));
        assert_eq!(
            linux_state_dir(&e),
            Some(PathBuf::from("/home/alice/.local/state/npu"))
        );
    }

    #[test]
    fn linux_state_dir_without_xdg_or_home_is_none() {
        assert_eq!(linux_state_dir(&env(None, None)), None);
    }

    #[test]
    fn macos_state_dir_is_application_support() {
        let e = env(None, Some("/Users/alice"));
        assert_eq!(
            macos_state_dir(&e),
            Some(PathBuf::from(
                "/Users/alice/Library/Application Support/npu/state"
            ))
        );
    }

    /// `XDG_STATE_HOME` has no meaning on macOS: setting it must not move
    /// the directory, or one machine ends up with two state locations.
    #[test]
    fn macos_state_dir_ignores_xdg_state_home() {
        let e = env(Some("/xdg-state"), Some("/Users/alice"));
        assert_eq!(
            macos_state_dir(&e),
            Some(PathBuf::from(
                "/Users/alice/Library/Application Support/npu/state"
            ))
        );
    }

    #[test]
    fn macos_state_dir_without_home_is_none() {
        assert_eq!(macos_state_dir(&env(Some("/xdg-state"), None)), None);
    }

    #[test]
    fn state_dir_without_any_home_is_an_io_error() {
        let err = state_dir(&env(None, None)).expect_err("no home means no state directory");
        assert!(matches!(err, crate::Error::Io(_)));
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn state_dir_names_both_variables_it_looked_for() {
        let err = state_dir(&env(None, None)).expect_err("no home means no state directory");
        let message = err.to_string();
        // Identifiers, not wording: a reader has to learn WHICH variables.
        assert!(message.contains("XDG_STATE_HOME"), "{message}");
        assert!(message.contains("HOME"), "{message}");
    }

    #[test]
    fn a_backend_identifier_that_escapes_the_directory_is_rejected() {
        let e = fixture_env("escape");
        for id in ["../evil", "a/b", "", ".", "..", "/etc/passwd"] {
            let err = state_path(&e, id, &source()).expect_err("must not become a path");
            assert!(matches!(err, crate::Error::Config(_)), "{id}");
            assert_eq!(err.exit_code(), 2, "{id}");
        }
    }

    #[test]
    fn the_rejection_names_the_offending_backend() {
        let e = fixture_env("escape-named");
        let err = state_path(&e, "../evil", &source()).expect_err("must not become a path");
        assert!(err.to_string().contains("../evil"), "{err}");
    }

    #[test]
    fn a_state_path_stays_inside_the_state_directory() {
        let e = fixture_env("inside");
        let dir = state_dir(&e).expect("a home is set");
        let path = state_path(&e, "qwen-fast", &source()).expect("a valid identifier");
        assert_eq!(path.parent(), Some(dir.as_path()));
    }

    #[test]
    fn each_backend_gets_its_own_file() {
        let e = fixture_env("per-backend");
        let first = state_path(&e, "qwen-fast", &source()).expect("a valid identifier");
        let second = state_path(&e, "qwen-big", &source()).expect("a valid identifier");
        assert_ne!(first, second);
    }

    /// The defect the digest exists for: the state directory is
    /// machine-global while backend identifiers are per-scope, so two
    /// projects each declaring `llamacpp` must not share one record — nor
    /// one log, which is the record's path with another extension.
    #[test]
    fn two_projects_declaring_the_same_backend_get_different_records() {
        let e = fixture_env("two-projects");
        let a = state_path(
            &e,
            "llamacpp",
            Path::new("/projA/.npu/backends/llamacpp.toml"),
        )
        .expect("a valid identifier");
        let b = state_path(
            &e,
            "llamacpp",
            Path::new("/projB/.npu/backends/llamacpp.toml"),
        )
        .expect("a valid identifier");

        assert_ne!(a, b);
        assert_ne!(a.with_extension("log"), b.with_extension("log"));
    }

    /// A name that moved between two calls would orphan a running server:
    /// `stop` would look for the record `serve` wrote and find nothing.
    #[test]
    fn the_same_backend_always_resolves_to_the_same_record() {
        let e = fixture_env("stable-digest");
        let first = state_path(&e, "llamacpp", &source()).expect("a valid identifier");
        let second = state_path(&e, "llamacpp", &source()).expect("a valid identifier");

        assert_eq!(first, second);
    }

    /// The identifier stays legible in the name: the record is documented
    /// as plain JSON to open by hand when a runtime was forgotten.
    #[test]
    fn a_record_is_still_named_after_its_backend() {
        let e = fixture_env("legible");
        let path = state_path(&e, "llamacpp", &source()).expect("a valid identifier");
        let name = path
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .expect("a UTF-8 file name");

        assert!(name.starts_with("llamacpp-"), "{name}");
        assert_eq!(
            path.extension().and_then(std::ffi::OsStr::to_str),
            Some("json")
        );
    }

    #[test]
    fn save_then_load_round_trips() {
        let e = fixture_env("round-trip");
        let state = sample("qwen-fast");
        save(&e, &state).expect("the record must be written");

        let read = load(&e, "qwen-fast", &source())
            .expect("the record must be readable")
            .expect("the record must be there");
        assert_eq!(read, state);
    }

    #[test]
    fn two_backends_do_not_overwrite_each_other() {
        let e = fixture_env("two-backends");
        let mut other = sample("qwen-big");
        other.pid = 77;
        save(&e, &sample("qwen-fast")).expect("the first record");
        save(&e, &other).expect("the second record");

        let first = load(&e, "qwen-fast", &source())
            .expect("readable")
            .expect("present");
        let second = load(&e, "qwen-big", &source())
            .expect("readable")
            .expect("present");
        assert_eq!(first.pid, 4242);
        assert_eq!(second.pid, 77);
    }

    #[test]
    fn load_of_an_absent_state_is_none_not_an_error() {
        let e = fixture_env("absent");
        assert_eq!(
            load(&e, "never-started", &source()).expect("absent is normal"),
            None
        );
    }

    #[test]
    fn a_second_save_fully_replaces_the_first() {
        let e = fixture_env("replace");
        let mut first = sample("qwen-fast");
        first.arguments = vec!["--a-very-long-argument-list".to_string(); 32];
        save(&e, &first).expect("the first record");

        let mut second = sample("qwen-fast");
        second.pid = 7;
        second.arguments = vec!["--short".to_string()];
        let path = save(&e, &second).expect("the second record");

        let read = load(&e, "qwen-fast", &source())
            .expect("readable")
            .expect("present");
        assert_eq!(read, second);

        // A rename replaces, it does not append: nothing of the longer
        // record may survive past the end of the shorter one.
        let bytes = std::fs::read(&path).expect("the file is readable");
        assert_eq!(
            bytes.len(),
            serde_json::to_vec_pretty(&second).expect("json").len()
        );
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let e = fixture_env("no-temp");
        save(&e, &sample("qwen-fast")).expect("the record must be written");

        let dir = state_dir(&e).expect("a home is set");
        let leftovers: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("the state directory exists")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn the_writer_owns_the_schema_version() {
        let e = fixture_env("owned-version");
        let mut state = sample("qwen-fast");
        state.version = 999;
        save(&e, &state).expect("the record must be written");

        let read = load(&e, "qwen-fast", &source())
            .expect("readable")
            .expect("present");
        assert_eq!(read.version, SCHEMA_VERSION);
    }

    /// Writes `contents` where `backend_id`'s state file belongs.
    fn plant(e: &StateEnv, backend_id: &str, contents: &str) -> PathBuf {
        let dir = state_dir(e).expect("a home is set");
        std::fs::create_dir_all(&dir).expect("the state directory");
        let path = state_path(e, backend_id, &source()).expect("a valid identifier");
        std::fs::write(&path, contents).expect("the planted file");
        path
    }

    #[test]
    fn an_unknown_schema_version_is_an_error_naming_the_path() {
        let e = fixture_env("unknown-version");
        let path = plant(&e, "qwen-fast", r#"{"version": 999}"#);

        let err = load(&e, "qwen-fast", &source())
            .expect_err("an unknown version must not be guessed at");
        assert!(matches!(err, crate::Error::Io(_)));
        assert_eq!(err.exit_code(), 1);
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
    }

    /// A record of an unknown schema may be the only trace of a RUNNING
    /// server: the message has to hand back the pid it holds, or its reader
    /// deletes the file and loses the process.
    #[test]
    fn an_unknown_schema_version_names_the_process_the_record_describes() {
        let e = fixture_env("unknown-version-pid");
        plant(&e, "qwen-fast", r#"{"version": 999, "pid": 31337}"#);

        let err = load(&e, "qwen-fast", &source())
            .expect_err("an unknown version must not be guessed at");
        assert!(err.to_string().contains("31337"), "{err}");
    }

    /// A newer `npu` adding a field must still be diagnosed as a VERSION
    /// mismatch: `deny_unknown_fields` on `State` would otherwise report a
    /// malformed file and send its reader hunting a typo in something `npu`
    /// itself wrote.
    #[test]
    fn a_future_version_with_an_extra_field_is_still_a_version_error() {
        let e = fixture_env("future-version");
        plant(
            &e,
            "qwen-fast",
            r#"{"version": 999, "something_new": true}"#,
        );

        let err = load(&e, "qwen-fast", &source()).expect_err("a future record must not be read");
        assert!(err.to_string().contains("999"), "{err}");
    }

    #[test]
    fn an_unknown_field_at_the_current_version_is_rejected() {
        let e = fixture_env("unknown-field");
        let mut json = serde_json::to_value(sample("qwen-fast")).expect("json");
        json.as_object_mut()
            .expect("an object")
            .insert("stowaway".to_string(), serde_json::Value::Bool(true));
        let path = plant(&e, "qwen-fast", &json.to_string());

        let err = load(&e, "qwen-fast", &source()).expect_err("an unknown key must not be ignored");
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn malformed_json_is_an_error_naming_the_path() {
        let e = fixture_env("malformed");
        let path = plant(&e, "qwen-fast", "{not json at all");

        let err =
            load(&e, "qwen-fast", &source()).expect_err("a malformed record must not be read");
        assert!(matches!(err, crate::Error::Io(_)));
        assert_eq!(err.exit_code(), 1);
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
    }

    /// A file that cannot be read is reported, never silently forgotten: a
    /// `None` here would let `serve` start a second process beside the one
    /// already running.
    #[test]
    fn a_malformed_state_file_is_left_on_disk() {
        let e = fixture_env("kept");
        let path = plant(&e, "qwen-fast", "{not json at all");
        let _ = load(&e, "qwen-fast", &source());
        assert!(path.exists());
    }

    #[test]
    fn clear_removes_the_record() {
        let e = fixture_env("clear");
        let path = save(&e, &sample("qwen-fast")).expect("the record must be written");

        clear(&e, "qwen-fast", &source()).expect("removal must succeed");

        assert!(!path.exists());
        assert_eq!(
            load(&e, "qwen-fast", &source()).expect("absent is normal"),
            None
        );
    }

    #[test]
    fn clear_is_idempotent() {
        let e = fixture_env("clear-idempotent");
        save(&e, &sample("qwen-fast")).expect("the record must be written");

        clear(&e, "qwen-fast", &source()).expect("the first removal");
        clear(&e, "qwen-fast", &source()).expect("removing an absent record is a success");
    }

    #[test]
    fn clear_of_a_never_started_backend_is_a_success() {
        let e = fixture_env("clear-never");
        clear(&e, "never-started", &source()).expect("removing nothing is a success");
    }

    #[test]
    fn clear_of_removes_the_record_when_the_pid_matches() {
        let e = fixture_env("clear-of-match");
        save(&e, &sample("qwen-fast")).expect("the record must be written");

        clear_of(&e, "qwen-fast", &source(), 4242).expect("removing our own record");

        assert_eq!(
            load(&e, "qwen-fast", &source()).expect("absent is normal"),
            None
        );
    }

    /// The race this exists for: a second `serve` replaced the record, and
    /// the first one's failure path must not delete a RUNNING server's only
    /// trace.
    #[test]
    fn clear_of_keeps_a_record_naming_another_process() {
        let e = fixture_env("clear-of-other");
        save(&e, &sample("qwen-fast")).expect("the record must be written");

        clear_of(&e, "qwen-fast", &source(), 7)
            .expect("another pid's record is not ours to remove");

        let kept = load(&e, "qwen-fast", &source())
            .expect("readable")
            .expect("present");
        assert_eq!(kept.pid, 4242);
    }

    #[test]
    fn clear_of_an_absent_record_is_a_success() {
        let e = fixture_env("clear-of-absent");
        clear_of(&e, "never-started", &source(), 4242).expect("removing nothing is a success");
    }

    #[test]
    fn clear_rejects_an_escaping_identifier_rather_than_removing_anything() {
        let e = fixture_env("clear-escape");
        let err = clear(&e, "../evil", &source()).expect_err("must not become a path");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn save_rejects_an_escaping_identifier() {
        let e = fixture_env("save-escape");
        let mut state = sample("qwen-fast");
        state.backend = "../evil".to_string();
        let err = save(&e, &state).expect_err("must not become a path");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("../evil"), "{err}");
    }
}
