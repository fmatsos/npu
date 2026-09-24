//! The `graph.pbtxt`/`plugin_config` shape one OVMS export uses, and the
//! arithmetic that sizes an NPU-compiled export's static context.
//!
//! ## Calibration
//!
//! An `OpenVINO` NPU graph is compiled for a STATIC context: `MAX_PROMPT_LEN`
//! tokens of prompt plus `MIN_RESPONSE_LEN` tokens of answer, both set at
//! the root of `plugin_config`. A longer prompt is refused, a longer answer
//! is cut. [`context_bytes`] estimates, per token of context, its KV cache
//! PLUS about [`ACTIVATION_TENTHS`]`/10 x hidden_size x layers` bytes of
//! static buffers — the second term dominates on models with few KV heads.
//!
//! Fitted on a Meteor Lake NPU (Core Ultra 7 165U, OVMS 2026.4, int4),
//! container memory minus weights:
//!
//! | model | context | measured |
//! | --- | --- | --- |
//! | Qwen3-4B | 7K | 7.3 GB |
//! | Qwen3-4B | 16K | 13.1 GB |
//! | Qwen3-4B | 32K | 33.6 GB |
//! | Qwen3-8B | 7K | 10.6 GB |
//! | Coder-7B | 16K | 13.4 GB |
//! | Coder-3B | 20K | 11.0 GB |
//!
//! The formula lands within +0..+15 % of each: never under. Recalibrate on
//! another NPU or OVMS version, and `npu backend tune --dry-run` prints the
//! constants it used so a user on other hardware can see why the numbers
//! are what they are.

use std::path::{Path, PathBuf};

/// Per token of context, past [`LONG_CONTEXT`] bytes of static buffers cost
/// this many tenths of `hidden_size x layers`; see the module's
/// `## Calibration` section.
pub const ACTIVATION_TENTHS: u64 = 62;
/// Past [`LONG_CONTEXT`] tokens (measured at 32K).
pub const ACTIVATION_TENTHS_LONG: u64 = 90;
/// The context length, in tokens, past which [`ACTIVATION_TENTHS_LONG`]
/// applies instead of [`ACTIVATION_TENTHS`].
pub const LONG_CONTEXT: u64 = 24_576;

/// Contexts are rounded down to this step, and never go below it.
pub const STEP: u64 = 1024;

// ponytail: a GPU model is served by continuous batching; one user never
// runs more than a handful of requests at once, and every slot reserves
// scheduler memory. Make it a flag the day several users share a backend.
pub const GPU_MAX_NUM_SEQS: u64 = 4;
pub const GIB: u64 = 1 << 30;

/// Estimated NPU memory for a `tokens` context, weights excluded.
#[must_use]
pub fn context_bytes(tokens: u64, shape: crate::vendor::huggingface::Shape) -> u64 {
    let tenths = if tokens <= LONG_CONTEXT {
        ACTIVATION_TENTHS
    } else {
        ACTIVATION_TENTHS_LONG
    };
    tokens * (shape.kv_per_token * 10 + tenths * shape.width) / 10
}

/// `(MAX_PROMPT_LEN, MIN_RESPONSE_LEN)` fitting both the model and `budget`
/// bytes; a quarter of the context goes to the answer.
#[must_use]
pub fn plan_context(shape: crate::vendor::huggingface::Shape, budget: u64) -> (u64, u64) {
    let mut total = STEP;
    let mut candidate = STEP;
    while candidate <= shape.max_context && context_bytes(candidate, shape) <= budget {
        total = candidate;
        candidate += STEP;
    }
    let response = (total / 4 / 256 * 256).max(STEP / 2);
    (total - response, response)
}

/// Sets `key: value` in the `node_options` of a `graph.pbtxt`, replacing the
/// line that holds it or adding it after `models_path`, with its
/// indentation.
///
/// # Errors
///
/// `Error::Config` naming `source` when the graph has no `models_path` to
/// anchor a new key on.
pub fn set_node_option(
    graph: &str,
    key: &str,
    value: &str,
    source: &Path,
) -> crate::Result<String> {
    let holds = |line: &str, name: &str| {
        line.trim_start()
            .strip_prefix(name)
            .is_some_and(|rest| rest.trim_start().starts_with(':'))
    };
    let mut lines: Vec<String> = graph.lines().map(str::to_string).collect();
    let entry = |line: &str| {
        let indent = &line[..line.len() - line.trim_start().len()];
        format!("{indent}{key}: {value},")
    };
    if let Some(i) = lines.iter().position(|l| holds(l, key)) {
        lines[i] = entry(&lines[i]);
    } else {
        let anchor = lines
            .iter()
            .position(|l| holds(l, "models_path"))
            .ok_or_else(|| {
                crate::Error::Config(crate::error::ConfigError::in_file(
                    source,
                    None::<String>,
                    "no models_path in node_options",
                ))
            })?;
        let line = entry(&lines[anchor]);
        lines.insert(anchor + 1, line);
    }
    Ok(lines.join("\n") + "\n")
}

