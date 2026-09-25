//! Unified crate error.
//!
//! A hand-written enum rather than `anyhow`/`thiserror`: one variant per
//! failure family, mapped to the CLI's stable exit codes. Each family (save
//! `Update` and `Io`, which stay a plain string / the wrapped `io::Error`)
//! carries a small struct of STRUCTURED fields — `message`, plus whatever
//! identifies where the failure came from (a file, a backend id, a URL...).
//! `Display` centralizes the `<file>: ` prefix so every caller stops
//! formatting it by hand; the identifying fields are also there for tests
//! and callers that want them without parsing the message.
//! stdout stays reserved for the command's result; these messages are meant
//! for stderr.

use std::fmt;
use std::path::PathBuf;

/// The exit code of a configuration error, per the exit-code contract.
pub(crate) const CONFIG_EXIT: i32 = 2;
/// The exit code of a backend error, per the exit-code contract.
pub(crate) const BACKEND_EXIT: i32 = 3;
/// The exit code of an output-contract error, per the exit-code contract.
pub(crate) const OUTPUT_EXIT: i32 = 4;
/// The exit code of an I/O or update error, per the exit-code contract.
const IO_EXIT: i32 = 1;

/// An invalid or missing configuration (backends/models/commands).
#[derive(Debug)]
pub struct ConfigError {
    pub message: String,
    /// The configuration file this error is about, when the failure was
    /// found while reading one (a directory-level failure, e.g. "cannot
    /// read directory", still names a path here — just not a file).
    pub file: Option<PathBuf>,
    /// The backend/model/command identifier the message is about, when the
    /// failure has one (`Config::resolve` on an unknown model id has none
    /// to name a file with, but does have this).
    pub id: Option<String>,
}

impl ConfigError {
    /// A configuration error found while reading `file`, about `id` when
    /// one applies.
    pub(crate) fn in_file(
        file: impl Into<PathBuf>,
        id: Option<impl Into<String>>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            file: Some(file.into()),
            id: id.map(Into::into),
        }
    }

    /// A configuration error with no file to name (e.g. resolving an
    /// unknown model id against an already-loaded `Config`).
    pub(crate) fn bare(id: Option<impl Into<String>>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            file: None,
            id: id.map(Into::into),
        }
    }
}

/// A failure on the AI backend side (request, network, unexpected
/// response).
#[derive(Debug)]
pub struct BackendError {
    pub message: String,
    /// The backend identifier this error is about, when known.
    pub backend: Option<String>,
    /// The URL the request was sent to, when the failure happened after one
    /// was built.
    pub url: Option<String>,
    /// The HTTP status the backend answered with, for a non-2xx response.
    pub status: Option<u16>,
}

impl BackendError {
    /// A backend error naming the backend it came from.
    pub(crate) fn at(backend: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            backend: Some(backend.into()),
            url: None,
            status: None,
        }
    }

    /// The same, with the URL the request was sent to.
    pub(crate) fn at_url(
        backend: impl Into<String>,
        url: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            backend: Some(backend.into()),
            url: Some(url.into()),
            status: None,
        }
    }

    /// The same, with the HTTP status answered.
    pub(crate) fn at_status(
        backend: impl Into<String>,
        url: impl Into<String>,
        status: u16,
        message: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            backend: Some(backend.into()),
            url: Some(url.into()),
            status: Some(status),
        }
    }

    /// A backend error with no backend identifier available (e.g. a
    /// generic dispatch failure).
    pub(crate) fn bare(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            backend: None,
            url: None,
            status: None,
        }
    }
}

/// A failure producing or validating the output: the model's answer
/// violated the command's declared `[output]` contract.
#[derive(Debug)]
pub struct OutputError {
    pub message: String,
    /// The command file whose `[output]` contract was violated, when the
    /// failure is tied to one.
    pub command_file: Option<PathBuf>,
    /// The JSON Schema file involved, when the failure is about the schema
    /// itself rather than the answer.
    pub schema: Option<PathBuf>,
}

impl OutputError {
    /// An output error with no command file known yet (filled in later by
    /// [`InFile`] at the call site that does know it).
    pub(crate) fn bare(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            command_file: None,
            schema: None,
        }
    }
}

/// Unified crate error, one variant per exit-code family.
#[derive(Debug)]
pub enum Error {
    /// Failure while checking, downloading, verifying, or installing an update.
    Update(String),
    /// Invalid or missing configuration (backends/models/commands).
    Config(ConfigError),
    /// Failure on the AI backend side (request, network, unexpected response).
    Backend(BackendError),
    /// Failure producing or validating the output.
    Output(OutputError),
    /// System I/O error (file, stdin, ...).
    Io(std::io::Error),
}

