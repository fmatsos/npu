//! CLI built-ins: `doctor`, `models`, `describe`, and the TCP
//! reachability probe they share.
//!
//! This module NEVER writes to the console itself: [`doctor`] returns
//! a report ([`Check`]) that the caller (`lib.rs::run`) formats with
//! [`format_doctor`] before writing it to stdout (contract rule: stdout
//! is reserved for the RESULT of a built-in, which IS its report). This is
//! what makes [`doctor`] fully testable without touching disk beyond
//! check (e), nor the network: the reachability probe (`probe`) is
//! injected by the caller rather than called directly, exactly like
//! `prompt::render`/`prompt::preflight` inject their environment
//! variable resolver (cf. `lib.rs::run`).
//!
//! **Deliberate omission: "NPU available".** This CLI is deliberately
//! agnostic of the inference runtime (the core only knows named backend
//! operations, never an NPU in the hardware sense): it therefore has NO way to
//! check the presence or availability of an NPU, unlike the
//! reachability of a backend (a TCP connection) or the validity of a
//! schema (a disk file read). Showing a checkmark for a check that
//! wasn't actually performed would be exactly the flaw that the L3
//! review architecture rule forbids ("a report that lies is worse than
//! no report"): [`doctor`] therefore NEVER produces a [`Check`] for
//! this line. This is not an oversight.

use std::net::ToSocketAddrs;
use std::time::Duration;

use serde::Serialize;

/// Outcome of a [`doctor`] check.
#[derive(Debug)]
pub enum Status {
    /// The check succeeded.
    Ok,
    /// The check failed, with an actionable message.
    Failed(String),
}

/// Category of a [`Check`], in the sense of the exit code from [`doctor_exit_code`]
/// (point 5 of the shared contract, spec §23): it is this value, and
/// NEVER the text of [`Check::label`], that distinguishes a
/// configuration failure ("fix your files") from a reachability failure
/// ("start your runtime") for a calling agent. A label is
/// display — it can be reworded, translated, or given a new
/// suffix without notice; the category is a machine contract and must
/// survive that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    /// Checks (a), (c), (d), (e): configuration, models, commands.
    Config,
    /// Check (b): TCP reachability of a backend.
    Reachability,
}

/// One line of the [`doctor`] report.
#[derive(Debug)]
pub struct Check {
    /// Category of this check, used by [`doctor_exit_code`].
    pub kind: CheckKind,
    /// What was checked (e.g. `backend "ovms" reachable`).
    pub label: String,
    /// The result of this check.
    pub status: Status,
}

/// The three built-in names exposed by this CLI, plus `help`, reserved by
/// `clap` itself (every `clap::Command` gets an automatic `-h`/`--help`
/// flag). `command.rs` (`reject_reserved_path`) rejects at load time
/// any command file whose FIRST path segment matches one of these
/// values (phase 5, point 2 of the shared contract): without this
/// rejection, `commands/doctor.md` would be silently shadowed by (or
/// would shadow) the `doctor` built-in built in `lib.rs`.
pub const RESERVED: &[&str] = &["doctor", "models", "describe", "help"];

/// Maximum delay granted to [`tcp_probe`] before considering a backend
/// unreachable. Short by design (point 4 of the shared contract):
/// `doctor` is a diagnostic command meant to stay fast even when
/// several backends are queried, never meant to wait out a full
/// network timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Check (a): was the configuration loaded successfully?
///
/// `load_error` carries the error KEPT by the degraded mode of `lib.rs::run`
/// (shared-contract decision 1) rather than propagated immediately: this is
/// precisely what lets `doctor` run despite a broken configuration.
/// The failure message is that of `crate::Error` itself (already
/// prefixed with `configuration error: `, cf. `error.rs`), never rebuilt
/// by hand.
fn check_config_loaded(load_error: Option<&crate::Error>) -> Check {
    let status = match load_error {
        Some(err) => Status::Failed(err.to_string()),
        None => Status::Ok,
    };
    Check {
        kind: CheckKind::Config,
        label: "configuration loaded".to_string(),
        status,
    }
}

/// Check (b): is each configured backend reachable? Iterates over
/// `config.backends` sorted by identifier (`BTreeMap`/`HashMap` is not
/// ordered otherwise), so that the report order is deterministic regardless
/// of the underlying `HashMap`'s iteration order.
///
/// `probe` is injected (never [`tcp_probe`] called directly): this is what
/// makes this function, and therefore [`doctor`] as a whole, testable
/// without ever opening a single socket.
fn check_backends_reachable(
    config: &crate::config::Config,
    probe: &dyn Fn(&str) -> Result<(), String>,
) -> Vec<Check> {
    let mut ids: Vec<&String> = config.backends.keys().collect();
    ids.sort_unstable();

    ids.into_iter()
        .map(|id| {
            // `id` comes from `config.backends`'s keys: the entry necessarily exists.
            let backend = &config.backends[id];
            let status = match probe(&backend.base_url) {
                Ok(()) => Status::Ok,
                Err(message) => Status::Failed(message),
            };
            Check {
                kind: CheckKind::Reachability,
                label: format!("backend \"{id}\" reachable"),
                status,
            }
        })
        .collect()
}

