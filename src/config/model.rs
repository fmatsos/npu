//! The `Model` type: a model configured in `models/*.toml`, referencing a
//! backend, plus its `fallback` validation.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Optional generation parameters for a model.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Generation {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

/// A model configured in `models/*.toml`, referencing a backend.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub id: String,
    pub backend: String,
    pub operation: String,
    pub model: String,
    /// Model to retry against when this one fails with `Error::Backend`.
    ///
    /// Exists because an NPU-compiled graph has a static maximum prompt
    /// length: OVMS answers `400 ... Input length exceeds the maximum
    /// allowed length` in milliseconds, which is an exact, cheap signal that
    /// the same prompt belongs on a GPU-served model instead. The retry is
    /// SINGLE HOP: the fallback's own `fallback` is not followed, which is
    /// what makes cycle detection unnecessary.
    pub fallback: Option<String>,
    #[serde(default)]
    pub generation: Generation,
    /// The file this model was loaded from, after the scope merge: what
    /// `npu backend tune` rewrites.
    #[serde(skip)]
    pub source: PathBuf,
}

/// Validates a model's `fallback`: it must name another loaded model.
///
/// Pointing at itself is rejected too — the retry is single hop, so a
/// self-fallback is an infinite intent expressed as a no-op, never what the
/// author meant.
pub(crate) fn validate_fallback(
    model: &Model,
    models: &HashMap<String, (Model, PathBuf)>,
    source: &Path,
) -> crate::Result<()> {
    let Some(fallback) = &model.fallback else {
        return Ok(());
    };

    if fallback == &model.id {
        return Err(crate::Error::Config(crate::error::ConfigError::in_file(
            source,
            Some(&model.id),
            format!("model \"{}\" declares itself as its own fallback", model.id),
        )));
    }

    if !models.contains_key(fallback) {
        return Err(crate::Error::Config(crate::error::ConfigError::in_file(
            source,
            Some(&model.id),
            format!(
                "model \"{}\" declares fallback \"{fallback}\", which is not a known model \
                 (available models: {})",
                model.id,
                crate::error::format_available(models.keys())
            ),
        )));
    }

    Ok(())
}
