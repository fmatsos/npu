//! Colours, in one place.
//!
//! Every styled byte goes out through `anstream`, which strips the escape
//! sequences when the stream is not a terminal and honours `NO_COLOR` /
//! `CLICOLOR_FORCE`: through a pipe, stdout and stderr are byte for byte
//! what they were without colours, which is what keeps the stdout contract.

use clap::builder::styling::AnsiColor;
pub use clap::builder::styling::Style;

pub const ERROR: Style = AnsiColor::Red.on_default().bold();
pub const WARN: Style = AnsiColor::Yellow.on_default().bold();
pub const INFO: Style = AnsiColor::Cyan.on_default();
pub const OK: Style = AnsiColor::Green.on_default().bold();
pub const HEADER: Style = AnsiColor::Green.on_default().bold();
pub const LITERAL: Style = AnsiColor::Cyan.on_default().bold();
/// The answer's header: bold in the terminal's OWN foreground, the one
/// colour guaranteed readable on a dark and a light theme alike; only the
/// mark carries a hue.
pub const ANSWER_MARK: Style = AnsiColor::Cyan.on_default().bold();
pub const ANSWER_HEADER: Style = Style::new().bold();
/// A figure `npu` estimated rather than measured, or a point to check.
pub const ESTIMATE: Style = AnsiColor::Yellow.on_default();
pub const PLACEHOLDER: Style = AnsiColor::Cyan.on_default();

/// `text` wrapped in `style`, for a stream written through `anstream`.
#[must_use]
pub fn paint(style: Style, text: &str) -> String {
    format!("{style}{text}{style:#}")
}

/// The palette of `clap`'s help and errors, built from the same styles.
#[must_use]
pub fn clap_styles() -> clap::builder::Styles {
    clap::builder::Styles::styled()
        .header(HEADER.underline())
        .usage(HEADER.underline())
        .literal(LITERAL)
        .placeholder(PLACEHOLDER)
        .error(ERROR)
        .valid(OK)
        .invalid(WARN)
}