/// Check (c): for each configured model, does its backend exist,
/// and does it expose the operation that model declares? A single [`Check`]
/// per model, covering both — a model whose backend is absent cannot
/// expose an operation to check anyway. Sorted by model
/// identifier, same determinism reason as [`check_backends_reachable`].
fn check_models(config: &crate::config::Config) -> Vec<Check> {
    let mut ids: Vec<&String> = config.models.keys().collect();
    ids.sort_unstable();

    ids.into_iter()
        .map(|id| {
            // `id` comes from `config.models`'s keys: the entry necessarily exists.
            let model = &config.models[id];
            let status = match config.backends.get(&model.backend) {
                None => Status::Failed(format!(
                    "backend \"{}\" not found (referenced by model \"{id}\"; available \
                     backends: {})",
                    model.backend,
                    crate::error::format_available(config.backends.keys())
                )),
                Some(backend) => {
                    if backend.operations.contains_key(&model.operation) {
                        Status::Ok
                    } else {
                        Status::Failed(format!(
                            "backend \"{}\" does not expose operation \"{}\" declared by model \
                             \"{id}\" (available operations: {})",
                            backend.id,
                            model.operation,
                            crate::error::format_available(backend.operations.keys())
                        ))
                    }
                }
            };
            Check {
                kind: CheckKind::Config,
                label: format!("model \"{id}\""),
                status,
            }
        })
        .collect()
}

/// Sorts `commands` by full path (`path.join("/")`), so that the order of
/// the [`doctor`] report is deterministic regardless of the order in which
/// files are discovered on disk — same reason as the sort in
/// `command::discover_scopes` itself.
fn sorted_commands(commands: &[crate::command::CommandSpec]) -> Vec<&crate::command::CommandSpec> {
    let mut sorted: Vec<&crate::command::CommandSpec> = commands.iter().collect();
    sorted.sort_by_key(|spec| spec.path.join("/"));
    sorted
}

/// Check (d): for each discovered command, in ALL scopes
/// (not just the one possibly invoked), does its model exist?
fn check_commands_model(
    config: &crate::config::Config,
    commands: &[crate::command::CommandSpec],
) -> Vec<Check> {
    sorted_commands(commands)
        .into_iter()
        .map(|spec| {
            let path = spec.path.join("/");
            let status = if config.models.contains_key(&spec.model) {
                Status::Ok
            } else {
                Status::Failed(format!(
                    "model \"{}\" not found (referenced by command \"{path}\"; available \
                     models: {})",
                    spec.model,
                    crate::error::format_available(config.models.keys())
                ))
            };
            Check {
                kind: CheckKind::Config,
                label: format!("command \"{path}\": model"),
                status,
            }
        })
        .collect()
}

/// Check (e): for each command declaring an output schema
/// (`[output].schema`), in ALL scopes, is that schema present,
/// readable, valid JSON, and a compilable JSON Schema? This is the
/// EXHAUSTIVE work that phase 4 deliberately deferred to `doctor` (cf.
/// `output.rs`, `command::resolve_schema_path`: a schema's existence and
/// compilation are only checked, outside `doctor`, at the moment the
/// command that requires it is actually invoked). Reuses
/// `output::compile_schema` — made `pub(crate)` for this phase (the only
/// change allowed in `output.rs`, cf. the report) — rather than
/// writing a second implementation of schema compilation: the three
/// distinct error messages (absent/unreadable, invalid JSON, schema
/// syntactically invalid) it already produces are exactly the ones
/// this check must report.
///
/// A command without `[output].schema` (text format, or JSON without a
/// schema — both explicitly allowed by `command::convert_output`)
/// produces no [`Check`] here: there is nothing to check.
fn check_commands_output_schema(commands: &[crate::command::CommandSpec]) -> Vec<Check> {
    sorted_commands(commands)
        .into_iter()
        .filter_map(|spec| {
            let schema_path = spec.output.schema.as_deref()?;
            let path = spec.path.join("/");
            let status = match crate::output::compile_schema(schema_path, &spec.file) {
                Ok(_validator) => Status::Ok,
                Err(err) => Status::Failed(err.to_string()),
            };
            Some(Check {
                kind: CheckKind::Config,
                label: format!("command \"{path}\": output schema"),
                status,
            })
        })
        .collect()
}

