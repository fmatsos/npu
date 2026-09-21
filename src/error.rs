//! Unified crate error.
//!
//! A hand-written enum rather than `anyhow`/`thiserror`: four variants,
//! one exit code each.
//! stdout stays reserved for the command's result; these messages are meant
//! for stderr.

use std::fmt;

/// Unified crate error, one variant per exit-code family.
#[derive(Debug)]
pub enum Error {
    /// Invalid or missing configuration (backends/models/commands).
    Config(String),
    /// Failure on the AI backend side (request, network, unexpected response).
    Backend(String),
    /// Failure producing or validating the output.
    Output(String),
    /// System I/O error (file, stdin, ...).
    Io(std::io::Error),
}

impl Error {
    /// Process exit code associated with this error.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Io(_) => 1,
            Error::Config(_) => 2,
            Error::Backend(_) => 3,
            Error::Output(_) => 4,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(msg) => write!(f, "configuration error: {msg}"),
            Error::Backend(msg) => write!(f, "backend error: {msg}"),
            Error::Output(msg) => write!(f, "output error: {msg}"),
            Error::Io(err) => write!(f, "I/O error: {err}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(err) => Some(err),
            Error::Config(_) | Error::Backend(_) | Error::Output(_) => None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_contract() {
        assert_eq!(Error::Io(std::io::Error::other("x")).exit_code(), 1);
        assert_eq!(Error::Config("x".into()).exit_code(), 2);
        assert_eq!(Error::Backend("x".into()).exit_code(), 3);
        assert_eq!(Error::Output("x".into()).exit_code(), 4);
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
}
