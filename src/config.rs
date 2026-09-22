//! Configuration loading: backends and models.
//!
//! `load` loads a single scope. `load_scopes` composes several scopes in
//! layers: replacement by `id`, with the most local scope winning entirely —
//! no field-by-field merge.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// An HTTP operation exposed by a backend (e.g. `chat`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub method: String,
    pub path: String,
}

/// How to start the runtime of a backend as a container, declared by the
/// optional `[docker]` table of `backends/*.toml`.
///
/// `options` and `args` are two lists rather than one because `docker run`'s
/// own grammar imposes it (`docker run [OPTIONS] IMAGE [ARG...]`): merging
/// them would make the position of the image implicit.
///
/// Both lists go through `crate::prompt::render` (`{{ args.model }}`,
/// `{{ env.NAME }}`) — the CLI embeds no container knowledge beyond the
/// shape of a `docker run` invocation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Docker {
    pub image: String,
    /// Passed to `docker run` BEFORE the image (ports, volumes, devices).
    #[serde(default)]
    pub options: Vec<String>,
    /// Passed to the image AFTER it (the server's own arguments).
    #[serde(default)]
    pub args: Vec<String>,
}

/// Default value of [`Process::startup_timeout_secs`]: how long `npu serve`
/// waits for a spawned server to start answering before it gives up, kills
/// it and reports the failure.
///
/// Thirty seconds because the process families this drives (a local
/// inference server loading a model from disk) are slow to become ready and
/// fast to fail: a server that crashed is detected by its exit, not by this
/// deadline, so the number only has to be generous enough for an honest
/// start.
const DEFAULT_STARTUP_TIMEOUT_SECS: u64 = 30;

/// `serde` default for [`Process::startup_timeout_secs`].
const fn default_startup_timeout_secs() -> u64 {
    DEFAULT_STARTUP_TIMEOUT_SECS
}

/// How to start the runtime of a backend as a plain child process, declared
/// by `[runtime] type = "process"`.
///
/// Carries no knowledge of any particular server: `command` and `arguments`
/// are what the author wrote, which is what makes llama.cpp, MLX or anything
/// else a CONFIGURATION change rather than a rebuild.
///
/// `arguments` and the values of `env` go through `crate::prompt::render`
/// (`{{ args.model }}`, `{{ env.NAME }}`) and through
/// [`substitute_port`] (`{{ backend.port }}`), exactly like the `[docker]`
/// lists — one template engine, not two.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    /// The executable to run: an absolute or relative path used as-is, or a
    /// bare name looked up on `PATH`.
    pub command: String,
    /// Arguments handed to `command`, in order.
    #[serde(default)]
    pub arguments: Vec<String>,
    /// Variables layered OVER the parent environment of `npu serve` for the
    /// child only — an overlay, never a replacement: a server that needs
    /// `HOME` or `PATH` must not have to redeclare them.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Seconds granted to the spawned server to start answering on its port
    /// before `npu serve` declares the start a failure.
    #[serde(default = "default_startup_timeout_secs")]
    pub startup_timeout_secs: u64,
}

/// How `npu serve` starts this backend's runtime, declared by the optional
/// tagged `[runtime]` table of `backends/*.toml`:
///
/// ```toml
/// [runtime]
/// type = "docker"
/// image = "openvino/model_server:latest"
/// ```
///
/// ```toml
/// [runtime]
/// type = "process"
/// command = "llama-server"
/// arguments = ["--port", "{{ backend.port }}"]
/// ```
///
/// Tagged rather than one table per family (`[docker]`, `[process]`) because
/// the tag is what makes an unsupported runtime a NAMED rejection
/// (`unknown variant "podman"`) instead of a table serde would have to guess
/// the meaning of.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Runtime {
    Docker(Docker),
    Process(Process),
}

/// The `type` value of the Docker runtime, quoted by the messages that tell
/// an author which table to write.
const RUNTIME_DOCKER: &str = "docker";

/// The `[docker]` table of a backend whose runtime is Docker, `None` for a
/// backend with no runtime at all — or one managed by another family.
///
/// Written as a `match` rather than a `matches!`/`map` pair so a new variant
/// makes this function fail to compile instead of silently answering `None`
/// for a runtime it does not know. `pub(crate)` for that reason too: it is
/// the ONE family test in the crate, so `doctor`'s container check joins the
/// same `match` instead of keeping a `matches!` of its own that a new
/// variant would leave silently answering `false`.
pub(crate) fn docker_of(backend: &Backend) -> Option<&Docker> {
    match backend.runtime.as_ref()? {
        Runtime::Docker(docker) => Some(docker),
        Runtime::Process(_) => None,
    }
}

/// Mutable twin of [`docker_of`], for the load-time `{{ backend.port }}`
/// substitution.
fn docker_of_mut(backend: &mut Backend) -> Option<&mut Docker> {
    match backend.runtime.as_mut()? {
        Runtime::Docker(docker) => Some(docker),
        Runtime::Process(_) => None,
    }
}

/// The `[runtime]` table of a backend whose runtime is a process, `None`
/// otherwise. The [`docker_of`] twin, exhaustive for the same reason.
pub(crate) fn process_of(backend: &Backend) -> Option<&Process> {
    match backend.runtime.as_ref()? {
        Runtime::Process(process) => Some(process),
        Runtime::Docker(_) => None,
    }
}

/// Mutable twin of [`process_of`], for the load-time `{{ backend.port }}`
/// substitution.
fn process_of_mut(backend: &mut Backend) -> Option<&mut Process> {
    match backend.runtime.as_mut()? {
        Runtime::Process(process) => Some(process),
        Runtime::Docker(_) => None,
    }
}

/// Does any of a `[docker]` table's templates read `{{ backend.port }}`?
fn docker_reads_port(docker: &Docker) -> bool {
    std::iter::once(&docker.image)
        .chain(&docker.options)
        .chain(&docker.args)
        .any(|template| substitute_port(template, 0).1)
}

/// Does any of a process runtime's templates read `{{ backend.port }}`?
///
/// `command` is deliberately excluded, here and in the substitution below:
/// an executable whose PATH depends on a port is not a case worth
/// supporting, and a key that is never substituted must not be able to
/// satisfy the "declares a port someone reads" rule.
fn process_reads_port(process: &Process) -> bool {
    process
        .arguments
        .iter()
        .chain(process.env.values())
        .any(|template| substitute_port(template, 0).1)
}

/// Rejects a backend declaring BOTH the tagged `[runtime]` table and the
/// legacy `[docker]` one.
///
/// Runs in the load-time mutating pass, immediately BEFORE
/// [`normalize_runtime`], and not in `validate_backend` where the other
/// inter-field rules live: normalization folds the legacy table into
/// `runtime` and leaves nothing behind, so by validation time the evidence
/// of a double declaration is gone. Reconciling the two instead would mean
/// picking a winner, which is a key read and then silently ignored.
fn reject_double_runtime(backend: &Backend, source: &Path) -> crate::Result<()> {
    if backend.runtime.is_some() && backend.docker.is_some() {
        return Err(crate::Error::Config(format!(
            "{}: backend \"{}\": declares both [runtime] and the legacy [docker] table — \
             keep [runtime] alone, npu will not guess which one wins",
            source.display(),
            backend.id
        )));
    }
    Ok(())
}

/// Folds a legacy `[docker]` table into `runtime = Runtime::Docker`.
///
/// Done at LOAD time, once, rather than on every read: it is what lets
/// [`Backend::runtime`] hand out a plain reference (no `Cow`, no clone per
/// call) and what keeps every reader downstream — `builtin.rs`, `runtime/`,
/// `lib.rs` — written against a runtime FAMILY instead of against Docker.
fn normalize_runtime(backend: &mut Backend) {
    if backend.runtime.is_none()
        && let Some(docker) = backend.docker.take()
    {
        backend.runtime = Some(Runtime::Docker(docker));
    }
}

/// Per-backend request timeout override, declared by the optional
/// `[timeouts]` table of `backends/*.toml`. Absent, `backend.rs` falls back
/// to its own default.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timeouts {
    /// Seconds granted to a request before failure. Must be non-zero: a
    /// backend that declares `[timeouts]` without meaning to bound anything
    /// should omit the table instead.
    pub request_secs: u64,
}

/// The value of a backend's optional `port` key: an explicit number, or the
/// keyword `"auto"`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Port {
    Fixed(u16),
    /// Any string; only `"auto"` is accepted, checked by [`resolve_port`]
    /// so that a typo names its file instead of being treated as an unknown
    /// type by serde.
    Keyword(String),
}

/// The keyword accepted by `port`.
const PORT_AUTO: &str = "auto";

/// The value substituted for `{{ backend.port }}` in the `[docker]` lists of
/// a `port = "auto"` backend: Docker's own "pick a free one".
///
/// Allocation is delegated to the kernel rather than derived or probed
/// because `npu` keeps no state between processes: `npu serve` and the
/// `npu <command>` that follows minutes later, in another process, must
/// agree on a port. Deriving one (a hash of the id) agrees but can collide
/// with an unrelated service; probing for a free one does not agree at all,
/// since by request time the port is occupied — by us. Letting Docker
/// allocate and then ASKING it what it allocated is the only variant that is
/// both collision-free and reproducible, at the cost of making Docker a
/// prerequisite for executing commands on such a backend, not just for its
/// lifecycle.
pub(crate) const DOCKER_EPHEMERAL_PORT: u16 = 0;

/// The only `{{ backend.<name> }}` placeholder that exists.
const BACKEND_PLACEHOLDER: &str = "backend.port";

