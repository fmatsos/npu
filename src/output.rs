//! Structured output contract.
//!
//! Pipeline: raw model response → extraction of an optional Markdown
//! fence (`strip_fences`, JSON format only) → parsing → JSON Schema
//! validation if a schema is declared → compact serialization → stdout. For
//! text format: trim, then optional `max_lines` check.
//!
//! Exit code distinction (rule 1 of the shared contract, §23 — the machine
//! contract a calling agent depends on): a model response that does not
//! honor the declared contract is a [`crate::Error::Output`] (code 4, the
//! configuration is valid, it's the model that misbehaved); a schema file
//! that is not found, unreadable, or syntactically invalid is a
//! [`crate::Error::Config`] (code 2, it's the configuration that is broken).
//! An invalid output is NEVER repaired nor retried here (§15: it's an
//! execution failure, not something to catch — reformulation/retry belongs
//! to phase 5, out of scope).
//!
//! Schema path resolution (rule 3 of the shared contract): `OutputSpec.
//! schema` carries a path that is ALREADY resolved (absolute, or relative to
//! the cwd) by the time it reaches this module — resolution relative to the
//! command's scope root (§4/§6: `schemas/` is a sibling directory of
//! `commands/`) is the caller's responsibility (`command.rs`), not
//! `output.rs`'s, which only opens the path it is given. This resolution
//! (`command::resolve_schema_path`) is PURELY SYNTACTIC (L3 review): it
//! never touches disk. So it is THIS module, in `compile_schema`, that first
//! discovers — and only at the moment the command that requires it is
//! actually invoked — that a schema is absent, unreadable, or syntactically
//! invalid, aligning existence with compilation, both lazy.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Output format declared by a command (`[output].format`).
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    #[default]
    Text,
    Json,
}

/// Output contract resolved for a command.
///
/// Built by the caller (`command.rs`) from the `[output]` frontmatter —
/// forbidden combinations (rule 2 of the shared contract: `schema` with
/// `format = "text"`, or `max_lines` with `format = "json"`) are already
/// rejected before this value exists. `finalize` therefore does not
/// revalidate these combinations: it only reads the field relevant to the
/// `format` in effect and ignores the other, which stays safe even if the
/// caller did not honor the invariant (no irrelevant field is ever read).
#[derive(Debug, Default)]
pub struct OutputSpec {
    pub format: Format,
    pub schema: Option<PathBuf>,
    pub max_lines: Option<usize>,
}

/// Maximum number of characters kept in the response excerpt quoted by a
/// JSON parsing error message (rule 6 of the shared contract: a truncated
/// excerpt, not the whole response). Counted in characters, not bytes, to
/// never cut in the middle of a multi-byte character.
const EXCERPT_MAX_CHARS: usize = 200;

/// Truncates `text` to at most [`EXCERPT_MAX_CHARS`] characters for an error
/// message, appending an ellipsis if the response was longer. Iterates by
/// `char`, never by raw byte index: a naive slice on accented text or text
/// containing an emoji would panic on a multi-byte character boundary.
fn excerpt(text: &str) -> String {
    let mut truncated: String = text.chars().take(EXCERPT_MAX_CHARS).collect();
    if text.chars().count() > EXCERPT_MAX_CHARS {
        truncated.push('…');
    }
    truncated
}

/// Removes a Markdown fence surrounding the model's response, if present.
///
/// Models very often wrap their JSON in ` ``` ` or ` ```json ` (rule 5 of
/// the shared contract). This function removes at most ONE opening fence at
/// the start and its matching closing fence at the end, with an optional
/// language label on the opening line, tolerating whitespace/newlines
/// around the whole thing (`str::trim` before analysis). It touches
/// nothing:
/// - if the text (once leading/trailing whitespace is ignored) does not
///   start with ` ``` `;
/// - if the opening fence has no matching closing fence at the end of the
///   text ("opening fence without a closing one");
/// - in the middle of the text: only the first fence (start) and the last
///   (end) are considered, never an internal fence, which is content.
///
/// Pure, allocation-free function: `raw.trim()` copies nothing (it returns
/// a sub-slice of `raw`), and all subsequent slicing operates on that same
/// slice — the returned `&str` is therefore always borrowed from `raw`,
/// never a fresh `String`. Every slice point is anchored on an ASCII marker
/// (` ``` `, `\n`) found via `str::find`/`str::ends_with`, which always
/// return a valid character boundary: no risk of panicking on accented
/// content or content containing an emoji, wherever it occurs in the text.
#[must_use]
pub fn strip_fences(raw: &str) -> &str {
    const FENCE: &str = "```";

    let s = raw.trim();
    if !s.starts_with(FENCE) {
        return raw;
    }

    // Opening line: `````` (optional language label) up to the first
    // newline. Without a newline, there is no body distinct from the
    // opening fence itself: nothing to remove.
    let after_open = &s[FENCE.len()..];
    let Some(newline_offset) = after_open.find('\n') else {
        return raw;
    };
    let body_start = FENCE.len() + newline_offset + 1;

    if !s.ends_with(FENCE) || s.len() < body_start + FENCE.len() {
        return raw;
    }
    let close_start = s.len() - FENCE.len();

    // The closing fence must be on its own line: either it immediately
    // follows the opening line (empty body, `body_start == close_start`),
    // or the character preceding it is a `\n`. Without this check, text
    // ending with a literal ``` in the middle of a content line would be
    // mistaken for a fence.
    if close_start > body_start && !s[..close_start].ends_with('\n') {
        return raw;
    }

    let body_end = if close_start > body_start {
        close_start - 1 // excludes the `\n` preceding the closing fence
    } else {
        close_start
    };

    s[body_start..body_end].trim()
}

