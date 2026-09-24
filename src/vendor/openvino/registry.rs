//! The registry of architectures `optimum-intel` exports to `OpenVINO`,
//! parsed from its `model_configs.py` at run time rather than frozen in
//! this binary.

use std::collections::{BTreeSet, HashMap};

// A release tag, not `main`: `pip install optimum-intel` may lag it by an
// architecture or two, which `npu-export`'s CPU check catches. Bump this
// constant to a newer tag when one ships.
pub const ARCHITECTURES_URL: &str = "https://raw.githubusercontent.com/huggingface/optimum-intel/v2.2.0/optimum/exporters/openvino/model_configs.py";

/// One name's resolved set, recursing into `*NAME` entries; a cycle
/// resolves to an empty set rather than looping.
fn resolve_one(
    name: &str,
    raw: &HashMap<String, Vec<String>>,
    resolved: &mut HashMap<String, BTreeSet<String>>,
    visiting: &mut BTreeSet<String>,
) -> BTreeSet<String> {
    if let Some(set) = resolved.get(name) {
        return set.clone();
    }
    if !visiting.insert(name.to_string()) {
        return BTreeSet::new();
    }
    let mut set = BTreeSet::new();
    for item in raw.get(name).into_iter().flatten() {
        if let Some(splat) = item.strip_prefix('*') {
            set.extend(resolve_one(splat, raw, resolved, visiting));
        } else if let Some(text) = item.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            set.insert(text.to_string());
        }
    }
    resolved.insert(name.to_string(), set.clone());
    set
}

/// Every top-level `NAME = [...]` list assignment in `source`, resolved to
/// the set of quoted strings it ultimately holds: a `*NAME` entry pulls in
/// another list's own resolved set (`COMMON_TEXT2TEXT_GENERATION_TASKS`
/// itself starts with `*COMMON_TEXT_GENERATION_TASKS`), followed as far as
/// the definitions go; a name that is never defined in `source` resolves to
/// an empty set rather than a panic or a guess.
fn resolve_task_lists(source: &str) -> HashMap<String, BTreeSet<String>> {
    let mut raw: HashMap<String, Vec<String>> = HashMap::new();
    let mut rest = source;
    while let Some(at) = rest.find(" = [") {
        let name_end = at;
        let name_start = rest[..name_end]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map_or(0, |i| i + 1);
        let name = &rest[name_start..name_end];
        let after_bracket = &rest[at + " = [".len()..];
        let mut depth = 1;
        let end = after_bracket
            .char_indices()
            .find(|&(_, c)| {
                match c {
                    '[' => depth += 1,
                    ']' => depth -= 1,
                    _ => {}
                }
                depth == 0
            })
            .map_or(after_bracket.len(), |(i, _)| i);
        if !name.is_empty() {
            let items = after_bracket[..end]
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            raw.insert(name.to_string(), items);
        }
        rest = &after_bracket[end..];
    }

    let mut resolved = HashMap::new();
    for name in raw.keys().cloned().collect::<Vec<_>>() {
        if !resolved.contains_key(&name) {
            let mut visiting = BTreeSet::new();
            resolve_one(&name, &raw, &mut resolved, &mut visiting);
        }
    }
    resolved
}

/// The `*NAME` splat identifiers referenced in `args`.
fn splat_names(args: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut rest = args;
    let mut consumed = 0;
    while let Some(at) = rest.find('*') {
        let start = consumed + at + 1;
        let end = args[start..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map_or(args.len(), |o| start + o);
        if end > start {
            names.push(&args[start..end]);
        }
        consumed = end.max(start);
        rest = &args[consumed..];
    }
    names
}

/// The architectures `optimum-intel` registers for `task`, parsed from its
/// `model_configs.py`: every `@register_in_tasks_manager("<type>", ...)`
/// whose arguments name `"<task>"` or `"<task>-with-past"`, literally or
/// through a `*NAME` splat of a list resolved from the same source (e.g.
/// `*COMMON_TEXT_GENERATION_TASKS`).
#[must_use]
pub fn exportable_architectures(source: &str, task: &str) -> BTreeSet<String> {
    const CALL: &str = "@register_in_tasks_manager(";
    let wanted = [task.to_string(), format!("{task}-with-past")];
    let lists = resolve_task_lists(source);
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
        let literal_match = wanted.iter().any(|w| args.contains(&format!("\"{w}\"")));
        let splat_match = splat_names(args).into_iter().any(|splat| {
            lists
                .get(splat)
                .is_some_and(|set| wanted.iter().any(|w| set.contains(w)))
        });
        if let Some(name) = name
            && (literal_match || splat_match)
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

    /// A verbatim excerpt of the pinned tag's `model_configs.py`
    /// (`optimum/exporters/openvino/model_configs.py` at
    /// `v2.2.0`, the constant list and one registration that names no task
    /// literally, only through the splat).
    const PINNED_TAG_EXCERPT: &str = r#"
COMMON_TEXT_GENERATION_TASKS = [
    "feature-extraction",
    "feature-extraction-with-past",
    "text-generation",
    "text-generation-with-past",
]

COMMON_TEXT2TEXT_GENERATION_TASKS = [
    *COMMON_TEXT_GENERATION_TASKS,
    "text2text-generation",
    "text2text-generation-with-past",
]

@register_in_tasks_manager("olmo2", *COMMON_TEXT_GENERATION_TASKS, library_name="transformers")
class Olmo2OpenVINOConfig(Base):
    pass
"#;

    #[test]
    fn a_registration_that_only_splats_a_task_list_is_still_recognised() {
        let found = exportable_architectures(PINNED_TAG_EXCERPT, "text-generation");
        assert_eq!(found.into_iter().collect::<Vec<_>>(), ["olmo2"]);
        let extraction = exportable_architectures(PINNED_TAG_EXCERPT, "feature-extraction");
        assert_eq!(extraction.into_iter().collect::<Vec<_>>(), ["olmo2"]);
        // Not registered for this task, splatted list or not.
        assert!(exportable_architectures(PINNED_TAG_EXCERPT, "text2text-generation").is_empty());
    }

    #[test]
    fn a_list_splatting_another_list_resolves_transitively() {
        let lists = resolve_task_lists(PINNED_TAG_EXCERPT);
        let text2text = lists.get("COMMON_TEXT2TEXT_GENERATION_TASKS");
        assert!(text2text.is_some_and(
            |set| set.contains("text-generation") && set.contains("text2text-generation")
        ));
    }

    #[test]
    fn an_unknown_splat_name_resolves_to_an_empty_set_rather_than_a_panic() {
        assert!(
            exportable_architectures(
                "@register_in_tasks_manager(\"x\", *NOT_DEFINED)",
                "text-generation"
            )
            .is_empty()
        );
    }
}
