//! The registry of architectures `optimum-intel` exports to `OpenVINO`,
//! parsed from its `model_configs.py` at run time rather than frozen in
//! this binary.

use std::collections::BTreeSet;

// ponytail: `main`, not the latest release — `pip install optimum-intel`
// may lag it by an architecture or two, which `npu-export`'s CPU check
// catches. Pin a release tag if that ever misleads someone.
pub const ARCHITECTURES_URL: &str = "https://raw.githubusercontent.com/huggingface/optimum-intel/main/optimum/exporters/openvino/model_configs.py";

/// The architectures `optimum-intel` registers for `task`, parsed from its
/// `model_configs.py`: every `@register_in_tasks_manager("<type>", ...)`
/// whose arguments name `"<task>"` or `"<task>-with-past"`.
#[must_use]
pub fn exportable_architectures(source: &str, task: &str) -> BTreeSet<String> {
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
}