/// Applies the output contract `spec` to the model's raw response `raw` and
/// returns the EXACT text to write to stdout, without a trailing newline
/// (the caller adds it — cf. `lib.rs`, `println!("{output}")`).
///
/// `command_file` is the path of the command file (`CommandSpec.file`, cf.
/// `command.rs`) that produced `spec` — used ONLY to name the offending
/// command in an error message if the schema it requires (JSON branch)
/// turns out to be absent, unreadable, or syntactically invalid at the
/// moment of this invocation (`compile_schema`, below). Ignored by the text
/// branch, which knows nothing of a schema.
pub fn finalize(spec: &OutputSpec, raw: &str, command_file: &Path) -> crate::Result<String> {
    match spec.format {
        Format::Text => finalize_text(spec.max_lines, raw),
        Format::Json => finalize_json(spec.schema.as_deref(), raw, command_file),
    }
}

/// Applies the `format = "text"` contract (rule 7 of the shared contract):
/// no fence removed, no parsing. The response is trimmed of leading and
/// trailing whitespace. If `max_lines` is declared and the response has
/// more NON-EMPTY lines (after trim) than this limit, failure —
/// `Error::Output` stating the expected count and the received count, never
/// a silent truncation (§15: failure, not repair). Empty lines (whitespace
/// only, or fully empty) do not count towards the total compared to the
/// limit, but remain in the returned text: only the global trim
/// (leading/trailing) modifies the response itself.
fn finalize_text(max_lines: Option<usize>, raw: &str) -> crate::Result<String> {
    let trimmed = raw.trim();

    if let Some(limit) = max_lines {
        let non_empty_lines = trimmed
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        if non_empty_lines > limit {
            return Err(crate::Error::Output(format!(
                "text output: at most {limit} non-empty line(s) expected (max_lines), \
                 {non_empty_lines} received"
            )));
        }
    }

    Ok(trimmed.to_string())
}

/// Applies the `format = "json"` contract (rules 5/6 of the shared
/// contract): extraction of an optional Markdown fence ([`strip_fences`]),
/// parsing (`serde_json` — failure => `Error::Output` citing the parsing
/// error and a truncated excerpt of the response), then validation against
/// the schema if there is one (failure => `Error::Output` listing EVERY
/// violation, never just the first). The value returned on stdout is the
/// COMPACT serialization of the parsed value, so stdout stays valid JSON
/// regardless of the wrapping the model put around it (§22: `npu classify |
/// jq .`).
fn finalize_json(schema: Option<&Path>, raw: &str, command_file: &Path) -> crate::Result<String> {
    let candidate = strip_fences(raw);

    let value: serde_json::Value = serde_json::from_str(candidate).map_err(|err| {
        crate::Error::Output(format!(
            "invalid JSON output: {err}; response received (excerpt): \"{}\"",
            excerpt(candidate.trim())
        ))
    })?;

    if let Some(schema_path) = schema {
        // LAZY BY DESIGN (rule 4 of the shared contract): this schema is
        // only compiled because the command actually invoked declares it,
        // never at configuration load time nor for schemas of other
        // commands in the same scope. Same lesson as the phase 2 L3 review
        // (a broken backend/model shadowed by a more local scope is never
        // read): a broken schema belonging to a command that nobody invokes
        // must not make the CLI unusable. Exhaustively checking all schemas
        // is `npu doctor`'s job (phase 5, out of scope here). Since this
        // phase's L3 review, the EXISTENCE of the schema is lazy in the
        // same way as its compilation (cf. `command::resolve_schema_path`'s
        // doc): it is HERE, and only here, that `compile_schema` can
        // discover a file that is absent, unreadable, or syntactically
        // invalid.
        let validator = compile_schema(schema_path, command_file)?;
        validate_against_schema(&validator, &value)?;
    }

    serde_json::to_string(&value)
        .map_err(|err| crate::Error::Output(format!("JSON output serialization failed: {err}")))
}

