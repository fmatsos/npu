//! The Docker runtime: what `serve`, `stop`, `status` and `logs` do for a
//! backend declaring `type = "docker"`.
//!
//! The core knows the SHAPE of a `docker run` invocation and nothing else —
//! image, options and arguments all come from the backend's `[runtime]`
//! table, so changing image, ports or accelerator is a configuration change,
//! never a rebuild. Everything that touches the outside world is injected
//! ([`runner`], [`streamer`], [`probe`] are passed in by `lib.rs`), which is
//! why no test in the suite needs Docker installed.

use std::collections::BTreeMap;

/// This family's name: the `type` value a `[runtime]` table declares, and
/// what the `RUNTIME` column of `npu status` shows.
pub const NAME: &str = "docker";

/// The container runtime driven by [`serve`] and probed by
/// [`probe`]. Hardcoded because it is the shape of the invocation
/// this CLI knows how to build (`docker run [OPTIONS] IMAGE [ARG...]`), not
/// a preference about which server to run: WHAT is started stays entirely in
/// the backend's `[runtime]` table.
const CONTAINER_RUNTIME: &str = NAME;

/// Prefix of the container name derived from the backend identifier, so that
/// a container started by this CLI is recognizable in `docker ps` and so
/// that starting the same backend twice fails on an explicit name conflict
/// rather than silently running a second container fighting for the same
/// port.
const CONTAINER_NAME_PREFIX: &str = "npu-";

/// The container name `npu` gives the runtime of `backend_id` — how
/// `stop`, `status` and `logs` find again what `serve` started.
#[must_use]
pub(crate) fn container_name(backend_id: &str) -> String {
    format!("{CONTAINER_NAME_PREFIX}{backend_id}")
}

/// The `--filter` expression matching exactly one container by name.
///
/// Docker matches this filter as a REGEX, and `.` is a legal character in a
/// backend identifier: escaped, so `npu-a.b` cannot be answered by `npu-axb`.
fn name_filter(container: &str) -> String {
    format!("name=^{}$", container.replace('.', "\\."))
}

/// Completes `backend`'s `base_url` when its port was allocated by Docker.
///
/// A `port = "auto"` backend leaves `{{ backend.port }}` in its `base_url`
/// at load time — the value does not exist until a container is running.
/// This asks Docker what it published, which is the only source that can
/// answer the same thing in `npu serve`'s process and in the unrelated
/// process that runs a command minutes later.
///
/// A fixed port needs none of this and never runs `runner`: Docker stays an
/// optional prerequisite for every backend that does not opt into `"auto"`.
///
/// # Errors
///
/// `Error::Backend` when the port cannot be read — the container is not
/// started, publishes nothing, or Docker itself is unusable. All three mean
/// "the runtime is not up", which is exit code `3`, never `2`: the
/// configuration is fine.
pub fn resolve_base_url(
    backend: &crate::config::Backend,
    runner: &dyn Fn(&[String]) -> crate::Result<String>,
) -> crate::Result<String> {
    if !backend.uses_auto_port() {
        return Ok(backend.base_url.clone());
    }

    let container = container_name(&backend.id);
    let output = runner(&["port".to_string(), container.clone()]).unwrap_or_default();

    let port = published_port(&output).ok_or_else(|| {
        crate::Error::Backend(format!(
            "backend \"{}\" (port = \"auto\"): no published port readable for container \
             \"{container}\" — it is not running, publishes nothing, or the container runtime \
             is unavailable",
            backend.id
        ))
    })?;

    Ok(crate::config::substitute_port(&backend.base_url, port).0)
}

/// Parses `docker port <container>`, whose lines read
/// `8000/tcp -> 0.0.0.0:23451` — one per address family, so the same host
/// port appears twice.
///
/// `None` when nothing is published, when a line cannot be read, or when
/// SEVERAL distinct host ports are: `npu` would have to guess which one
/// serves the API, and guessing wrong means talking to the wrong port with
/// no error.
fn published_port(output: &str) -> Option<u16> {
    let mut found: Option<u16> = None;

    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let host_side = line.split_once("->")?.1;
        let port: u16 = host_side.rsplit_once(':')?.1.trim().parse().ok()?;
        match found {
            Some(existing) if existing != port => return None,
            _ => found = Some(port),
        }
    }

    found
}

