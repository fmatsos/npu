//! `npu model discover`: searches Hugging Face for models this host's Intel
//! NPU can run through `OpenVINO`, and lists only those.
//!
//! A candidate survives when ALL of these hold:
//!
//! - its `model_type` is an architecture `optimum-intel` exports to `OpenVINO`
//!   for the requested task — read from `optimum-intel`'s own registry at run
//!   time, never from a list frozen in this binary;
//! - it is not gated, unless `HF_TOKEN` is set;
//! - its INT4 weights fit `--max-memory` percent of the host's RAM (a client
//!   NPU has no memory of its own).
//!
//! Everything else is left out, not listed with a caveat. When the `llmfit`
//! CLI is on `PATH`, its score and use case are added to each survivor.
//!
//! Everything that touches the outside world — HTTP, `llmfit`, the NPU
//! device, the RAM — comes in through [`World`], so the filter is tested
//! without a network.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

// ponytail: `main`, not the latest release — `pip install optimum-intel`
// may lag it by an architecture or two, which `npu-export`'s CPU check
// catches. Pin a release tag if that ever misleads someone.
pub const ARCHITECTURES_URL: &str = "https://raw.githubusercontent.com/huggingface/optimum-intel/main/optimum/exporters/openvino/model_configs.py";
const HUB_API: &str = "https://huggingface.co/api/models";

// ponytail: INT4 is half a byte per parameter; embeddings and the head stay
// wider, measured at about +20 % on the Qwen3 exports. Refine per
// architecture if a model that fits here fails to load.
const INT4_OVERHEAD_PERCENT: u64 = 120;

/// What the caller asks for.
#[derive(Debug, Clone)]
pub struct Query {
    pub text: Option<String>,
    pub task: String,
    pub limit: usize,
    pub candidates: usize,
    pub max_memory_percent: u64,
    pub hf_token: Option<String>,
}

/// The outside world, injected.
pub struct World<'a> {
    /// GET `url`, with an optional bearer token; the body as text.
    pub fetch: &'a dyn Fn(&str, Option<&str>) -> crate::Result<String>,
    /// `llmfit fit --json`'s stdout, `None` when `llmfit` is not available.
    pub llmfit: &'a dyn Fn() -> Option<String>,
    pub has_npu: bool,
    pub total_ram: u64,
}

// A struct of `&dyn Fn` cannot derive `Debug`: the closures have nothing to
// print, the facts have.
impl std::fmt::Debug for World<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("World")
            .field("has_npu", &self.has_npu)
            .field("total_ram", &self.total_ram)
            .finish_non_exhaustive()
    }
}

/// The architectures `optimum-intel` registers for `task`, parsed from its
/// `model_configs.py`: every `@register_in_tasks_manager("<type>", ...)`
/// whose arguments name `"<task>"` or `"<task>-with-past"`.
fn exportable_architectures(source: &str, task: &str) -> BTreeSet<String> {
    const CALL: &str = "@register_in_tasks_manager(";
    let wanted = [format!("\"{task}\""), format!("\"{task}-with-past\"")];
    let mut found = BTreeSet::new();
    let mut rest = source;
    while let Some(at) = rest.find(CALL) {
        rest = &rest[at + CALL.len()..];
        let mut depth = 1;
        let end = rest
            .char_indices()
            .find(|&(_, c)| {
                match c {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                depth == 0
            })
            .map_or(rest.len(), |(i, _)| i);
        let args = &rest[..end];
        let name = args
            .trim_start()
            .strip_prefix('"')
            .and_then(|s| s.split('"').next());
        if let Some(name) = name
            && wanted.iter().any(|w| args.contains(w.as_str()))
        {
            found.insert(name.to_string());
        }
    }
    found
}

/// Percent-encodes a query-string value.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn search_url(query: &Query) -> String {
    let mut url = format!(
        "{HUB_API}?filter={}&sort=downloads&direction=-1&limit={}\
         &expand[]=config&expand[]=gated&expand[]=safetensors&expand[]=cardData&expand[]=downloads&expand[]=pipeline_tag",
        encode(&query.task),
        query.candidates
    );
    if let Some(text) = &query.text {
        let _ = write!(url, "&search={}", encode(text));
    }
    url
}

#[derive(Debug, PartialEq)]
struct Survivor {
    id: String,
    model_type: String,
    parameters: u64,
    license: String,
    downloads: u64,
}

// ponytail: below this, a "model" is a tokenizer test fixture or a toy, not
// something to chat with. Make it a flag if a tiny model is ever the point.
const MIN_PARAMETERS: u64 = 100_000_000;

fn int4_bytes(parameters: u64) -> u64 {
    parameters / 2 * INT4_OVERHEAD_PERCENT / 100
}