/// Runs all `npu doctor` checks (point 3 of the shared contract) and
/// returns the report — without ever writing to the console or touching
/// the network (`probe` is injected); only check (e) touches disk, by
/// reading the declared schema files.
///
/// `config`/`commands` and `load_error` reflect the degraded mode of
/// `lib.rs::run` (shared-contract decision 1, corollary of the debt
/// tracked since phase 1): when loading fails, `run` KEEPS the error
/// instead of propagating it, so that `doctor` can still run and
/// report it as a failed check (a). This function does not assume that
/// `config`/`commands`/`load_error` vary as a block, though: each
/// family of checks (b/c, then d/e) only runs if the data it needs is
/// actually available, which stays correct whether the caller treats
/// loading as a single atomic operation (the case expected in
/// practice, cf. the report) or distinguishes a genuine configuration
/// failure from a command discovery failure.
#[must_use]
pub fn doctor(
    config: Option<&crate::config::Config>,
    commands: Option<&[crate::command::CommandSpec]>,
    load_error: Option<&crate::Error>,
    probe: &dyn Fn(&str) -> Result<(), String>,
) -> Vec<Check> {
    let mut checks = vec![check_config_loaded(load_error)];

    if let Some(config) = config {
        checks.extend(check_backends_reachable(config, probe));
        checks.extend(check_models(config));
    }

    if let (Some(config), Some(commands)) = (config, commands) {
        checks.extend(check_commands_model(config, commands));
        checks.extend(check_commands_output_schema(commands));
    }

    checks
}

/// Formats the `doctor` report for stdout (point 6 of the shared contract):
/// a checkmark (`✓`) followed by the label for each successful check, a
/// cross (`✗`) followed by the label THEN the message for each failure —
/// never the reverse, or the message explaining the failure would end up
/// without context on what it concerns.
#[must_use]
pub fn format_doctor(checks: &[Check]) -> String {
    checks
        .iter()
        .map(|check| match &check.status {
            Status::Ok => format!("✓ {}", check.label),
            Status::Failed(message) => format!("✗ {}: {message}", check.label),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Exit code of the `doctor` report (point 5 of the shared contract):
/// - `0` if all checks pass;
/// - `2` if at least one CONFIGURATION check (a, c, d, e) fails —
///   configuration takes priority, including when a reachability check
///   (b) fails at the same time;
/// - `3` if ONLY reachability (b) fails.
///
/// Distinguishes the two failure families via [`Check::kind`], never via
/// the text of [`Check::label`]: a label is display — it can be
/// reworded, translated, or given a new suffix without notice — while
/// the category is a machine contract (spec §23) that this exit code
/// must keep honoring no matter what happens to the label.
#[must_use]
pub fn doctor_exit_code(checks: &[Check]) -> i32 {
    let mut config_failed = false;
    let mut reachability_failed = false;

    for check in checks {
        let Status::Failed(_) = &check.status else {
            continue;
        };
        match check.kind {
            CheckKind::Reachability => reachability_failed = true,
            CheckKind::Config => config_failed = true,
        }
    }

    if config_failed {
        2
    } else if reachability_failed {
        3
    } else {
        0
    }
}

/// Formats the `npu models` table (point 6 of the shared contract, spec
/// §16): NAME/BACKEND/OPERATION columns, sorted by name to stay
/// deterministic regardless of the underlying `HashMap`'s iteration
/// order, aligned to the ACTUAL width of the content (never a hardcoded
/// width: a model name longer than "NAME" widens its column). A
/// configuration with no models still produces the header — never an
/// empty table nor a panic.
#[must_use]
pub fn format_models(config: &crate::config::Config) -> String {
    const NAME_HEADER: &str = "NAME";
    const BACKEND_HEADER: &str = "BACKEND";
    const OPERATION_HEADER: &str = "OPERATION";

    let mut rows: Vec<(&str, &str, &str)> = config
        .models
        .values()
        .map(|model| {
            (
                model.id.as_str(),
                model.backend.as_str(),
                model.operation.as_str(),
            )
        })
        .collect();
    rows.sort_unstable_by_key(|&(name, _backend, _operation)| name);

    let name_width = rows
        .iter()
        .map(|&(name, _backend, _operation)| name.len())
        .max()
        .unwrap_or(0)
        .max(NAME_HEADER.len());
    let backend_width = rows
        .iter()
        .map(|&(_name, backend, _operation)| backend.len())
        .max()
        .unwrap_or(0)
        .max(BACKEND_HEADER.len());

    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(format!(
        "{NAME_HEADER:<name_width$}  {BACKEND_HEADER:<backend_width$}  {OPERATION_HEADER}"
    ));
    for (name, backend, operation) in rows {
        lines.push(format!(
            "{name:<name_width$}  {backend:<backend_width$}  {operation}"
        ));
    }

    lines.join("\n")
}

/// A declared argument as serialized by [`describe`]: same fields as
/// `command::ArgSpec`, never that one directly — `ArgSpec` only derives
/// `Deserialize` (it is never written to output elsewhere in this crate),
/// and this file is not allowed to modify `command.rs` to add
/// `Serialize` there (absolute rule of the task: exclusive owner of
/// `builtin.rs`).
#[derive(Serialize)]
struct DescribeArg<'a> {
    short: Option<char>,
    required: bool,
    description: &'a str,
}

/// The output contract as serialized by [`describe`]: same data as
/// `output::OutputSpec`, but with `format` and `schema` already converted
/// to serializable types (`&str`, `Option<String>`) rather than the
/// internal types (`output::Format`, `Option<PathBuf>`), for the same
/// reason as [`DescribeArg`] above.
#[derive(Serialize)]
struct DescribeOutput<'a> {
    format: &'a str,
    schema: Option<String>,
    max_lines: Option<usize>,
}

/// The complete JSON description of a command, as serialized by
/// [`describe`].
#[derive(Serialize)]
struct Describe<'a> {
    name: String,
    description: &'a str,
    model: &'a str,
    input: &'a str,
    args: std::collections::BTreeMap<&'a str, DescribeArg<'a>>,
    output: DescribeOutput<'a>,
}

