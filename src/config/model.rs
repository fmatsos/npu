//! The `Model` type: a model configured in `models/*.toml`, referencing a
//! backend, plus its `fallback` validation.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The typed `[generation]` keys, i.e. every top-level key `extra` is NOT
/// allowed to shadow (see [`validate_generation`]): the request fields
/// `build_chat_request` writes itself, whether or not a `Generation` sets
/// them.
pub(crate) const RESERVED_GENERATION_KEYS: [&str; 9] = [
    "model",
    "messages",
    "stream",
    "response_format",
    "temperature",
    "max_tokens",
    "seed",
    "top_p",
    "stop",
];

/// Optional generation parameters for a model, and (identically shaped) for
/// a command's override (see [`Generation::merged`]).
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Generation {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Deterministic sampling seed, forwarded as-is: the crate does not
    /// interpret it, only the backend does.
    #[serde(default)]
    pub seed: Option<u64>,
    /// Nucleus sampling cutoff. Same f32-via-text serialization as
    /// `temperature` (`backend::f64_from_f32_text`): avoids exposing f32
    /// binary noise in the JSON sent to the backend.
    #[serde(default)]
    pub top_p: Option<f32>,
    /// Stop sequences: a non-empty list of non-empty strings, no upper
    /// bound (deliberately NOT the OpenAI-specific 1-4 entry limit — a
    /// bound specific to one provider would contradict the project's
    /// backend-agnostic stance; llama.cpp, for one, accepts more).
    #[serde(default)]
    pub stop: Option<Vec<String>>,
    /// Free-form passthrough, forwarded verbatim (TOML structure mapped to
    /// JSON by shape) at the top level of the request body, after the typed
    /// keys — the one escape hatch for an engine-specific knob
    /// (`chat_template_kwargs.enable_thinking = false` on Qwen3 is the
    /// motivating case). Every key of `extra` is honoured BY DEFINITION:
    /// this is the one place in the crate where "honoured" means "forwarded
    /// verbatim" rather than interpreted.
    #[serde(default)]
    #[cfg_attr(
        test,
        schemars(with = "Option<serde_json::Map<String, serde_json::Value>>")
    )]
    pub extra: Option<toml::Table>,
}

impl Generation {
    /// Whether no key at all is declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.temperature.is_none()
            && self.max_tokens.is_none()
            && self.seed.is_none()
            && self.top_p.is_none()
            && self.stop.is_none()
            && self.extra.is_none()
    }

    /// Merges `command` (a command's own `[generation]` override, if any)
    /// onto `model`'s: the ONLY field-by-field merge in the project — an
    /// explicit, documented exception to "replacement, never merge"
    /// (`docs/configuration.md`). Every typed key present in `command`
    /// replaces the model's; a typed key `command` leaves unset keeps the
    /// model's value. `extra` is merged the same way, key by key, at the
    /// TOP LEVEL only: a key `command.extra` sets replaces the model's
    /// value for that key WHOLESALE (no deep merge inside a nested table),
    /// a key only `model.extra` sets is kept.
    #[must_use]
    pub fn merged(model: &Self, command: Option<&Self>) -> Self {
        let Some(command) = command else {
            return Self {
                temperature: model.temperature,
                max_tokens: model.max_tokens,
                seed: model.seed,
                top_p: model.top_p,
                stop: model.stop.clone(),
                extra: model.extra.clone(),
            };
        };

        let extra = match (&model.extra, &command.extra) {
            (None, None) => None,
            (model_extra, command_extra) => {
                let mut merged = model_extra.clone().unwrap_or_default();
                if let Some(command_extra) = command_extra {
                    for (key, value) in command_extra {
                        merged.insert(key.clone(), value.clone());
                    }
                }
                Some(merged)
            }
        };

        Self {
            temperature: command.temperature.or(model.temperature),
            max_tokens: command.max_tokens.or(model.max_tokens),
            seed: command.seed.or(model.seed),
            top_p: command.top_p.or(model.top_p),
            stop: command.stop.clone().or_else(|| model.stop.clone()),
            extra,
        }
    }
}