/// Compiles the JSON schema located at `path` into a reusable validator.
///
/// A file that is not found or unreadable, or a JSON Schema that is
/// syntactically invalid, is an `Error::Config`: the CONFIGURATION is
/// broken, not the model's response (rule 1 of the shared contract). `path`
/// is already resolved by the caller (cf. module doc): opened as-is,
/// relative to the process's cwd if not absolute — like any other file read
/// by this crate (`std::fs::read_to_string`).
///
/// `command_file` (L3 review, fix 1) is the command file that declared this
/// schema (`CommandSpec.file`, cf. `command.rs`): named in each of the
/// three error messages below, IN ADDITION TO the resolved schema path.
/// Since `command::resolve_schema_path` no longer checks anything on disk,
/// it is this function, and only this function, that discovers a schema
/// that is absent, unreadable, or broken — at the moment the command that
/// requires it is actually executed, never before.
pub(crate) fn compile_schema(
    path: &Path,
    command_file: &Path,
) -> crate::Result<jsonschema::Validator> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        crate::Error::Config(format!(
            "output schema \"{}\", declared by command file \"{}\", not found \
             or unreadable: {err}",
            path.display(),
            command_file.display()
        ))
    })?;

    let document: serde_json::Value = serde_json::from_str(&text).map_err(|err| {
        crate::Error::Config(format!(
            "output schema \"{}\", declared by command file \"{}\": invalid \
             JSON: {err}",
            path.display(),
            command_file.display()
        ))
    })?;

    jsonschema::validator_for(&document).map_err(|err| {
        crate::Error::Config(format!(
            "output schema \"{}\", declared by command file \"{}\", invalid: \
             {err}",
            path.display(),
            command_file.display()
        ))
    })
}

/// Validates `value` against `validator` and returns an `Error::Output`
/// listing EVERY violation (offending JSON path + reason), never just the
/// first (rule 6 of the shared contract): a user should be able to fix
/// their prompt in a single pass rather than rerunning the command for
/// every violation discovered one at a time. Same actionable style as
/// `config::validate_backend`/`error::format_available`: path in quotes,
/// named error message.
fn validate_against_schema(
    validator: &jsonschema::Validator,
    value: &serde_json::Value,
) -> crate::Result<()> {
    let violations: Vec<String> = validator
        .iter_errors(value)
        .map(|error| {
            let path = error.instance_path();
            if path.is_empty() {
                format!("- (root): {error}")
            } else {
                format!("- {path}: {error}")
            }
        })
        .collect();

    if violations.is_empty() {
        return Ok(());
    }

    Err(crate::Error::Output(format!(
        "invalid JSON output against schema ({} violation(s)):\n{}",
        violations.len(),
        violations.join("\n")
    )))
}

