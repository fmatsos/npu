//! The optional `llmfit` CLI, asked for its view of this host.

/// `llmfit fit --json`'s stdout; `None` when `llmfit` is not on `PATH`,
/// fails, or prints nothing. Its stderr is discarded: an optional helper
/// that is missing or broken must not add noise to `npu model discover`.
#[must_use]
pub fn fit_json() -> Option<String> {
    crate::runtime::capture("llmfit", &["fit", "--json"])
}
