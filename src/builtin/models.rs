//! `npu config models`: a formatted table of every configured model.

/// One row of `npu config models --json`: the same fields `format_models`
/// tabulates, serialized instead of aligned into columns.
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
struct ModelRow<'a> {
    name: &'a str,
    backend: &'a str,
    operation: &'a str,
    fallback: Option<&'a str>,
}

/// `npu config models --json`: every configured model, sorted by name like
/// [`format_models`], serialized rather than tabulated.
#[must_use]
pub fn format_models_json(config: &crate::config::Config) -> String {
    let mut rows: Vec<ModelRow<'_>> = config
        .models
        .values()
        .map(|model| ModelRow {
            name: model.id.as_str(),
            backend: model.backend.as_str(),
            operation: model.operation.as_str(),
            fallback: model.fallback.as_deref(),
        })
        .collect();
    rows.sort_unstable_by_key(|row| row.name);
    serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string())
}

/// Formats the `npu models` table:
/// NAME/BACKEND/OPERATION columns, sorted by name to stay
/// deterministic regardless of the underlying `HashMap`'s iteration
/// order, aligned to the ACTUAL width of the content (never a hardcoded
/// width: a model name longer than "NAME" widens its column). A
/// configuration with no models still produces the header — never an
/// empty table nor a panic.
#[must_use]
pub fn format_models(config: &crate::config::Config) -> String {
    const NAME_HEADER: &str = "NAME";
    const BACKEND_HEADER: &str = "BACKEND";
    const OPERATION_HEADER: &str = "OPERATION";
    // A routing key invisible here would be as good as silently ignored:
    // `npu models` is how one sees where a command actually goes.
    const FALLBACK_HEADER: &str = "FALLBACK";

    let mut rows: Vec<(&str, &str, &str, &str)> = config
        .models
        .values()
        .map(|model| {
            (
                model.id.as_str(),
                model.backend.as_str(),
                model.operation.as_str(),
                model.fallback.as_deref().unwrap_or("-"),
            )
        })
        .collect();
    rows.sort_unstable_by_key(|&(name, ..)| name);

    let name_width = rows
        .iter()
        .map(|&(name, ..)| name.len())
        .max()
        .unwrap_or(0)
        .max(NAME_HEADER.len());
    let backend_width = rows
        .iter()
        .map(|&(_name, backend, ..)| backend.len())
        .max()
        .unwrap_or(0)
        .max(BACKEND_HEADER.len());
    let operation_width = rows
        .iter()
        .map(|&(_name, _backend, operation, _fallback)| operation.len())
        .max()
        .unwrap_or(0)
        .max(OPERATION_HEADER.len());

    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(format!(
        "{NAME_HEADER:<name_width$}  {BACKEND_HEADER:<backend_width$}  \
         {OPERATION_HEADER:<operation_width$}  {FALLBACK_HEADER}"
    ));
    for (name, backend, operation, fallback) in rows {
        lines.push(format!(
            "{name:<name_width$}  {backend:<backend_width$}  \
             {operation:<operation_width$}  {fallback}"
        ));
    }

    lines.join("\n")
}