/// Does a container named `container` already exist, running or not?
///
/// A runtime that cannot be asked answers `false`: this only exists to give
/// a better message than the one `docker run` would produce on its own, and
/// a probe that turned an unavailable Docker into a refusal to serve would
/// be worse than the message it replaces.
fn container_exists(container: &str, runner: &dyn Fn(&[String]) -> crate::Result<String>) -> bool {
    runner(&[
        "ps".to_string(),
        "--all".to_string(),
        "--filter".to_string(),
        name_filter(container),
        "--format".to_string(),
        "{{.Names}}".to_string(),
    ])
    .is_ok_and(|output| !output.trim().is_empty())
}

/// Builds the full `docker run` argument list for `model` on `backend`.
///
/// `-d` (detached) is imposed by this CLI rather than left to the
/// configuration: the command's RESULT is the container identifier, and
/// stdout carries nothing else — an attached container would write the
/// server's logs there.
///
/// Every element of `options`/`args` goes through `prompt::render`, the
/// prompt templating engine, so `{{ args.model }}` and `{{ env.NAME }}` work
/// here exactly as they do in a command file, with the same errors naming
/// the same things. `config`'s load-time validation has already rejected any
/// placeholder this function could not resolve.
fn run_args(
    backend: &crate::config::Backend,
    docker: &crate::config::Docker,
    model: &crate::config::Model,
    env: &dyn Fn(&str) -> Option<String>,
) -> crate::Result<Vec<String>> {
    let mut args = BTreeMap::new();
    args.insert("model".to_string(), model.model.clone());

    let render =
        |template: &String| crate::prompt::render(template, "", &args, env, &BTreeMap::new());

    let mut out = vec![
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        container_name(&backend.id),
    ];
    for option in &docker.options {
        out.push(render(option)?);
    }
    out.push(render(&docker.image)?);
    for arg in &docker.args {
        out.push(render(arg)?);
    }

    Ok(out)
}

/// Starts `backend`'s container and returns its identifier — which IS
/// `npu serve`'s result, hence what the caller writes to stdout.
///
/// `runner` is injected, exactly like `probe` in `builtin::doctor`: the
/// tests of this module never need Docker installed, and nothing here spawns
/// a process itself.
///
/// # Errors
///
/// `Error::Backend` if the backend is already served or its fixed port is
/// taken, otherwise whatever `runner` returns (`Error::Backend` for the real
/// runner, cf. [`runner`]); `Error::Config` if a `[runtime]` template
/// references an environment variable that is not set.
pub fn serve(
    backend: &crate::config::Backend,
    docker: &crate::config::Docker,
    model: &crate::config::Model,
    env: &dyn Fn(&str) -> Option<String>,
    runner: &dyn Fn(&[String]) -> crate::Result<String>,
) -> crate::Result<String> {
    // Asked BEFORE the port, because the two failures look identical from
    // the outside and only one of them is about the port: a backend already
    // served holds its own port, and telling its user to edit a `port` key
    // that is perfectly correct sends them to fix a file that is not broken.
    let container = container_name(&backend.id);
    if container_exists(&container, runner) {
        return Err(crate::Error::Backend(format!(
            "backend \"{}\" is already served by container \"{container}\" — `npu status` to \
             see it, `npu stop {}` to remove it",
            backend.id, model.id
        )));
    }

    // A fixed port already taken is reported HERE rather than left to
    // `docker run`: Docker's own message names the port but neither the
    // backend asking for it nor the key to change. `port = "auto"` skips the
    // check by construction — Docker allocates a free port, so there is
    // nothing to collide with.
    if let Some(crate::config::Port::Fixed(port)) = &backend.port
        && !super::port_is_free(*port)
    {
        return Err(crate::Error::Backend(format!(
            "backend \"{}\": port {port} is already in use by something else — change its \
             \"port\" key, stop what is listening on it, or use port = \"auto\" to let Docker \
             allocate one",
            backend.id
        )));
    }

    let args = run_args(backend, docker, model, env)?;
    runner(&args)
}

