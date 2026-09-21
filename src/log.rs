//! Diagnostic logging, on STDERR only.
//!
//! The contract rule this module exists to respect: **stdout carries the
//! command result and nothing else**. A trace is not a result — it is how
//! the engine explains what it did — so every line this module writes goes
//! to stderr, on every level, including `info`.
//!
//! Three levels, ordered by decreasing severity: `error` < `warn` < `info`.
//! `--verbose <level>` sets the threshold, and everything at or above it is
//! printed. The default is `warn`, which is what the CLI already printed
//! before this module existed (engine warnings such as an unloadable
//! configuration): asking for `info` adds a trace, it never silences one.
//!
//! Errors returned as `Err` are NOT logged here: `main` prints them, once,
//! whatever the level. Logging them a second time at `error` level would
//! double every failure message.

/// Verbosity threshold, from the least to the most talkative.
///
/// `Ord` is derived on purpose: [`Logger::enabled`] is a comparison, not a
/// table of special cases — adding a level cannot forget a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Failures only.
    Error,
    /// Failures, plus what the engine had to work around. The default.
    Warn,
    /// Everything: what was resolved, what was sent where, what came back.
    Info,
}

impl Level {
    /// The accepted values, in the order `--help` should list them. Shared
    /// with `lib.rs`, which builds the `--verbose` argument from it: the CLI
    /// cannot accept a value this module does not know, nor the reverse.
    pub const NAMES: [&'static str; 3] = ["error", "warn", "info"];

    /// The default threshold, applied when `--verbose` is absent.
    pub const DEFAULT: &'static str = "warn";

    /// The label printed in front of a message.
    fn label(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
        }
    }
}

impl std::str::FromStr for Level {
    type Err = crate::Error;

    /// Parses a `--verbose` value. `clap` already restricts the value to
    /// [`Level::NAMES`], so the error branch is defense in depth rather than
    /// an expected path — it must still name the offending value and the
    /// accepted ones, never panic.
    fn from_str(value: &str) -> crate::Result<Self> {
        match value {
            "error" => Ok(Level::Error),
            "warn" => Ok(Level::Warn),
            "info" => Ok(Level::Info),
            other => Err(crate::Error::Config(format!(
                "unknown verbosity level \"{other}\" (accepted levels: {})",
                Level::NAMES.join(", ")
            ))),
        }
    }
}

/// Reads the threshold from the RAW command line, before `clap` has parsed
/// anything.
///
/// Needed because the CLI's first diagnostic (an unloadable configuration)
/// is emitted before `get_matches()` — `clap`'s `--help` exits the process
/// internally, so a warning printed after parsing would never appear for
/// `npu --help`. Without this function, `--verbose error` would silence
/// every trace except that one, and the flag would be lying.
///
/// Deliberately permissive: an unknown or missing value falls back to
/// [`Level::DEFAULT`] instead of failing. This is not where an invalid value
/// is diagnosed — `clap` rejects it a few lines later, with its own message
/// and the usage text. Two diagnostics for one typo would be worse than one.
#[must_use]
pub fn level_from_args<I, S>(args: I) -> Level
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut level = None;
    let mut expecting_value = false;

    for arg in args {
        let arg = arg.as_ref();
        if expecting_value {
            level = arg.parse().ok();
            expecting_value = false;
        } else if arg == "--verbose" || arg == "-v" {
            expecting_value = true;
        } else if let Some(value) = arg
            .strip_prefix("--verbose=")
            .or_else(|| arg.strip_prefix("-v="))
            // `clap` also accepts a short option attached to its value
            // (`-vinfo`): missing it here would make `--verbose error` print
            // the very warning it asked to silence, for that spelling only.
            .or_else(|| arg.strip_prefix("-v"))
        {
            level = value.parse().ok();
        }
    }

    level.unwrap_or_else(|| {
        // `Level::DEFAULT` is one of `Level::NAMES`, so this parse cannot
        // fail; falling back on `Warn` rather than unwrapping keeps the
        // function panic-free whatever a later edit does to those constants.
        Level::DEFAULT.parse().unwrap_or(Level::Warn)
    })
}