impl Error {
    /// Process exit code associated with this error.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Update(_) | Error::Io(_) => IO_EXIT,
            Error::Config(_) => CONFIG_EXIT,
            Error::Backend(_) => BACKEND_EXIT,
            Error::Output(_) => OUTPUT_EXIT,
        }
    }

    /// Machine-readable diagnostic shared by CLI and MCP tool failures.
    #[must_use]
    pub fn envelope(&self) -> serde_json::Value {
        let (kind, fields) = match self {
            Self::Update(_) => ("update", serde_json::json!({})),
            Self::Io(_) => ("io", serde_json::json!({})),
            Self::Config(err) => (
                "config",
                serde_json::json!({
                    "file": err.file.as_ref().map(|path| path.display().to_string()),
                    "id": err.id,
                }),
            ),
            Self::Backend(err) => (
                "backend",
                serde_json::json!({
                    "backend": err.backend, "url": err.url, "status": err.status,
                }),
            ),
            Self::Output(err) => (
                "output",
                serde_json::json!({
                    "command_file": err.command_file.as_ref().map(|path| path.display().to_string()),
                    "schema": err.schema.as_ref().map(|path| path.display().to_string()),
                }),
            ),
        };
        let mut result = serde_json::json!({
            "kind": kind,
            "message": self.to_string(),
            "exit_code": self.exit_code(),
        });
        if let (Some(to), Some(from)) = (result.as_object_mut(), fields.as_object()) {
            to.extend(from.clone());
        }
        result
    }

    /// Shorthand for `Error::Config(ConfigError::bare(None::<String>, message))`,
    /// for the many call sites with no file or identifier to attach.
    pub(crate) fn config(message: impl Into<String>) -> Self {
        Error::Config(ConfigError::bare(None::<String>, message))
    }

    /// Shorthand for `Error::Backend(BackendError::bare(message))`.
    pub(crate) fn backend(message: impl Into<String>) -> Self {
        Error::Backend(BackendError::bare(message))
    }

    /// Shorthand for `Error::Output(OutputError::bare(message))`.
    pub(crate) fn output(message: impl Into<String>) -> Self {
        Error::Output(OutputError::bare(message))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Update(msg) => write!(f, "update error: {msg}"),
            Error::Config(ConfigError { message, file, .. }) => match file {
                Some(path) => write!(f, "configuration error: {}: {message}", path.display()),
                None => write!(f, "configuration error: {message}"),
            },
            Error::Backend(BackendError { message, .. }) => write!(f, "backend error: {message}"),
            Error::Output(OutputError {
                message,
                command_file,
                ..
            }) => match command_file {
                Some(path) => write!(f, "output error: {}: {message}", path.display()),
                None => write!(f, "output error: {message}"),
            },
            Error::Io(err) => write!(f, "I/O error: {err}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(err) => Some(err),
            Error::Update(_) | Error::Config(_) | Error::Backend(_) | Error::Output(_) => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err)
    }
}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Fills the `file` of a `Config`/`Output` error when the code that produced
/// it does not know which file is at fault, but its caller does.
///
/// Used by callers of `prompt::*` and `output::*`: those modules know
/// templates and schemas, not files, so their own errors carry `file: None`;
/// the command-file-aware caller then attaches it here instead of
/// re-formatting the message with its own "{path}: " prefix.
pub(crate) trait InFile {
    /// Attaches `path` to the error's `file`/`command_file`, if it does not
    /// already have one. A `Backend`/`Update`/`Io` error, or one that
    /// already names a file, is returned unchanged.
    fn in_file(self, path: &std::path::Path) -> Self;
}

impl<T> InFile for std::result::Result<T, Error> {
    fn in_file(self, path: &std::path::Path) -> Self {
        self.map_err(|err| match err {
            Error::Config(mut config_err) => {
                if config_err.file.is_none() {
                    config_err.file = Some(path.to_path_buf());
                }
                Error::Config(config_err)
            }
            Error::Output(mut output_err) => {
                if output_err.command_file.is_none() {
                    output_err.command_file = Some(path.to_path_buf());
                }
                Error::Output(output_err)
            }
            other => other,
        })
    }
}

/// Formats a list of available identifiers for an actionable error
/// message: sorted, comma-joined, or `"none"` if empty.
///
/// Centralizes an idiom repeated identically in `config::resolve` (models,
/// backends), `backend::chat` (operations) and `run` (commands), so that
/// error messages stay consistent across modules.
pub(crate) fn format_available<S: AsRef<str>>(ids: impl Iterator<Item = S>) -> String {
    let mut sorted: Vec<String> = ids.map(|s| s.as_ref().to_string()).collect();
    sorted.sort_unstable();
    if sorted.is_empty() {
        "none".to_string()
    } else {
        sorted.join(", ")
    }
}