/// Edits the JSON object held by `plugin_config: '...'`, starting from an
/// empty one when the graph has none, and leaves the graph without the key
/// when the result is empty and there was none. `MAX_PROMPT_LEN` and
/// `MIN_RESPONSE_LEN` belong at its ROOT: under `DEVICE_PROPERTIES` they are
/// ignored.
///
/// # Errors
///
/// `Error::Config` naming `source` when `plugin_config` is unterminated,
/// not JSON, or not a JSON object.
pub fn edit_plugin_config(
    graph: &str,
    source: &Path,
    edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> crate::Result<String> {
    const OPEN: &str = "plugin_config: '";
    let malformed = |why: &str| {
        crate::Error::Config(crate::error::ConfigError::in_file(
            source,
            None::<String>,
            why,
        ))
    };
    let current = match graph.find(OPEN) {
        Some(at) => {
            let start = at + OPEN.len();
            let end = start
                + graph[start..]
                    .find('\'')
                    .ok_or_else(|| malformed("unterminated plugin_config"))?;
            Some(&graph[start..end])
        }
        None => None,
    };
    let mut plugin: serde_json::Value = serde_json::from_str(current.unwrap_or("{}"))
        .map_err(|e| malformed(&format!("plugin_config is not JSON: {e}")))?;
    let object = plugin
        .as_object_mut()
        .ok_or_else(|| malformed("plugin_config is not a JSON object"))?;
    edit(object);
    if current.is_none() && object.is_empty() {
        return Ok(graph.to_string());
    }
    set_node_option(graph, "plugin_config", &format!("'{plugin}'"), source)
}

/// A device an export's `graph.pbtxt` is compiled for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Npu,
    Gpu,
}

impl Device {
    #[must_use]
    pub fn of(graph: &str) -> Option<Self> {
        if graph.contains("device: \"NPU\"") {
            Some(Self::Npu)
        } else if graph.contains("device: \"GPU\"") {
            Some(Self::Gpu)
        } else {
            None
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Npu => "NPU",
            Self::Gpu => "GPU",
        }
    }
}

/// One model `npu backend tune` is about to plan a `graph.pbtxt` for.
#[derive(Debug)]
pub struct Planned {
    pub id: String,
    pub device: Device,
    pub shape: crate::vendor::huggingface::Shape,
    pub weights: u64,
    pub graph: PathBuf,
}