/// The Hub's answer, reduced to the candidates that pass every filter, in
/// the Hub's (downloads) order.
fn survivors(
    hub: &serde_json::Value,
    architectures: &BTreeSet<String>,
    ceiling: u64,
    task: &str,
    authenticated: bool,
) -> Vec<Survivor> {
    hub.as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let model_type = m.pointer("/config/model_type")?.as_str()?;
            let parameters = m.pointer("/safetensors/total")?.as_u64()?;
            let gated = m
                .get("gated")
                .is_some_and(|g| g != &serde_json::Value::Bool(false));
            let id = m.get("id")?.as_str()?;
            // A repository already quantized (AWQ, GPTQ, FP8...) is not what
            // `optimum-cli export --weight-format int4` starts from, and its
            // `safetensors.total` counts packed tensors, not parameters.
            let quantized = m.pointer("/config/quantization_config").is_some();
            let keep = architectures.contains(model_type)
                && m.get("pipeline_tag").and_then(serde_json::Value::as_str) == Some(task)
                && !quantized
                && parameters >= MIN_PARAMETERS
                && (authenticated || !gated)
                && int4_bytes(parameters) <= ceiling;
            keep.then(|| Survivor {
                id: id.to_string(),
                model_type: model_type.to_string(),
                parameters,
                license: m
                    .pointer("/cardData/license")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("-")
                    .to_string(),
                downloads: m
                    .get("downloads")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
            })
        })
        .collect()
}

/// `llmfit fit --json`, indexed by lowercased Hugging Face id: score and use
/// case.
fn llmfit_index(json: &str) -> HashMap<String, (f64, String)> {
    let parsed: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
    parsed
        .get("models")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let name = m.get("name")?.as_str()?.to_lowercase();
            let score = m.get("score")?.as_f64()?;
            let use_case = m
                .get("use_case")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("-");
            Some((name, (score, use_case.to_string())))
        })
        .collect()
}

/// `8_190_735_360` -> `"8.2B"`.
fn billions(parameters: u64) -> String {
    let tenths = (parameters + 50_000_000) / 100_000_000;
    format!("{}.{}B", tenths / 10, tenths % 10)
}

/// Runs the search and returns the report — this command's result.
///
/// # Errors
///
/// `Error::Backend` when the host has no Intel NPU, when Hugging Face or
/// `optimum-intel`'s registry cannot be reached, or when that registry no
/// longer yields any architecture — each naming what failed.
pub fn discover(
    query: &Query,
    world: &World<'_>,
    logger: crate::log::Logger,
) -> crate::Result<String> {
    if !world.has_npu {
        return Err(crate::Error::Backend(
            "no Intel NPU on this host (no /dev/accel/accel* device): \
             nothing found here could run on it"
                .to_string(),
        ));
    }
    let architectures =
        exportable_architectures(&(world.fetch)(ARCHITECTURES_URL, None)?, &query.task);
    if architectures.is_empty() {
        return Err(crate::Error::Backend(format!(
            "no architecture exportable for task \"{}\" found in {ARCHITECTURES_URL}",
            query.task
        )));
    }
    logger.info(&format!(
        "{} architectures exportable to OpenVINO for \"{}\"",
        architectures.len(),
        query.task
    ));

    let url = search_url(query);
    let body = (world.fetch)(&url, query.hf_token.as_deref())?;
    let hub: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| crate::Error::Backend(format!("unexpected answer from {HUB_API}: {e}")))?;
    let ceiling = world.total_ram / 100 * query.max_memory_percent;
    let mut found = survivors(
        &hub,
        &architectures,
        ceiling,
        &query.task,
        query.hf_token.is_some(),
    );
    found.truncate(query.limit);

    let llmfit = (world.llmfit)().map(|json| llmfit_index(&json));
    if llmfit.is_none() {
        logger.info("llmfit is not on PATH: no score or use case to add");
    }

    let mut report = format!(
        "{:48} {:14} {:>7} {:>8} {:14} {:>11}",
        "model", "type", "params", "int4 ~GB", "license", "downloads"
    );
    if llmfit.is_some() {
        let _ = write!(report, " {:>6}  use case", "score");
    }
    for s in &found {
        let int4 = int4_bytes(s.parameters);
        let _ = write!(
            report,
            "\n{:48} {:14} {:>7} {:>8} {:14} {:>11}",
            s.id,
            s.model_type,
            billions(s.parameters),
            format!("{}.{}", int4 / 1_000_000_000, int4 / 100_000_000 % 10),
            s.license,
            s.downloads
        );
        if let Some(index) = &llmfit {
            match index.get(&s.id.to_lowercase()) {
                Some((score, use_case)) => {
                    let _ = write!(report, " {score:>6.1}  {use_case}");
                }
                None => report.push_str("      -  -"),
            }
        }
    }
    if found.is_empty() {
        logger.warn("no candidate passed the filter: try a broader query or more --candidates");
    }
    Ok(report)
}

/// The real [`World::fetch`]: a GET through `ureq`, `Error::Backend` on any
/// failure, naming the URL.
///
/// # Errors
///
/// `Error::Backend` when the request fails or the status is not 2xx.
pub fn fetch(url: &str, token: Option<&str>) -> crate::Result<String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(60)))
        .build()
        .new_agent();
    let mut request = agent
        .get(url)
        .header("User-Agent", concat!("npu/", env!("CARGO_PKG_VERSION")));
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let failed =
        |e: &dyn std::fmt::Display| crate::Error::Backend(format!("GET {url} failed: {e}"));
    request
        .call()
        .map_err(|e| failed(&e))?
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| failed(&e))
}

