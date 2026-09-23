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

/// Sets `MAX_PROMPT_LEN` and `MIN_RESPONSE_LEN` at the ROOT of the JSON held
/// by `plugin_config: '...'` — under `DEVICE_PROPERTIES` they are ignored.
fn set_plugin_lengths(
    graph: &str,
    prompt: u64,
    response: u64,
    source: &Path,
) -> crate::Result<String> {
    const OPEN: &str = "plugin_config: '";
    let malformed = |why: &str| crate::Error::Config(format!("{}: {why}", source.display()));
    let start = graph
        .find(OPEN)
        .ok_or_else(|| malformed("no plugin_config"))?
        + OPEN.len();
    let end = start
        + graph[start..]
            .find('\'')
            .ok_or_else(|| malformed("unterminated plugin_config"))?;
    let mut plugin: serde_json::Value = serde_json::from_str(&graph[start..end])
        .map_err(|e| malformed(&format!("plugin_config is not JSON: {e}")))?;
    let object = plugin
        .as_object_mut()
        .ok_or_else(|| malformed("plugin_config is not a JSON object"))?;
    object.insert("MAX_PROMPT_LEN".into(), prompt.into());
    object.insert("MIN_RESPONSE_LEN".into(), response.into());
    Ok(format!("{}{plugin}{}", &graph[..start], &graph[end..]))
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

/// What the caller constrains: how many NPU models run at once (`None`: all
/// of them) and which share of the total RAM they get together, in percent.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_models: Option<usize>,
    pub max_memory_percent: u64,
}

struct Planned {
    id: String,
    shape: Shape,
    weights: u64,
    graph: PathBuf,
}

/// Plans the context of every model whose export under `models_dir` holds
/// an NPU graph, writes it unless `dry_run`, and returns the plan — this
/// command's result. Every new content is computed before anything is
/// written, so a malformed file aborts the whole run instead of half of it.
///
/// # Errors
///
/// - `Error::Config` when no model has an NPU export, or when an export or a
///   model file is missing or malformed — naming the file;
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
        let is_npu = std::fs::read_to_string(&graph).is_ok_and(|g| g.contains("device: \"NPU\""));
        if !is_npu {
            continue;
        }
        let config_json = export.join("config.json");
        let json: serde_json::Value = serde_json::from_str(&read_export(&config_json)?)
            .map_err(|e| crate::Error::Config(format!("{}: {e}", config_json.display())))?;
        let weights_file = export.join("openvino_model.bin");
        let weights = std::fs::metadata(&weights_file)
            .map_err(|e| crate::Error::Config(format!("{}: {e}", weights_file.display())))?
            .len();
        planned.push(Planned {
            id: id.clone(),
            shape: shape_of(&json, &config_json)?,
            weights,
            graph,
        });
    }
    if planned.is_empty() {
        return Err(crate::Error::Config(format!(
            "no configured model has an NPU export under {}",
            models_dir.display()
        )));
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
        "RAM {} GB x {}% - weights {} GB = {} GB per model ({at_once} of {} NPU models at once)\n\n\
         {:32} {:>9} {:>9} {:>7} {:>7} {:>11}\n",
        gb(total_ram),
        limits.max_memory_percent,
        gb(weights),
        gb(budget),
        planned.len(),
        "model",
        "model max",
        "KV/token",
        "prompt",
        "answer",
        "est. memory",
    );
    let mut writes = Vec::new();
    for p in &planned {
        let (prompt, response) = plan_context(p.shape, budget);
        let context = context_bytes(prompt + response, p.shape);
        if context > budget {
            logger.warn(&format!(
                "model \"{}\": even the smallest context ({} tokens) exceeds its share of {} GB",
                p.id,
                prompt + response,
                gb(budget)
            ));
        }
        let _ = writeln!(
            report,
            "{:32} {:>9} {:>7}KB {prompt:>7} {response:>7} {:>9}GB",
            p.id,
            p.shape.max_context,
            p.shape.kv_per_token / 1024,
            gb(context + p.weights),
        );

        let graph_text = std::fs::read_to_string(&p.graph)?;
        writes.push((
            p.graph.clone(),
            set_plugin_lengths(&graph_text, prompt, response, &p.graph)?,
        ));
        let model = &config.models[&p.id];
        let fallback = model.fallback.as_ref().and_then(|f| config.models.get(f));
        for file in std::iter::once(model).chain(fallback).map(|m| &m.source) {
            let text = std::fs::read_to_string(file)?;
            writes.push((file.clone(), set_max_tokens(&text, response, file)?));
        }
    }

    if !dry_run {
        for (path, text) in &writes {
            replace_file(path, text)?;
        }
        logger.info(
            "applied MAX_PROMPT_LEN and MIN_RESPONSE_LEN to each graph.pbtxt, and max_tokens to each \
             model and its fallback: the next `npu backend serve` recompiles the NPU graph",
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

    #[test]
    fn plugin_lengths_go_to_the_root_of_plugin_config() {
        let graph = "a\nplugin_config: '{\"DEVICE_PROPERTIES\":{\"NPU\":{}}}',\nb";
        let patched = set_plugin_lengths(graph, 3072, 1024, path()).unwrap_or_default();
        let json: serde_json::Value =
            serde_json::from_str(patched.split('\'').nth(1).unwrap_or_default())
                .unwrap_or_default();
        assert_eq!(json["MAX_PROMPT_LEN"], 3072);
        assert_eq!(json["MIN_RESPONSE_LEN"], 1024);
        assert!(json["DEVICE_PROPERTIES"].is_object());
        assert!(patched.starts_with("a\n") && patched.ends_with("',\nb"));
        assert!(set_plugin_lengths("nothing", 1, 1, path()).is_err());
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
