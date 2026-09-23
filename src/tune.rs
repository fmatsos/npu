//! `npu backend tune`: sizes the static context of every NPU-compiled model
//! from the model's own characteristics and the host's memory.
//!
//! An `OpenVINO` NPU graph is compiled for a STATIC context: `MAX_PROMPT_LEN`
//! tokens of prompt plus `MIN_RESPONSE_LEN` tokens of answer, both read from
//! the root of `plugin_config` in the export's `graph.pbtxt`. A longer prompt
//! is refused, a longer answer is cut. This module picks both values, and the
//! matching `[generation].max_tokens` of the model file and of its fallback,
//! from:
//!
//! - the model: `max_position_embeddings` and the per-token cost read from
//!   its Hugging Face `config.json`;
//! - the host: `--max-memory` percent of the total RAM, minus the weights of
//!   the `--max-models` heaviest NPU models, split evenly between them so they
//!   can all run at once.
//!
//! GPU twins are outside the budget: their context is dynamic, so there is
//! nothing to size, but on unified memory a served twin adds its own KV
//! cache on top of it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

// ponytail: an NPU graph costs, per token of context, its KV cache PLUS about
// ACTIVATION_TENTHS / 10 x hidden_size x layers bytes of static buffers — the
// second term dominates on models with few KV heads. Fitted on a Meteor Lake
// NPU (Core Ultra 7 165U, OVMS 2026.4, int4), container memory minus weights:
//   Qwen3-4B      7K 7.3 GB, 16K 13.1 GB, 32K 33.6 GB
//   Qwen3-8B      7K 10.6 GB
//   Coder-7B     16K 13.4 GB
//   Coder-3B     20K 11.0 GB
// The formula lands within +0..+15 % of each: never under. Recalibrate on
// another NPU or OVMS version.
const ACTIVATION_TENTHS: u64 = 62;
/// Past [`LONG_CONTEXT`] tokens (measured at 32K).
const ACTIVATION_TENTHS_LONG: u64 = 90;
const LONG_CONTEXT: u64 = 24_576;

/// Contexts are rounded down to this step, and never go below it.
const STEP: u64 = 1024;

/// What `plan_context` needs from a model's `config.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    max_context: u64,
    /// fp16 KV cache bytes per token: 2 x layers x KV heads x head dim x 2.
    kv_per_token: u64,
    /// hidden size x layers: what the per-token static buffers scale with.
    width: u64,
}

fn shape_of(config: &serde_json::Value, source: &Path) -> crate::Result<Shape> {
    let field = |name: &str| config.get(name).and_then(serde_json::Value::as_u64);
    let required = |name: &str| {
        field(name).ok_or_else(|| {
            crate::Error::Config(format!(
                "{}: missing or non-integer \"{name}\"",
                source.display()
            ))
        })
    };
    let layers = required("num_hidden_layers")?;
    let heads = required("num_attention_heads")?;
    let hidden = required("hidden_size")?;
    let head_dim = field("head_dim").unwrap_or(hidden / heads.max(1));
    let kv_heads = field("num_key_value_heads").unwrap_or(heads);
    Ok(Shape {
        max_context: required("max_position_embeddings")?,
        kv_per_token: 2 * layers * kv_heads * head_dim * 2,
        width: hidden * layers,
    })
}

/// Estimated NPU memory for a `tokens` context, weights excluded.
fn context_bytes(tokens: u64, shape: Shape) -> u64 {
    let tenths = if tokens <= LONG_CONTEXT {
        ACTIVATION_TENTHS
    } else {
        ACTIVATION_TENTHS_LONG
    };
    tokens * (shape.kv_per_token * 10 + tenths * shape.width) / 10
}

