//! Regression cases for configured prompts.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileInput {
    file: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CaseInput {
    Inline(String),
    File(FileInput),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCase {
    #[serde(default)]
    args: BTreeMap<String, String>,
    input: CaseInput,
    expect: BTreeMap<String, toml::Value>,
}

#[derive(Debug)]
enum Comparison {
    Equal(serde_json::Value),
    Min(f64),
}

#[derive(Debug)]
struct Expectation {
    exit_code: i32,
    lines: Option<usize>,
    contains: Vec<String>,
    pointers: BTreeMap<String, Comparison>,
}

#[derive(Debug)]
struct Case<'a> {
    command: &'a crate::command::CommandSpec,
    name: String,
    file: PathBuf,
    args: BTreeMap<String, String>,
    input: String,
    expect: Expectation,
}

#[derive(Debug, Serialize)]
struct Row {
    command: String,
    case: String,
    result: &'static str,
    duration_ms: u128,
    runs: u16,
    distinct_outputs: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Options<'a> {
    pub selected: Option<&'a str>,
    pub model_override: Option<&'a str>,
    pub repeat: u16,
    pub dry_run: bool,
    pub json: bool,
}

fn invalid(file: &Path, message: impl Into<String>) -> crate::Error {
    crate::Error::Config(crate::error::ConfigError::in_file(
        file,
        None::<String>,
        message,
    ))
}

fn case_files(root: &Path, out: &mut Vec<PathBuf>) -> crate::Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root).map_err(|e| invalid(root, e.to_string()))? {
        let entry = entry.map_err(|e| invalid(root, e.to_string()))?;
        let path = entry.path();
        if path.is_dir() {
            case_files(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "toml") {
            out.push(path);
        }
    }
    Ok(())
}

fn parse_expect(
    file: &Path,
    raw: BTreeMap<String, toml::Value>,
    json: bool,
) -> crate::Result<Expectation> {
    let has_output_comparison = raw.keys().any(|key| key != "exit_code");
    let mut expect = Expectation {
        exit_code: 0,
        lines: None,
        contains: Vec::new(),
        pointers: BTreeMap::new(),
    };
    for (key, value) in raw {
        match key.as_str() {
            "exit_code" => {
                let code = value
                    .as_integer()
                    .ok_or_else(|| invalid(file, "expect.exit_code must be an integer"))?;
                if code != 0 && code != 4 {
                    return Err(invalid(file, "expect.exit_code accepts only 0 or 4"));
                }
                expect.exit_code = i32::try_from(code).map_err(|e| invalid(file, e.to_string()))?;
            }
            "lines" if !json => {
                let n = value
                    .as_integer()
                    .ok_or_else(|| invalid(file, "expect.lines must be an integer"))?;
                expect.lines = Some(usize::try_from(n).map_err(|e| invalid(file, e.to_string()))?);
            }
            "contains" if !json => {
                let values = value
                    .as_array()
                    .ok_or_else(|| invalid(file, "expect.contains must be an array"))?;
                expect.contains = values
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_string)
                            .ok_or_else(|| invalid(file, "expect.contains needs strings"))
                    })
                    .collect::<crate::Result<Vec<_>>>()?;
            }
            _ if json && key.starts_with('/') => {
                // Check pointer syntax before any request is made.
                if key.split('/').skip(1).any(|segment| {
                    let mut chars = segment.chars();
                    while let Some(c) = chars.next() {
                        if c == '~' && !matches!(chars.next(), Some('0' | '1')) {
                            return true;
                        }
                    }
                    false
                }) {
                    return Err(invalid(file, format!("invalid JSON pointer {key:?}")));
                }
                let comparison = if let Some(table) = value.as_table() {
                    if table.len() == 1 && table.contains_key("min") {
                        let n = table["min"]
                            .as_float()
                            .or_else(|| {
                                table["min"]
                                    .as_integer()
                                    .and_then(|n| n.to_string().parse::<f64>().ok())
                            })
                            .ok_or_else(|| invalid(file, format!("{key}: min must be numeric")))?;
                        if !n.is_finite() {
                            return Err(invalid(file, format!("{key}: min must be finite")));
                        }
                        Comparison::Min(n)
                    } else {
                        Comparison::Equal(
                            serde_json::to_value(&value)
                                .map_err(|e| invalid(file, e.to_string()))?,
                        )
                    }
                } else {
                    Comparison::Equal(
                        serde_json::to_value(&value).map_err(|e| invalid(file, e.to_string()))?,
                    )
                };
                expect.pointers.insert(key, comparison);
            }
            _ => {
                return Err(invalid(
                    file,
                    format!("expect.{key} is incompatible with this command's output format"),
                ));
            }
        }
    }
    if expect.exit_code == 4 && has_output_comparison {
        return Err(invalid(
            file,
            "output comparisons require expect.exit_code = 0",
        ));
    }
    Ok(expect)
}

