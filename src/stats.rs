//! Execution statistics: one JSON line per business invocation, appended to
//! the file `NPU_STATS_FILE` names.
//!
//! A record never holds the prompt, the answer or a header value: only what
//! happened. It is written after the result, and a failed write is a
//! warning on stderr, never a change of exit code.

use std::io::Write as _;

/// The environment variable naming the file records are appended to.
pub(crate) const STATS_FILE_VAR: &str = "NPU_STATS_FILE";

/// What one invocation did. A field `null` in the file is one the run never
/// got to know (the backend failed before answering, it reported no
/// `usage`).
#[derive(Debug, Default, serde::Serialize)]
pub(crate) struct Record {
    /// Milliseconds since the Unix epoch, when the record was started.
    pub(crate) timestamp_ms: u64,
    pub(crate) command: String,
    /// The `config test` case this run belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) case: Option<String>,
    pub(crate) model_requested: String,
    pub(crate) model_answered: Option<String>,
    pub(crate) fallback_used: Option<bool>,
    pub(crate) backend: Option<String>,
    /// Time spent reaching the backend and waiting for its answer, the
    /// fallback included.
    pub(crate) duration_ms: Option<u64>,
    pub(crate) prompt_tokens: Option<u64>,
    pub(crate) completion_tokens: Option<u64>,
    pub(crate) finish_reason: Option<String>,
    pub(crate) exit_code: i32,
}

impl Record {
    pub(crate) fn new(command: &crate::command::CommandSpec, model_requested: &str) -> Self {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        Self {
            timestamp_ms,
            command: command.path.join("/"),
            model_requested: model_requested.to_string(),
            ..Self::default()
        }
    }

    /// Sets the exit code `result` gives the invocation.
    pub(crate) fn finish<T>(&mut self, result: &crate::Result<T>) {
        self.exit_code = result.as_ref().map_or_else(crate::Error::exit_code, |_| 0);
    }
}

/// Appends `record` to the file `NPU_STATS_FILE` names, if it names one.
pub(crate) fn append(
    env: &dyn Fn(&str) -> Option<String>,
    record: &Record,
    logger: crate::log::Logger,
) {
    let Some(path) = env(STATS_FILE_VAR).filter(|path| !path.is_empty()) else {
        return;
    };
    let written = serde_json::to_string(record)
        .map_err(std::io::Error::other)
        .and_then(|mut line| {
            line.push('\n');
            // One `write_all` on an `O_APPEND` file: concurrent invocations
            // appending to the same file do not interleave their lines.
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?
                .write_all(line.as_bytes())
        });
    if let Err(err) = written {
        logger.warn(&format!(
            "{STATS_FILE_VAR}: cannot append a record to {path}: {err}"
        ));
    }
}