/// `(MAX_PROMPT_LEN, MIN_RESPONSE_LEN)` fitting both the model and `budget`
/// bytes; a quarter of the context goes to the answer.
fn plan_context(shape: Shape, budget: u64) -> (u64, u64) {
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
fn set_node_option(graph: &str, key: &str, value: &str, source: &Path) -> crate::Result<String> {
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
                crate::Error::Config(format!(
                    "{}: no models_path in node_options",
                    source.display()
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
fn edit_plugin_config(
    graph: &str,
    source: &Path,
    edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> crate::Result<String> {
    const OPEN: &str = "plugin_config: '";
    let malformed = |why: &str| crate::Error::Config(format!("{}: {why}", source.display()));
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

/// Sets `max_tokens` inside the `[generation]` table of a model file, adding
/// the key or the table when absent, and leaving every other line — comments
/// included — untouched. The result is re-parsed: any layout this line edit
/// does not understand (a dotted key, an inline table) is refused naming the
/// file, never written.
fn set_max_tokens(text: &str, max_tokens: u64, source: &Path) -> crate::Result<String> {
    let line = format!("max_tokens = {max_tokens}");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let header = lines
        .iter()
        .position(|l| l.trim_start().starts_with("[generation]"));
    match header {
        Some(header) => {
            let table_end = lines[header + 1..]
                .iter()
                .position(|l| l.trim_start().starts_with('['))
                .map_or(lines.len(), |i| header + 1 + i);
            let existing = (header + 1..table_end).find(|&i| {
                lines[i]
                    .trim_start()
                    .strip_prefix("max_tokens")
                    .is_some_and(|rest| rest.trim_start().starts_with('='))
            });
            match existing {
                Some(i) => lines[i] = line,
                None => lines.insert(header + 1, line),
            }
        }
        None => lines.extend([String::new(), "[generation]".into(), line]),
    }
    let edited = lines.join("\n") + "\n";

    let parsed: crate::config::Model = toml::from_str(&edited).map_err(|e| {
        crate::Error::Config(format!(
            "{}: cannot set [generation].max_tokens in this layout: {e}",
            source.display()
        ))
    })?;
    if parsed.generation.max_tokens != Some(u32::try_from(max_tokens).unwrap_or(u32::MAX)) {
        return Err(crate::Error::Config(format!(
            "{}: cannot set [generation].max_tokens in this layout",
            source.display()
        )));
    }
    Ok(edited)
}

/// Replaces `path` rather than rewriting it: a `graph.pbtxt` generated by a
/// container running as another user cannot be opened for writing, but can
/// be replaced when its directory is ours. A crash mid-way never leaves a
/// half-written file either.
fn replace_file(path: &Path, text: &str) -> crate::Result<()> {
    let mut staging = path.as_os_str().to_owned();
    staging.push(".npu-tune.tmp");
    let with_path =
        |e: std::io::Error| std::io::Error::new(e.kind(), format!("{}: {e}", path.display()));
    std::fs::write(&staging, text).map_err(with_path)?;
    std::fs::rename(&staging, path).map_err(with_path)?;
    Ok(())
}

fn read_export(path: &Path) -> crate::Result<String> {
    std::fs::read_to_string(path)
        .map_err(|e| crate::Error::Config(format!("{}: {e}", path.display())))
}

/// `12_345_678_901` -> `"12.3"`: gigabytes with one decimal, in integers.
fn gb(bytes: u64) -> String {
    format!("{}.{}", bytes / 1_000_000_000, bytes / 100_000_000 % 10)
}

/// A device an export's `graph.pbtxt` is compiled for, as far as `tune` is
/// concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Npu,
    Gpu,
}

impl Device {
    fn of(graph: &str) -> Option<Self> {
        if graph.contains("device: \"NPU\"") {
            Some(Self::Npu)
        } else if graph.contains("device: \"GPU\"") {
            Some(Self::Gpu)
        } else {
            None
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Npu => "NPU",
            Self::Gpu => "GPU",
        }
    }
}

/// What the caller constrains: which devices are tuned, how many of their
/// models run at once (`None`: all of them), which share of the total RAM
/// they get together, in percent, and whether GPU KV caches are `u8`.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub npu: bool,
    pub gpu: bool,
    pub max_models: Option<usize>,
    pub max_memory_percent: u64,
    pub kv_u8: bool,
}

// ponytail: a GPU model is served by continuous batching; one user never
// runs more than a handful of requests at once, and every slot reserves
// scheduler memory. Make it a flag the day several users share a backend.
const GPU_MAX_NUM_SEQS: u64 = 4;
const GIB: u64 = 1 << 30;

struct Planned {
    id: String,
    device: Device,
    shape: Shape,
    weights: u64,
    graph: PathBuf,
}

/// The new `graph.pbtxt`, the answer length, and the estimated memory of
/// one model inside `budget` bytes.
fn plan_graph(
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

/// Plans every model whose export under `models_dir` is compiled for a
/// device in `limits`, writes it unless `dry_run`, and returns the plan —
/// this command's result. Every new content is computed before anything is
/// written, so a malformed file aborts the whole run instead of half of it.
///
/// # Errors
///
/// - `Error::Config` when no model has an export for those devices, or when
///   an export or a model file is missing or malformed — naming the file;
/// - `Error::Io` when a file cannot be replaced.
pub fn tune(
    config: &crate::config::Config,
    models_dir: &Path,
    total_ram: u64,
    limits: Limits,
    dry_run: bool,
    logger: crate::log::Logger,
) -> crate::Result<String> {
    let mut ids: Vec<&String> = config.models.keys().collect();
    ids.sort_unstable();

    let mut planned = Vec::new();
    for id in ids {
        let export = models_dir.join(&config.models[id].model);
        let graph = export.join("graph.pbtxt");
        let device = std::fs::read_to_string(&graph)
            .ok()
            .and_then(|g| Device::of(&g))
            .filter(|d| match d {
                Device::Npu => limits.npu,
                Device::Gpu => limits.gpu,
            });
        let Some(device) = device else {
            continue;
        };
        let config_json = export.join("config.json");
        let json: serde_json::Value = serde_json::from_str(&read_export(&config_json)?)
            .map_err(|e| crate::Error::Config(format!("{}: {e}", config_json.display())))?;
        let weights_file = export.join("openvino_model.bin");
        let weights = std::fs::metadata(&weights_file)
            .map_err(|e| crate::Error::Config(format!("{}: {e}", weights_file.display())))?
            .len();
        planned.push(Planned {
            id: id.clone(),
            device,
            shape: shape_of(&json, &config_json)?,
            weights,
            graph,
        });
    }
    let devices = match (limits.npu, limits.gpu) {
        (true, false) => "NPU",
        (false, true) => "GPU",
        _ => "NPU or GPU",
    };
    if planned.is_empty() {
        return Err(crate::Error::Config(format!(
            "no configured model has an {devices} export under {}",
            models_dir.display()
        )));
    }
    if limits.kv_u8 && !limits.gpu {
        logger.warn("--kv-u8 only applies to GPU models: ignored with --npu alone");
    }

    let at_once = limits
        .max_models
        .unwrap_or(planned.len())
        .clamp(1, planned.len());
    let mut heaviest: Vec<u64> = planned.iter().map(|p| p.weights).collect();
    heaviest.sort_unstable_by(|a, b| b.cmp(a));
    let weights: u64 = heaviest.iter().take(at_once).sum();
    let granted = total_ram / 100 * limits.max_memory_percent;
    let budget = granted.saturating_sub(weights) / at_once as u64;

    let mut report = format!(
        "RAM {} GB x {}% - weights {} GB = {} GB per model ({at_once} of {} {devices} models at once)\n\n\
         {:32} {:6} {:>9} {:>9} {:>7} {:>7} {:>11}\n",
        gb(total_ram),
        limits.max_memory_percent,
        gb(weights),
        gb(budget),
        planned.len(),
        "model",
        "device",
        "model max",
        "KV/token",
        "prompt",
        "answer",
        "est. memory",
    );
    let mut writes = Vec::new();
    for p in &planned {
        let graph_text = std::fs::read_to_string(&p.graph)?;
        let (text, prompt, response, memory) = plan_graph(p, &graph_text, budget, limits.kv_u8)?;
        if memory > budget {
            logger.warn(&format!(
                "model \"{}\": even the smallest context ({} tokens) exceeds its share of {} GB",
                p.id,
                prompt + response,
                gb(budget)
            ));
        }
        let _ = writeln!(
            report,
            "{:32} {:6} {:>9} {:>7}KB {prompt:>7} {response:>7} {:>9}GB",
            p.id,
            p.device.label(),
            p.shape.max_context,
            p.shape.kv_per_token / 1024,
            gb(memory + p.weights),
        );
        writes.push((p.graph.clone(), text));
        let file = &config.models[&p.id].source;
        let model_text = std::fs::read_to_string(file)?;
        writes.push((file.clone(), set_max_tokens(&model_text, response, file)?));
    }

    if !dry_run {
        for (path, text) in &writes {
            replace_file(path, text)?;
        }
        logger.info(
            "applied the plan to each graph.pbtxt and the max_tokens of each model: \
             the next `npu backend serve` recompiles the graph",
        );
    }
    Ok(report.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const QWEN3_4B: Shape = Shape {
        max_context: 40_960,
        kv_per_token: 147_456,
        width: 92_160,
    };

    fn path() -> &'static Path {
        Path::new("models/x.toml")
    }

    #[test]
    fn shape_defaults_head_dim_and_kv_heads() {
        let coder = serde_json::json!({"num_hidden_layers": 36, "num_key_value_heads": 2,
            "num_attention_heads": 16, "hidden_size": 2048, "max_position_embeddings": 32768});
        assert_eq!(
            shape_of(&coder, path()).map(|s| s.kv_per_token).ok(),
            Some(36_864)
        );
        let qwen = serde_json::json!({"num_hidden_layers": 36, "num_key_value_heads": 8,
            "num_attention_heads": 32, "hidden_size": 2560, "head_dim": 128,
            "max_position_embeddings": 40960});
        assert_eq!(shape_of(&qwen, path()).ok(), Some(QWEN3_4B));
    }

    #[test]
    fn a_config_missing_a_field_is_a_config_error_naming_the_file() {
        let err = shape_of(&serde_json::json!({}), path()).err();
        assert!(matches!(&err, Some(crate::Error::Config(m)) if m.contains("models/x.toml")));
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

    const MODEL: &str =
        "# comment\nid = \"x\"\nbackend = \"b\"\noperation = \"chat\"\nmodel = \"m\"\n";

    fn max_tokens_of(text: &str) -> Option<u32> {
        toml::from_str::<crate::config::Model>(text)
            .ok()
            .and_then(|m| m.generation.max_tokens)
    }

    #[test]
    fn max_tokens_is_replaced_added_or_given_a_table() {
        let replaced = set_max_tokens(
            &format!("{MODEL}[generation]\nmax_tokens = 1\n"),
            99,
            path(),
        );
        assert_eq!(replaced.as_deref().map(max_tokens_of).ok(), Some(Some(99)));
        let added = set_max_tokens(
            &format!("{MODEL}[generation]\ntemperature = 0.0\n"),
            7,
            path(),
        );
        assert_eq!(added.as_deref().map(max_tokens_of).ok(), Some(Some(7)));
        let table = set_max_tokens(MODEL, 5, path()).unwrap_or_default();
        assert_eq!(max_tokens_of(&table), Some(5));
        assert!(table.starts_with("# comment\n"));
    }

    #[test]
    fn a_layout_the_line_edit_does_not_understand_is_refused() {
        let inline = format!("{MODEL}generation = {{ max_tokens = 1 }}\n");
        assert!(matches!(
            set_max_tokens(&inline, 9, path()),
            Err(crate::Error::Config(_))
        ));
    }

    #[test]
    fn gigabytes_keep_one_truncated_decimal() {
        assert_eq!(gb(65_500_000_000), "65.5");
        assert_eq!(gb(7_079_999_999), "7.0");
    }
}
