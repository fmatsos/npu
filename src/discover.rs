//! `npu model discover`: searches Hugging Face for the models this host can
//! run, whatever runs them — CPU, GPU or NPU.
//!
//! By default a candidate is kept when its weights fit the host: llmfit's
//! verdict (`Perfect` or `Good`, and a score of at least `--min-score`) when
//! the `llmfit` CLI is on `PATH` and knows the model, otherwise an INT4
//! estimate against `--max-memory` percent of the RAM. Opt-in filters
//! narrow the list further:
//!
//! - `--backend openvino`: an architecture `optimum-intel` exports to
//!   `OpenVINO` for the task — read from its own registry at run time, never
//!   frozen in this binary —, original weights (not already quantized), not
//!   gated unless `HF_TOKEN` is set;
//! - `--backend llamacpp`: a GGUF repository;
//! - `--backend mlx`: an MLX repository;
//! - `--npu`: `--backend openvino`, on a host that has an Intel NPU.
//!
//! `--backend` also takes a configured backend's identifier, whose engine is
//! read from its runtime (cf. [`Engine::of_backend`]).
//!
//! Everything else is left out, not listed with a caveat. Everything that
//! touches the outside world — HTTP, `llmfit`, the NPU device, the RAM —
//! comes in through [`World`], so the filters are tested without a network.

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
    pub min_score: f64,
    pub engine: Option<Engine>,
    pub npu: bool,
    pub hf_token: Option<String>,
}

/// The inference engine a model must be packaged for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    OpenVino,
    LlamaCpp,
    Mlx,
}

impl Engine {
    /// The names `--backend` accepts for an engine.
    pub const NAMES: &'static str = "openvino, llamacpp, mlx";

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "openvino" | "ovms" => Some(Self::OpenVino),
            "llamacpp" | "llama.cpp" | "llama-cpp" | "gguf" => Some(Self::LlamaCpp),
            "mlx" => Some(Self::Mlx),
            _ => None,
        }
    }

    /// The engine a configured backend runs, read from what its runtime
    /// starts: the Docker image and arguments, or the process command and
    /// arguments. `None` when neither names a known engine.
    #[must_use]
    pub fn of_backend(backend: &crate::config::Backend) -> Option<Self> {
        let started = match backend.runtime()? {
            crate::config::Runtime::Docker(d) => format!("{} {}", d.image, d.args.join(" ")),
            crate::config::Runtime::Process(p) => {
                format!("{} {}", p.command, p.arguments.join(" "))
            }
        }
        .to_ascii_lowercase();
        if started.contains("model_server")
            || started.contains("openvino")
            || started.contains("ovms")
        {
            Some(Self::OpenVino)
        } else if started.contains("llama") {
            Some(Self::LlamaCpp)
        } else if started.contains("mlx") {
            Some(Self::Mlx)
        } else {
            None
        }
    }

    /// The Hub tag a repository carries for this engine, searched server-side.
    fn hub_filter(self) -> Option<&'static str> {
        match self {
            Self::OpenVino => None,
            Self::LlamaCpp => Some("gguf"),
            Self::Mlx => Some("mlx"),
        }
    }
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
    if let Some(tag) = query.engine.and_then(Engine::hub_filter) {
        let _ = write!(url, "&filter={tag}&expand[]=gguf");
    }
    if let Some(text) = &query.text {
        let _ = write!(url, "&search={}", encode(text));
    }
    url
}

/// llmfit's view of one model.
#[derive(Debug, Clone, PartialEq)]
struct Fit {
    score: f64,
    level: String,
    memory_gb: f64,
    run_mode: String,
    use_case: String,
}

#[derive(Debug, PartialEq)]
struct Survivor {
    id: String,
    model_type: String,
    parameters: Option<u64>,
    /// Estimated weights in memory, for a model llmfit does not size.
    estimate: Option<u64>,
    license: String,
    downloads: u64,
    fit: Option<Fit>,
}