/// A TOML value that would silently become something other than what its
/// author wrote once forwarded as JSON: a `Datetime` (JSON has no datetime
/// type — `toml`'s own JSON-adjacent serializers turn one into a private
/// map shape) or a non-finite float (`NaN`/`Infinity`, legal TOML float
/// literals; `serde_json::Number::from_f64` silently drops them). Checked
/// recursively through arrays and tables, since either can nest one.
fn find_unsupported_value(value: &toml::Value) -> Option<&'static str> {
    match value {
        toml::Value::Datetime(_) => Some("a datetime, which has no JSON equivalent"),
        toml::Value::Float(f) if !f.is_finite() => {
            Some("a non-finite float (NaN or +/-Infinity), which has no JSON equivalent")
        }
        toml::Value::Array(items) => items.iter().find_map(find_unsupported_value),
        toml::Value::Table(table) => table.values().find_map(find_unsupported_value),
        toml::Value::String(_)
        | toml::Value::Integer(_)
        | toml::Value::Float(_)
        | toml::Value::Boolean(_) => None,
    }
}

/// Converts a validated `[generation.extra]` table to JSON, by structure:
/// TOML tables become objects, arrays become arrays, scalars convert
/// directly. Assumes [`generation_errors`] already ran (no `Datetime`, no
/// non-finite float): a value that should not reach here after validation
/// still degrades gracefully rather than panicking (`null` for a
/// non-finite float, the RFC 3339 text for a datetime), since `backend.rs`
/// calls this on data it trusts but does not re-validate.
pub(crate) fn extra_to_json(extra: &toml::Table) -> serde_json::Map<String, serde_json::Value> {
    extra
        .iter()
        .map(|(key, value)| (key.clone(), toml_value_to_json(value)))
        .collect()
}

fn toml_value_to_json(value: &toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::Value::from(*i),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Datetime(dt) => serde_json::Value::String(dt.to_string()),
        toml::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(toml_value_to_json).collect())
        }
        toml::Value::Table(table) => serde_json::Value::Object(extra_to_json(table)),
    }
}

/// Validates one `[generation]` table (a model's or a command's override):
/// `stop` is a non-empty list of non-empty strings, and `extra` neither
/// shadows a typed key nor carries a datetime or non-finite float anywhere
/// in its structure. `label` names the offending model/command in the
/// message. `file: None` — the caller attaches it via
/// `crate::error::InFile::in_file`, the same idiom `command.rs` already
/// uses for `prompt`/`output` errors: this function has no file of its own
/// to name, its callers (models and commands) do.
pub(crate) fn generation_errors(generation: &Generation, label: &str) -> crate::Result<()> {
    // A NaN or infinite float has no JSON form: it would be dropped from the
    // request while the configuration says it is sent.
    for (key, value) in [
        ("temperature", generation.temperature),
        ("top_p", generation.top_p),
    ] {
        if value.is_some_and(|v| !v.is_finite()) {
            return Err(crate::Error::config(format!(
                "\"{label}\": [generation].{key} must be a finite number"
            )));
        }
    }
    if let Some(stop) = &generation.stop {
        if stop.is_empty() {
            return Err(crate::Error::config(format!(
                "\"{label}\": [generation].stop is present but empty; remove the key or give \
                 it at least one entry"
            )));
        }
        if stop.iter().any(String::is_empty) {
            return Err(crate::Error::config(format!(
                "\"{label}\": [generation].stop contains an empty string"
            )));
        }
    }

    if let Some(extra) = &generation.extra {
        for key in extra.keys() {
            if RESERVED_GENERATION_KEYS.contains(&key.as_str()) {
                return Err(crate::Error::config(format!(
                    "\"{label}\": [generation.extra].{key} collides with the typed \
                     [generation].{key} key; set {key} there instead (extra cannot shadow a \
                     typed key)"
                )));
            }
            if let Some(reason) = find_unsupported_value(&extra[key]) {
                return Err(crate::Error::config(format!(
                    "\"{label}\": [generation.extra].{key} is {reason}"
                )));
            }
        }
    }

    Ok(())
}

/// [`generation_errors`], with `file` attached directly: the idiom
/// `config/mod.rs` uses for a model, whose source file is already known at
/// the call site (unlike a command's, resolved later by `read_and_parse`).
pub(crate) fn validate_generation(
    generation: &Generation,
    label: &str,
    source: &Path,
) -> crate::Result<()> {
    use crate::error::InFile;
    generation_errors(generation, label).in_file(source)
}

/// A model configured in `models/*.toml`, referencing a backend.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
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