/// Describes a dynamically configured command (point 6 of the shared
/// contract, spec §16): produces JSON on stdout, serialized by
/// `serde_json` (never built by hand — a manual `format!` could not
/// correctly escape a description or a prompt containing quotes).
/// Extends the §16 example with what phases 3 and 4 added: the declared
/// arguments (`args`) and the output contract (`output`, with its
/// format, its schema if any, and its line limit if any).
///
/// Does NO resolution by name: the shared contract fixes this signature
/// to an already-resolved `CommandSpec` — it is up to the caller
/// (`lib.rs::run`) to look up this `CommandSpec` (with its own
/// `find_command`, already written and tested there) before calling
/// this function. See the report for the explicit decision behind this
/// choice: "describe on an unknown command" is therefore not a
/// behavior this module can produce or test, for lack of receiving a
/// name to resolve.
pub fn describe(spec: &crate::command::CommandSpec) -> crate::Result<String> {
    let input = match spec.input {
        crate::command::InputMode::Stdin => "stdin",
        crate::command::InputMode::File => "file",
        crate::command::InputMode::StdinOrFile => "stdin_or_file",
    };
    let format = match spec.output.format {
        crate::output::Format::Text => "text",
        crate::output::Format::Json => "json",
    };

    let args = spec
        .args
        .iter()
        .map(|(name, arg_spec)| {
            (
                name.as_str(),
                DescribeArg {
                    short: arg_spec.short,
                    required: arg_spec.required,
                    description: arg_spec.description.as_str(),
                },
            )
        })
        .collect();

    let dto = Describe {
        name: spec.path.join("/"),
        description: &spec.description,
        model: &spec.model,
        input,
        args,
        output: DescribeOutput {
            format,
            schema: spec
                .output
                .schema
                .as_ref()
                .map(|path| format!("{}", path.display())),
            max_lines: spec.output.max_lines,
        },
    };

    serde_json::to_string(&dto)
        .map_err(|err| crate::Error::Config(format!("description serialization failed: {err}")))
}

/// Extracts `(host, port)` from a base URL "http(s)://host[:port][/...]"
/// (point 4 of the shared contract). Falls back to the scheme's implicit
/// port (80 for `http`, 443 for `https`) when no explicit port is
/// present. PURELY SYNTACTIC: never touches the network, only the
/// `base_url` string itself — it's [`tcp_probe`] that opens the
/// connection.
///
/// **Bracketed IPv6 notation** (`[::1]` or `[::1]:8000`, RFC 3986
/// §3.2.2) handled separately, BEFORE the general `rsplit_once(':')`: a
/// bare IPv6 address itself contains `:` characters, so a plain
/// `rsplit_once(':')` would cut `[::1]:8000` on the last `:` inside the
/// brackets rather than on the host/port separator. `host` is returned
/// WITHOUT the brackets (`"::1"`, not `"[::1]"`): `Ipv6Addr::from_str`,
/// used by `ToSocketAddrs` in [`tcp_probe`], rejects the bracketed form
/// — keeping it would make any IPv6 resolution fail with a spurious DNS
/// error, never with the invalid-port message one would expect.
fn parse_host_port(base_url: &str) -> Result<(String, u16), String> {
    let Some((scheme, rest)) = base_url.split_once("://") else {
        return Err(format!(
            "base_url \"{base_url}\": missing scheme (expected \"http://\" or \"https://\")"
        ));
    };

    let default_port: u16 = match scheme {
        "http" => 80,
        "https" => 443,
        other => {
            return Err(format!(
                "base_url \"{base_url}\": unsupported scheme \"{other}\" (expected \"http\" or \
                 \"https\")"
            ));
        }
    };

    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if authority.is_empty() {
        return Err(format!("base_url \"{base_url}\": missing host"));
    }

    if let Some(after_bracket) = authority.strip_prefix('[') {
        return parse_ipv6_authority(base_url, after_bracket, default_port);
    }

    match authority.rsplit_once(':') {
        Some((host, port_text)) if !host.is_empty() => {
            let port: u16 = port_text
                .parse()
                .map_err(|_| format!("base_url \"{base_url}\": invalid port \"{port_text}\""))?;
            Ok((host.to_string(), port))
        }
        _ => Ok((authority.to_string(), default_port)),
    }
}