/// Replaces every `{{ backend.port }}` in `template` with `port`, and reports
/// whether it replaced anything.
///
/// Handled HERE rather than through `prompt.rs`: this placeholder has no
/// meaning in a command file, so the prompt engine stays unaware of it and
/// `validate_docker_template` stays as it is. A fixed `port` is substituted
/// at load time; a `port = "auto"` base URL keeps its placeholder until
/// `builtin::resolve_base_url` reads the real value back from Docker.
pub(crate) fn substitute_port(template: &str, port: u16) -> (String, bool) {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    let mut substituted = false;

    while let Some(open) = rest.find("{{") {
        let after_open = &rest[open + 2..];
        let Some(close) = after_open.find("}}") else {
            break;
        };
        if after_open[..close].trim() != BACKEND_PLACEHOLDER {
            out.push_str(&rest[..open + 2]);
            rest = after_open;
            continue;
        }
        out.push_str(&rest[..open]);
        out.push_str(&port.to_string());
        rest = &after_open[close + 2..];
        substituted = true;
    }

    out.push_str(rest);
    (out, substituted)
}

/// Substitutes `{{ backend.port }}` throughout a backend, and enforces that
/// `port` and the placeholder are declared together.
///
/// Both directions are configuration errors naming the file: a placeholder
/// without a `port` cannot be resolved, and a `port` no placeholder reads is
/// a key that would be silently ignored.
fn resolve_port(backend: &mut Backend, source: &Path) -> crate::Result<()> {
    let references_port = |backend: &Backend| {
        substitute_port(&backend.base_url, 0).1
            || docker_of(backend).is_some_and(docker_reads_port)
            || process_of(backend).is_some_and(process_reads_port)
    };

    let port = match &backend.port {
        None => {
            if references_port(backend) {
                return Err(crate::Error::Config(format!(
                    "{}: backend \"{}\": references {{{{ {BACKEND_PLACEHOLDER} }}}} but \
                     declares no \"port\" key",
                    source.display(),
                    backend.id
                )));
            }
            return Ok(());
        }
        Some(Port::Fixed(0)) => {
            return Err(crate::Error::Config(format!(
                "{}: backend \"{}\": port 0 is not a port (\"{PORT_AUTO}\" lets Docker \
                 allocate one instead)",
                source.display(),
                backend.id
            )));
        }
        Some(Port::Fixed(number)) => *number,
        Some(Port::Keyword(keyword)) if keyword == PORT_AUTO => {
            // `auto` means "Docker allocates, npu asks it back", so both
            // halves must exist: something to start, and a base URL whose
            // port can be filled in afterwards.
            if docker_of(backend).is_none() {
                return Err(crate::Error::Config(format!(
                    "{}: backend \"{}\": port = \"{PORT_AUTO}\" requires a Docker runtime \
                     ([runtime] type = \"{RUNTIME_DOCKER}\") — npu can only read back a port it \
                     asked Docker to allocate",
                    source.display(),
                    backend.id
                )));
            }
            // BOTH sides must read the placeholder, for the same reason:
            // without it in [docker] nothing is published, and without it in
            // base_url nothing reaches what was published. Either way the
            // allocated port is unreachable — a failure that would only
            // surface at the first command, as advice ("start it with npu
            // serve") that could never work.
            let in_docker = docker_of(backend).is_some_and(docker_reads_port);
            if !substitute_port(&backend.base_url, 0).1 || !in_docker {
                return Err(crate::Error::Config(format!(
                    "{}: backend \"{}\": port = \"{PORT_AUTO}\" requires \
                     {{{{ {BACKEND_PLACEHOLDER} }}}} in BOTH base_url and [docker], otherwise \
                     the allocated port is never published or never reached",
                    source.display(),
                    backend.id
                )));
            }
            DOCKER_EPHEMERAL_PORT
        }
        Some(Port::Keyword(keyword)) => {
            return Err(crate::Error::Config(format!(
                "{}: backend \"{}\": port \"{keyword}\" is neither a number nor \
                 \"{PORT_AUTO}\"",
                source.display(),
                backend.id
            )));
        }
    };

    let mut used = false;

    // A `port = "auto"` base URL keeps its placeholder: the value is only
    // known once Docker has allocated it, so substituting 0 here would send
    // every request to port 0. Its `used` bookkeeping is already settled by
    // the stricter both-sides check above.
    if backend.uses_auto_port() {
        used = true;
    } else {
        let (base_url, hit) = substitute_port(&backend.base_url, port);
        backend.base_url = base_url;
        used |= hit;
    }

    if let Some(docker) = docker_of_mut(backend) {
        for template in std::iter::once(&mut docker.image)
            .chain(&mut docker.options)
            .chain(&mut docker.args)
        {
            let (resolved, hit) = substitute_port(template, port);
            *template = resolved;
            used |= hit;
        }
    }

    if let Some(process) = process_of_mut(backend) {
        for template in process.arguments.iter_mut().chain(process.env.values_mut()) {
            let (resolved, hit) = substitute_port(template, port);
            *template = resolved;
            used |= hit;
        }
    }

    if !used {
        return Err(crate::Error::Config(format!(
            "{}: backend \"{}\": declares \"port\" but never references \
             {{{{ {BACKEND_PLACEHOLDER} }}}}, so the value would be ignored",
            source.display(),
            backend.id
        )));
    }

    Ok(())
}

/// An AI backend configured in `backends/*.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    pub id: String,
    pub base_url: String,
    /// TOML field `type` (a reserved Rust word), e.g. `"openai-compatible"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub operations: HashMap<String, Operation>,
    /// Optional: the TCP port this backend listens on, substituted for
    /// `{{ backend.port }}` in `base_url` and in the `[docker]` lists.
    ///
    /// Exists so the port is declared ONCE. Written twice — in `-p` and in
    /// `base_url` — a divergence is invisible to `npu doctor` (its probe
    /// reaches whatever answers on the `base_url` port, possibly another
    /// backend) and only surfaces as exit `3` at execution.
    #[serde(default)]
    pub port: Option<Port>,
    /// Optional: how `npu serve` starts this backend's runtime, tagged form.
    ///
    /// Read through [`Backend::runtime`] only: a legacy `[docker]` backend
    /// is normalized into this field at load time, so this is the single
    /// shape every reader sees.
    ///
    /// `pub(crate)`, unlike every other field: the "read through the
    /// accessor" rule above is the whole point of the normalization, and a
    /// `pub` field would leave it to a doc comment nobody is forced to
    /// read. Serde is indifferent to field visibility.
    #[serde(default)]
    pub(crate) runtime: Option<Runtime>,
    /// Deprecated: the untagged `[docker]` table of earlier versions.
    ///
    /// Still DECLARED rather than dropped, for two reasons: removing it
    /// would make every existing backend file fail on
    /// `deny_unknown_fields`, and declaring it is what lets a file carrying
    /// both forms be rejected with its own message instead of being
    /// diagnosed as an unknown key. `normalize_runtime` empties it at load
    /// time; nothing downstream ever reads it. `pub(crate)` for the same
    /// reason as `runtime`.
    #[serde(default)]
    pub(crate) docker: Option<Docker>,
    /// Optional: overrides `backend::REQUEST_TIMEOUT` for this backend.
    #[serde(default)]
    pub timeouts: Option<Timeouts>,
    /// The file this backend was loaded from, filled in by [`load_scopes`]
    /// once the merge picked a winner.
    ///
    /// Not a configuration key: `#[serde(skip)]` and NOT `#[serde(default)]`,
    /// so a file spelling `source = "..."` is rejected by
    /// `deny_unknown_fields` instead of forging the value. It is the
    /// discriminator [`crate::runtime::state`] records, because backend
    /// identifiers are per-scope (`./.npu` is a documented scope) while the
    /// state directory is machine-global: two projects naming a backend
    /// `llamacpp` would otherwise share one record, and `npu stop` in one
    /// would signal the other's server.
    #[serde(skip)]
    pub source: PathBuf,
}

impl Backend {
    /// How this backend's runtime is started, in its normalized form.
    ///
    /// `None` means the backend was never told how to start anything — a
    /// perfectly valid backend, just one `npu serve` cannot act on.
    #[must_use]
    pub fn runtime(&self) -> Option<&Runtime> {
        self.runtime.as_ref()
    }

    /// Does this backend let Docker allocate its port?
    ///
    /// Its `base_url` then still carries `{{ backend.port }}` after loading,
    /// and only `builtin::resolve_base_url` can complete it.
    #[must_use]
    pub fn uses_auto_port(&self) -> bool {
        matches!(&self.port, Some(Port::Keyword(keyword)) if keyword == PORT_AUTO)
    }
}

/// Optional generation parameters for a model.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Generation {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

/// A model configured in `models/*.toml`, referencing a backend.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub id: String,
    pub backend: String,
    pub operation: String,
    pub model: String,
    /// Model to retry against when this one fails with `Error::Backend`.
    ///
    /// Exists because an NPU-compiled graph has a static maximum prompt
    /// length: OVMS answers `400 ... Input length exceeds the maximum
    /// allowed length` in milliseconds, which is an exact, cheap signal that
    /// the same prompt belongs on a GPU-served model instead. The retry is
    /// SINGLE HOP: the fallback's own `fallback` is not followed, which is
    /// what makes cycle detection unnecessary.
    pub fallback: Option<String>,
    #[serde(default)]
    pub generation: Generation,
}

/// Only backend type supported so far.
///
/// A backend declaring a different `type` would, absent validation, be
/// silently treated as `openai-compatible` by `backend.rs`: we reject it at
/// load time rather than ignore the value (cf. review L3).
const SUPPORTED_BACKEND_KIND: &str = "openai-compatible";

/// Only HTTP method supported in phase 1: `backend.rs` hardcodes
/// `client.post()`. A different `Operation.method`
/// would therefore be silently ignored without this validation (cf. review
/// L3).
const SUPPORTED_METHOD: &str = "POST";

/// The only `{{ args.<name> }}` placeholder a `[runtime]` template may
/// reference: `npu serve` takes a model identifier and nothing else, so
/// `model` is the only value it can substitute. Any other name is a
/// configuration error rather than an empty string silently handed to the
/// runtime.
const RUNTIME_PLACEHOLDER_ARG: &str = "model";

