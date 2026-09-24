//! `npu backend serve/stop/logs/status`: dispatch on the runtime family
//! (Docker or process) and format of their result — the container/process
//! lifecycle itself lives in `src/runtime/`, never here.

/// One line of the [`status`] report: a backend declaring a runtime, and
/// the state that runtime is in.
struct RuntimeStatus {
    backend: String,
    /// Which runtime family manages it — `npu status` reports every family
    /// in one table, so a line that did not say which one it belongs to
    /// would be ambiguous the day a second family exists.
    runtime: String,
    /// What this family calls the thing it started: a container name for
    /// Docker. Named INSTANCE rather than CONTAINER because the column is
    /// shared by families that start no container at all.
    instance: String,
    /// The backend's resolved `base_url`. Reported because `port = "auto"`
    /// derives a number nothing else in the CLI shows: a value the user
    /// cannot read is only half a feature.
    url: String,
    state: String,
}

/// What [`status`] reports for a backend whose runtime does not exist.
/// Not an error: "not started" is a legitimate state of a lifecycle, and a
/// report that failed on it could never describe a stopped runtime.
pub(crate) const NOT_STARTED: &str = "not started";

/// What [`status`] shows in the URL column when a `port = "auto"` backend's
/// port cannot be read back — the runtime is not running yet.
const UNKNOWN_URL: &str = "-";

/// Resolves `model_id` to the model, its backend, and the runtime that
/// backend declares.
///
/// Shared by [`serve`], [`stop`] and [`logs`] so the three refuse the same
/// way: a model that does not exist, or a backend that was never told how to
/// start anything, is a configuration problem naming the identifier at
/// fault. Guessing a runtime from a `base_url` instead would let `npu stop`
/// act on a server npu never started.
fn lifecycle_target<'a>(
    config: &'a crate::config::Config,
    model_id: &str,
) -> crate::Result<(
    &'a crate::config::Model,
    &'a crate::config::Backend,
    &'a crate::config::Runtime,
)> {
    let (model, backend) = config.resolve(model_id)?;

    let Some(runtime) = backend.runtime() else {
        return Err(crate::Error::Config(crate::error::ConfigError::bare(
            Some(&backend.id),
            format!(
                "backend \"{}\" (used by model \"{model_id}\") declares no [runtime] table: \
                 npu only manages the runtimes it starts",
                backend.id
            ),
        )));
    };

    Ok((model, backend, runtime))
}

/// `npu serve <model>`: starts the runtime of the backend the model points
/// at, and returns the started instance's identifier — which IS this
/// command's result, hence what the caller writes to stdout.
///
/// A model names exactly one backend, so the model identifier alone is
/// unambiguous: several backends with a runtime coexist without anything to
/// disambiguate.
///
/// Orchestration only: resolve, `match` the family, delegate. Everything
/// that touches the outside world lives in `crate::runtime`, and `runner` is
/// injected exactly like `probe` in [`super::doctor::doctor`] — the tests of this module
/// never need Docker installed.
///
/// # Errors
///
/// - `Error::Config` if the model is unknown, if its backend is unknown, or
///   if that backend declares no runtime — in every case naming the
///   identifier at fault;
/// - whatever the runtime family returns otherwise (`Error::Backend` for the
///   real runner, cf. [`crate::runtime::docker::runner`]).
pub fn serve(
    config: &crate::config::Config,
    model_id: &str,
    env: &dyn Fn(&str) -> Option<String>,
    runner: &dyn Fn(&[String]) -> crate::Result<String>,
    host: &crate::runtime::process::Host<'_>,
) -> crate::Result<String> {
    let (model, backend, runtime) = lifecycle_target(config, model_id)?;

    match runtime {
        crate::config::Runtime::Docker(docker) => {
            crate::runtime::docker::serve(backend, docker, model, env, runner)
        }
        crate::config::Runtime::Process(process) => {
            crate::runtime::process::serve(backend, process, model, host)
        }
    }
}

/// `npu stop <model>`: tears down what [`serve`] started for that model's
/// backend, and returns the instance identifier — this command's result.
///
/// # Errors
///
/// Same as [`serve`].
pub fn stop(
    config: &crate::config::Config,
    model_id: &str,
    runner: &dyn Fn(&[String]) -> crate::Result<String>,
    host: &crate::runtime::process::Host<'_>,
) -> crate::Result<String> {
    let (_model, backend, runtime) = lifecycle_target(config, model_id)?;

    match runtime {
        crate::config::Runtime::Docker(_) => crate::runtime::docker::stop(backend, runner),
        crate::config::Runtime::Process(_) => crate::runtime::process::stop(backend, host),
    }
}

