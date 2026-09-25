//! The `Backend` type: an AI backend configured in `backends/*.toml`, plus
//! the validation that runs on it after scope merging.

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use super::port::Port;
use super::runtime::{Docker, Runtime, docker_of, process_of};

/// An HTTP operation exposed by a backend (e.g. `chat`).
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Operation {
    #[cfg_attr(test, schemars(extend("pattern" = "^[Pp][Oo][Ss][Tt]$")))]
    pub method: String,
    pub path: String,
}

/// Per-backend request timeout override, declared by the optional
/// `[timeouts]` table of `backends/*.toml`. Absent, `backend.rs` falls back
/// to its own default.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Timeouts {
    /// Seconds granted to a request before failure. Must be non-zero: a
    /// backend that declares `[timeouts]` without meaning to bound anything
    /// should omit the table instead.
    pub request_secs: u64,
}

/// An AI backend configured in `backends/*.toml`.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Backend {
    pub id: String,
    pub base_url: String,
    /// TOML field `type` (a reserved Rust word), e.g. `"openai-compatible"`.
    #[serde(rename = "type")]
    #[cfg_attr(test, schemars(extend("enum" = ["openai-compatible"])))]
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
    #[cfg_attr(test, schemars(extend("deprecated" = true)))]
    pub(crate) docker: Option<Docker>,
    /// Optional: overrides `backend::REQUEST_TIMEOUT` for this backend.
    #[serde(default)]
    pub timeouts: Option<Timeouts>,
    /// Optional, `false` by default: the backend accepts an `OpenAI`
    /// `response_format` of type `json_schema`, so a command's
    /// `[output].schema` is SENT with the request and constrains the
    /// model's answer. Off, the schema is only checked after the answer
    /// arrives — a server that rejects the field must never receive it.
    #[serde(default)]
    pub structured_output: bool,
    /// Optional `[headers]` table: extra HTTP headers sent with every
    /// `chat` request to this backend (e.g. `Authorization = "Bearer {{
    /// env.OPENAI_API_KEY }}"`). Values are templates accepting ONLY
    /// `{{ env.NAME }}` (validated by [`validate_headers`]); the environment
    /// variable is resolved at preflight, before the input is read. Header
    /// names are validated at load time and `Content-Type`/`Content-Length`
    /// are rejected: `ureq` sets both itself, and a silently overridden
    /// header is a key read and ignored.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// The file this backend was loaded from, filled in by [`super::load_scopes`]
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
        matches!(&self.port, Some(Port::Keyword(keyword)) if keyword == super::port::PORT_AUTO)
    }
}

/// The backend types `backend.rs` knows how to talk to. `Backend.kind`
/// stays a string for the error message; [`validate_backend`] parses it, so
/// a new type is a variant here and a compile error wherever it must be
/// handled, never a value `backend.rs` silently treats as another:
/// `backend::chat` matches on it.
pub(crate) enum BackendKind {
    OpenAiCompatible,
}

impl BackendKind {
    const SUPPORTED: &str = "openai-compatible";

    pub(crate) fn parse(kind: &str) -> Option<Self> {
        (kind == Self::SUPPORTED).then_some(Self::OpenAiCompatible)
    }
}

/// The HTTP methods `backend.rs` can send; `backend::chat` matches on it
/// to pick the request builder. Parsed without regard to case, as `"post"`
/// has always been accepted.
pub(crate) enum Method {
    Post,
}

impl Method {
    const SUPPORTED: &str = "POST";