/// Complements [`parse_host_port`] for bracketed IPv6 notation:
/// `after_bracket` is what follows the opening `[` already consumed by
/// the caller (e.g. `"::1]:8000"` for `"[::1]:8000"`). Isolated in its
/// own function because [`parse_host_port`] already has two levels of
/// `match` / early-return; nesting it in place would hurt the
/// readability that `clippy::pedantic` (`too_many_lines`) would
/// otherwise penalize.
fn parse_ipv6_authority(
    base_url: &str,
    after_bracket: &str,
    default_port: u16,
) -> Result<(String, u16), String> {
    let Some(end) = after_bracket.find(']') else {
        return Err(format!(
            "base_url \"{base_url}\": unclosed opening IPv6 bracket \"[\""
        ));
    };
    // `end` points at `]` (ASCII, 1 byte): the two split bounds below
    // therefore always fall on a character boundary, whatever
    // `base_url`'s content around these brackets.
    let host = &after_bracket[..end];
    if host.is_empty() {
        return Err(format!(
            "base_url \"{base_url}\": empty IPv6 address between brackets"
        ));
    }
    let trailer = &after_bracket[end + 1..];

    match trailer.strip_prefix(':') {
        Some(port_text) if !port_text.is_empty() => port_text
            .parse()
            .map(|port| (host.to_string(), port))
            .map_err(|_| format!("base_url \"{base_url}\": invalid port \"{port_text}\"")),
        Some(_) => Err(format!("base_url \"{base_url}\": missing port after \":\"")),
        None if trailer.is_empty() => Ok((host.to_string(), default_port)),
        None => Err(format!(
            "base_url \"{base_url}\": unexpected characters after the IPv6 address \
             (\"{trailer}\")"
        )),
    }
}

/// Tests a backend's reachability with a TCP connection to its
/// `base_url` (point 4 of the shared contract), with a short timeout
/// ([`PROBE_TIMEOUT`]), then closes it immediately. NO HTTP request: a
/// `POST` on the `chat` operation would actually invoke the model, an
/// unacceptable side effect for a diagnostic command — this function
/// therefore only opens and closes a socket, never writing or reading a
/// single byte on it. The error message says "unreachable" on the
/// `doctor` side (via the label `"reachable"` — cf.
/// [`check_backends_reachable`]) and never "available": only a
/// socket's acceptance is checked, not the model's ability to respond.
///
/// # Errors
///
/// Returns `Err` if `base_url` does not have the expected shape, if
/// resolving the host/port pair fails, or if the connection itself
/// fails or times out.
pub fn tcp_probe(base_url: &str) -> Result<(), String> {
    let (host, port) = parse_host_port(base_url)?;

    let mut addrs = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|err| format!("address resolution for \"{host}:{port}\" failed: {err}"))?;

    let addr = addrs
        .next()
        .ok_or_else(|| format!("address resolution for \"{host}:{port}\" produced no address"))?;

    std::net::TcpStream::connect_timeout(&addr, PROBE_TIMEOUT)
        .map(|_stream| ())
        .map_err(|err| format!("TCP connection to \"{host}:{port}\" failed: {err}"))
}

#[cfg(test)]
#[allow(clippy::expect_used)] // allowed in tests (cf. Cargo.toml [lints.clippy]).
mod tests {
    use super::*;

    fn backend(id: &str, base_url: &str, operations: &[&str]) -> crate::config::Backend {
        crate::config::Backend {
            id: id.to_string(),
            base_url: base_url.to_string(),
            kind: "openai-compatible".to_string(),
            operations: operations
                .iter()
                .map(|op| {
                    (
                        (*op).to_string(),
                        crate::config::Operation {
                            method: "POST".to_string(),
                            path: format!("/{op}"),
                        },
                    )
                })
                .collect(),
        }
    }

    fn model(id: &str, backend: &str, operation: &str) -> crate::config::Model {
        crate::config::Model {
            id: id.to_string(),
            backend: backend.to_string(),
            operation: operation.to_string(),
            model: format!("{id}-underlying"),
            generation: crate::config::Generation::default(),
        }
    }

    fn command_spec(path: &[&str], model: &str) -> crate::command::CommandSpec {
        crate::command::CommandSpec {
            path: path.iter().map(ToString::to_string).collect(),
            description: String::new(),
            model: model.to_string(),
            input: crate::command::InputMode::Stdin,
            prompt: "{{ input }}".to_string(),
            args: std::collections::BTreeMap::new(),
            output: crate::output::OutputSpec::default(),
            file: std::path::PathBuf::new(),
        }
    }

    // `unnecessary_wraps`: these two functions are stub probes with a
    // FIXED signature (`&dyn Fn(&str) -> Result<(), String>`, cf.
    // `doctor`); they cannot be simplified without breaking that
    // signature.
    #[allow(clippy::unnecessary_wraps)]
    fn always_ok(_base_url: &str) -> Result<(), String> {
        Ok(())
    }

    fn always_fails(_base_url: &str) -> Result<(), String> {
        Err("connection refused".to_string())
    }

    // -- doctor: nominal scenario -----------------------------------------