/// The shape of a `clap` USAGE error printed on stderr: plain text
/// (`clap`'s own rendering, unchanged) or a one-line JSON envelope
/// (`--error-format json`). Only a usage error is ever affected — `--help`
/// and `--version` (`clap::error::ErrorKind::DisplayHelp` /
/// `DisplayVersion`) keep their normal stdout rendering and exit `0`
/// whatever this format is, since they are not errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorFormat {
    Text,
    Json,
}

impl ErrorFormat {
    /// The accepted `--error-format` values, in the order `--help` should
    /// list them. Shared with `cli::mod`, which builds the argument from
    /// it, same idiom as `log::Level::NAMES`.
    pub const NAMES: [&'static str; 2] = ["text", "json"];
}

impl std::str::FromStr for ErrorFormat {
    type Err = ();

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "text" => Ok(ErrorFormat::Text),
            "json" => Ok(ErrorFormat::Json),
            _ => Err(()),
        }
    }
}

/// Reads `--error-format` from the RAW command line, before `clap` has
/// parsed anything — same reason and same idiom as
/// `log::level_from_args`: the case this flag exists for (a `clap` usage
/// error) is raised by `clap` itself while parsing, before a declared
/// argument's value could ever be read back from `ArgMatches`.
///
/// Deliberately permissive: an unknown or missing value falls back to
/// [`ErrorFormat::Text`] instead of failing — `clap` diagnoses an invalid
/// value itself, with its own message, once parsing actually runs.
#[must_use]
pub fn error_format_from_args<I, S>(args: I) -> ErrorFormat
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut format = None;
    let mut expecting_value = false;

    for arg in args {
        let arg = arg.as_ref();
        // `--` ends option parsing: a literal positional spelled
        // `--error-format` after it must never be read as this flag.
        if arg == "--" {
            break;
        }
        if expecting_value {
            format = arg.parse().ok();
            expecting_value = false;
        } else if arg == "--error-format" {
            expecting_value = true;
        } else if let Some(value) = arg.strip_prefix("--error-format=") {
            format = value.parse().ok();
        }
    }

    format.unwrap_or(ErrorFormat::Text)
}