/// `npu logs <model>`: streams the runtime's logs.
///
/// Uses `streamer` and not `runner`: the logs ARE this command's result, and
/// a server writes them to both stdout and stderr. Capturing only stdout
/// would silently drop half of them.
///
/// # Errors
///
/// Same as [`serve`].
pub fn logs(
    config: &crate::config::Config,
    model_id: &str,
    follow: bool,
    streamer: &dyn Fn(&[String]) -> crate::Result<()>,
    host: &crate::runtime::process::Host<'_>,
    sink: &dyn Fn(&[u8]) -> crate::Result<()>,
) -> crate::Result<()> {
    let (_model, backend, runtime) = lifecycle_target(config, model_id)?;

    match runtime {
        crate::config::Runtime::Docker(_) => {
            crate::runtime::docker::logs(backend, follow, streamer)
        }
        // `sink` where Docker has `streamer`, for the same reason and with
        // the same contract: the logs ARE the result, and this family reads
        // them from the file `serve` redirected both streams into rather
        // than from a child process's own streams.
        crate::config::Runtime::Process(_) => {
            crate::runtime::process::logs(backend, follow, host, sink)
        }
    }
}

/// `npu status`: for every backend declaring a runtime, is that runtime up?
///
/// Backends are iterated sorted by identifier, like every other report in
/// this module: the report must not depend on the runtime's own ordering,
/// nor on a `HashMap`'s.
///
/// A backend the runtime cannot be asked about does not fail the command:
/// its state becomes the runtime's own message. `status` is a report, and a
/// report that dies on its first unknown line is not a report.
///
/// # Errors
///
/// Never returns `Err`; the signature stays `Result` for symmetry with the
/// other built-ins and so a future stricter mode does not break the caller.
pub fn status(
    config: &crate::config::Config,
    runner: &dyn Fn(&[String]) -> crate::Result<String>,
    host: &crate::runtime::process::Host<'_>,
) -> crate::Result<String> {
    let mut ids: Vec<&String> = config.backends.keys().collect();
    ids.sort_unstable();

    let rows: Vec<RuntimeStatus> = ids
        .into_iter()
        .filter_map(|id| {
            // `id` comes from `config.backends`'s keys: the entry exists.
            let backend = &config.backends[id];
            // A backend with no runtime is not a line of this report: npu
            // was never told how to start it, so it has no state to show.
            let runtime = backend.runtime()?;

            // A report that dies on its first unreadable line is not a
            // report: an unresolvable URL (runtime down, Docker absent)
            // becomes a dash, exactly like an absent FALLBACK in `models`.
            let configured = crate::runtime::resolve_base_url(backend, runner)
                .unwrap_or_else(|_| UNKNOWN_URL.to_string());

            let (instance, url, state) = match runtime {
                crate::config::Runtime::Docker(_) => (
                    crate::runtime::docker::container_name(&backend.id),
                    configured,
                    crate::runtime::docker::state(&backend.id, runner),
                ),
                // The INSTANCE of a process is its pid, and the URL is the
                // one its record holds — the address it was actually served
                // on, which is also the one its state was decided against.
                // A record that cannot be read becomes the state of THIS row
                // and nothing more: one corrupt state file must not suppress
                // the other backends' lines.
                crate::config::Runtime::Process(_) => {
                    crate::runtime::process::report(backend, &configured, host)
                }
            };

            Some(RuntimeStatus {
                backend: id.clone(),
                runtime: crate::runtime::label(runtime).to_string(),
                instance,
                url,
                state: state.unwrap_or_else(|| NOT_STARTED.to_string()),
            })
        })
        .collect();

    Ok(format_status(&rows))
}

/// Formats the [`status`] table, columns sized to the content like
/// [`super::models::format_models`]. A configuration with no backend declaring a runtime
/// still produces the header: an empty output would be indistinguishable
/// from a command that did nothing.
fn format_status(rows: &[RuntimeStatus]) -> String {
    const BACKEND_HEADER: &str = "BACKEND";
    const RUNTIME_HEADER: &str = "RUNTIME";
    const INSTANCE_HEADER: &str = "INSTANCE";
    const URL_HEADER: &str = "URL";
    const STATE_HEADER: &str = "STATE";

    let width = |header: &str, of: &dyn Fn(&RuntimeStatus) -> usize| {
        rows.iter().map(of).max().unwrap_or(0).max(header.len())
    };
    let backend_width = width(BACKEND_HEADER, &|row: &RuntimeStatus| row.backend.len());
    let runtime_width = width(RUNTIME_HEADER, &|row: &RuntimeStatus| row.runtime.len());
    let instance_width = width(INSTANCE_HEADER, &|row: &RuntimeStatus| row.instance.len());
    let url_width = width(URL_HEADER, &|row: &RuntimeStatus| row.url.len());

    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(format!(
        "{BACKEND_HEADER:<backend_width$}  {RUNTIME_HEADER:<runtime_width$}  \
         {INSTANCE_HEADER:<instance_width$}  {URL_HEADER:<url_width$}  {STATE_HEADER}"
    ));
    for row in rows {
        lines.push(format!(
            "{:<backend_width$}  {:<runtime_width$}  {:<instance_width$}  {:<url_width$}  {}",
            row.backend, row.runtime, row.instance, row.url, row.state
        ));
    }

    lines.join("\n")
}