fn load_cases<'a>(
    roots: &[PathBuf],
    specs: &'a [crate::command::CommandSpec],
    selected: Option<&str>,
) -> crate::Result<Vec<Case<'a>>> {
    let mut winners = BTreeMap::new();
    for root in roots {
        let tests = root.join("tests");
        let mut files = Vec::new();
        case_files(&tests, &mut files)?;
        for file in files {
            let relative = file
                .strip_prefix(&tests)
                .map_err(|e| invalid(&file, e.to_string()))?;
            let command = relative.parent().unwrap_or_else(|| Path::new(""));
            let command = command
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            if selected.is_none_or(|s| s == command) {
                let name = relative
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                winners.insert((command, name), file);
            }
        }
    }
    let mut cases = Vec::new();
    for ((command, name), file) in winners {
        let spec = crate::cli::find_command(specs, &command)
            .map_err(|_| invalid(&file, format!("unknown command {command:?}")))?;
        let source = std::fs::read_to_string(&file).map_err(|e| invalid(&file, e.to_string()))?;
        let raw: RawCase = toml::from_str(&source).map_err(|e| invalid(&file, e.to_string()))?;
        for key in raw.args.keys() {
            if !spec.args.contains_key(key) {
                return Err(invalid(&file, format!("undeclared argument {key:?}")));
            }
        }
        for (key, arg) in &spec.args {
            if arg.required && !raw.args.contains_key(key) {
                return Err(invalid(
                    &file,
                    format!("required argument {key:?} is missing"),
                ));
            }
        }
        let input = match raw.input {
            CaseInput::Inline(text) => text,
            CaseInput::File(item) => {
                let path = Path::new(&item.file);
                if path.components().count() != 1
                    || !matches!(
                        path.components().next(),
                        Some(std::path::Component::Normal(_))
                    )
                {
                    return Err(invalid(
                        &file,
                        format!("input file {:?} must be next to the case", item.file),
                    ));
                }
                let path = file.parent().unwrap_or_else(|| Path::new("")).join(path);
                crate::input::resolve(&crate::command::InputMode::File, Some(&path))
                    .map_err(|e| invalid(&file, format!("input {}: {e}", path.display())))?
            }
        };
        let expect = parse_expect(
            &file,
            raw.expect,
            spec.output.format == crate::output::Format::Json,
        )?;
        cases.push(Case {
            command: spec,
            name,
            file,
            args: raw.args,
            input,
            expect,
        });
    }
    if cases.is_empty() {
        return Err(crate::Error::config(format!(
            "no test cases found for {}",
            selected.unwrap_or("configured commands")
        )));
    }
    Ok(cases)
}

fn compare(output: &str, expect: &Expectation, json: bool) -> crate::Result<Option<String>> {
    if json {
        let value: serde_json::Value =
            serde_json::from_str(output).map_err(|e| crate::Error::output(e.to_string()))?;
        for (pointer, comparison) in &expect.pointers {
            let Some(actual) = value.pointer(pointer) else {
                return Ok(Some(format!("missing JSON pointer {pointer}")));
            };
            let matches = match comparison {
                Comparison::Equal(expected) => actual == expected,
                Comparison::Min(min) => actual.as_f64().is_some_and(|n| n >= *min),
            };
            if !matches {
                return Ok(Some(format!("JSON pointer {pointer} did not match")));
            }
        }
    } else {
        if let Some(lines) = expect.lines
            && output
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count()
                != lines
        {
            return Ok(Some(format!("expected {lines} non-empty lines")));
        }
        for needle in &expect.contains {
            if !output.contains(needle) {
                return Ok(Some(format!("missing substring {needle:?}")));
            }
        }
    }
    Ok(None)
}