/// Largest `[runtime].startup_timeout_secs` a process backend may declare:
/// one day. Not a taste question — `Instant + Duration` panics on a value
/// near `u64::MAX`, and a panic replaces the exit-code contract with exit
/// `101`. A bound rejected at load time naming the file is the same remedy
/// the zero case gets.
const MAX_STARTUP_TIMEOUT_SECS: u64 = 24 * 60 * 60;

/// Validates one `[runtime]` template, whatever the family it belongs to:
/// `{{ input }}` has no meaning here (`npu serve` reads no input) and
/// `{{ args.<name> }}` is restricted to [`RUNTIME_PLACEHOLDER_ARG`].
/// `{{ env.NAME }}` passes: it is resolved at `serve` time, against the
/// environment injected by the caller.
///
/// One validator for every family rather than one per family: the rule is
/// about what `npu serve` can resolve, which does not depend on whether the
/// template ends up in a `docker run` line or in a child process's argument
/// vector. The message names `[runtime]` for the same reason — a Docker
/// backend spelling the legacy `[docker]` table has already been folded into
/// `runtime` by the time this runs.
fn validate_runtime_template(template: &str, backend_id: &str, source: &Path) -> crate::Result<()> {
    // Wrapped, not propagated: `prompt::placeholders` diagnoses the FORM of
    // a placeholder and knows nothing about files, so its own message names
    // neither the offending file nor the backend — while the arms below,
    // about the MEANING of a well-formed one, name both. A scope holding
    // several backend files would otherwise tell its reader that a
    // placeholder is wrong without saying which file to open.
    let found = crate::prompt::placeholders(template).map_err(|err| {
        // Its own message, not its `Display`: the latter re-prefixes
        // "configuration error:", which the caller already prints.
        let detail = match err {
            crate::Error::Config(message) => message,
            other => other.to_string(),
        };
        crate::Error::Config(format!(
            "{}: backend \"{backend_id}\": [runtime]: {detail}",
            source.display()
        ))
    })?;

    for placeholder in found {
        match placeholder {
            crate::prompt::Placeholder::Env(_) => {}
            crate::prompt::Placeholder::Arg(name) if name == RUNTIME_PLACEHOLDER_ARG => {}
            crate::prompt::Placeholder::Arg(name) => {
                return Err(crate::Error::Config(format!(
                    "{}: backend \"{backend_id}\": [runtime] references unknown argument \
                     \"{name}\" (only \"{RUNTIME_PLACEHOLDER_ARG}\" is available here)",
                    source.display()
                )));
            }
            crate::prompt::Placeholder::Input => {
                return Err(crate::Error::Config(format!(
                    "{}: backend \"{backend_id}\": [runtime] references {{{{ input }}}}, which \
                     has no meaning for a runtime start",
                    source.display()
                )));
            }
        }
    }
    Ok(())
}

/// Characters Docker accepts in a container name (`[a-zA-Z0-9][a-zA-Z0-9_.-]*`).
/// `npu serve` derives the container name from the backend identifier, which
/// is a free-form TOML string: an identifier outside this set is rejected at
/// load time, naming its file, rather than transformed silently or handed to
/// `docker run` to fail on its own terms.
///
/// The crate's SINGLE identifier predicate, hence `pub(crate)`: the same set
/// is what makes an identifier safe as a file name in `runtime::state` (it
/// excludes the empty string, `/`, `.` and `..` by construction), and two
/// predicates would be two chances to disagree about what a backend may be
/// called.
pub(crate) fn is_valid_runtime_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

/// Validates the optional `[docker]` table of a backend: the identifier must
/// be usable as a container name, and every list entry must only reference
/// placeholders `serve` can actually resolve.
fn validate_docker(backend: &Backend, source: &Path) -> crate::Result<()> {
    let Some(docker) = docker_of(backend) else {
        return Ok(());
    };

    if !is_valid_runtime_id(&backend.id) {
        return Err(crate::Error::Config(format!(
            "{}: backend \"{}\": declaring [docker] requires an identifier usable as a \
             container name (ASCII letters, digits, \"_\", \".\" and \"-\", starting with a \
             letter or a digit)",
            source.display(),
            backend.id
        )));
    }

    for template in docker
        .options
        .iter()
        .chain(docker.args.iter())
        .chain(std::iter::once(&docker.image))
    {
        validate_runtime_template(template, &backend.id, source)?;
    }

    Ok(())
}

/// Validates a `[runtime] type = "process"` table: the identifier must be
/// usable as a state file name, the startup budget must be able to elapse,
/// and every template must only reference placeholders `serve` can resolve.
///
/// The identifier check is [`is_valid_runtime_id`], the same predicate the
/// Docker family uses for a container name: `runtime::state` derives this
/// backend's state file name from the identifier, so the two families
/// constrain it for different reasons but with one rule.
fn validate_process(backend: &Backend, source: &Path) -> crate::Result<()> {
    let Some(process) = process_of(backend) else {
        return Ok(());
    };

    // Refused on Windows rather than half-honoured. The state directory this
    // family needs is derived from `$XDG_STATE_HOME`/`$HOME`, two variables
    // that platform does not define, and `stop`'s graceful step is a
    // `SIGTERM` `sysinfo` reports as unsupported there — so the escalation
    // collapses to an immediate hard kill with no chance to flush. Left
    // alone, the first symptom is exit `1` ("neither $XDG_STATE_HOME nor
    // $HOME is set"), the I/O code, which tells a calling program its disk
    // is broken. Exit `2` naming the file says what is actually true.
    if cfg!(target_os = "windows") {
        return Err(crate::Error::Config(format!(
            "{}: backend \"{}\": [runtime] type = \"{}\" is not supported on Windows (no state \
             directory convention, and no SIGTERM to stop a server with) — use type = \"{}\", \
             or start the server outside npu",
            source.display(),
            backend.id,
            crate::runtime::process::NAME,
            crate::runtime::docker::NAME
        )));
    }

    // A `base_url` the readiness probe can never parse is a defect of this
    // file, and it is only this family that turns it into a kill: `serve`
    // spawns the server, probes an address that cannot resolve for the whole
    // `startup_timeout_secs`, then SIGTERM/SIGKILLs a perfectly working
    // process and blames the budget. Rejected here, it is exit `2` naming
    // the file, before anything is started.
    if let Err(detail) = crate::builtin::parse_host_port(&backend.base_url) {
        return Err(crate::Error::Config(format!(
            "{}: backend \"{}\": {detail} — a process runtime is started only once its \
             base_url answers, so an address npu cannot parse can never be satisfied",
            source.display(),
            backend.id
        )));
    }

    if !is_valid_runtime_id(&backend.id) {
        return Err(crate::Error::Config(format!(
            "{}: backend \"{}\": declaring a process runtime requires an identifier usable as a \
             state file name (ASCII letters, digits, \"_\", \".\" and \"-\", starting with a \
             letter or a digit)",
            source.display(),
            backend.id
        )));
    }

    // Same rule as `[timeouts].request_secs`: a budget of zero is honoured
    // literally, so `serve` could never succeed — and a key read then
    // clamped would be a key silently ignored.
    if process.startup_timeout_secs == 0 {
        return Err(crate::Error::Config(format!(
            "{}: backend \"{}\": [runtime].startup_timeout_secs must be greater than 0 — \
             a zero budget leaves no time for anything to start",
            source.display(),
            backend.id
        )));
    }

    // And an upper bound, for a reason the zero check shares: `serve` builds
    // its deadline as `Instant::now() + Duration::from_secs(value)`, which
    // PANICS on a large one — exit `101` and a panic message, in place of
    // the exit-code contract this CLI is driven by. A day is already longer
    // than any start worth waiting for.
    if process.startup_timeout_secs > MAX_STARTUP_TIMEOUT_SECS {
        return Err(crate::Error::Config(format!(
            "{}: backend \"{}\": [runtime].startup_timeout_secs must be at most \
             {MAX_STARTUP_TIMEOUT_SECS} (a day); npu cannot build a deadline out of \
             {}",
            source.display(),
            backend.id,
            process.startup_timeout_secs
        )));
    }

    for template in process
        .arguments
        .iter()
        .chain(process.env.values())
        .chain(std::iter::once(&process.command))
    {
        validate_runtime_template(template, &backend.id, source)?;
    }

    Ok(())
}

/// Validates that a loaded backend only declares properties honored in
/// phase 1: `type = "openai-compatible"` and `method = "POST"` for each of
/// its operations. Any other value is a configuration error detected at
/// load time, not a feature to implement.
///
/// Runs AFTER scope merging (cf. `load_scopes`), on surviving entries only:
/// an invalid backend from a general scope, entirely replaced by a more
/// local scope, must never reach this function (cf. review L3, phase 2).
/// `source` is the path of the file the surviving entry comes from, so that
/// the message names the file the user must actually fix.
fn validate_backend(backend: &Backend, source: &Path) -> crate::Result<()> {
    if backend.kind != SUPPORTED_BACKEND_KIND {
        return Err(crate::Error::Config(format!(
            "{}: backend \"{}\": type \"{}\" not supported (only \"{SUPPORTED_BACKEND_KIND}\" \
             is supported in phase 1)",
            source.display(),
            backend.id,
            backend.kind
        )));
    }

    for (operation_name, operation) in &backend.operations {
        if !operation.method.eq_ignore_ascii_case(SUPPORTED_METHOD) {
            return Err(crate::Error::Config(format!(
                "{}: backend \"{}\", operation \"{operation_name}\": method \"{}\" not \
                 supported (only \"{SUPPORTED_METHOD}\" is supported in phase 1)",
                source.display(),
                backend.id,
                operation.method
            )));
        }
    }

    if let Some(timeouts) = &backend.timeouts
        && timeouts.request_secs == 0
    {
        return Err(crate::Error::Config(format!(
            "{}: backend \"{}\": [timeouts].request_secs must be greater than 0",
            source.display(),
            backend.id
        )));
    }

    validate_docker(backend, source)?;
    validate_process(backend, source)?;

    Ok(())
}