// ponytail: below this, a "model" is a tokenizer test fixture or a toy that
// no score would call good. Make it a flag if a tiny model is ever the point.
const MIN_PARAMETERS: u64 = 100_000_000;

fn int4_bytes(parameters: u64) -> u64 {
    parameters / 2 * INT4_OVERHEAD_PERCENT / 100
}

// ponytail: an MLX repository is already quantized, and the Hub counts its
// parameters unpacked; the bit width is read from its name (`-4bit`,
// `-8bit`...), 4 when it says nothing. Read `config.json`'s
// `quantization.bits` if a name ever lies.
fn mlx_bytes(id: &str, parameters: u64) -> u64 {
    let id = id.to_ascii_lowercase();
    let bits = (2..=16)
        .rev()
        .find(|b| id.contains(&format!("{b}bit")) || id.contains(&format!("{b}-bit")))
        .unwrap_or(4);
    parameters * bits / 8 * INT4_OVERHEAD_PERCENT / 100
}

/// The `--backend openvino` filter: `None` when it is off.
struct OpenVino<'a> {
    architectures: &'a BTreeSet<String>,
    authenticated: bool,
}

/// The Hub's answer, reduced to the candidates that pass every filter, in
/// the Hub's (downloads) order.
fn survivors(
    hub: &serde_json::Value,
    query: &Query,
    ceiling: u64,
    llmfit: Option<&HashMap<String, Fit>>,
    openvino: Option<&OpenVino<'_>>,
) -> Vec<Survivor> {
    hub.as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?;
            if m.get("pipeline_tag").and_then(serde_json::Value::as_str)
                != Some(query.task.as_str())
            {
                return None;
            }

            let gguf = m.get("gguf").filter(|g| g.is_object());
            let model_type = m
                .pointer("/config/model_type")
                .or_else(|| gguf.and_then(|g| g.get("architecture")))
                .and_then(serde_json::Value::as_str);
            let parameters = m
                .pointer("/safetensors/total")
                .or_else(|| gguf.and_then(|g| g.get("total")))
                .and_then(serde_json::Value::as_u64);
            if parameters.is_some_and(|p| p < MIN_PARAMETERS) {
                return None;
            }
            if query.engine == Some(Engine::LlamaCpp) && gguf.is_none() {
                return None;
            }
            if let Some(ov) = openvino {
                // An already-quantized repository (AWQ, GPTQ, FP8...) is not
                // what `optimum-cli export --weight-format int4` starts from.
                let quantized = m.pointer("/config/quantization_config").is_some();
                let gated = m
                    .get("gated")
                    .is_some_and(|g| g != &serde_json::Value::Bool(false));
                if !model_type.is_some_and(|t| ov.architectures.contains(t))
                    || quantized
                    || (gated && !ov.authenticated)
                {
                    return None;
                }
            }
            let fit = llmfit
                .and_then(|index| index.get(&id.to_lowercase()))
                .cloned();
            let estimate = parameters.map(|p| match query.engine {
                Some(Engine::Mlx) => mlx_bytes(id, p),
                _ => int4_bytes(p),
            });
            let runs = match &fit {
                Some(f) => {
                    (f.level == "Perfect" || f.level == "Good") && f.score >= query.min_score
                }
                // A repository without safetensors (GGUF...) llmfit does not
                // know cannot be sized: it cannot be said to run.
                None => estimate.is_some_and(|e| e <= ceiling),
            };
            runs.then(|| Survivor {
                id: id.to_string(),
                model_type: model_type.unwrap_or("-").to_string(),
                parameters,
                estimate,
                license: m
                    .pointer("/cardData/license")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("-")
                    .to_string(),
                downloads: m
                    .get("downloads")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                fit,
            })
        })
        .collect()
}