/// The new `graph.pbtxt`, the answer length, and the estimated memory of
/// one model inside `budget` bytes.
///
/// # Errors
///
/// Whatever [`set_node_option`] or [`edit_plugin_config`] return, naming
/// `p.graph`.
pub fn plan_graph(
    p: &Planned,
    graph: &str,
    budget: u64,
    kv_u8: bool,
) -> crate::Result<(String, u64, u64, u64)> {
    match p.device {
        Device::Npu => {
            let (prompt, response) = plan_context(p.shape, budget);
            let text = edit_plugin_config(graph, &p.graph, |plugin| {
                plugin.insert("MAX_PROMPT_LEN".into(), prompt.into());
                plugin.insert("MIN_RESPONSE_LEN".into(), response.into());
                let npu = plugin
                    .entry("DEVICE_PROPERTIES")
                    .or_insert_with(|| serde_json::json!({}))
                    .as_object_mut()
                    .and_then(|d| {
                        d.entry("NPU")
                            .or_insert_with(|| serde_json::json!({}))
                            .as_object_mut()
                    });
                if let Some(npu) = npu {
                    npu.insert("NPUW_LLM_ENABLE_PREFIX_CACHING".into(), true.into());
                }
            })?;
            let memory = context_bytes(prompt + response, p.shape);
            Ok((text, prompt, response, memory))
        }
        Device::Gpu => {
            // ponytail: the GPU budget goes to the KV cache alone; activations
            // are transient and uncalibrated here. Measure a GPU twin under
            // load and reserve their share if it ever overshoots.
            let kv = if kv_u8 {
                p.shape.kv_per_token / 2
            } else {
                p.shape.kv_per_token
            };
            let cache_gib = (budget / GIB).max(1);
            let tokens = (cache_gib * GIB / kv.max(1))
                .min(p.shape.max_context)
                .max(STEP)
                / STEP
                * STEP;
            let response = (tokens / 4 / 256 * 256).max(STEP / 2);
            let mut text = set_node_option(graph, "cache_size", &cache_gib.to_string(), &p.graph)?;
            text = set_node_option(
                &text,
                "max_num_seqs",
                &GPU_MAX_NUM_SEQS.to_string(),
                &p.graph,
            )?;
            text = edit_plugin_config(&text, &p.graph, |plugin| {
                if kv_u8 {
                    plugin.insert("KV_CACHE_PRECISION".into(), "u8".into());
                } else {
                    plugin.remove("KV_CACHE_PRECISION");
                }
            })?;
            Ok((text, tokens - response, response, cache_gib * GIB))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vendor::huggingface::Shape;

    const QWEN3_4B: Shape = Shape {
        max_context: 40_960,
        kv_per_token: 147_456,
        width: 92_160,
    };

    fn path() -> &'static Path {
        Path::new("models/x.toml")
    }

    #[test]
    fn the_plan_fits_the_budget_the_model_and_the_floor() {
        let (prompt, response) = plan_context(QWEN3_4B, 5_000_000_000);
        assert_eq!((prompt + response) % STEP, 0);
        assert!(response <= prompt);
        assert!(context_bytes(prompt + response, QWEN3_4B) <= 5_000_000_000);
        let tiny = Shape {
            max_context: 2048,
            kv_per_token: 1,
            width: 1,
        };
        assert_eq!(plan_context(tiny, u64::MAX / 1_000_000), (1536, 512));
        assert_eq!(plan_context(QWEN3_4B, 0), (512, 512));
    }

    #[test]
    fn calibration_points_are_never_underestimated() {
        for (tokens, kv_per_token, width, measured) in [
            (7168, 147_456, 92_160, 5_100_000_000),
            (7168, 147_456, 147_456, 5_950_000_000),
            (16_384, 57_344, 100_352, 9_190_000_000),
            (20_480, 36_864, 73_728, 9_290_000_000),
            (32_768, 147_456, 92_160, 31_400_000_000),
        ] {
            let shape = Shape {
                max_context: 0,
                kv_per_token,
                width,
            };
            assert!(context_bytes(tokens, shape) >= measured, "{tokens} {width}");
        }
    }

    const NPU_GRAPH: &str = "  node_options: {\n      max_num_seqs:256,\n      device: \"NPU\",\n      models_path: \".\",\n      plugin_config: '{\"DEVICE_PROPERTIES\":{\"NPU\":{}}}',\n      cache_size: 0,\n  }\n";
    const GPU_GRAPH: &str = "  node_options: {\n      max_num_seqs:256,\n      device: \"GPU\",\n      models_path: \".\",\n      cache_size: 0,\n  }\n";

    fn plugin_of(graph: &str) -> serde_json::Value {
        serde_json::from_str(graph.split('\'').nth(1).unwrap_or("null")).unwrap_or_default()
    }

    fn planned(device: Device) -> Planned {
        Planned {
            id: "x".into(),
            device,
            shape: QWEN3_4B,
            weights: 0,
            graph: PathBuf::from("graph.pbtxt"),
        }
    }

    #[test]
    fn an_npu_graph_gets_its_lengths_at_the_root_and_prefix_caching() {
        let (graph, prompt, response, _) =
            plan_graph(&planned(Device::Npu), NPU_GRAPH, 5_000_000_000, false).unwrap_or_default();
        let json = plugin_of(&graph);
        assert_eq!(json["MAX_PROMPT_LEN"], prompt);
        assert_eq!(json["MIN_RESPONSE_LEN"], response);
        assert_eq!(
            json["DEVICE_PROPERTIES"]["NPU"]["NPUW_LLM_ENABLE_PREFIX_CACHING"],
            true
        );
        assert!(graph.contains("      cache_size: 0,"));
    }

    #[test]
    fn a_gpu_graph_gets_a_bounded_cache_few_sequences_and_an_optional_u8_cache() {
        let (graph, prompt, response, memory) =
            plan_graph(&planned(Device::Gpu), GPU_GRAPH, 5 * GIB, false).unwrap_or_default();
        assert!(graph.contains("      cache_size: 5,") && graph.contains("      max_num_seqs: 4,"));
        assert!(!graph.contains("plugin_config"));
        assert_eq!(memory, 5 * GIB);
        assert!((prompt + response) * QWEN3_4B.kv_per_token <= memory);
        let (u8_graph, u8_prompt, u8_response, _) =
            plan_graph(&planned(Device::Gpu), GPU_GRAPH, 5 * GIB, true).unwrap_or_default();
        assert_eq!(plugin_of(&u8_graph)["KV_CACHE_PRECISION"], "u8");
        assert!(u8_prompt + u8_response > prompt + response);
        let (back, ..) =
            plan_graph(&planned(Device::Gpu), &u8_graph, 5 * GIB, false).unwrap_or_default();
        assert!(plugin_of(&back).get("KV_CACHE_PRECISION").is_none());
    }

    #[test]
    fn a_graph_without_models_path_is_refused() {
        assert!(set_node_option("nothing", "cache_size", "1", path()).is_err());
    }
}