    pub(crate) fn parse(method: &str) -> Option<Self> {
        method
            .eq_ignore_ascii_case(Self::SUPPORTED)
            .then_some(Self::Post)
    }
}

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
            crate::Error::Config(config_err) => config_err.message,
            other => other.to_string(),
        };
        crate::Error::Config(crate::error::ConfigError::in_file(
            source,
            Some(backend_id),
            format!("backend \"{backend_id}\": [runtime]: {detail}"),
        ))
    })?;

    for placeholder in found {
        match placeholder {
            crate::prompt::Placeholder::Env(_) => {}
            crate::prompt::Placeholder::Arg(name) if name == RUNTIME_PLACEHOLDER_ARG => {}
            crate::prompt::Placeholder::Arg(name) => {
                return Err(crate::Error::Config(crate::error::ConfigError::in_file(
                    source,
                    Some(backend_id),
                    format!(
                        "backend \"{backend_id}\": [runtime] references unknown argument \
                         \"{name}\" (only \"{RUNTIME_PLACEHOLDER_ARG}\" is available here)"
                    ),
                )));
            }
            crate::prompt::Placeholder::Input => {
                return Err(crate::Error::Config(crate::error::ConfigError::in_file(
                    source,
                    Some(backend_id),
                    format!(
                        "backend \"{backend_id}\": [runtime] references {{{{ input }}}}, which \
                         has no meaning for a runtime start"
                    ),
                )));
            }
            crate::prompt::Placeholder::Schema(id) => {
                return Err(crate::Error::Config(crate::error::ConfigError::in_file(
                    source,
                    Some(backend_id),
                    format!(
                        "backend \"{backend_id}\": [runtime] references {{{{ schemas.{id} }}}}, \
                         which has no meaning for a runtime start"
                    ),
                )));
            }
            crate::prompt::Placeholder::Partial(id) => {
                return Err(crate::Error::Config(crate::error::ConfigError::in_file(
                    source,
                    Some(backend_id),
                    format!(
                        "backend \"{backend_id}\": [runtime] references {{{{ partials.{id} }}}}, \
                         which has no meaning for a runtime start"
                    ),
                )));
            }
        }
    }
    Ok(())
}

/// Header names `ureq` sets itself for every request body it sends as
/// JSON: silently overriding either from `[headers]` would be a key read
/// and ignored, so both are rejected at load time instead.
const RESERVED_HEADER_NAMES: [&str; 2] = ["content-type", "content-length"];

/// Whether `c` is a legal RFC 9110 `tchar` (the character set an HTTP field
/// name may use).
fn is_tchar(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

/// Validates one `[headers]` name: non-empty, made only of RFC 9110 `tchar`
/// characters, and not one `ureq` already sets itself.
fn validate_header_name(name: &str, backend_id: &str, source: &Path) -> crate::Result<()> {
    if name.is_empty() || !name.chars().all(is_tchar) {
        return Err(crate::Error::Config(crate::error::ConfigError::in_file(
            source,
            Some(backend_id),
            format!(
                "backend \"{backend_id}\": [headers] name \"{name}\" is not a legal HTTP \
                 header name (RFC 9110 token characters only)"
            ),
        )));
    }
    if RESERVED_HEADER_NAMES.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(crate::Error::Config(crate::error::ConfigError::in_file(
            source,
            Some(backend_id),
            format!(
                "backend \"{backend_id}\": [headers] cannot set \"{name}\": npu owns this \
                 header (set by the HTTP client for every JSON request)"
            ),
        )));
    }
    Ok(())
}

/// Validates one `[headers]` VALUE template: only `{{ env.NAME }}` is
/// accepted (the same reasoning as `[runtime]`'s templates, but even more
/// restricted — a header cannot depend on the command's input, its
/// arguments or a schema, none of which exist yet when the backend is
/// loaded nor make sense as an HTTP header).
fn validate_header_value(
    template: &str,
    name: &str,
    backend_id: &str,
    source: &Path,
) -> crate::Result<()> {
    let found = crate::prompt::placeholders(template).map_err(|err| {
        let detail = match err {
            crate::Error::Config(config_err) => config_err.message,
            other => other.to_string(),
        };
        crate::Error::Config(crate::error::ConfigError::in_file(
            source,
            Some(backend_id),
            format!("backend \"{backend_id}\": [headers].{name}: {detail}"),
        ))
    })?;

    for placeholder in found {
        if !matches!(placeholder, crate::prompt::Placeholder::Env(_)) {
            return Err(crate::Error::Config(crate::error::ConfigError::in_file(
                source,
                Some(backend_id),
                format!(
                    "backend \"{backend_id}\": [headers].{name}: only {{{{ env.NAME }}}} is \
                     accepted here (a header cannot depend on the command's input, arguments \
                     or schemas)"
                ),
            )));
        }
    }
    Ok(())
}

