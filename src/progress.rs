//! Visual feedback on STDERR: a spinner while the engine waits, a progress while
//! it downloads.
//!
//! Same contract as `log.rs`, and stricter: an indicator is drawn only when
//! stderr is a terminal AND the threshold is not `error`. A program driving
//! `npu` through a pipe therefore never receives a control character, and
//! `--verbose error` means silence. Nothing here ever touches stdout.
//!
//! An [`Indicator`] clears itself when dropped, so every path — success,
//! `?`, panic (`panic = "unwind"`) — leaves the terminal clean.

use std::io::IsTerminal;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

/// The indicator on screen, if any, so that [`suspend`] can clear it
/// around a log line.
// ponytail: one indicator at a time, a `MultiProgress` if two ever coexist.
static ACTIVE: Mutex<Option<ProgressBar>> = Mutex::new(None);

/// Set once by `lib.rs::run`; `false` until then, which is what keeps every
/// test — none calls [`init`] — free of drawing.
// ponytail: a process-wide switch rather than a parameter threaded through
// every `runtime::process::Host`; a field there if indicators ever need to
// differ between two calls of the same run.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Enables indicators for this run when the threshold allows them and
/// stderr is a terminal.
pub fn init(level: crate::log::Level) {
    ENABLED.store(
        allowed(level, std::io::stderr().is_terminal()),
        Ordering::Relaxed,
    );
}

/// Pure rule behind [`init`].
#[must_use]
pub fn allowed(level: crate::log::Level, stderr_is_terminal: bool) -> bool {
    level > crate::log::Level::Error && stderr_is_terminal
}

/// A spinner or a progress; a no-op when built disabled.
#[derive(Debug)]
pub struct Indicator(Option<ProgressBar>);

impl Indicator {
    /// A spinner with `message`, for a wait of unknown length.
    #[must_use]
    pub fn spinner(message: &str) -> Self {
        Self::start(|| {
            let progress = ProgressBar::new_spinner();
            progress.set_message(message.to_string());
            progress
        })
    }

    /// A byte counter for a download: a progress when `total` is known, a
    /// spinner counting bytes otherwise.
    #[must_use]
    pub fn bytes(total: Option<u64>, message: &str) -> Self {
        Self::start(|| {
            let (progress, template) = match total {
                Some(total) => (
                    ProgressBar::new(total),
                    "{spinner} {msg} [{bar:30}] {bytes}/{total_bytes}",
                ),
                None => (ProgressBar::new_spinner(), "{spinner} {msg} {bytes}"),
            };
            if let Ok(style) = ProgressStyle::with_template(template) {
                progress.set_style(style.progress_chars("=> "));
            }
            progress.set_message(message.to_string());
            progress
        })
    }

    fn start(build: impl FnOnce() -> ProgressBar) -> Self {
        if !ENABLED.load(Ordering::Relaxed) {
            return Self(None);
        }
        let progress = build();
        progress.enable_steady_tick(Duration::from_millis(100));
        if let Ok(mut active) = ACTIVE.lock() {
            *active = Some(progress.clone());
        }
        Self(Some(progress))
    }

    pub fn set_message(&self, message: &str) {
        if let Some(progress) = &self.0 {
            progress.set_message(message.to_string());
        }
    }

    pub fn inc(&self, delta: u64) {
        if let Some(progress) = &self.0 {
            progress.inc(delta);
        }
    }
}

impl Drop for Indicator {
    fn drop(&mut self) {
        if let Some(progress) = self.0.take() {
            progress.finish_and_clear();
            if let Ok(mut active) = ACTIVE.lock() {
                *active = None;
            }
        }
    }
}

/// Runs `write` with the active indicator, if any, cleared from the
/// screen, so a diagnostic line never lands in the middle of a frame.
pub fn suspend(write: impl FnOnce()) {
    let active = ACTIVE.lock().ok().and_then(|active| active.clone());
    match active {
        Some(progress) => progress.suspend(write),
        None => write(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indicators_need_a_terminal_and_a_threshold_above_error() {
        use crate::log::Level;
        assert!(allowed(Level::Warn, true));
        assert!(allowed(Level::Info, true));
        assert!(!allowed(Level::Error, true));
        assert!(!allowed(Level::Warn, false));
    }

    #[test]
    fn an_indicator_draws_nothing_before_init() {
        let spinner = Indicator::spinner("waiting");
        spinner.set_message("still waiting");
        spinner.inc(1);
        assert!(spinner.0.is_none());
    }
}