    #[test]
    fn doctor_all_checks_passing_yields_no_failure_and_exit_code_zero() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "ovms".to_string(),
            backend("ovms", "http://127.0.0.1:8000", &["chat"]),
        );
        config
            .models
            .insert("qwen-fast".to_string(), model("qwen-fast", "ovms", "chat"));
        let commands = vec![command_spec(&["classify"], "qwen-fast")];

        let checks = doctor(Some(&config), Some(&commands), None, &always_ok);

        assert!(
            checks.iter().all(|c| matches!(c.status, Status::Ok)),
            "got: {checks:?}"
        );
        assert_eq!(doctor_exit_code(&checks), 0);
    }

    // -- (a) configuration loaded ------------------------------------------

    #[test]
    fn doctor_load_error_fails_configuration_check_only_and_exit_code_is_two() {
        let err = crate::Error::Config("broken command file".to_string());

        let checks = doctor(None, None, Some(&err), &always_ok);

        assert_eq!(
            checks.len(),
            1,
            "config/commands absent: only (a) must be produced, got: {checks:?}"
        );
        assert!(matches!(&checks[0].status, Status::Failed(_)));
        assert_eq!(doctor_exit_code(&checks), 2);
    }

    // -- (c) models ----------------------------------------------------------

    #[test]
    fn doctor_model_referencing_unknown_backend_fails_check_c_and_exit_code_is_two() {
        let mut config = crate::config::Config::default();
        config
            .models
            .insert("gpt".to_string(), model("gpt", "does-not-exist", "chat"));

        let checks = doctor(Some(&config), Some(&[]), None, &always_ok);

        let model_check = checks
            .iter()
            .find(|c| c.label.contains("gpt"))
            .expect("a check for model \"gpt\" must exist");
        assert!(matches!(&model_check.status, Status::Failed(_)));
        assert_eq!(doctor_exit_code(&checks), 2);
    }

    #[test]
    fn doctor_model_referencing_unexposed_operation_fails_check_c() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "ovms".to_string(),
            backend("ovms", "http://127.0.0.1:8000", &["chat"]),
        );
        config.models.insert(
            "whisper".to_string(),
            model("whisper", "ovms", "audio_transcriptions"),
        );

        let checks = doctor(Some(&config), Some(&[]), None, &always_ok);

        let model_check = checks
            .iter()
            .find(|c| c.label.contains("whisper"))
            .expect("a check for model \"whisper\" must exist");
        assert!(matches!(
            &model_check.status,
            Status::Failed(message) if message.contains("audio_transcriptions")
        ));
        assert_eq!(doctor_exit_code(&checks), 2);
    }

    // -- (d) commands / model ------------------------------------------------

    #[test]
    fn doctor_command_referencing_unknown_model_fails_check_d() {
        let config = crate::config::Config::default();
        let commands = vec![command_spec(&["classify"], "does-not-exist")];

        let checks = doctor(Some(&config), Some(&commands), None, &always_ok);

        let command_check = checks
            .iter()
            .find(|c| c.label.contains("classify") && c.label.contains("model"))
            .expect("a check (d) for \"classify\" must exist");
        assert!(matches!(&command_check.status, Status::Failed(_)));
        assert_eq!(doctor_exit_code(&checks), 2);
    }

    // -- (e) commands / output schema ---------------------------------------

    #[test]
    fn doctor_command_with_missing_schema_file_fails_check_e() {
        let mut spec = command_spec(&["classify"], "qwen-fast");
        spec.output = crate::output::OutputSpec {
            format: crate::output::Format::Json,
            schema: Some(std::path::PathBuf::from(
                "/does/not/exist/schema-not-found.json",
            )),
            max_lines: None,
        };
        let config = crate::config::Config::default();

        let checks = doctor(Some(&config), Some(&[spec]), None, &always_ok);

        let schema_check = checks
            .iter()
            .find(|c| c.label.contains("schema"))
            .expect("a check (e) must exist");
        assert!(matches!(&schema_check.status, Status::Failed(_)));
    }

    #[test]
    fn doctor_command_without_schema_produces_no_check_e() {
        let spec = command_spec(&["commit-message"], "qwen-fast"); // text format by default
        let config = crate::config::Config::default();

        let checks = doctor(Some(&config), Some(&[spec]), None, &always_ok);

        assert!(
            !checks.iter().any(|c| c.label.contains("schema")),
            "no check (e) must be produced in the absence of a schema, got: {checks:?}"
        );
    }

    // -- exit code: configuration takes priority over reachability -----

    #[test]
    fn doctor_reachability_failure_alone_yields_exit_code_three() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "ovms".to_string(),
            backend("ovms", "http://127.0.0.1:8000", &["chat"]),
        );

        let checks = doctor(Some(&config), Some(&[]), None, &always_fails);

        assert_eq!(doctor_exit_code(&checks), 3);
    }

    /// Regression: classification must come from [`CheckKind`], never
    /// from the label text. A reachability label deliberately reworded,
    /// without the word "reachable" nor the historical suffix, must
    /// still produce exit code 3 — text-based classification silently
    /// reclassified this case as a configuration failure (code 2).
    #[test]
    fn doctor_exit_code_uses_kind_not_label_text_for_reachability_failure() {
        let checks = vec![Check {
            kind: CheckKind::Reachability,
            label: "backend \"ovms\" status".to_string(),
            status: Status::Failed("connection refused".to_string()),
        }];

        assert_eq!(doctor_exit_code(&checks), 3);
    }

    #[test]
    fn doctor_config_failure_takes_priority_over_reachability_failure() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "ovms".to_string(),
            backend("ovms", "http://127.0.0.1:8000", &["chat"]),
        );
        // (c) fails: the model references a nonexistent backend.
        config.models.insert(
            "orphan".to_string(),
            model("orphan", "does-not-exist", "chat"),
        );

        // (b) also fails: the probe fails for every backend queried.
        let checks = doctor(Some(&config), Some(&[]), None, &always_fails);

        let reachability_failed = checks
            .iter()
            .any(|c| c.kind == CheckKind::Reachability && matches!(c.status, Status::Failed(_)));
        let config_check_failed = checks
            .iter()
            .any(|c| c.label.contains("orphan") && matches!(c.status, Status::Failed(_)));
        assert!(
            reachability_failed,
            "precondition: the probe must have failed"
        );
        assert!(
            config_check_failed,
            "precondition: check (c) must have failed"
        );

        assert_eq!(
            doctor_exit_code(&checks),
            2,
            "configuration must take priority over reachability (point 5 of the shared contract)"
        );
    }

    // -- format_doctor -----------------------------------------------------------

    #[test]
    fn format_doctor_uses_checkmark_for_ok_and_cross_with_message_for_failed() {
        let checks = vec![
            Check {
                kind: CheckKind::Config,
                label: "a".to_string(),
                status: Status::Ok,
            },
            Check {
                kind: CheckKind::Config,
                label: "b".to_string(),
                status: Status::Failed("boom".to_string()),
            },
        ];

        assert_eq!(format_doctor(&checks), "✓ a\n✗ b: boom");
    }

    // -- format_models -------------------------------------------------------------

    #[test]
    fn format_models_aligns_backend_column_across_rows_of_differing_name_length() {
        let mut config = crate::config::Config::default();
        config
            .models
            .insert("a".to_string(), model("a", "ovms", "chat"));
        config.models.insert(
            "much-longer-model-name".to_string(),
            model("much-longer-model-name", "ovms", "chat"),
        );

        let out = format_models(&config);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);

        let header_backend_at = lines[0].find("BACKEND").expect("BACKEND header");
        let row_a_backend_at = lines[1].find("ovms").expect("row \"a\"");
        let row_long_backend_at = lines[2].find("ovms").expect("row \"much-longer…\"");

        assert_eq!(header_backend_at, row_a_backend_at);
        assert_eq!(header_backend_at, row_long_backend_at);
    }

    #[test]
    fn format_models_sorts_deterministically_by_name() {
        let mut config = crate::config::Config::default();
        config
            .models
            .insert("zebra".to_string(), model("zebra", "ovms", "chat"));
        config
            .models
            .insert("alpha".to_string(), model("alpha", "ovms", "chat"));

        let out = format_models(&config);
        let lines: Vec<&str> = out.lines().collect();

        let alpha_at = lines
            .iter()
            .position(|l| l.starts_with("alpha"))
            .expect("\"alpha\" must be present");
        let zebra_at = lines
            .iter()
            .position(|l| l.starts_with("zebra"))
            .expect("\"zebra\" must be present");
        assert!(alpha_at < zebra_at);
    }

    #[test]
    fn format_models_with_no_models_prints_only_the_header_without_panicking() {
        let config = crate::config::Config::default();

        let out = format_models(&config);

        assert_eq!(out, "NAME  BACKEND  OPERATION");
    }

    // -- describe --------------------------------------------------------------

    fn sample_translate_spec() -> crate::command::CommandSpec {
        let mut args = std::collections::BTreeMap::new();
        args.insert(
            "language".to_string(),
            crate::command::ArgSpec {
                short: Some('l'),
                required: true,
                description: "Target language".to_string(),
            },
        );
        crate::command::CommandSpec {
            path: vec!["translate".to_string()],
            description: "Translate input text".to_string(),
            model: "qwen-fast".to_string(),
            input: crate::command::InputMode::StdinOrFile,
            prompt: "Translate {{ input }} to {{ args.language }}".to_string(),
            args,
            output: crate::output::OutputSpec {
                format: crate::output::Format::Json,
                schema: Some(std::path::PathBuf::from("schemas/translation.json")),
                max_lines: None,
            },
            file: std::path::PathBuf::from(".npu/commands/translate.md"),
        }
    }

    #[test]
    fn describe_produces_valid_json_with_declared_args_and_output_contract() {
        let spec = sample_translate_spec();

        let json_text = describe(&spec).expect("describe must succeed");
        let value: serde_json::Value =
            serde_json::from_str(&json_text).expect("describe must produce valid JSON");

        assert_eq!(value["name"], "translate");
        assert_eq!(value["description"], "Translate input text");
        assert_eq!(value["model"], "qwen-fast");
        assert_eq!(value["input"], "stdin_or_file");
        assert_eq!(value["args"]["language"]["short"], "l");
        assert_eq!(value["args"]["language"]["required"], true);
        assert_eq!(value["args"]["language"]["description"], "Target language");
        assert_eq!(value["output"]["format"], "json");
        assert_eq!(value["output"]["schema"], "schemas/translation.json");
    }

    #[test]
    fn describe_text_output_without_schema_has_a_null_schema_field() {
        let mut spec = sample_translate_spec();
        spec.output = crate::output::OutputSpec {
            format: crate::output::Format::Text,
            schema: None,
            max_lines: Some(1),
        };

        let json_text = describe(&spec).expect("describe must succeed");
        let value: serde_json::Value =
            serde_json::from_str(&json_text).expect("describe must produce valid JSON");

        assert_eq!(value["output"]["format"], "text");
        assert!(value["output"]["schema"].is_null());
        assert_eq!(value["output"]["max_lines"], 1);
    }

    // -- parse_host_port -------------------------------------------------------

    #[test]
    fn parse_host_port_defaults_http_to_port_80() {
        assert_eq!(
            parse_host_port("http://127.0.0.1").expect("must parse"),
            ("127.0.0.1".to_string(), 80)
        );
    }

    #[test]
    fn parse_host_port_defaults_https_to_port_443() {
        assert_eq!(
            parse_host_port("https://example.com").expect("must parse"),
            ("example.com".to_string(), 443)
        );
    }

    #[test]
    fn parse_host_port_explicit_port_is_used() {
        assert_eq!(
            parse_host_port("http://127.0.0.1:8000").expect("must parse"),
            ("127.0.0.1".to_string(), 8000)
        );
    }

    #[test]
    fn parse_host_port_tolerates_a_path_after_the_authority() {
        assert_eq!(
            parse_host_port("http://127.0.0.1:8000/v3/chat").expect("must parse"),
            ("127.0.0.1".to_string(), 8000)
        );
    }

    #[test]
    fn parse_host_port_missing_scheme_is_an_error() {
        assert!(parse_host_port("127.0.0.1:8000").is_err());
    }

    #[test]
    fn parse_host_port_unsupported_scheme_is_an_error() {
        assert!(parse_host_port("ftp://127.0.0.1:21").is_err());
    }

    // -- parse_host_port: bracketed IPv6 ----------------------------------

    #[test]
    fn parse_host_port_ipv6_with_explicit_port_strips_brackets_from_host() {
        // The returned host must be WITHOUT brackets: `Ipv6Addr::from_str`
        // (used by `ToSocketAddrs` in `tcp_probe`) rejects the bracketed
        // form, cf. `parse_ipv6_authority`'s doc.
        assert_eq!(
            parse_host_port("http://[::1]:8000").expect("must parse"),
            ("::1".to_string(), 8000)
        );
    }

    #[test]
    fn parse_host_port_ipv6_without_port_defaults_to_scheme_port() {
        assert_eq!(
            parse_host_port("https://[2001:db8::1]").expect("must parse"),
            ("2001:db8::1".to_string(), 443)
        );
    }

    #[test]
    fn parse_host_port_ipv6_tolerates_a_path_after_the_authority() {
        assert_eq!(
            parse_host_port("http://[::1]:8000/v3/chat").expect("must parse"),
            ("::1".to_string(), 8000)
        );
    }

    #[test]
    fn parse_host_port_ipv6_unclosed_bracket_is_an_error_not_a_panic() {
        assert!(parse_host_port("http://[::1:8000").is_err());
    }

    #[test]
    fn parse_host_port_ipv6_empty_brackets_is_an_error() {
        assert!(parse_host_port("http://[]:8000").is_err());
    }

    #[test]
    fn parse_host_port_ipv6_trailing_colon_without_port_is_an_error() {
        assert!(parse_host_port("http://[::1]:").is_err());
    }

    #[test]
    fn parse_host_port_ipv6_garbage_after_bracket_is_an_error_not_a_panic() {
        assert!(parse_host_port("http://[::1]garbage").is_err());
    }

    #[test]
    fn parse_host_port_ipv6_non_numeric_port_is_an_error() {
        assert!(parse_host_port("http://[::1]:notaport").is_err());
    }

    // -- tcp_probe --------------------------------------------------------------

    #[test]
    fn tcp_probe_succeeds_against_a_locally_bound_listener() {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind of the ephemeral listener");
        let addr = listener.local_addr().expect("listener's local address");
        let base_url = format!("http://{addr}");

        // Accepts the incoming connection then finishes: no thread nor
        // socket must survive this test.
        let acceptor = std::thread::spawn(move || {
            let _ = listener.accept();
        });

        let result = tcp_probe(&base_url);

        acceptor.join().expect("the acceptor thread must not panic");
        assert!(result.is_ok(), "got: {result:?}");
    }

    #[test]
    fn tcp_probe_fails_against_a_closed_port() {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind of the ephemeral listener");
        let addr = listener.local_addr().expect("listener's local address");
        drop(listener); // closes immediately: nobody is listening here anymore.

        let result = tcp_probe(&format!("http://{addr}"));

        assert!(
            result.is_err(),
            "a closed port must be reported as unreachable"
        );
    }
}