fn format_rows(rows: &[Row], json: bool) -> crate::Result<String> {
    if json {
        return serde_json::to_string(rows).map_err(|e| crate::Error::output(e.to_string()));
    }
    let mut report = "COMMAND CASE RESULT DURATION\n".to_string();
    for row in rows {
        let _ = write!(
            report,
            "{} {} {} {}ms",
            row.command, row.case, row.result, row.duration_ms
        );
        if row.runs > 1 {
            let _ = write!(
                report,
                " {} distinct outputs / {} runs",
                row.distinct_outputs, row.runs
            );
        }
        if let Some(message) = &row.message {
            let _ = write!(report, " ({message})");
        }
        report.push('\n');
    }
    Ok(report.trim_end().to_string())
}

/// Returns the complete report and its exit code; an error returns no partial report.
pub(crate) fn run(
    roots: &[PathBuf],
    specs: &[crate::command::CommandSpec],
    config: &crate::config::Config,
    options: Options<'_>,
    env: &dyn Fn(&str) -> Option<String>,
    logger: crate::log::Logger,
) -> crate::Result<(String, i32)> {
    let Options {
        selected,
        model_override,
        repeat,
        dry_run,
        json,
    } = options;
    let cases = load_cases(roots, specs, selected)?;
    if let Some(id) = model_override {
        config.resolve(id)?;
    }
    if dry_run {
        let mut rendered = Vec::new();
        for case in &cases {
            let model = model_override.unwrap_or(&case.command.model);
            let messages = crate::exec::test_messages(
                case.command,
                config,
                model,
                &case.args,
                &case.input,
                env,
                logger,
            )
            .map_err(|err| match err {
                crate::Error::Config(_) => invalid(&case.file, err.to_string()),
                other => other,
            })?;
            rendered.push(serde_json::json!({
                "command": case.command.path.join("/"), "case": case.name,
                "model": model,
                "messages": messages.iter().map(|m| serde_json::json!({"role": m.role, "content": m.content})).collect::<Vec<_>>()
            }));
        }
        let report = if json {
            serde_json::to_string(&rendered).map_err(|e| crate::Error::output(e.to_string()))?
        } else {
            rendered
                .iter()
                .map(|item| {
                    format!(
                        "{} {}\n{}",
                        item["command"].as_str().unwrap_or_default(),
                        item["case"].as_str().unwrap_or_default(),
                        item["messages"]
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        return Ok((report, 0));
    }
    let mut rows = Vec::new();
    let mut failed = false;
    for case in &cases {
        let start = Instant::now();
        let mut outputs = BTreeSet::new();
        let mut message = None;
        for _ in 0..repeat {
            let model = model_override.unwrap_or(&case.command.model);
            match crate::exec::execute_test_case(
                case.command,
                config,
                model,
                &case.args,
                &case.input,
                env,
                logger,
            ) {
                Ok(output) => {
                    outputs.insert(output.clone());
                    if case.expect.exit_code != 0 {
                        message.get_or_insert_with(|| "expected exit code 4, got 0".to_string());
                    } else if let Some(reason) = compare(
                        &output,
                        &case.expect,
                        case.command.output.format == crate::output::Format::Json,
                    )? {
                        message.get_or_insert(reason);
                    }
                }
                Err(crate::Error::Output(err)) => {
                    if case.expect.exit_code != 4 {
                        message.get_or_insert(err.message);
                    }
                }
                Err(err) => {
                    return Err(match err {
                        crate::Error::Config(_) => invalid(&case.file, err.to_string()),
                        other => other,
                    });
                }
            }
        }
        let result = if message.is_some() {
            failed = true;
            "fail"
        } else {
            "pass"
        };
        rows.push(Row {
            command: case.command.path.join("/"),
            case: case.name.clone(),
            result,
            duration_ms: start.elapsed().as_millis(),
            runs: repeat,
            distinct_outputs: outputs.len(),
            message,
        });
    }
    Ok((format_rows(&rows, json)?, if failed { 4 } else { 0 }))
}