/// Validates what a model's protocol allows: an `embeddings` model takes
/// no `fallback` (two models' vectors cannot be compared) and no
/// `[generation]` (there is nothing to sample), and a fallback must speak
/// the same protocol as its primary. A protocol that does not resolve
/// (`None`) is not judged here.
pub(crate) fn validate_protocol(
    model: &Model,
    protocol: Option<super::Protocol>,
    fallback_protocol: Option<super::Protocol>,
    source: &Path,
) -> crate::Result<()> {
    let reject = |message: String| {
        Err(crate::Error::Config(crate::error::ConfigError::in_file(
            source,
            Some(&model.id),
            message,
        )))
    };
    match protocol {
        Some(super::Protocol::Embeddings) => {
            if model.fallback.is_some() {
                return reject(format!(
                    "model \"{}\" calls an embeddings operation and declares a fallback: \
                     vectors from two models cannot be compared, remove \"fallback\"",
                    model.id
                ));
            }
            if !model.generation.is_empty() {
                return reject(format!(
                    "model \"{}\" calls an embeddings operation, which samples nothing: \
                     remove its [generation] table",
                    model.id
                ));
            }
        }
        Some(super::Protocol::Transcriptions) => {
            if !model.generation.is_empty() {
                return reject(format!(
                    "model \"{}\" calls a transcriptions operation, to which no [generation] \
                     key applies: remove its [generation] table",
                    model.id
                ));
            }
        }
        Some(super::Protocol::Chat) | None => {}
    }
    if let (Some(protocol), Some(fallback_protocol)) = (protocol, fallback_protocol)
        && protocol != fallback_protocol
    {
        return reject(format!(
            "model \"{}\" speaks {} but its fallback \"{}\" speaks {}",
            model.id,
            protocol.as_str(),
            model.fallback.as_deref().unwrap_or_default(),
            fallback_protocol.as_str()
        ));
    }
    Ok(())
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

#[cfg(test)]
#[allow(clippy::expect_used)] // tolerated in tests (cf. Cargo.toml [lints.clippy]).
mod tests {
    use super::*;

    fn generation() -> Generation {
        Generation {
            temperature: None,
            max_tokens: None,
            seed: None,
            top_p: None,
            stop: None,
            extra: None,
        }
    }

    // -- generation_errors: stop --------------------------------------

    #[test]
    fn empty_stop_list_is_rejected() {
        let g = Generation {
            stop: Some(Vec::new()),
            ..generation()
        };
        assert!(generation_errors(&g, "m").is_err());
    }

    #[test]
    fn stop_with_an_empty_string_is_rejected() {
        let g = Generation {
            stop: Some(vec!["ok".to_string(), String::new()]),
            ..generation()
        };
        assert!(generation_errors(&g, "m").is_err());
    }

    #[test]
    fn stop_with_many_entries_is_accepted_no_upper_bound() {
        let g = Generation {
            stop: Some((0..10).map(|i| format!("s{i}")).collect()),
            ..generation()
        };
        assert!(generation_errors(&g, "m").is_ok());
    }

    // -- generation_errors: extra ---------------------------------------

    #[test]
    fn extra_shadowing_a_typed_key_is_rejected_naming_it() {
        let mut extra = toml::Table::new();
        extra.insert("temperature".to_string(), toml::Value::Float(0.1));
        let g = Generation {
            extra: Some(extra),
            ..generation()
        };
        let err = generation_errors(&g, "m").expect_err("must reject");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("temperature"));
    }

    #[test]
    fn extra_with_a_datetime_is_rejected() {
        let mut extra = toml::Table::new();
        let datetime: toml::value::Datetime =
            "1979-05-27T07:32:00Z".parse().expect("valid rfc3339");
        extra.insert("when".to_string(), toml::Value::Datetime(datetime));
        let g = Generation {
            extra: Some(extra),
            ..generation()
        };
        assert!(generation_errors(&g, "m").is_err());
    }

    #[test]
    fn extra_with_a_nested_datetime_inside_a_table_is_rejected() {
        let mut inner = toml::Table::new();
        let datetime: toml::value::Datetime =
            "1979-05-27T07:32:00Z".parse().expect("valid rfc3339");
        inner.insert("when".to_string(), toml::Value::Datetime(datetime));
        let mut extra = toml::Table::new();
        extra.insert("nested".to_string(), toml::Value::Table(inner));
        let g = Generation {
            extra: Some(extra),
            ..generation()
        };
        assert!(generation_errors(&g, "m").is_err());
    }

    #[test]
    fn extra_with_a_non_finite_float_is_rejected() {
        let mut extra = toml::Table::new();
        extra.insert("ratio".to_string(), toml::Value::Float(f64::NAN));
        let g = Generation {
            extra: Some(extra),
            ..generation()
        };
        assert!(generation_errors(&g, "m").is_err());
    }

    #[test]
    fn extra_with_a_plain_string_datetime_shaped_value_is_accepted() {
        // A plain STRING that merely looks like a date ("2024-01-01") is
        // legitimate JSON and must be forwarded: only a genuine TOML
        // Datetime value is rejected, never a string.
        let mut extra = toml::Table::new();
        extra.insert(
            "note".to_string(),
            toml::Value::String("2024-01-01".to_string()),
        );
        let g = Generation {
            extra: Some(extra),
            ..generation()
        };
        assert!(generation_errors(&g, "m").is_ok());
    }

    #[test]
    fn extra_with_ordinary_nested_content_is_accepted() {
        let mut kwargs = toml::Table::new();
        kwargs.insert("enable_thinking".to_string(), toml::Value::Boolean(false));
        let mut extra = toml::Table::new();
        extra.insert(
            "chat_template_kwargs".to_string(),
            toml::Value::Table(kwargs),
        );
        let g = Generation {
            extra: Some(extra),
            ..generation()
        };
        assert!(generation_errors(&g, "m").is_ok());
    }

    // -- Generation::merged ----------------------------------------------

    #[test]
    fn a_non_finite_top_p_or_temperature_is_rejected_naming_the_key() {
        for (generation, key) in [
            (
                Generation {
                    top_p: Some(f32::NAN),
                    ..Generation::default()
                },
                "top_p",
            ),
            (
                Generation {
                    temperature: Some(f32::INFINITY),
                    ..Generation::default()
                },
                "temperature",
            ),
        ] {
            let err = generation_errors(&generation, "m").expect_err("non-finite");
            assert!(matches!(err, crate::Error::Config(_)));
            assert!(err.to_string().contains(key));
        }
    }

    #[test]
    fn merged_without_command_override_keeps_the_model_as_is() {
        let model = Generation {
            temperature: Some(0.2),
            seed: Some(1),
            ..generation()
        };
        let merged = Generation::merged(&model, None);
        assert_eq!(merged.temperature, Some(0.2));
        assert_eq!(merged.seed, Some(1));
    }

    #[test]
    fn merged_command_seed_wins_model_temperature_kept() {
        let model = Generation {
            temperature: Some(0.2),
            seed: None,
            ..generation()
        };
        let command = Generation {
            temperature: None,
            seed: Some(42),
            ..generation()
        };
        let merged = Generation::merged(&model, Some(&command));
        assert_eq!(merged.seed, Some(42));
        assert_eq!(merged.temperature, Some(0.2));
    }

    #[test]
    fn merged_extra_replaces_wholesale_key_by_key_command_wins() {
        let mut model_extra = toml::Table::new();
        model_extra.insert("a".to_string(), toml::Value::String("model-a".to_string()));
        model_extra.insert("b".to_string(), toml::Value::String("model-b".to_string()));
        let model = Generation {
            extra: Some(model_extra),
            ..generation()
        };

        let mut command_extra = toml::Table::new();
        command_extra.insert(
            "a".to_string(),
            toml::Value::String("command-a".to_string()),
        );
        let command = Generation {
            extra: Some(command_extra),
            ..generation()
        };

        let merged = Generation::merged(&model, Some(&command));
        let extra = merged.extra.expect("extra must be present");
        assert_eq!(
            extra.get("a"),
            Some(&toml::Value::String("command-a".to_string())),
            "command's extra.a must replace the model's"
        );
        assert_eq!(
            extra.get("b"),
            Some(&toml::Value::String("model-b".to_string())),
            "model's extra.b, untouched by the command, must be kept"
        );
    }

    // -- extra_to_json -----------------------------------------------------

    #[test]
    fn extra_to_json_converts_by_structure() {
        let mut kwargs = toml::Table::new();
        kwargs.insert("enable_thinking".to_string(), toml::Value::Boolean(false));
        let mut extra = toml::Table::new();
        extra.insert(
            "chat_template_kwargs".to_string(),
            toml::Value::Table(kwargs),
        );
        extra.insert(
            "stop_words".to_string(),
            toml::Value::Array(vec![toml::Value::String("###".to_string())]),
        );

        let json = extra_to_json(&extra);
        assert_eq!(
            json.get("chat_template_kwargs"),
            Some(&serde_json::json!({ "enable_thinking": false }))
        );
        assert_eq!(json.get("stop_words"), Some(&serde_json::json!(["###"])));
    }
}