/// Whether the host exposes an Intel NPU: an `accel*` node under
/// `/dev/accel`, which is what OVMS's `--device /dev/accel` passes through.
#[must_use]
pub fn host_has_npu() -> bool {
    std::fs::read_dir("/dev/accel").is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("accel"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: &str = r#"
@register_in_tasks_manager(
    "qwen3",
    *["text-generation", "text-generation-with-past"],
    library_name="transformers",
)
class Qwen3OpenVINOConfig(Base):
    pass

@register_in_tasks_manager("bert", *["feature-extraction", "text2text-generation"])
class BertConfig(Base):
    pass

@register_in_tasks_manager("llama", *COMMON_TEXT_GENERATION_TASKS, "text-generation-with-past")
class LlamaConfig(Base):
    pass
"#;

    #[test]
    fn only_architectures_registered_for_the_task_are_kept() {
        let found = exportable_architectures(REGISTRY, "text-generation");
        assert_eq!(found.into_iter().collect::<Vec<_>>(), ["llama", "qwen3"]);
        let features = exportable_architectures(REGISTRY, "feature-extraction");
        assert_eq!(features.into_iter().collect::<Vec<_>>(), ["bert"]);
    }

    fn hub() -> serde_json::Value {
        serde_json::json!([
            {"id": "Qwen/Qwen3-8B", "gated": false, "downloads": 10, "pipeline_tag": "text-generation",
             "config": {"model_type": "qwen3"}, "safetensors": {"total": 8_190_735_360_u64},
             "cardData": {"license": "apache-2.0"}},
            {"id": "someone/huge", "gated": false, "pipeline_tag": "text-generation",
             "config": {"model_type": "qwen3"}, "safetensors": {"total": 400_000_000_000_u64}},
            {"id": "meta/gated", "gated": "manual", "pipeline_tag": "text-generation",
             "config": {"model_type": "llama"}, "safetensors": {"total": 1_000_000_000}},
            {"id": "x/bert", "gated": false,
             "config": {"model_type": "bert"}, "safetensors": {"total": 1_000_000}},
            {"id": "x/gguf-only", "gated": false},
            {"id": "x/awq", "gated": false, "pipeline_tag": "text-generation",
             "config": {"model_type": "qwen3", "quantization_config": {}},
             "safetensors": {"total": 1_000_000_000}},
            {"id": "x/embedding", "gated": false, "pipeline_tag": "feature-extraction",
             "config": {"model_type": "qwen3"}, "safetensors": {"total": 1_000_000_000}},
            {"id": "x/toy", "gated": false, "pipeline_tag": "text-generation",
             "config": {"model_type": "qwen3"}, "safetensors": {"total": 5_000_000}}
        ])
    }

    #[test]
    fn a_candidate_failing_any_filter_is_left_out() {
        let archs: BTreeSet<String> = ["qwen3", "llama"].map(String::from).into();
        let found = survivors(&hub(), &archs, 32_000_000_000, "text-generation", false);
        let ids: Vec<&str> = found.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["Qwen/Qwen3-8B"]);
        assert_eq!(found[0].license, "apache-2.0");
        let with_token = survivors(&hub(), &archs, 32_000_000_000, "text-generation", true);
        assert!(with_token.iter().any(|s| s.id == "meta/gated"));
    }

    #[test]
    fn the_search_url_encodes_the_query() {
        let query = Query {
            text: Some("qwen coder 7b".into()),
            task: "text-generation".into(),
            limit: 5,
            candidates: 50,
            max_memory_percent: 50,
            hf_token: None,
        };
        let url = search_url(&query);
        assert!(url.contains("search=qwen%20coder%207b") && url.contains("limit=50"));
    }

    #[test]
    fn llmfit_scores_are_indexed_by_lowercased_id() {
        let index =
            llmfit_index(r#"{"models":[{"name":"Qwen/Qwen3-8B","score":80.5,"use_case":"chat"}]}"#);
        assert_eq!(
            index.get("qwen/qwen3-8b"),
            Some(&(80.5, "chat".to_string()))
        );
        assert!(llmfit_index("not json").is_empty());
    }

    #[test]
    fn no_npu_is_a_backend_error_before_any_request() {
        let fetch = |_: &str, _: Option<&str>| -> crate::Result<String> {
            Err(crate::Error::Config("must not be called".into()))
        };
        let world = World {
            fetch: &fetch,
            llmfit: &|| None,
            has_npu: false,
            total_ram: 1,
        };
        let query = Query {
            text: None,
            task: "text-generation".into(),
            limit: 1,
            candidates: 1,
            max_memory_percent: 50,
            hf_token: None,
        };
        let err = discover(
            &query,
            &world,
            crate::log::Logger::new(crate::log::Level::Error),
        )
        .err();
        assert!(matches!(err, Some(crate::Error::Backend(_))));
    }

    #[test]
    fn parameters_read_as_billions() {
        assert_eq!(billions(8_190_735_360), "8.2B");
        assert_eq!(billions(751_632_384), "0.8B");
    }
}