/// Validates the optional `[headers]` table of a backend: legal names,
/// `Content-Type`/`Content-Length` rejected, no two names colliding once
/// lower-cased (two TOML keys folding onto the same HTTP header would leave
/// one of them silently overridden), and values restricted to
/// `{{ env.NAME }}`.
fn validate_headers(backend: &Backend, source: &Path) -> crate::Result<()> {
    let mut seen: HashMap<String, &str> = HashMap::new();
    for (name, value) in &backend.headers {
        validate_header_name(name, &backend.id, source)?;
        validate_header_value(value, name, &backend.id, source)?;
        if let Some(existing) = seen.insert(name.to_ascii_lowercase(), name) {
            return Err(crate::Error::Config(crate::error::ConfigError::in_file(
                source,
                Some(&backend.id),
                format!(
                    "backend \"{}\": [headers] \"{existing}\" and \"{name}\" collide once \
                     case is ignored (HTTP header names are case-insensitive)",
                    backend.id
                ),
            )));
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
        return Err(crate::Error::Config(crate::error::ConfigError::in_file(
            source,
            Some(&backend.id),
            format!(
                "backend \"{}\": declaring [docker] requires an identifier usable as a \
                 container name (ASCII letters, digits, \"_\", \".\" and \"-\", starting with a \
                 letter or a digit)",
                backend.id
            ),
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
        return Err(crate::Error::Config(crate::error::ConfigError::in_file(
            source,
            Some(&backend.id),
            format!(
                "backend \"{}\": [runtime] type = \"{}\" is not supported on Windows (no state \
                 directory convention, and no SIGTERM to stop a server with) — use type = \"{}\", \
                 or start the server outside npu",
                backend.id,
                crate::runtime::process::NAME,
                crate::runtime::docker::NAME
            ),
        )));
    }

    // A `base_url` the readiness probe can never parse is a defect of this
    // file, and it is only this family that turns it into a kill: `serve`
    // spawns the server, probes an address that cannot resolve for the whole
    // `startup_timeout_secs`, then SIGTERM/SIGKILLs a perfectly working
    // process and blames the budget. Rejected here, it is exit `2` naming
    // the file, before anything is started.
    if let Err(detail) = crate::builtin::parse_host_port(&backend.base_url) {
        return Err(crate::Error::Config(crate::error::ConfigError::in_file(
            source,
            Some(&backend.id),
            format!(
                "backend \"{}\": {detail} — a process runtime is started only once its \
                 base_url answers, so an address npu cannot parse can never be satisfied",
                backend.id
            ),
        )));
    }

    if !is_valid_runtime_id(&backend.id) {
        return Err(crate::Error::Config(crate::error::ConfigError::in_file(
            source,
            Some(&backend.id),
            format!(
                "backend \"{}\": declaring a process runtime requires an identifier usable as a \
                 state file name (ASCII letters, digits, \"_\", \".\" and \"-\", starting with a \
                 letter or a digit)",
                backend.id
            ),
        )));
    }

    // Same rule as `[timeouts].request_secs`: a budget of zero is honoured
    // literally, so `serve` could never succeed — and a key read then
    // clamped would be a key silently ignored.
    if process.startup_timeout_secs == 0 {
        return Err(crate::Error::Config(crate::error::ConfigError::in_file(
            source,
            Some(&backend.id),
            format!(
                "backend \"{}\": [runtime].startup_timeout_secs must be greater than 0 — \
                 a zero budget leaves no time for anything to start",
                backend.id
            ),
        )));
    }

    // And an upper bound, for a reason the zero check shares: `serve` builds
    // its deadline as `Instant::now() + Duration::from_secs(value)`, which
    // PANICS on a large one — exit `101` and a panic message, in place of
    // the exit-code contract this CLI is driven by. A day is already longer
    // than any start worth waiting for.
    if process.startup_timeout_secs > MAX_STARTUP_TIMEOUT_SECS {
        return Err(crate::Error::Config(crate::error::ConfigError::in_file(
            source,
            Some(&backend.id),
            format!(
                "backend \"{}\": [runtime].startup_timeout_secs must be at most \
                 {MAX_STARTUP_TIMEOUT_SECS} (a day); npu cannot build a deadline out of \
                 {}",
                backend.id, process.startup_timeout_secs
            ),
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

/// Validates that a loaded backend only declares currently supported
/// properties: `type = "openai-compatible"` and `method = "POST"` for each of
/// its operations. Any other value is a configuration error detected at
/// load time, not a feature to implement.
///
/// Runs AFTER scope merging (cf. `super::load_scopes`), on surviving entries
/// only: an invalid backend from a general scope, entirely replaced by a
/// more local scope, must never reach this function.
/// `source` is the path of the file the surviving entry comes from, so that
/// the message names the file the user must actually fix.
pub(crate) fn validate_backend(backend: &Backend, source: &Path) -> crate::Result<()> {
    let Some(BackendKind::OpenAiCompatible) = BackendKind::parse(&backend.kind) else {
        return Err(crate::Error::Config(crate::error::ConfigError::in_file(
            source,
            Some(&backend.id),
            format!(
                "backend \"{}\": type \"{}\" not supported (only \"{}\" \
                 is supported)",
                backend.id,
                backend.kind,
                BackendKind::SUPPORTED
            ),
        )));
    };

    for (operation_name, operation) in &backend.operations {
        let Some(Method::Post) = Method::parse(&operation.method) else {
            return Err(crate::Error::Config(crate::error::ConfigError::in_file(
                source,
                Some(&backend.id),
                format!(
                    "backend \"{}\", operation \"{operation_name}\": method \"{}\" not \
                     supported (only \"{}\" is supported)",
                    backend.id,
                    operation.method,
                    Method::SUPPORTED
                ),
            )));
        };
    }

    if let Some(timeouts) = &backend.timeouts
        && timeouts.request_secs == 0
    {
        return Err(crate::Error::Config(crate::error::ConfigError::in_file(
            source,
            Some(&backend.id),
            format!(
                "backend \"{}\": [timeouts].request_secs must be greater than 0",
                backend.id
            ),
        )));
    }

    validate_docker(backend, source)?;
    validate_process(backend, source)?;
    validate_headers(backend, source)?;

    Ok(())
}

/// Resolves the `[headers]` table of `backend` for one request: each
/// `{{ env.NAME }}` template is substituted with the variable's actual
/// value, via the injected `env` (never `std::env::var` directly, for the
/// same testability reason as `prompt::render`). An undefined variable is
/// `Error::Config` naming the file and the header — resolved at preflight,
/// before the input is read (see `exec::execute_business_command`'s
/// invariant), so `git diff | npu ...` fails before the diff is consumed.
/// A resolved value containing a CR, LF or NUL is also rejected here: `ureq`
/// would otherwise fail later, at request time, with a less actionable
/// message (exit 3 instead of 2) for what is a configuration defect.
pub fn resolve_headers(
    backend: &Backend,
    env: &dyn Fn(&str) -> Option<String>,
) -> crate::Result<BTreeMap<String, String>> {
    let mut resolved = BTreeMap::new();
    for (name, template) in &backend.headers {
        let none = BTreeMap::new();
        let value = crate::prompt::render(template, "", &none, env, &none, &none).map_err(
            |err| match err {
                crate::Error::Config(config_err) => {
                    crate::Error::Config(crate::error::ConfigError::in_file(
                        &backend.source,
                        Some(&backend.id),
                        format!(
                            "backend \"{}\": [headers].{name}: {}",
                            backend.id, config_err.message
                        ),
                    ))
                }
                other => other,
            },
        )?;
        // RFC 9110 field-value: visible characters, space and HTAB; every
        // other control byte (CR, LF, NUL, VT, DEL...) is refused here, as
        // a configuration error, rather than by the HTTP client later.
        if value.chars().any(|c| c != '\t' && c.is_ascii_control()) {
            return Err(crate::Error::Config(crate::error::ConfigError::in_file(
                &backend.source,
                Some(&backend.id),
                format!(
                    "backend \"{}\": [headers].{name} resolves to a value containing a \
                     control character, which is not a legal HTTP header value",
                    backend.id
                ),
            )));
        }
        resolved.insert(name.clone(), value);
    }
    Ok(resolved)
}