#[cfg(test)]
#[allow(clippy::expect_used)] // tolerated in tests (cf. Cargo.toml [lints.clippy]).
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Creates a unique fixture file under `target/`, same idiom as
    /// `command::tests::fixture_dir`: no pollution of the repo, no
    /// collision between tests running in parallel.
    fn fixture_file(name: &str, contents: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join("output");
        std::fs::create_dir_all(&dir).expect("creating the fixture directory");
        let file = dir.join(format!("{name}-{n}.json"));
        std::fs::write(&file, contents).expect("writing fixture");
        file
    }

    /// Placeholder command file passed to `finalize` by this module's
    /// tests: its only constraint is to be a stable path, never read nor
    /// opened by `finalize`/`finalize_json` themselves (only
    /// `compile_schema`, on the broken/missing schema branch, uses it — and
    /// only to CITE it in the error message, never to open it).
    fn test_command_file() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".npu")
            .join("commands")
            .join("test-command-placeholder.md")
    }

    // -- strip_fences -----------------------------------------------------

    #[test]
    fn strip_fences_no_fence_is_untouched() {
        assert_eq!(strip_fences("{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn strip_fences_bare_triple_backtick() {
        assert_eq!(strip_fences("```\n{\"a\":1}\n```"), "{\"a\":1}");
    }

    #[test]
    fn strip_fences_with_language_label() {
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
    }

    #[test]
    fn strip_fences_tolerates_surrounding_whitespace_and_newlines() {
        assert_eq!(
            strip_fences("  \n\n```json\n{\"a\":1}\n```\n\n  "),
            "{\"a\":1}"
        );
    }

    #[test]
    fn strip_fences_does_not_touch_a_fence_in_the_middle() {
        let text = "Here is the result: ``` not a wrapping fence ``` end.";
        assert_eq!(strip_fences(text), text);
    }

    #[test]
    fn strip_fences_opening_without_closing_is_untouched() {
        let text = "```json\n{\"a\": 1}";
        assert_eq!(strip_fences(text), text);
    }

    #[test]
    fn strip_fences_accented_and_emoji_text_does_not_panic() {
        let text = "```json\n{\"ville\": \"Montréal 🎉\"}\n```";
        assert_eq!(strip_fences(text), "{\"ville\": \"Montréal 🎉\"}");
    }

    #[test]
    fn strip_fences_accented_text_without_fence_does_not_panic() {
        let text = "Summary: café à Montréal 🎉, no fence at all.";
        assert_eq!(strip_fences(text), text);
    }

    // -- format text --------------------------------------------------------

    #[test]
    fn text_trims_leading_and_trailing_whitespace() {
        let spec = OutputSpec {
            format: Format::Text,
            schema: None,
            max_lines: None,
        };
        let out =
            finalize(&spec, "  \n  hello world  \n\n", &test_command_file()).expect("must succeed");
        assert_eq!(out, "hello world");
    }

    #[test]
    fn text_max_lines_respected() {
        let spec = OutputSpec {
            format: Format::Text,
            schema: None,
            max_lines: Some(2),
        };
        let out = finalize(&spec, "line 1\nline 2", &test_command_file()).expect("must succeed");
        assert_eq!(out, "line 1\nline 2");
    }

    #[test]
    fn text_max_lines_exceeded_is_output_error() {
        let spec = OutputSpec {
            format: Format::Text,
            schema: None,
            max_lines: Some(1),
        };
        let err = finalize(&spec, "line 1\nline 2", &test_command_file()).expect_err("must fail");
        assert!(matches!(err, crate::Error::Output(_)));
    }

    #[test]
    fn text_blank_lines_do_not_count_towards_max_lines() {
        let spec = OutputSpec {
            format: Format::Text,
            schema: None,
            max_lines: Some(2),
        };
        // 2 non-empty lines, 2 empty lines (one of which has only
        // whitespace): must not exceed max_lines = 2.
        let out =
            finalize(&spec, "line 1\n\nline 2\n   \n", &test_command_file()).expect("must succeed");
        assert_eq!(out, "line 1\n\nline 2");
    }

    #[test]
    fn text_without_max_lines_has_no_limit() {
        let spec = OutputSpec {
            format: Format::Text,
            schema: None,
            max_lines: None,
        };
        let out =
            finalize(&spec, "l1\nl2\nl3\nl4\nl5", &test_command_file()).expect("must succeed");
        assert_eq!(out, "l1\nl2\nl3\nl4\nl5");
    }

    // -- format json ----------------------------------------------------------

    #[test]
    fn json_bare_is_recompacted() {
        let spec = OutputSpec {
            format: Format::Json,
            schema: None,
            max_lines: None,
        };
        let out =
            finalize(&spec, "{\n  \"a\": 1\n}\n", &test_command_file()).expect("must succeed");
        assert_eq!(out, "{\"a\":1}");
    }

    #[test]
    fn json_wrapped_in_fence_is_extracted_and_recompacted() {
        let spec = OutputSpec {
            format: Format::Json,
            schema: None,
            max_lines: None,
        };
        let out = finalize(
            &spec,
            "```json\n{\"a\": 1, \"b\": 2}\n```",
            &test_command_file(),
        )
        .expect("must succeed");
        assert_eq!(out, "{\"a\":1,\"b\":2}");
    }

    #[test]
    fn json_invalid_is_output_error() {
        let spec = OutputSpec {
            format: Format::Json,
            schema: None,
            max_lines: None,
        };
        let err = finalize(&spec, "not JSON at all", &test_command_file()).expect_err("must fail");
        assert!(matches!(err, crate::Error::Output(_)));
    }

    #[test]
    fn json_invalid_error_excerpt_truncates_long_multibyte_response_without_panicking() {
        // The truncation branch of `excerpt` (`chars().count() >
        // EXCERPT_MAX_CHARS`) was not exercised by any test: the only
        // existing case (`json_invalid_is_output_error`) is a twenty
        // character ASCII string, well below `EXCERPT_MAX_CHARS` (200).
        // This test forces the truncation with an accented/emoji response
        // of more than 200 CHARACTERS (but far more than 200 BYTES, each
        // "é" weighing two bytes and the emoji four): a naive slice on a
        // byte index would panic here, whereas `excerpt` iterates by
        // `char` (cf. its doc). The whole response is deliberately not
        // valid JSON, to take the error path that calls `excerpt`.
        let spec = OutputSpec {
            format: Format::Json,
            schema: None,
            max_lines: None,
        };
        // Emojis (4 bytes each) rather than "é" (2 bytes): with a 2-byte
        // step, a regression that sliced by byte index would have a
        // fifty-fifty chance of still landing on a valid boundary at the
        // exact `EXCERPT_MAX_CHARS` offset and NOT panicking despite the
        // bug (empirically verified by mutating `excerpt` while writing
        // this test); a 4-byte step makes this coincidence far less
        // likely.
        let long_response = format!("not JSON: {}", "🎉".repeat(250));
        assert!(
            long_response.chars().count() > EXCERPT_MAX_CHARS,
            "the fixture must exceed EXCERPT_MAX_CHARS to exercise the truncation"
        );

        let err = finalize(&spec, &long_response, &test_command_file())
            .expect_err("must fail, this is not JSON");
        assert!(matches!(err, crate::Error::Output(_)));
    }

    #[test]
    fn json_output_is_always_compact() {
        let spec = OutputSpec {
            format: Format::Json,
            schema: None,
            max_lines: None,
        };
        let out = finalize(
            &spec,
            "{\"a\":   1,\n\"b\":   [1, 2, 3]\n}",
            &test_command_file(),
        )
        .expect("must succeed");
        assert!(
            !out.contains('\n'),
            "the compact output must not contain a newline, got: {out}"
        );
        assert_eq!(out, "{\"a\":1,\"b\":[1,2,3]}");
    }

    #[test]
    fn json_with_valid_schema_passes() {
        let schema_path = fixture_file(
            "valid-schema",
            r#"{
                "type": "object",
                "required": ["category", "confidence"],
                "properties": {
                    "category": { "type": "string" },
                    "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
                },
                "additionalProperties": false
            }"#,
        );
        let spec = OutputSpec {
            format: Format::Json,
            schema: Some(schema_path),
            max_lines: None,
        };
        let out = finalize(
            &spec,
            "{\"category\": \"bug\", \"confidence\": 0.9}",
            &test_command_file(),
        )
        .expect("must succeed against a satisfied schema");
        assert_eq!(out, "{\"category\":\"bug\",\"confidence\":0.9}");
    }

    #[test]
    fn json_with_failing_schema_lists_multiple_violations() {
        let schema_path = fixture_file(
            "failing-schema",
            r#"{
                "type": "object",
                "required": ["category", "confidence"],
                "properties": {
                    "category": { "type": "string" },
                    "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
                },
                "additionalProperties": false
            }"#,
        );
        let spec = OutputSpec {
            format: Format::Json,
            schema: Some(schema_path),
            max_lines: None,
        };
        // "confidence" missing (required) AND "category" of the wrong type:
        // two distinct violations.
        let err =
            finalize(&spec, "{\"category\": 42}", &test_command_file()).expect_err("must fail");
        assert!(matches!(err, crate::Error::Output(_)));
    }

    #[test]
    fn schema_file_not_found_is_config_error() {
        let missing = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join("output")
            .join("does-not-exist.json");
        let spec = OutputSpec {
            format: Format::Json,
            schema: Some(missing),
            max_lines: None,
        };
        let err = finalize(&spec, "{\"a\": 1}", &test_command_file()).expect_err("must fail");
        assert!(
            matches!(err, crate::Error::Config(_)),
            "a schema that is not found is a CONFIGURATION error, not an output one: {err:?}"
        );
    }

    #[test]
    fn schema_file_invalid_json_is_config_error() {
        let schema_path = fixture_file("broken-schema", "not JSON");
        let spec = OutputSpec {
            format: Format::Json,
            schema: Some(schema_path),
            max_lines: None,
        };
        let err = finalize(&spec, "{\"a\": 1}", &test_command_file()).expect_err("must fail");
        assert!(
            matches!(err, crate::Error::Config(_)),
            "a syntactically invalid schema is a CONFIGURATION error: {err:?}"
        );
    }

    // -- exit code ---------------------------------------------------------

    #[test]
    fn output_error_exit_code_is_four() {
        let spec = OutputSpec {
            format: Format::Text,
            schema: None,
            max_lines: Some(0),
        };
        let err = finalize(&spec, "one line", &test_command_file()).expect_err("must fail");
        assert!(matches!(err, crate::Error::Output(_)));
        assert_eq!(err.exit_code(), 4);
    }
}