/// Removes the container [`serve`] started for `backend`, and returns what
/// the runtime printed — the container name, which is `npu stop`'s result.
///
/// `docker rm --force` rather than `docker stop`: a stopped-but-present
/// container still owns its name, so the next `npu serve` would fail on a
/// conflict. Stopping without removing would make the lifecycle a one-way
/// trip.
///
/// # Errors
///
/// Whatever `runner` returns (`Error::Backend` for the real one) — including
/// the case where no such container exists, which the runtime reports
/// itself.
pub fn stop(
    backend: &crate::config::Backend,
    runner: &dyn Fn(&[String]) -> crate::Result<String>,
) -> crate::Result<String> {
    let name = container_name(&backend.id);
    let printed = runner(&["rm".to_string(), "--force".to_string(), name.clone()])?;

    // `docker rm --force` on a container that does not exist succeeds
    // silently, which is what makes `stop` idempotent — but it leaves
    // nothing to write. The container name is the result in both cases:
    // "this container is now absent", whether it was running or never
    // started. Returning the empty output instead would put a blank line on
    // stdout, which carries the RESULT and nothing else.
    if printed.trim().is_empty() {
        return Ok(name);
    }
    Ok(printed)
}

/// Streams `backend`'s container logs.
///
/// Uses `streamer` and not `runner`: the logs ARE this command's result, and
/// a container writes them to both stdout and stderr. Capturing only stdout
/// would silently drop half of them — most servers log to stderr — so the
/// child's streams are inherited and reach the caller's stdout/stderr
/// unchanged, in order.
///
/// # Errors
///
/// Same as [`stop`].
pub fn logs(
    backend: &crate::config::Backend,
    follow: bool,
    streamer: &dyn Fn(&[String]) -> crate::Result<()>,
) -> crate::Result<()> {
    let mut args = vec!["logs".to_string()];
    if follow {
        args.push("--follow".to_string());
    }
    args.push(container_name(&backend.id));

    streamer(&args)
}

/// What `npu status` shows in the STATE column for `backend_id`.
///
/// `None` means no such container: "not started" is a legitimate state of a
/// lifecycle, and the vocabulary for it belongs to the report, not here.
/// A runtime that cannot be asked yields its own message rather than an
/// error — `status` is a report, and a report that dies on its first unknown
/// line is not a report.
///
/// Queries the runtime once per backend rather than filtering a single
/// listing: `docker ps` has no way to say "these names, in this order", and
/// a report whose lines depend on the runtime's own ordering would not be
/// deterministic.
#[must_use]
pub fn state(
    backend_id: &str,
    runner: &dyn Fn(&[String]) -> crate::Result<String>,
) -> Option<String> {
    // ponytail: `status` uses the unbounded runner, so a wedged daemon hangs
    // it — unlike `doctor`, which bounds its probe. Bound this call the same
    // way if that ever bites; `serve` must stay unbounded, an image pull
    // takes minutes.
    match runner(&[
        "ps".to_string(),
        "--all".to_string(),
        "--filter".to_string(),
        name_filter(&container_name(backend_id)),
        "--format".to_string(),
        "{{.Status}}".to_string(),
    ]) {
        Ok(output) if output.trim().is_empty() => None,
        Ok(output) => Some(output.trim().to_string()),
        Err(err) => Some(err.to_string()),
    }
}