/// `llmfit fit --json`, indexed by lowercased Hugging Face id — the model's
/// own, and each GGUF repository llmfit lists in its `gguf_sources`, so a
/// third-party GGUF of a model llmfit knows gets that model's verdict. A
/// model's own entry wins over a GGUF alias of another one.
fn llmfit_index(json: &str) -> HashMap<String, Fit> {
    let parsed: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
    let text = |m: &serde_json::Value, key: &str| {
        m.get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("-")
            .to_string()
    };
    let mut own = HashMap::new();
    let mut aliases = HashMap::new();
    for m in parsed
        .get("models")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(name), Some(score)) = (
            m.get("name").and_then(serde_json::Value::as_str),
            m.get("score").and_then(serde_json::Value::as_f64),
        ) else {
            continue;
        };
        let fit = Fit {
            score,
            level: text(m, "fit_level"),
            memory_gb: m
                .get("memory_required_gb")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
            run_mode: text(m, "run_mode"),
            use_case: text(m, "use_case"),
        };
        for repo in m
            .get("gguf_sources")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|g| g.get("repo").and_then(serde_json::Value::as_str))
        {
            aliases
                .entry(repo.to_lowercase())
                .or_insert_with(|| fit.clone());
        }
        own.insert(name.to_lowercase(), fit);
    }
    aliases.extend(own);
    aliases
}

/// `8_190_735_360` -> `"8.2B"`.
fn billions(parameters: u64) -> String {
    let tenths = (parameters + 50_000_000) / 100_000_000;
    format!("{}.{}B", tenths / 10, tenths % 10)
}

fn format_report(found: &[Survivor], with_llmfit: bool) -> String {
    let width = found
        .iter()
        .map(|s| s.id.chars().count())
        .max()
        .unwrap_or(0)
        .max(5);
    let mut report = format!(
        "{:width$} {:14} {:>7} {:>7} {:14} {:>11}",
        "model", "type", "params", "mem GB", "license", "downloads"
    );
    if with_llmfit {
        let _ = write!(report, " {:>6} {:8} {:4}  use case", "score", "fit", "on");
    }
    for s in found {
        // llmfit's memory when it knows the model, the INT4 estimate otherwise.
        let memory = match (&s.fit, s.estimate) {
            (Some(f), _) => format!("{:.1}", f.memory_gb),
            (None, Some(b)) => {
                format!("~{}.{}", b / 1_000_000_000, b / 100_000_000 % 10)
            }
            (None, None) => "-".to_string(),
        };
        let _ = write!(
            report,
            "\n{:width$} {:14} {:>7} {:>7} {:14} {:>11}",
            s.id,
            s.model_type,
            s.parameters.map_or_else(|| "-".to_string(), billions),
            memory,
            s.license,
            s.downloads
        );
        if with_llmfit {
            match &s.fit {
                Some(f) => {
                    let _ = write!(
                        report,
                        " {:>6.1} {:8} {:4}  {}",
                        f.score, f.level, f.run_mode, f.use_case
                    );
                }
                None => report.push_str("      - -        -     -"),
            }
        }
    }
    report
}