/// Validates a model's `fallback`: it must name another loaded model.
///
/// Pointing at itself is rejected too — the retry is single hop, so a
/// self-fallback is an infinite intent expressed as a no-op, never what the
/// author meant.
fn validate_fallback(
    model: &Model,
    models: &HashMap<String, (Model, PathBuf)>,
    source: &Path,
) -> crate::Result<()> {
    let Some(fallback) = &model.fallback else {
        return Ok(());
    };

    if fallback == &model.id {
        return Err(crate::Error::Config(format!(
            "{}: model \"{}\" declares itself as its own fallback",
            source.display(),
            model.id
        )));
    }

    if !models.contains_key(fallback) {
        return Err(crate::Error::Config(format!(
            "{}: model \"{}\" declares fallback \"{fallback}\", which is not a known model \
             (available models: {})",
            source.display(),
            model.id,
            crate::error::format_available(models.keys())
        )));
    }

    Ok(())
}

/// Resolved configuration: backends and models indexed by their `id`.
#[derive(Debug, Default)]
pub struct Config {
    pub backends: HashMap<String, Backend>,
    pub models: HashMap<String, Model>,
}

/// Loads and deserializes each `*.toml` file in `dir`, indexed by the `id`
/// key that `key_of` produces from the deserialized value. Each entry is
/// paired with the path of the file it comes from, so that the most local
/// scope can carry its origin through to the semantic validation that runs
/// after the merge (cf. `load_scopes`).
///
/// A missing directory produces an empty `HashMap` (this is not an error).
/// An unreadable file or invalid TOML produces an `Error::Config` that
/// mentions the path of the offending file — this PARSING error remains
/// fatal in every scope, unlike semantic validation: before `key_of` could
/// be called, the entry's identity (and thus the question "is it
/// shadowed?") is not knowable (cf. review L3, phase 2).
///
/// Two files in `dir` declaring the same `id` are a configuration
/// ambiguity, not an intention (cf. review L3): `Error::Config` names the
/// duplicated identifier and both file paths involved. The directory's
/// entries are sorted before reading so that this diagnostic (which file is
/// "the first", which file is "the duplicate") is deterministic rather than
/// dependent on filesystem order.
///
/// This constraint only holds within `dir`: `load_scopes` merges several
/// calls to this function (one per scope), where replacement by `id` is
/// precisely the requested feature, not an ambiguity.
fn load_toml_dir<T, F>(dir: &Path, key_of: F) -> crate::Result<HashMap<String, (T, PathBuf)>>
where
    T: for<'de> Deserialize<'de>,
    F: Fn(&T) -> String,
{
    let mut out: HashMap<String, (T, PathBuf)> = HashMap::new();
    let mut sources: HashMap<String, PathBuf> = HashMap::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => {
            return Err(crate::Error::Config(format!(
                "cannot read directory {}: {err}",
                dir.display()
            )));
        }
    };

    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| {
            crate::Error::Config(format!(
                "cannot read a directory entry from {}: {err}",
                dir.display()
            ))
        })?;
        let path = entry.path();
        if path.extension().and_then(std::ffi::OsStr::to_str) == Some("toml") {
            paths.push(path);
        }
    }
    paths.sort_unstable();

    for path in paths {
        let contents = std::fs::read_to_string(&path).map_err(|err| {
            crate::Error::Config(format!("cannot read file {}: {err}", path.display()))
        })?;
        let value: T = toml::from_str(&contents).map_err(|err| {
            crate::Error::Config(format!("invalid TOML in {}: {err}", path.display()))
        })?;
        let key = key_of(&value);

        if let Some(previous_path) = sources.get(&key) {
            return Err(crate::Error::Config(format!(
                "identifier \"{key}\" defined multiple times in the same scope: {} and {} \
                 (within a single scope, each identifier must be unique; across scopes, \
                 redefinition is the expected feature)",
                previous_path.display(),
                path.display()
            )));
        }

        sources.insert(key.clone(), path.clone());
        out.insert(key, (value, path));
    }

    Ok(out)
}

/// Loads `<root>/backends/*.toml` and `<root>/models/*.toml`: a simple
/// single scope, in `load_scopes` terms. This is not duplicated into a
/// separate loading path: single-scope and multi-scope loading can
/// therefore never diverge on when semantic validation runs (cf. review L3,
/// phase 2).
///
/// The key of each table is the file's `id` field.
pub fn load(root: &Path) -> crate::Result<Config> {
    load_scopes(&[root.to_path_buf()])
}

/// Loads and merges several configuration scopes.
///
/// `roots` must be ordered from most general to most local — this is the
/// order `scope::roots()` produces. Each root is loaded with
/// `load_toml_dir`, then merged in the order received: for both `backends`
/// and `models`, an entry from a more local scope entirely replaces the one
/// with the same `id` coming from a more general scope (`HashMap::extend`
/// applied in root order — no field-by-field merge; a field missing from a
/// local redefinition is not inherited from the general scope).
///
/// Semantic validation (`validate_backend`) runs AFTER this merge, and only
/// on surviving entries: an invalid backend from a general scope, entirely
/// shadowed by a more local scope, must never make loading fail (cf. review
/// L3, phase 2 — unlike a TOML PARSING error, which remains fatal in every
/// scope since the identity of an unreadable file is not knowable, and thus
/// neither is whether it is shadowed: cf. `load_toml_dir`).
///
/// A root in `roots` that does not exist on disk is not an error:
/// `load_toml_dir` already returns empty tables in that case (a `NotFound`
/// directory is treated as empty), so nothing special is needed here. An
/// empty `roots` list produces an empty `Config`.
pub fn load_scopes(roots: &[PathBuf]) -> crate::Result<Config> {
    let mut backends: HashMap<String, (Backend, PathBuf)> = HashMap::new();
    let mut models: HashMap<String, (Model, PathBuf)> = HashMap::new();

    for root in roots {
        let scope_backends = load_toml_dir::<Backend, _>(&root.join("backends"), |b| b.id.clone())?;
        backends.extend(scope_backends);
        let scope_models = load_toml_dir::<Model, _>(&root.join("models"), |m| m.id.clone())?;
        models.extend(scope_models);
    }

    // Before `validate_backend`: substituting `{{ backend.port }}` here means
    // everything downstream — the docker template whitelist included — only
    // ever sees a resolved value.
    for (backend, source) in backends.values_mut() {
        // The origin, kept rather than dropped with the pair below: it is
        // what tells two same-named backends of two different projects apart
        // in the machine-global state directory. Canonicalized so the same
        // file reached through a symlink or a different spelling still
        // produces one value; the file was just read, so the fallback is
        // only there for a filesystem that refuses the call.
        backend.source = std::fs::canonicalize(&*source).unwrap_or_else(|_| source.clone());
        // Normalization first: `resolve_port` and everything after it read
        // the runtime, never the legacy table, so the fold must already have
        // happened. The double declaration is caught just before, while both
        // halves still exist (cf. `reject_double_runtime`).
        reject_double_runtime(backend, source)?;
        normalize_runtime(backend);
        resolve_port(backend, source)?;
    }

    for (backend, source) in backends.values() {
        validate_backend(backend, source)?;
    }

    // A `fallback` is a COLD path: left unchecked it would only fail the day
    // the primary model fails, i.e. exactly when the recovery is needed.
    // Checked here, after the merge, so that a fallback declared in a general
    // scope and satisfied by a model from a more local scope stays valid.
    for (model, source) in models.values() {
        validate_fallback(model, &models, source)?;
    }

    let backends = backends.into_iter().map(|(id, (b, _))| (id, b)).collect();
    let models = models.into_iter().map(|(id, (m, _))| (id, m)).collect();

    Ok(Config { backends, models })
}