/// The real runner behind [`serve`]: runs `docker` with `args` and returns
/// its stdout (the container identifier), trailing newline removed.
///
/// The child's stderr is INHERITED — Docker's own diagnostics belong on this
/// process's stderr, never on its stdout, which carries the result only.
///
/// # Errors
///
/// `Error::Backend` if `docker` cannot be spawned (not installed, not on
/// `PATH`) or exits non-zero: from a calling program's point of view both
/// mean "the runtime could not be brought up", the same class as an
/// unreachable backend.
pub fn runner(args: &[String]) -> crate::Result<String> {
    let output = std::process::Command::new(CONTAINER_RUNTIME)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|err| {
            crate::Error::Backend(format!("cannot run \"{CONTAINER_RUNTIME}\": {err}"))
        })?;

    if !output.status.success() {
        return Err(crate::Error::Backend(format!(
            "\"{CONTAINER_RUNTIME} {}\" failed ({})",
            args.join(" "),
            output.status
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

/// The real streamer behind [`logs`]: runs `docker` with both streams
/// INHERITED, so the container's logs reach the caller as the runtime wrote
/// them, interleaved and unbuffered, instead of being captured and reprinted
/// in the wrong order.
///
/// # Errors
///
/// `Error::Backend` if `docker` cannot be spawned or exits non-zero — the
/// same classification as [`runner`], for the same reason.
pub fn streamer(args: &[String]) -> crate::Result<()> {
    let status = std::process::Command::new(CONTAINER_RUNTIME)
        .args(args)
        .stdin(std::process::Stdio::null())
        .status()
        .map_err(|err| {
            crate::Error::Backend(format!("cannot run \"{CONTAINER_RUNTIME}\": {err}"))
        })?;

    if !status.success() {
        return Err(crate::Error::Backend(format!(
            "\"{CONTAINER_RUNTIME} {}\" failed ({status})",
            args.join(" ")
        )));
    }

    Ok(())
}

/// Probes the container runtime for `doctor`'s check (f): runs
/// `docker info`, which reaches the DAEMON and not merely the binary — a
/// report claiming the runtime is available while its daemon is dead would
/// be exactly the kind of lie `builtin.rs` refuses (cf. its module doc).
///
/// Bounded by [`super::PROBE_TIMEOUT`], like the TCP probe and for the same
/// reason: `doctor` must stay fast, and a wedged daemon would otherwise hang
/// it indefinitely (`std::process` offers no timeout).
///
/// # Errors
///
/// Returns `Err` if `docker` cannot be spawned, does not answer within
/// [`super::PROBE_TIMEOUT`], or exits non-zero.
pub fn probe() -> Result<(), String> {
    let mut child = std::process::Command::new(CONTAINER_RUNTIME)
        .arg("info")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|err| format!("cannot run \"{CONTAINER_RUNTIME}\": {err}"))?;

    let deadline = std::time::Instant::now() + super::PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Err(err) => {
                return Err(format!(
                    "waiting for \"{CONTAINER_RUNTIME} info\" failed: {err}"
                ));
            }
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(format!("\"{CONTAINER_RUNTIME} info\" failed ({status})"));
            }
            Ok(None) => {}
        }

        if std::time::Instant::now() >= deadline {
            // Best effort: the probe has already made up its mind, and a
            // child that cannot be killed must not turn a diagnostic into a
            // failure of its own.
            drop(child.kill());
            return Err(format!(
                "\"{CONTAINER_RUNTIME} info\" did not answer within {} s",
                super::PROBE_TIMEOUT.as_secs()
            ));
        }

        std::thread::sleep(super::PROBE_POLL_INTERVAL);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // allowed in tests (cf. Cargo.toml [lints.clippy]).
#[allow(clippy::panic)] // a stub that must never be called says so by panicking.
mod tests {
    use super::*;

    /// A backend with no runtime at all: the shape every `[docker]`-free
    /// backend has after loading.
    fn backend(id: &str, base_url: &str) -> crate::config::Backend {
        crate::config::Backend {
            id: id.to_string(),
            base_url: base_url.to_string(),
            kind: "openai-compatible".to_string(),
            operations: std::collections::HashMap::new(),
            port: None,
            runtime: None,
            docker: None,
            timeouts: None,
            structured_output: false,
            source: std::path::PathBuf::new(),
        }
    }

    /// A backend declaring `port = "auto"`: its `base_url` keeps the
    /// placeholder until Docker is asked what it published.
    fn auto_port_backend(id: &str) -> crate::config::Backend {
        let mut backend = backend(id, "http://127.0.0.1:{{ backend.port }}");
        backend.port = Some(crate::config::Port::Keyword("auto".to_string()));
        backend.runtime = Some(crate::config::Runtime::Docker(crate::config::Docker {
            image: "img".to_string(),
            options: vec!["-p".to_string(), "0:8000".to_string()],
            args: vec![],
        }));
        backend
    }

    /// A runner a test asserts is never called.
    fn unused_runner(_args: &[String]) -> crate::Result<String> {
        panic!("the container runtime must not be consulted here");
    }

    #[test]
    fn published_port_reads_both_address_families_as_one_port() {
        assert_eq!(
            published_port("8000/tcp -> 0.0.0.0:23451\n8000/tcp -> [::]:23451\n"),
            Some(23451)
        );
    }

    /// Two distinct published ports would force `npu` to guess which one
    /// serves the API, and guessing wrong talks to the wrong port silently.
    #[test]
    fn published_port_refuses_an_ambiguous_mapping() {
        assert_eq!(
            published_port("8000/tcp -> 0.0.0.0:23451\n9000/tcp -> 0.0.0.0:23452\n"),
            None
        );
    }

    #[test]
    fn published_port_on_nothing_published_or_unreadable_output() {
        assert_eq!(published_port(""), None);
        assert_eq!(
            published_port("no public port '8000' published for npu-x"),
            None
        );
    }

    #[test]
    fn resolve_base_url_completes_an_auto_port_from_docker() {
        let backend = auto_port_backend("gpu");
        let runner = |args: &[String]| {
            assert_eq!(args, ["port".to_string(), "npu-gpu".to_string()]);
            Ok("8000/tcp -> 0.0.0.0:23451\n8000/tcp -> [::]:23451\n".to_string())
        };

        let url = resolve_base_url(&backend, &runner).expect("the published port must resolve");

        assert_eq!(url, "http://127.0.0.1:23451");
    }

    /// Exit code `3`, never `2`: a container that is not started is
    /// something to START, not a file to fix.
    #[test]
    fn resolve_base_url_on_a_stopped_container_is_a_backend_error_naming_it() {
        let backend = auto_port_backend("gpu");
        let runner = |_args: &[String]| Err(crate::Error::Backend("no such container".to_string()));

        let err = resolve_base_url(&backend, &runner).expect_err("a stopped container must fail");

        assert_eq!(err.exit_code(), 3);
        assert!(err.to_string().contains("gpu"), "got: {err}");
    }

    /// The invariant that keeps Docker OPTIONAL: a backend with a fixed port
    /// must never consult it.
    #[test]
    fn resolve_base_url_never_runs_docker_for_a_fixed_port() {
        let backend = backend("ovms", "http://127.0.0.1:8000");

        let url = resolve_base_url(&backend, &unused_runner).expect("a fixed port resolves");

        assert_eq!(url, "http://127.0.0.1:8000");
    }

    /// The container name is what `stop`, `status` and `logs` use to find
    /// again what `serve` started: derivable from the backend identifier
    /// alone, identically in all four.
    #[test]
    fn container_name_is_the_backend_identifier_behind_the_npu_prefix() {
        assert_eq!(container_name("ovms"), "npu-ovms");
    }

    /// A `.` is legal in a backend identifier and is a regex metacharacter
    /// for `docker ps --filter`: unescaped, `npu-a.b` would be answered by
    /// `npu-axb`.
    #[test]
    fn name_filter_escapes_the_dot_of_an_identifier() {
        assert_eq!(name_filter("npu-a.b"), "name=^npu-a\\.b$");
    }

    /// `state` speaks about the CONTAINER, not about the report: an absent
    /// container is `None`, and the vocabulary for it belongs to
    /// `builtin::status`.
    #[test]
    fn state_of_an_absent_container_is_none_rather_than_a_report_word() {
        assert_eq!(state("ovms", &|_args| Ok(String::new())), None);
    }

    /// A runtime that cannot be asked yields its own message: `status` is a
    /// report, and a report that dies on its first unknown line is not one.
    #[test]
    fn state_of_an_unreachable_runtime_carries_the_runtime_message() {
        let state = state("ovms", &|_args| {
            Err(crate::Error::Backend(
                "cannot reach the daemon serving \"npu-ovms\"".to_string(),
            ))
        })
        .expect("an unreachable runtime must still produce a state");
        // The container name, not the prose around it: what must survive is
        // that the runtime's own message reaches the report naming what it
        // could not be asked about.
        assert!(state.contains("npu-ovms"), "got: {state}");
    }
}