/// Renders a `clap` USAGE error under `format`: `clap`'s own rendering
/// unchanged for [`ErrorFormat::Text`], or a one-line JSON envelope
/// (`{"kind":"usage","message":"..."}`) for [`ErrorFormat::Json`] — never
/// applied to `DisplayHelp`/`DisplayVersion`, which the caller filters out
/// before reaching here (see `run`'s doc).
#[must_use]
pub fn render_clap_usage_error(err: &clap::Error, format: ErrorFormat) -> String {
    match format {
        // `clap`'s own rendering already ends with a newline.
        ErrorFormat::Text => err.render().to_string(),
        ErrorFormat::Json => {
            // `clap`'s own rendering carries ANSI styling and the full
            // usage block; the envelope keeps only the message a machine
            // needs, stripped of styling, on ONE line (no embedded
            // newline) so a line-oriented reader never has to buffer.
            let message = err
                .render()
                .to_string()
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            format!(
                "{}\n",
                serde_json::json!({ "kind": "usage", "message": message })
            )
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // tolerated in tests (cf. Cargo.toml [lints.clippy]).
mod tests {
    use super::*;

    #[test]
    fn pipeline_errors_have_a_machine_readable_envelope() {
        let error = Error::Backend(BackendError::at_status(
            "local",
            "http://localhost",
            503,
            "unavailable",
        ));
        let envelope = error.envelope();
        assert_eq!(envelope["kind"], "backend");
        assert_eq!(envelope["exit_code"], 3);
        assert_eq!(envelope["backend"], "local");
        assert_eq!(envelope["status"], 503);
    }

    #[test]
    fn exit_codes_match_contract() {
        assert_eq!(Error::Update("x".into()).exit_code(), 1);
        assert_eq!(Error::Io(std::io::Error::other("x")).exit_code(), 1);
        assert_eq!(Error::config("x").exit_code(), 2);
        assert_eq!(Error::backend("x").exit_code(), 3);
        assert_eq!(Error::output("x").exit_code(), 4);
    }

    #[test]
    fn from_io_error_wraps() {
        let io_err = std::io::Error::other("boom");
        let err: Error = io_err.into();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn format_available_sorts_and_joins() {
        assert_eq!(format_available(["b", "a", "c"].into_iter()), "a, b, c");
    }

    #[test]
    fn format_available_empty_is_none() {
        assert_eq!(format_available(std::iter::empty::<&str>()), "none");
    }

    #[test]
    fn config_display_with_file_names_it_once() {
        let err = Error::Config(ConfigError::in_file(
            "backends/b.toml",
            Some("b"),
            "bad thing",
        ));
        let rendered = err.to_string();
        assert_eq!(rendered, "configuration error: backends/b.toml: bad thing");
        assert!(
            matches!(&err, Error::Config(e) if e.file.as_deref() == Some(std::path::Path::new("backends/b.toml")))
        );
        assert!(matches!(&err, Error::Config(e) if e.id.as_deref() == Some("b")));
    }

    #[test]
    fn config_display_without_file_has_no_colon_prefix() {
        let err = Error::config("no such model");
        assert_eq!(err.to_string(), "configuration error: no such model");
    }

    #[test]
    fn output_display_with_command_file_names_it_once() {
        let err = Error::Output(OutputError {
            message: "bad schema".to_string(),
            command_file: Some("commands/x.md".into()),
            schema: None,
        });
        assert_eq!(err.to_string(), "output error: commands/x.md: bad schema");
    }

    #[test]
    fn backend_display_carries_structured_fields() {
        let err = Error::Backend(BackendError::at_status(
            "ovms",
            "http://x/y",
            500,
            "server error",
        ));
        assert_eq!(err.to_string(), "backend error: server error");
        assert!(matches!(&err, Error::Backend(e) if e.backend.as_deref() == Some("ovms")));
        assert!(matches!(&err, Error::Backend(e) if e.url.as_deref() == Some("http://x/y")));
        assert!(matches!(&err, Error::Backend(e) if e.status == Some(500)));
    }

    #[test]
    fn in_file_fills_an_absent_file_but_not_an_existing_one() {
        let err: Result<()> = Err(Error::config("boom")).in_file(std::path::Path::new("a.toml"));
        assert!(
            matches!(err, Err(Error::Config(e)) if e.file.as_deref() == Some(std::path::Path::new("a.toml")))
        );

        let already_named: Result<()> = Err(Error::Config(ConfigError::in_file(
            "first.toml",
            None::<String>,
            "boom",
        )))
        .in_file(std::path::Path::new("second.toml"));
        assert!(
            matches!(already_named, Err(Error::Config(e)) if e.file.as_deref() == Some(std::path::Path::new("first.toml")))
        );
    }

    #[test]
    fn error_format_from_args_reads_the_value() {
        assert_eq!(
            error_format_from_args(["npu", "--error-format", "json", "x"]),
            ErrorFormat::Json
        );
        assert_eq!(
            error_format_from_args(["npu", "--error-format=json", "x"]),
            ErrorFormat::Json
        );
    }

    /// `--` ends option parsing: a literal positional spelled
    /// `--error-format` after it must never be read as the flag.
    #[test]
    fn error_format_from_args_stops_at_a_double_dash() {
        assert_eq!(
            error_format_from_args(["npu", "--", "--error-format", "json"]),
            ErrorFormat::Text
        );
        assert_eq!(
            error_format_from_args(["npu", "--error-format", "json", "--", "x"]),
            ErrorFormat::Json,
            "a value read BEFORE the -- is still honoured"
        );
    }

    #[test]
    fn error_format_from_args_defaults_to_text() {
        assert_eq!(error_format_from_args(["npu", "x"]), ErrorFormat::Text);
        assert_eq!(
            error_format_from_args(["npu", "--error-format", "not-a-format"]),
            ErrorFormat::Text
        );
    }

    #[test]
    fn render_clap_usage_error_json_is_one_line_valid_json_naming_the_kind() {
        let cli = clap::Command::new("npu").subcommand(clap::Command::new("x"));
        let err = cli
            .try_get_matches_from(["npu", "does-not-exist"])
            .expect_err("an unknown subcommand must be a clap usage error");

        let rendered = render_clap_usage_error(&err, ErrorFormat::Json);
        assert_eq!(rendered.lines().count(), 1);
        let parsed: serde_json::Value =
            serde_json::from_str(rendered.trim()).expect("must be valid JSON");
        assert_eq!(parsed["kind"], "usage");
        assert!(parsed["message"].is_string());
    }

    #[test]
    fn render_clap_usage_error_text_is_clap_s_own_rendering() {
        let cli = clap::Command::new("npu").subcommand(clap::Command::new("x"));
        let err = cli
            .try_get_matches_from(["npu", "does-not-exist"])
            .expect_err("an unknown subcommand must be a clap usage error");

        let rendered = render_clap_usage_error(&err, ErrorFormat::Text);
        assert_eq!(rendered, err.render().to_string());
    }
}