/// Writes diagnostics to stderr, above a threshold.
#[derive(Debug, Clone, Copy)]
pub struct Logger {
    level: Level,
}

impl Logger {
    #[must_use]
    pub fn new(level: Level) -> Self {
        Self { level }
    }

    /// Is `level` printed with this threshold? Pure, hence testable without
    /// capturing stderr — which is also why the module's real writing path
    /// stays a single `eprintln!`.
    #[must_use]
    pub fn enabled(self, level: Level) -> bool {
        level <= self.level
    }

    fn log(self, level: Level, message: &str) {
        if self.enabled(level) {
            eprintln!("npu: {}: {message}", level.label());
        }
    }

    /// A trace of what the engine did. Only with `--verbose info`.
    pub fn info(self, message: &str) {
        self.log(Level::Info, message);
    }

    /// Something the engine worked around, and the user probably wants to
    /// know about. Printed by default.
    pub fn warn(self, message: &str) {
        self.log(Level::Warn, message);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // tolerated in tests (cf. Cargo.toml [lints.clippy]).
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn levels_are_ordered_from_error_to_info() {
        assert!(Level::Error < Level::Warn);
        assert!(Level::Warn < Level::Info);
    }

    #[test]
    fn default_threshold_prints_warnings_but_not_traces() {
        let logger = Logger::new(Level::from_str(Level::DEFAULT).expect("the default must parse"));

        assert!(logger.enabled(Level::Error));
        assert!(logger.enabled(Level::Warn));
        assert!(!logger.enabled(Level::Info));
    }

    #[test]
    fn error_threshold_silences_warnings_and_traces() {
        let logger = Logger::new(Level::Error);

        assert!(logger.enabled(Level::Error));
        assert!(!logger.enabled(Level::Warn));
        assert!(!logger.enabled(Level::Info));
    }

    #[test]
    fn info_threshold_prints_everything() {
        let logger = Logger::new(Level::Info);

        assert!(logger.enabled(Level::Error));
        assert!(logger.enabled(Level::Warn));
        assert!(logger.enabled(Level::Info));
    }

    #[test]
    fn every_accepted_name_parses_back() {
        for name in Level::NAMES {
            Level::from_str(name).expect("a name advertised by the CLI must parse");
        }
    }

    #[test]
    fn unknown_level_is_a_config_error_naming_the_value() {
        let err = Level::from_str("chatty").expect_err("an unknown level must fail");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("chatty"));
    }

    #[test]
    fn level_from_args_defaults_to_warn() {
        assert_eq!(level_from_args(["npu", "classify"]), Level::Warn);
    }

    #[test]
    fn level_from_args_reads_the_separated_and_joined_forms() {
        assert_eq!(level_from_args(["npu", "--verbose", "info"]), Level::Info);
        assert_eq!(level_from_args(["npu", "--verbose=error"]), Level::Error);
        assert_eq!(level_from_args(["npu", "-v", "info"]), Level::Info);
        assert_eq!(level_from_args(["npu", "-v=info"]), Level::Info);
        assert_eq!(level_from_args(["npu", "-verror"]), Level::Error);
    }

    #[test]
    fn level_from_args_falls_back_on_an_unknown_value_instead_of_failing() {
        // clap diagnoses the invalid value itself, a few lines later: this
        // function must not turn a typo into a second error.
        assert_eq!(level_from_args(["npu", "--verbose", "chatty"]), Level::Warn);
        assert_eq!(level_from_args(["npu", "--verbose"]), Level::Warn);
    }

    #[test]
    fn level_from_args_keeps_the_last_occurrence() {
        assert_eq!(
            level_from_args(["npu", "--verbose", "info", "--verbose", "error"]),
            Level::Error
        );
    }
}