impl Config {
    /// Resolves a model identifier to the corresponding `(Model, Backend)` pair.
    ///
    /// Returns `Error::Config` if the model is unknown, or if its backend does
    /// not exist; in both cases the message lists the available identifiers.
    pub fn resolve(&self, model_id: &str) -> crate::Result<(&Model, &Backend)> {
        let Some(model) = self.models.get(model_id) else {
            return Err(crate::Error::Config(format!(
                "unknown model: \"{model_id}\" (available models: {})",
                crate::error::format_available(self.models.keys())
            )));
        };

        let Some(backend) = self.backends.get(&model.backend) else {
            return Err(crate::Error::Config(format!(
                "unknown backend: \"{}\" (referenced by model \"{model_id}\", available backends: {})",
                model.backend,
                crate::error::format_available(self.backends.keys())
            )));
        };

        Ok((model, backend))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // tolerated in tests (cf. Cargo.toml [lints.clippy]).
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Creates a unique fixture directory under `target/`, so as not to
    /// pollute the repo or collide between tests run in parallel.
    fn fixture_dir(name: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!("config-{name}-{n}"));
        std::fs::create_dir_all(&dir).expect("fixture directory creation");
        dir
    }

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent directory creation");
        }
        std::fs::write(path, contents).expect("fixture write");
    }

    /// The `[docker]` table of a LOADED backend.
    ///
    /// Goes through `runtime()` and not the `docker` field: the legacy table
    /// is folded into `runtime` at load time, so `runtime()` is the only
    /// shape that survives loading — whichever form the file declared.
    fn docker_table(backend: &Backend) -> &Docker {
        docker_of(backend).expect("a Docker runtime must be kept")
    }

    /// Its twin for the process family.
    fn process_table(backend: &Backend) -> &Process {
        process_of(backend).expect("a process runtime must be kept")
    }

    /// Writes a backend declaring `port = <port_value>` (raw TOML), whose
    /// `base_url` and `[docker]` both read `{{ backend.port }}`.
    fn write_port_backend(root: &Path, port_value: &str) {
        write(
            root,
            "backends/b.toml",
            &format!(
                r#"
                id = "b"
                base_url = "http://127.0.0.1:{{{{ backend.port }}}}"
                type = "openai-compatible"
                port = {port_value}

                [operations.chat]
                method = "POST"
                path = "/v1/chat/completions"

                [docker]
                image = "img"
                options = ["-p", "{{{{ backend.port }}}}:8000"]
                "#
            ),
        );
    }

    #[test]
    fn port_number_is_substituted_in_base_url_and_docker() {
        let root = fixture_dir("port-fixed");
        write_port_backend(&root, "8001");

        let config = load(&root).expect("a declared port must load");
        let backend = config.backends.get("b").expect("backend b");

        assert_eq!(backend.base_url, "http://127.0.0.1:8001");
        assert_eq!(
            docker_table(backend).options,
            vec!["-p".to_string(), "8001:8000".to_string()]
        );
    }

    #[test]
    fn port_auto_leaves_base_url_templated_and_asks_docker_for_an_ephemeral_port() {
        let root = fixture_dir("port-auto");
        write_port_backend(&root, "\"auto\"");

        let config = load(&root).expect("port = \"auto\" must load");
        let backend = config.backends.get("b").expect("backend b");

        assert!(backend.uses_auto_port());
        // Still templated: the value does not exist until Docker allocates
        // it, and `builtin::resolve_base_url` reads it back.
        assert_eq!(backend.base_url, "http://127.0.0.1:{{ backend.port }}");
        // `-p 0:8000` is how Docker is asked to allocate a free one.
        assert_eq!(
            docker_table(backend).options[1],
            format!("{DOCKER_EPHEMERAL_PORT}:8000")
        );
    }

    /// The same `port = "auto"` machinery, reached through the TAGGED form
    /// instead of the legacy one — no normalization hop involved. Both paths
    /// must substitute the ephemeral port in `[runtime]` and leave the
    /// `base_url` templated, or `auto` silently works for one spelling only.
    #[test]
    fn port_auto_works_through_the_tagged_runtime_table_too() {
        let root = fixture_dir("port-auto-tagged");
        write(
            &root,
            "backends/b.toml",
            r#"
            id = "b"
            base_url = "http://127.0.0.1:{{ backend.port }}"
            type = "openai-compatible"
            port = "auto"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"

            [runtime]
            type = "docker"
            image = "img"
            options = ["-p", "{{ backend.port }}:8000"]
            "#,
        );

        let config = load(&root).expect("a tagged runtime must support port = \"auto\"");
        let backend = config.backends.get("b").expect("backend b");

        assert!(backend.uses_auto_port());
        assert_eq!(backend.base_url, "http://127.0.0.1:{{ backend.port }}");
        assert_eq!(
            docker_table(backend).options[1],
            format!("{DOCKER_EPHEMERAL_PORT}:8000")
        );
    }

    /// Symmetric to the legacy case: the rejection must not be tied to the
    /// spelling the author used.
    #[test]
    fn port_auto_whose_tagged_runtime_ignores_the_placeholder_is_rejected() {
        let root = fixture_dir("port-auto-tagged-unpublished");
        write(
            &root,
            "backends/b.toml",
            r#"
            id = "b"
            base_url = "http://127.0.0.1:{{ backend.port }}"
            type = "openai-compatible"
            port = "auto"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"

            [runtime]
            type = "docker"
            image = "img"
            options = ["--rm"]
            "#,
        );

        let err = load(&root).expect_err("an auto port nothing publishes must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("b.toml"), "got: {err}");
    }

    #[test]
    fn port_auto_without_a_docker_table_is_rejected() {
        let root = fixture_dir("port-auto-no-docker");
        write(
            &root,
            "backends/b.toml",
            r#"
            id = "b"
            base_url = "http://127.0.0.1:{{ backend.port }}"
            type = "openai-compatible"
            port = "auto"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"
            "#,
        );

        let err = load(&root).expect_err("auto without [docker] must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("b.toml"), "got: {err}");
    }

    #[test]
    fn port_auto_whose_base_url_ignores_the_placeholder_is_rejected() {
        let root = fixture_dir("port-auto-fixed-url");
        write(
            &root,
            "backends/b.toml",
            r#"
            id = "b"
            base_url = "http://127.0.0.1:8001"
            type = "openai-compatible"
            port = "auto"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"

            [docker]
            image = "img"
            options = ["-p", "{{ backend.port }}:8000"]
            "#,
        );

        let err = load(&root).expect_err("an auto port no base_url reads must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("b.toml"), "got: {err}");
    }

    /// Symmetric to the `base_url` case: with no `-p {{ backend.port }}`
    /// the container publishes nothing, so `docker port` has nothing to
    /// report and every command fails on advice that could never work.
    #[test]
    fn port_auto_whose_docker_table_ignores_the_placeholder_is_rejected() {
        let root = fixture_dir("port-auto-unpublished");
        write(
            &root,
            "backends/b.toml",
            r#"
            id = "b"
            base_url = "http://127.0.0.1:{{ backend.port }}"
            type = "openai-compatible"
            port = "auto"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"

            [docker]
            image = "img"
            options = ["--rm"]
            "#,
        );

        let err = load(&root).expect_err("an auto port nothing publishes must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("b.toml"), "got: {err}");
    }

    #[test]
    fn port_placeholder_without_the_key_is_rejected() {
        let root = fixture_dir("port-missing-key");
        write(
            &root,
            "backends/b.toml",
            r#"
            id = "b"
            base_url = "http://127.0.0.1:{{ backend.port }}"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"
            "#,
        );

        let err = load(&root).expect_err("an unresolvable placeholder must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("b.toml"), "got: {err}");
    }

    #[test]
    fn port_key_never_referenced_is_rejected() {
        let root = fixture_dir("port-unused");
        write(
            &root,
            "backends/b.toml",
            r#"
            id = "b"
            base_url = "http://127.0.0.1:8001"
            type = "openai-compatible"
            port = 8001

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"
            "#,
        );

        let err = load(&root).expect_err("a port nothing reads must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("b.toml"), "got: {err}");
    }

    #[test]
    fn port_keyword_other_than_auto_is_rejected() {
        let root = fixture_dir("port-bad-keyword");
        write_port_backend(&root, "\"random\"");

        let err = load(&root).expect_err("an unknown port keyword must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("b.toml"), "got: {message}");
        assert!(message.contains("random"), "got: {message}");
    }

    #[test]
    fn port_zero_is_rejected() {
        let root = fixture_dir("port-zero");
        write_port_backend(&root, "0");

        let err = load(&root).expect_err("port 0 must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("b.toml"), "got: {err}");
    }

    #[test]
    fn substitute_port_leaves_other_placeholders_alone() {
        let (out, hit) = substitute_port("{{ args.model }}:{{backend.port}}", 8001);
        assert_eq!(out, "{{ args.model }}:8001");
        assert!(hit);

        let (out, hit) = substitute_port("{{ args.model }}", 8001);
        assert_eq!(out, "{{ args.model }}");
        assert!(!hit);
    }

    /// The scan skips a non-matching placeholder by advancing past its `{{`
    /// only, so its body is re-scanned. This is the case that would expose a
    /// mis-paired `}}`: a `{{ env.NAME }}` BEFORE the port, in the same
    /// string — the exact shape of a volume option in a real `[docker]`
    /// table.
    #[test]
    fn substitute_port_after_another_placeholder_in_the_same_string() {
        let (out, hit) = substitute_port("{{ env.HOME }}/m:{{ backend.port }}:8000", 8001);
        assert_eq!(out, "{{ env.HOME }}/m:8001:8000");
        assert!(hit);

        // And the detection used by `resolve_port` to reject a placeholder
        // with no `port` key must see it in that same position.
        assert!(substitute_port("{{ env.HOME }}:{{ backend.port }}", 0).1);
    }

    /// An unclosed `{{` must not swallow the rest, matching `prompt.rs`'s own
    /// rule that an unclosed placeholder is left alone.
    #[test]
    fn substitute_port_leaves_an_unclosed_brace_alone() {
        let (out, hit) = substitute_port("{{ backend.port", 8001);
        assert_eq!(out, "{{ backend.port");
        assert!(!hit);
    }

    /// The three `fallback` tests share this backend: only the models vary.
    const FALLBACK_BACKEND: &str = r#"
        id = "b"
        base_url = "http://127.0.0.1:8000"
        type = "openai-compatible"

        [operations.chat]
        method = "POST"
        path = "/v1/chat/completions"
    "#;

    #[test]
    fn fallback_pointing_at_a_known_model_loads() {
        let root = fixture_dir("fallback-ok");
        write(&root, "backends/b.toml", FALLBACK_BACKEND);
        write(
            &root,
            "models/npu.toml",
            r#"
            id = "npu"
            backend = "b"
            operation = "chat"
            model = "m-npu"
            fallback = "gpu"
            "#,
        );
        write(
            &root,
            "models/gpu.toml",
            r#"
            id = "gpu"
            backend = "b"
            operation = "chat"
            model = "m-gpu"
            "#,
        );

        let config = load(&root).expect("a fallback naming a loaded model must load");
        assert_eq!(
            config.models.get("npu").and_then(|m| m.fallback.as_deref()),
            Some("gpu")
        );
    }

    #[test]
    fn fallback_pointing_at_an_unknown_model_is_rejected_naming_both() {
        let root = fixture_dir("fallback-unknown");
        write(&root, "backends/b.toml", FALLBACK_BACKEND);
        write(
            &root,
            "models/npu.toml",
            r#"
            id = "npu"
            backend = "b"
            operation = "chat"
            model = "m-npu"
            fallback = "nope"
            "#,
        );

        let err = load(&root).expect_err("an unknown fallback must be rejected at load time");
        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("npu.toml"), "got: {message}");
        assert!(message.contains("nope"), "got: {message}");
    }

    #[test]
    fn fallback_pointing_at_itself_is_rejected() {
        let root = fixture_dir("fallback-self");
        write(&root, "backends/b.toml", FALLBACK_BACKEND);
        write(
            &root,
            "models/npu.toml",
            r#"
            id = "npu"
            backend = "b"
            operation = "chat"
            model = "m-npu"
            fallback = "npu"
            "#,
        );

        let err = load(&root).expect_err("a self-fallback must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("npu.toml"), "got: {err}");
    }

    #[test]
    fn resolve_returns_model_and_backend() {
        let root = fixture_dir("resolve-ok");
        write(
            &root,
            "backends/openai.toml",
            r#"
            id = "openai"
            base_url = "https://api.openai.com"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"
            "#,
        );
        write(
            &root,
            "models/gpt.toml",
            r#"
            id = "gpt"
            backend = "openai"
            operation = "chat"
            model = "gpt-4o"
            "#,
        );

        let config = load(&root).expect("config loading");
        let (model, backend) = config.resolve("gpt").expect("model resolution");
        assert_eq!(model.id, "gpt");
        assert_eq!(backend.id, "openai");
        assert_eq!(backend.kind, "openai-compatible");
    }

    #[test]
    fn resolve_unknown_model_lists_available_ids() {
        let root = fixture_dir("resolve-unknown-model");
        write(
            &root,
            "backends/openai.toml",
            r#"
            id = "openai"
            base_url = "https://api.openai.com"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"
            "#,
        );
        write(
            &root,
            "models/gpt.toml",
            r#"
            id = "gpt"
            backend = "openai"
            operation = "chat"
            model = "gpt-4o"
            "#,
        );

        let config = load(&root).expect("config loading");
        let err = config.resolve("unknown").expect_err("must fail");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("gpt"));
    }

    #[test]
    fn resolve_missing_backend_lists_available_ids() {
        let root = fixture_dir("resolve-missing-backend");
        write(
            &root,
            "models/gpt.toml",
            r#"
            id = "gpt"
            backend = "absent"
            operation = "chat"
            model = "gpt-4o"
            "#,
        );

        let config = load(&root).expect("config loading");
        let err = config.resolve("gpt").expect_err("must fail");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(msg.contains("absent"));
    }

    #[test]
    fn load_missing_directories_yields_empty_config() {
        let root = fixture_dir("load-missing-dirs");
        let config = load(&root).expect("a missing directory is not an error");
        assert!(config.backends.is_empty());
        assert!(config.models.is_empty());
    }

    #[test]
    fn load_invalid_toml_reports_file_path() {
        let root = fixture_dir("load-invalid-toml");
        write(&root, "backends/broken.toml", "not = [valid");

        let err = load(&root).expect_err("invalid TOML must fail");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("broken.toml"));
    }

    #[test]
    fn load_rejects_backend_operation_with_non_post_method() {
        let root = fixture_dir("reject-non-post-method");
        write(
            &root,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "GET"
            path = "/v3/chat/completions"
            "#,
        );

        let err = load(&root).expect_err("a non-POST method must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(msg.contains("ovms"));
    }

    /// Writes a backend whose runtime block is `runtime_toml` verbatim, so
    /// a test can hand serde any shape it wants to see rejected.
    fn write_runtime_backend(root: &Path, runtime_toml: &str) {
        write(
            root,
            "backends/b.toml",
            &format!(
                r#"
                id = "b"
                base_url = "http://127.0.0.1:8000"
                type = "openai-compatible"

                [operations.chat]
                method = "POST"
                path = "/v1/chat/completions"

                {runtime_toml}
                "#
            ),
        );
    }

    /// The tagged form is the one every reader sees, and it must arrive
    /// there intact.
    #[test]
    fn load_accepts_the_tagged_runtime_table() {
        let root = fixture_dir("runtime-tagged");
        write_runtime_backend(
            &root,
            r#"
            [runtime]
            type = "docker"
            image = "openvino/model_server:latest"
            options = ["-p", "8000:8000"]
            "#,
        );

        let config = load(&root).expect("a tagged [runtime] table must load");
        let docker = docker_table(&config.backends["b"]);
        assert_eq!(docker.image, "openvino/model_server:latest");
        assert_eq!(docker.options.len(), 2);
    }

    /// An unsupported family is NAMED by serde instead of being silently
    /// treated as the only one that exists — the whole reason the enum is
    /// tagged.
    #[test]
    fn load_rejects_an_unknown_runtime_type() {
        let root = fixture_dir("runtime-unknown-type");
        write_runtime_backend(
            &root,
            r#"
            [runtime]
            type = "podman"
            image = "img"
            "#,
        );

        let err = load(&root).expect_err("an unknown runtime type must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("b.toml"), "got: {err}");
        assert!(err.to_string().contains("podman"), "got: {err}");
    }

    /// `deny_unknown_fields` must survive the tagged form: a key read by
    /// nobody is a defect, not a shortcut.
    #[test]
    fn load_rejects_an_unknown_key_inside_the_runtime_table() {
        let root = fixture_dir("runtime-unknown-key");
        write_runtime_backend(
            &root,
            r#"
            [runtime]
            type = "docker"
            image = "img"
            bogus = 1
            "#,
        );

        let err = load(&root).expect_err("an unknown key in [runtime] must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("b.toml"), "got: {err}");
        assert!(err.to_string().contains("bogus"), "got: {err}");
    }

    /// The legacy form keeps working, and lands in the SAME place: nothing
    /// downstream is allowed to care which one was written.
    #[test]
    fn load_normalizes_the_legacy_docker_table_into_the_runtime() {
        let root = fixture_dir("runtime-legacy-normalized");
        write_runtime_backend(
            &root,
            r#"
            [docker]
            image = "img"
            "#,
        );

        let config = load(&root).expect("the legacy [docker] table must still load");
        assert_eq!(docker_table(&config.backends["b"]).image, "img");
    }

    // -- the process runtime family ---------------------------------------

    /// Everything the family reads, in one declaration, arriving intact.
    #[test]
    fn load_accepts_a_process_runtime() {
        let root = fixture_dir("process-tagged");
        write_runtime_backend(
            &root,
            r#"
            [runtime]
            type = "process"
            command = "llama-server"
            arguments = ["--model", "{{ args.model }}"]
            startup_timeout_secs = 90

            [runtime.env]
            LLAMA_CACHE = "/var/cache"
            "#,
        );

        let config = load(&root).expect("a process runtime must load");
        let process = process_table(&config.backends["b"]);
        assert_eq!(process.command, "llama-server");
        assert_eq!(process.arguments, vec!["--model", "{{ args.model }}"]);
        assert_eq!(process.startup_timeout_secs, 90);
        assert_eq!(
            process.env.get("LLAMA_CACHE").map(String::as_str),
            Some("/var/cache")
        );
    }

    /// The optional keys have to have a value the code actually uses, or
    /// the default is a fiction.
    #[test]
    fn a_process_runtime_reduced_to_its_command_gets_the_default_budget() {
        let root = fixture_dir("process-minimal");
        write_runtime_backend(
            &root,
            r#"
            [runtime]
            type = "process"
            command = "llama-server"
            "#,
        );

        let config = load(&root).expect("a command alone must load");
        let process = process_table(&config.backends["b"]);
        assert!(process.arguments.is_empty());
        assert!(process.env.is_empty());
        assert_eq!(process.startup_timeout_secs, DEFAULT_STARTUP_TIMEOUT_SECS);
    }

    #[test]
    fn load_rejects_an_unknown_key_inside_a_process_runtime() {
        let root = fixture_dir("process-unknown-key");
        write_runtime_backend(
            &root,
            r#"
            [runtime]
            type = "process"
            command = "llama-server"
            bogus = 1
            "#,
        );

        let err = load(&root).expect_err("an unknown key must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("b.toml"), "got: {message}");
        assert!(message.contains("bogus"), "got: {message}");
    }

    /// An unsupported family is a NAMED rejection, which is the whole point
    /// of the tag.
    #[test]
    fn load_rejects_an_unknown_runtime_family() {
        let root = fixture_dir("process-unknown-family");
        write_runtime_backend(
            &root,
            r#"
            [runtime]
            type = "podman"
            image = "img"
            "#,
        );

        let err = load(&root).expect_err("an unknown family must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("b.toml"), "got: {message}");
        assert!(message.contains("podman"), "got: {message}");
    }

    /// `{{ backend.port }}` works in the two places the family renders, and
    /// nowhere else: one template engine, not a second one.
    #[test]
    fn a_process_runtime_substitutes_the_port_in_arguments_and_env_values() {
        let root = fixture_dir("process-port");
        write(
            &root,
            "backends/b.toml",
            r#"
            id = "b"
            base_url = "http://127.0.0.1:{{ backend.port }}"
            type = "openai-compatible"
            port = 8081

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"

            [runtime]
            type = "process"
            command = "llama-server"
            arguments = ["--port", "{{ backend.port }}"]

            [runtime.env]
            SERVER_PORT = "{{ backend.port }}"
            "#,
        );

        let config = load(&root).expect("a declared port must load");
        let backend = &config.backends["b"];
        let process = process_table(backend);
        assert_eq!(backend.base_url, "http://127.0.0.1:8081");
        assert_eq!(process.arguments, vec!["--port", "8081"]);
        assert_eq!(
            process.env.get("SERVER_PORT").map(String::as_str),
            Some("8081")
        );
    }

    /// A port read ONLY by the environment overlay still counts as read:
    /// rejecting it would make a legitimate declaration impossible.
    #[test]
    fn a_port_read_only_by_the_environment_overlay_is_not_reported_as_unused() {
        let root = fixture_dir("process-port-env-only");
        write(
            &root,
            "backends/b.toml",
            r#"
            id = "b"
            base_url = "http://127.0.0.1:8081"
            type = "openai-compatible"
            port = 8081

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"

            [runtime]
            type = "process"
            command = "llama-server"

            [runtime.env]
            SERVER_PORT = "{{ backend.port }}"
            "#,
        );

        let config = load(&root).expect("a port read by the overlay is read");
        assert_eq!(
            process_table(&config.backends["b"])
                .env
                .get("SERVER_PORT")
                .map(String::as_str),
            Some("8081")
        );
    }

    /// `port = "auto"` cannot work here: npu has no way to ask a process
    /// which port it ended up on. The configuration is wrong — exit 2, not
    /// a runtime that is merely down.
    #[test]
    fn port_auto_with_a_process_runtime_is_rejected() {
        let root = fixture_dir("process-port-auto");
        write(
            &root,
            "backends/b.toml",
            r#"
            id = "b"
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

        let err = load(&root).expect_err("auto must be refused for a process runtime");
        assert!(matches!(err, crate::Error::Config(_)));
        assert_eq!(err.exit_code(), 2);
        let message = err.to_string();
        assert!(message.contains("b.toml"), "got: {message}");
        assert!(message.contains("\"b\""), "got: {message}");
    }

    #[test]
    fn a_process_runtime_referencing_an_unknown_argument_is_rejected() {
        let root = fixture_dir("process-unknown-arg");
        write_runtime_backend(
            &root,
            r#"
            [runtime]
            type = "process"
            command = "llama-server"
            arguments = ["{{ args.temperature }}"]
            "#,
        );

        let err = load(&root).expect_err("only the model argument is available here");
        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("b.toml"), "got: {message}");
        assert!(message.contains("temperature"), "got: {message}");
    }

    /// `npu serve` reads no input, so the placeholder can never resolve —
    /// in an argument as in an environment value.
    #[test]
    fn a_process_runtime_referencing_the_input_placeholder_is_rejected() {
        let root = fixture_dir("process-input-placeholder");
        write_runtime_backend(
            &root,
            r#"
            [runtime]
            type = "process"
            command = "llama-server"

            [runtime.env]
            PROMPT = "{{ input }}"
            "#,
        );

        let err = load(&root).expect_err("the input placeholder has no meaning for a start");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("b.toml"), "got: {err}");
    }

    /// Honoured literally, a zero budget makes every start fail; clamped, it
    /// would be a key read and silently ignored.
    #[test]
    fn a_zero_startup_budget_is_rejected() {
        let root = fixture_dir("process-zero-budget");
        write_runtime_backend(
            &root,
            r#"
            [runtime]
            type = "process"
            command = "llama-server"
            startup_timeout_secs = 0
            "#,
        );

        let err = load(&root).expect_err("a zero budget must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("b.toml"), "got: {message}");
        assert!(message.contains("startup_timeout_secs"), "got: {message}");
    }

    /// A budget `Instant + Duration` cannot represent PANICS at `serve`
    /// time — exit `101` instead of the exit-code contract. Rejected at load
    /// time naming the file, like the zero case.
    #[test]
    fn an_unrepresentable_startup_budget_is_rejected() {
        let root = fixture_dir("process-huge-budget");
        write_runtime_backend(
            &root,
            &format!(
                r#"
            [runtime]
            type = "process"
            command = "llama-server"
            startup_timeout_secs = {}
            "#,
                u64::MAX
            ),
        );

        let err = load(&root).expect_err("an unrepresentable budget must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert_eq!(err.exit_code(), 2);
        let message = err.to_string();
        assert!(message.contains("b.toml"), "got: {message}");
        assert!(message.contains("startup_timeout_secs"), "got: {message}");
    }

    /// A `base_url` the readiness probe can never parse is a defect of the
    /// FILE: left to `serve`, it burns the whole budget and then kills a
    /// perfectly working server, blaming a timeout key that is correct.
    #[test]
    fn a_process_runtime_whose_base_url_cannot_be_parsed_is_rejected() {
        let root = fixture_dir("process-unparseable-url");
        write(
            &root,
            "backends/b.toml",
            r#"
            id = "b"
            base_url = "127.0.0.1:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"

            [runtime]
            type = "process"
            command = "llama-server"
            "#,
        );

        let err = load(&root).expect_err("an unprobeable base_url must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert_eq!(err.exit_code(), 2);
        let message = err.to_string();
        assert!(message.contains("b.toml"), "got: {message}");
        assert!(message.contains("127.0.0.1:8000"), "got: {message}");
    }

    /// A placeholder whose FORM is not recognized used to be reported by
    /// `prompt` alone, which knows about templates and nothing about files:
    /// its reader was told a placeholder was wrong without being told which
    /// of the scope's backend files to open.
    #[test]
    fn a_malformed_runtime_placeholder_names_its_file_and_backend() {
        let root = fixture_dir("runtime-bad-placeholder");
        write_runtime_backend(
            &root,
            r#"
            [runtime]
            type = "process"
            command = "srv-{{ backend.port }}"
            "#,
        );

        let err = load(&root).expect_err("an unknown placeholder form must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("b.toml"), "got: {message}");
        assert!(message.contains("\"b\""), "got: {message}");
    }

    /// The identifier becomes a state file name, so the same predicate that
    /// guards a container name guards this too.
    #[test]
    fn a_process_runtime_on_an_unusable_identifier_is_rejected() {
        let root = fixture_dir("process-bad-id");
        write(
            &root,
            "backends/b.toml",
            r#"
            id = "../escape"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"

            [runtime]
            type = "process"
            command = "llama-server"
            "#,
        );

        let err = load(&root).expect_err("an identifier that is not a file name must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("b.toml"), "got: {message}");
        assert!(message.contains("../escape"), "got: {message}");
    }

    /// Declaring both forms has no honest resolution: picking a winner would
    /// silently ignore half of what the author wrote.
    #[test]
    fn load_rejects_a_backend_declaring_both_runtime_and_the_legacy_docker_table() {
        let root = fixture_dir("runtime-double-declaration");
        write_runtime_backend(
            &root,
            r#"
            [runtime]
            type = "docker"
            image = "tagged"

            [docker]
            image = "legacy"
            "#,
        );

        let err = load(&root).expect_err("declaring both forms must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("b.toml"), "got: {err}");
        assert!(err.to_string().contains("\"b\""), "got: {err}");
    }

    #[test]
    fn load_accepts_a_backend_declaring_docker() {
        let root = fixture_dir("accept-docker-table");
        write(
            &root,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"

            [docker]
            image = "openvino/model_server:latest"
            options = ["-p", "8000:8000", "-v", "{{ env.HOME }}/models:/models:rw"]
            args = ["--source_model", "{{ args.model }}", "--rest_port", "8000"]
            "#,
        );

        let config = load(&root).expect("a valid [docker] table must load");
        let docker = docker_table(&config.backends["ovms"]);
        assert_eq!(docker.image, "openvino/model_server:latest");
        assert_eq!(docker.options.len(), 4);
        assert_eq!(docker.args.len(), 4);
    }

    #[test]
    fn load_accepts_a_docker_table_reduced_to_its_image() {
        let root = fixture_dir("accept-docker-image-only");
        write(
            &root,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"

            [docker]
            image = "openvino/model_server:latest"
            "#,
        );

        let config = load(&root).expect("[docker] reduced to its image must load");
        let docker = docker_table(&config.backends["ovms"]);
        assert!(docker.options.is_empty());
        assert!(docker.args.is_empty());
    }

    #[test]
    fn load_rejects_docker_referencing_an_unknown_argument() {
        let root = fixture_dir("reject-docker-unknown-arg");
        write(
            &root,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"

            [docker]
            image = "openvino/model_server:latest"
            args = ["--source_model", "{{ args.modle }}"]
            "#,
        );

        let err = load(&root).expect_err("a misspelled argument must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(msg.contains("ovms.toml"), "the message must name the file");
        assert!(
            msg.contains("modle"),
            "the message must name the placeholder"
        );
    }

    #[test]
    fn load_rejects_docker_referencing_the_input() {
        let root = fixture_dir("reject-docker-input");
        write(
            &root,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"

            [docker]
            image = "openvino/model_server:latest"
            options = ["{{ input }}"]
            "#,
        );

        let err = load(&root).expect_err("{{ input }} has no meaning in [docker]");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("ovms.toml"));
    }

    #[test]
    fn load_rejects_docker_on_a_backend_whose_id_is_not_a_container_name() {
        let root = fixture_dir("reject-docker-non-ascii-id");
        write(
            &root,
            "backends/cafe.toml",
            r#"
            id = "café"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"

            [docker]
            image = "openvino/model_server:latest"
            "#,
        );

        let err = load(&root).expect_err("a non-ASCII id cannot name a container");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(msg.contains("cafe.toml"), "the message must name the file");
        assert!(msg.contains("café"), "the message must name the backend");
    }

    #[test]
    fn load_accepts_a_non_ascii_backend_id_without_docker() {
        let root = fixture_dir("accept-non-ascii-id-without-docker");
        write(
            &root,
            "backends/cafe.toml",
            r#"
            id = "café"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let config = load(&root).expect("the container-name constraint only applies to [docker]");
        assert!(config.backends.contains_key("café"));
    }

    #[test]
    fn load_rejects_backend_with_unknown_kind() {
        let root = fixture_dir("reject-unknown-kind");
        write(
            &root,
            "backends/ollama.toml",
            r#"
            id = "ollama"
            base_url = "http://127.0.0.1:11434"
            type = "ollama"

            [operations.chat]
            method = "POST"
            path = "/api/chat"
            "#,
        );

        let err = load(&root).expect_err("an unknown backend type must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(msg.contains("ollama"));
    }

    #[test]
    fn load_rejects_backend_with_unknown_toml_key() {
        let root = fixture_dir("reject-unknown-backend-key");
        write(
            &root,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"
            retries = 3

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let err = load(&root).expect_err("an unknown TOML key on a backend must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn load_rejects_timeouts_with_unknown_toml_key() {
        let root = fixture_dir("reject-unknown-timeouts-key");
        write(
            &root,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [timeouts]
            connect_secs = 5

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let err = load(&root).expect_err("an unknown key inside [timeouts] must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("ovms.toml"));
    }

    #[test]
    fn load_rejects_zero_request_secs() {
        let root = fixture_dir("reject-zero-request-secs");
        write(
            &root,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [timeouts]
            request_secs = 0

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let err = load(&root).expect_err("a zero request_secs must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(msg.contains("ovms.toml"));
        assert!(msg.contains("ovms"));
    }

    #[test]
    fn load_accepts_valid_timeouts() {
        let root = fixture_dir("accept-valid-timeouts");
        write(
            &root,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [timeouts]
            request_secs = 120

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let config = load(&root).expect("a valid [timeouts] table must load");
        let backend = &config.backends["ovms"];
        let timeouts = backend
            .timeouts
            .as_ref()
            .expect("[timeouts] must be present");
        assert_eq!(timeouts.request_secs, 120);
    }

    #[test]
    fn load_rejects_model_with_unknown_toml_key() {
        let root = fixture_dir("reject-unknown-model-key");
        write(
            &root,
            "models/gpt.toml",
            r#"
            id = "gpt"
            backend = "openai"
            operation = "chat"
            model = "gpt-4o"

            [generation]
            temperature = 0.0
            max_tokes = 512
            "#,
        );

        let err = load(&root).expect_err("an unknown TOML key on a model must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn real_npu_fixture_still_parses() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".npu");
        let config = load(&root).expect("the real .npu/ fixture must always load");

        let backend = config
            .backends
            .get("ovms")
            .expect("the ovms backend must be present");
        assert_eq!(backend.kind, "openai-compatible");
        assert!(backend.operations.contains_key("chat"));

        // No model assertion: the project scope declares backends and
        // commands only. Its models live in the user scope since feb76a7 —
        // they are tied to this machine's local `optimum-cli` exports, which
        // nothing in the repository can provide.
    }

    #[test]
    fn load_rejects_duplicate_id_within_same_scope_naming_both_files() {
        let root = fixture_dir("duplicate-id-same-scope");
        write(
            &root,
            "models/a.toml",
            r#"
            id = "qwen-fast"
            backend = "ovms"
            operation = "chat"
            model = "qwen-2.5-1.5b"
            "#,
        );
        write(
            &root,
            "models/b.toml",
            r#"
            id = "qwen-fast"
            backend = "ovms"
            operation = "chat"
            model = "qwen-2.5-3b"
            "#,
        );

        let err = load(&root).expect_err("a duplicate id within the same scope must fail");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(msg.contains("qwen-fast"));
        assert!(msg.contains("a.toml"));
        assert!(msg.contains("b.toml"));
    }

    #[test]
    fn load_scopes_local_backend_replaces_general_one_entirely() {
        let general = fixture_dir("scopes-backend-general");
        write(
            &general,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://general:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"

            [operations.embeddings]
            method = "POST"
            path = "/v3/embeddings"
            "#,
        );

        let local = fixture_dir("scopes-backend-local");
        write(
            &local,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://local:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let config = load_scopes(&[general, local]).expect("scope merging must succeed");
        let backend = config
            .backends
            .get("ovms")
            .expect("the ovms backend must be present");
        assert_eq!(backend.base_url, "http://local:8000");
        // The field absent from the local redefinition (embeddings) must NOT
        // be inherited from the general scope: full replacement, not a
        // field-by-field merge.
        assert!(!backend.operations.contains_key("embeddings"));
        assert!(backend.operations.contains_key("chat"));
    }

    #[test]
    fn load_scopes_local_model_resolves_against_general_scope_backend() {
        let general = fixture_dir("scopes-model-general");
        write(
            &general,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let local = fixture_dir("scopes-model-local");
        write(
            &local,
            "models/qwen-fast.toml",
            r#"
            id = "qwen-fast"
            backend = "ovms"
            operation = "chat"
            model = "qwen-2.5-1.5b"
            "#,
        );

        let config = load_scopes(&[general, local]).expect("scope merging must succeed");
        let (model, backend) = config
            .resolve("qwen-fast")
            .expect("the local model must resolve the general backend");
        assert_eq!(model.id, "qwen-fast");
        assert_eq!(backend.id, "ovms");
    }

    #[test]
    fn load_scopes_nonexistent_root_is_ignored() {
        let existing = fixture_dir("scopes-nonexistent-existing");
        write(
            &existing,
            "models/gpt.toml",
            r#"
            id = "gpt"
            backend = "openai"
            operation = "chat"
            model = "gpt-4o"
            "#,
        );
        let missing = existing.join("does-not-exist");

        let config =
            load_scopes(&[missing, existing]).expect("a missing root must not make the merge fail");
        assert!(config.models.contains_key("gpt"));
    }

    /// `source` names the backend's state file, so two spellings of one
    /// root — a symlink, a `..` detour — must yield one value: otherwise
    /// `stop` run through the other spelling finds no record and orphans
    /// the server `serve` started.
    #[cfg(unix)]
    #[test]
    fn a_backend_source_does_not_depend_on_how_its_root_was_spelled() {
        let real = fixture_dir("source-canonical");
        write(
            &real,
            "backends/local.toml",
            r#"
            id = "local"
            base_url = "http://127.0.0.1:8080"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"
            "#,
        );
        let link = real.with_extension("link");
        drop(std::fs::remove_file(&link));
        std::os::unix::fs::symlink(&real, &link).expect("the symlink");
        let detour = real.join("backends").join("..");

        let sources: Vec<PathBuf> = [real, link, detour]
            .into_iter()
            .map(|root| {
                load_scopes(&[root]).expect("the scope must load").backends["local"]
                    .source
                    .clone()
            })
            .collect();

        assert_eq!(sources[0], sources[1]);
        assert_eq!(sources[0], sources[2]);
    }

    #[test]
    fn load_scopes_empty_roots_yields_empty_config() {
        let config = load_scopes(&[]).expect("an empty root list must succeed");
        assert!(config.backends.is_empty());
        assert!(config.models.is_empty());
    }

    #[test]
    fn load_scopes_precedence_is_general_to_local() {
        let etc = fixture_dir("scopes-precedence-etc");
        write(
            &etc,
            "models/qwen-fast.toml",
            r#"
            id = "qwen-fast"
            backend = "ovms"
            operation = "chat"
            model = "etc-model"
            "#,
        );

        let xdg = fixture_dir("scopes-precedence-xdg");
        write(
            &xdg,
            "models/qwen-fast.toml",
            r#"
            id = "qwen-fast"
            backend = "ovms"
            operation = "chat"
            model = "xdg-model"
            "#,
        );

        let cwd = fixture_dir("scopes-precedence-cwd");
        write(
            &cwd,
            "models/qwen-fast.toml",
            r#"
            id = "qwen-fast"
            backend = "ovms"
            operation = "chat"
            model = "cwd-model"
            "#,
        );

        let config =
            load_scopes(&[etc.clone(), xdg.clone(), cwd.clone()]).expect("the merge must succeed");
        assert_eq!(
            config.models.get("qwen-fast").expect("qwen-fast").model,
            "cwd-model",
            "the most local root (cwd, at the end of the list) must win"
        );

        // A partial order (just etc then xdg, without cwd) must also
        // respect general -> local.
        let config2 = load_scopes(&[etc, xdg]).expect("the two-root merge must succeed");
        assert_eq!(
            config2.models.get("qwen-fast").expect("qwen-fast").model,
            "xdg-model"
        );
    }

    #[test]
    fn load_scopes_invalid_backend_fully_masked_by_local_scope_resolves_successfully() {
        // Review L3 (phase 2): an invalid backend from a general scope,
        // entirely replaced by a valid local scope, must never reach
        // semantic validation.
        let general = fixture_dir("scopes-invalid-backend-masked-general");
        write(
            &general,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://x"
            type = "ollama"

            [operations.chat]
            method = "POST"
            path = "/x"
            "#,
        );

        let local = fixture_dir("scopes-invalid-backend-masked-local");
        write(
            &local,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://local:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let config = load_scopes(&[general, local])
            .expect("the shadowed invalid backend must not prevent resolution");
        let backend = config
            .backends
            .get("ovms")
            .expect("the ovms backend (local version) must be present");
        assert_eq!(backend.kind, "openai-compatible");
        assert_eq!(backend.base_url, "http://local:8000");
    }

    #[test]
    fn load_scopes_invalid_backend_not_masked_still_fails_and_names_its_file() {
        // The reverse case, the easiest to break by fixing the first one: an
        // invalid backend present ONLY in a general scope must always fail,
        // and the message must name its file (not just its id), since
        // validation now runs after the merge on an entry that carries its
        // origin.
        let general = fixture_dir("scopes-invalid-backend-unmasked-general");
        write(
            &general,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://x"
            type = "ollama"

            [operations.chat]
            method = "POST"
            path = "/x"
            "#,
        );

        let err =
            load_scopes(&[general]).expect_err("an unshadowed invalid backend must always fail");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("ovms.toml"),
            "the message must name the offending file, got: {msg}"
        );
    }

    #[test]
    fn load_scopes_toml_parse_error_in_general_scope_remains_fatal_even_when_masked() {
        // Unlike semantic validation, a PARSING error remains fatal in
        // every scope: an unreadable file has no knowable identity, so we
        // cannot know whether it is shadowed (cf. review L3, phase 2). This
        // is not a bug.
        let general = fixture_dir("scopes-parse-error-masked-general");
        write(&general, "backends/ovms.toml", "not = [valid");

        let local = fixture_dir("scopes-parse-error-masked-local");
        write(
            &local,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://local:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let err = load_scopes(&[general, local]).expect_err(
            "a TOML parsing error in a general scope must remain fatal even when a valid id \
             exists locally",
        );
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("ovms.toml"));
    }
}