/// Runs the search and returns the report — this command's result.
///
/// # Errors
///
/// `Error::Backend` when `--npu` is asked on a host without an Intel NPU,
/// when Hugging Face or `optimum-intel`'s registry cannot be reached, or when
/// that registry no longer yields any architecture — each naming what failed.
pub fn discover(
    query: &Query,
    world: &World<'_>,
    logger: crate::log::Logger,
) -> crate::Result<String> {
    if query.npu && !world.has_npu {
        return Err(crate::Error::Backend(
            "--npu: no Intel NPU on this host (no /dev/accel/accel* device)".to_string(),
        ));
    }
    let architectures = if query.engine == Some(Engine::OpenVino) || query.npu {
        let found = exportable_architectures(&(world.fetch)(ARCHITECTURES_URL, None)?, &query.task);
        if found.is_empty() {
            return Err(crate::Error::Backend(format!(
                "no architecture exportable for task \"{}\" found in {ARCHITECTURES_URL}",
                query.task
            )));
        }
        logger.info(&format!(
            "{} architectures exportable to OpenVINO for \"{}\"",
            found.len(),
            query.task
        ));
        Some(found)
    } else {
        None
    };
    let openvino = architectures.as_ref().map(|architectures| OpenVino {
        architectures,
        authenticated: query.hf_token.is_some(),
    });

    let body = (world.fetch)(&search_url(query), query.hf_token.as_deref())?;
    let hub: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| crate::Error::Backend(format!("unexpected answer from {HUB_API}: {e}")))?;

    let llmfit = (world.llmfit)().map(|json| llmfit_index(&json));
    if llmfit.is_none() {
        logger.info("llmfit is not on PATH: fit judged from an INT4 estimate, no score");
    }
    let ceiling = world.total_ram / 100 * query.max_memory_percent;
    let mut found = survivors(&hub, query, ceiling, llmfit.as_ref(), openvino.as_ref());
    found.truncate(query.limit);
    if found.is_empty() {
        logger.warn("no candidate passed the filters: try broader words or more --candidates");
    }
    Ok(format_report(&found, llmfit.is_some()))
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

    fn query() -> Query {
        Query {
            text: None,
            task: "text-generation".into(),
            limit: 10,
            candidates: 10,
            max_memory_percent: 50,
            min_score: 60.0,
            engine: None,
            npu: false,
            hf_token: None,
        }
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
            {"id": "x/gguf", "gated": false, "pipeline_tag": "text-generation"},
            {"id": "x/awq", "gated": false, "pipeline_tag": "text-generation",
             "config": {"model_type": "qwen3", "quantization_config": {}},
             "safetensors": {"total": 1_000_000_000}},
            {"id": "x/mamba", "gated": false, "pipeline_tag": "text-generation",
             "config": {"model_type": "mamba"}, "safetensors": {"total": 1_000_000_000}},
            {"id": "x/embedding", "gated": false, "pipeline_tag": "feature-extraction",
             "config": {"model_type": "qwen3"}, "safetensors": {"total": 1_000_000_000}},
            {"id": "x/toy", "gated": false, "pipeline_tag": "text-generation",
             "config": {"model_type": "qwen3"}, "safetensors": {"total": 5_000_000}}
        ])
    }

    fn ids(found: &[Survivor]) -> Vec<&str> {
        found.iter().map(|s| s.id.as_str()).collect()
    }

    #[test]
    fn by_default_only_the_fit_to_the_host_filters() {
        let found = survivors(&hub(), &query(), 32_000_000_000, None, None);
        assert_eq!(
            ids(&found),
            ["Qwen/Qwen3-8B", "meta/gated", "x/awq", "x/mamba"]
        );
    }

    #[test]
    fn llmfit_decides_the_fit_of_the_models_it_knows() {
        let fit = |level: &str, score: f64| Fit {
            score,
            level: level.into(),
            memory_gb: 5.0,
            run_mode: "GPU".into(),
            use_case: "chat".into(),
        };
        let index: HashMap<String, Fit> = [
            ("x/gguf".to_string(), fit("Perfect", 80.0)),
            ("qwen/qwen3-8b".to_string(), fit("Perfect", 40.0)),
            ("x/mamba".to_string(), fit("Too Tight", 90.0)),
        ]
        .into();
        let found = survivors(&hub(), &query(), 32_000_000_000, Some(&index), None);
        assert_eq!(ids(&found), ["meta/gated", "x/gguf", "x/awq"]);
    }

    #[test]
    fn the_openvino_filter_keeps_exportable_original_open_weights() {
        let archs: BTreeSet<String> = ["qwen3", "llama"].map(String::from).into();
        let ov = OpenVino {
            architectures: &archs,
            authenticated: false,
        };
        let found = survivors(&hub(), &query(), 32_000_000_000, None, Some(&ov));
        assert_eq!(ids(&found), ["Qwen/Qwen3-8B"]);
        let ov = OpenVino {
            architectures: &archs,
            authenticated: true,
        };
        let found = survivors(&hub(), &query(), 32_000_000_000, None, Some(&ov));
        assert_eq!(ids(&found), ["Qwen/Qwen3-8B", "meta/gated"]);
    }

    #[test]
    fn the_llamacpp_engine_keeps_gguf_repositories_sized_from_their_header() {
        let hub = serde_json::json!([
            {"id": "x/model-GGUF", "pipeline_tag": "text-generation", "gguf": {"total": 8_000_000_000_u64}},
            {"id": "x/safetensors", "pipeline_tag": "text-generation",
             "config": {"model_type": "qwen3"}, "safetensors": {"total": 1_000_000_000}}
        ]);
        let query = Query {
            engine: Some(Engine::LlamaCpp),
            ..query()
        };
        let found = survivors(&hub, &query, 32_000_000_000, None, None);
        assert_eq!(ids(&found), ["x/model-GGUF"]);
        assert_eq!(found[0].estimate, Some(int4_bytes(8_000_000_000)));
        assert!(search_url(&query).contains("&filter=gguf"));
    }

    #[test]
    fn an_mlx_repository_is_sized_from_the_bit_width_in_its_name() {
        assert_eq!(mlx_bytes("mlx-community/Qwen3-8B-8bit", 1_000), 1_200);
        assert_eq!(mlx_bytes("mlx-community/Qwen3-8B-4bit", 1_000), 600);
        assert_eq!(mlx_bytes("mlx-community/Qwen3-8B", 1_000), 600);
    }

    #[test]
    fn engine_names_parse_case_insensitively() {
        assert_eq!(Engine::parse("OpenVINO"), Some(Engine::OpenVino));
        assert_eq!(Engine::parse("llama.cpp"), Some(Engine::LlamaCpp));
        assert_eq!(Engine::parse("vllm"), None);
    }

    #[test]
    fn the_search_url_encodes_the_query() {
        let query = Query {
            text: Some("qwen coder 7b".into()),
            candidates: 50,
            ..query()
        };
        let url = search_url(&query);
        assert!(url.contains("search=qwen%20coder%207b") && url.contains("limit=50"));
    }

    #[test]
    fn llmfit_scores_are_indexed_by_lowercased_id() {
        let index = llmfit_index(
            r#"{"models":[{"name":"Qwen/Qwen3-8B","score":80.5,"fit_level":"Good","use_case":"chat"}]}"#,
        );
        let fit = index.get("qwen/qwen3-8b");
        assert_eq!(
            fit.map(|f| (f.score, f.level.as_str())),
            Some((80.5, "Good"))
        );
        assert!(llmfit_index("not json").is_empty());
    }

    #[test]
    fn a_gguf_source_gets_the_verdict_of_the_model_it_packages() {
        let index = llmfit_index(
            r#"{"models":[
                {"name":"Qwen/Qwen3-8B","score":70.0,"fit_level":"Perfect",
                 "gguf_sources":[{"provider":"unsloth","repo":"unsloth/Qwen3-8B-GGUF"},
                                 {"provider":"x","repo":"Qwen/Qwen3-4B"}]},
                {"name":"Qwen/Qwen3-4B","score":72.0,"fit_level":"Good"}]}"#,
        );
        assert_eq!(
            index.get("unsloth/qwen3-8b-gguf").map(|f| f.score),
            Some(70.0)
        );
        assert_eq!(index.get("qwen/qwen3-4b").map(|f| f.score), Some(72.0));
    }

    #[test]
    fn npu_on_a_host_without_one_is_a_backend_error_before_any_request() {
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
            npu: true,
            ..query()
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
