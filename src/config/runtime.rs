//! How `npu serve` starts a backend's runtime: the Docker and process
//! families, and the tagged `[runtime]` enum that dispatches between them.

use serde::Deserialize;

use super::backend::Backend;

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
pub(crate) const DEFAULT_STARTUP_TIMEOUT_SECS: u64 = 30;

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
/// [`super::port::substitute_port`] (`{{ backend.port }}`), exactly like the
/// `[docker]` lists — one template engine, not two.
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
pub(crate) const RUNTIME_DOCKER: &str = "docker";

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
pub(crate) fn docker_of_mut(backend: &mut Backend) -> Option<&mut Docker> {
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
pub(crate) fn process_of_mut(backend: &mut Backend) -> Option<&mut Process> {
    match backend.runtime.as_mut()? {
        Runtime::Process(process) => Some(process),
        Runtime::Docker(_) => None,
    }
}

/// Does any of a `[docker]` table's templates read `{{ backend.port }}`?
pub(crate) fn docker_reads_port(docker: &Docker) -> bool {
    std::iter::once(&docker.image)
        .chain(&docker.options)
        .chain(&docker.args)
        .any(|template| super::port::substitute_port(template, 0).1)
}

/// Does any of a process runtime's templates read `{{ backend.port }}`?
///
/// `command` is deliberately excluded, here and in the substitution below:
/// an executable whose PATH depends on a port is not a case worth
/// supporting, and a key that is never substituted must not be able to
/// satisfy the "declares a port someone reads" rule.
pub(crate) fn process_reads_port(process: &Process) -> bool {
    process
        .arguments
        .iter()
        .chain(process.env.values())
        .any(|template| super::port::substitute_port(template, 0).1)
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
pub(crate) fn reject_double_runtime(
    backend: &Backend,
    source: &std::path::Path,
) -> crate::Result<()> {
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
pub(crate) fn normalize_runtime(backend: &mut Backend) {
    if backend.runtime.is_none()
        && let Some(docker) = backend.docker.take()
    {
        backend.runtime = Some(Runtime::Docker(docker));
    }
}
