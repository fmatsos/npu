//! Prompt interpolation (phase 3, npu-cli-spec.md §11/§12).
//!
//! Recognized placeholders: `{{ input }}`, `{{ args.<name> }}`, `{{ env.NAME }}`.
//! A CLOSED placeholder (`{{ ... }}`) whose name matches none of these three
//! forms is a configuration error, never copied through as-is: a
//! misspelled `{{ args.langauge }}` must fail loudly rather than being
//! sent to the model as literal text (cf. L3 review of phases 1 and
//! 2: a key read then ignored is a defect). An `{{` never closed remains
//! copied through as-is, as in phase 1: an intention cannot be distinguished
//! from a typo (§12, §25 — no heuristic).
//!
//! The three public functions ([`placeholders`], [`validate`], [`render`])
//! share a single scanner (`scan`) rather than duplicating the
//! `find("{{")` / `find("}}")` loop three times.

use std::collections::{BTreeMap, BTreeSet};

/// What a `{{ ... }}` placeholder can refer to.
#[derive(Debug, PartialEq, Eq)]
pub enum Placeholder {
    Input,
    Arg(String),
    Env(String),
}

/// Recognized placeholder forms, for error messages.
const ACCEPTED_FORMS: &str = "\"input\", \"args.<name>\" or \"env.<NAME>\"";

/// A template fragment after a first scanning pass.
///
/// `Placeholder` carries the RAW (untrimmed) content between `{{` and `}}`;
/// its interpretation (recognized name or not) is delegated to `parse_placeholder`
/// so that `placeholders`/`validate` (which only need the name) and
/// `render` (which must also copy through the surrounding literal text)
/// share the same pass.
enum Token<'a> {
    Literal(&'a str),
    Placeholder(&'a str),
    /// `{{` with no matching `}}`: the rest of the template, to be copied
    /// through as-is with the `{{` put back in front (cf. module doc).
    Unclosed(&'a str),
}

/// Splits `template` into literal fragments and raw contents of
/// closed placeholders. Generalizes the `find("{{")` / `find("}}")` loop
/// used by the module's three public functions.
fn scan(template: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut rest = template;

    loop {
        let Some(pos) = rest.find("{{") else {
            tokens.push(Token::Literal(rest));
            break;
        };

        tokens.push(Token::Literal(&rest[..pos]));
        rest = &rest[pos + 2..];

        let Some(end_pos) = rest.find("}}") else {
            tokens.push(Token::Unclosed(rest));
            break;
        };

        tokens.push(Token::Placeholder(&rest[..end_pos]));
        rest = &rest[end_pos + 2..];
    }

    tokens
}

/// Characters accepted in an argument name (`args.<name>`) or environment
/// variable name (`env.<NAME>`), after trimming and removing the prefix: ASCII
/// alphanumeric, `_` or `-`.
///
/// These are exactly the characters that a bare TOML key (`[args.foo-bar]`,
/// the only form `serde`/`toml` accept without quotes) and a usual
/// environment variable name can both carry unambiguously.
/// Everything else (space, dot, brace, quote, ...) is either a
/// template separator, or the sign of a typo that we would rather
/// reject at load time than accept silently.
///
/// `pub(crate)`: `command::validate_arg_name` reuses EXACTLY this
/// rule for the `[args.<name>]` key itself, rather than duplicating a
/// divergent one. TOML allows a quoted table key
/// (`[args."café"]`, `[args."foo.bar"]`) with characters that this module
/// never accepts in an `{{ args.<name> }}` placeholder: without this
/// sharing, such an argument would load silently (name never referenced
/// in the prompt) or fail with a message pointing at the placeholder
/// rather than at the faulty declaration — exactly the defect targeted by
/// the L3 review architecture rule ("a key read then silently
/// ignored is a defect"), moved from the placeholder key to the
/// declared argument name.
pub(crate) fn is_valid_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Validates and returns the name after an `args.`/`env.` prefix: non-empty,
/// no spaces, accepted characters only. `{{ args. }}` (empty name) is thus
/// rejected here, not just as an unrecognized string.
fn parse_named(rest: &str) -> Option<String> {
    if rest.is_empty() || !rest.chars().all(is_valid_name_char) {
        return None;
    }
    Some(rest.to_string())
}

/// Builds the configuration error for a placeholder whose name (once
/// trimmed) matches no recognized form. `raw` is the raw,
/// untrimmed content as found between `{{` and `}}`.
fn unknown_placeholder(raw: &str) -> crate::Error {
    let name = raw.trim();
    crate::Error::Config(format!(
        "unknown placeholder \"{{{{ {name} }}}}\": recognized forms: {ACCEPTED_FORMS}"
    ))
}

/// Interprets the raw content of a closed placeholder (`raw`, between `{{`
/// and `}}`, delimiters excluded) as a `Placeholder`, or returns the error
/// describing the faulty placeholder and the accepted forms.
fn parse_placeholder(raw: &str) -> crate::Result<Placeholder> {
    let trimmed = raw.trim();

    if trimmed == "input" {
        return Ok(Placeholder::Input);
    }
    if let Some(rest) = trimmed.strip_prefix("args.") {
        return parse_named(rest)
            .map(Placeholder::Arg)
            .ok_or_else(|| unknown_placeholder(raw));
    }
    if let Some(rest) = trimmed.strip_prefix("env.") {
        return parse_named(rest)
            .map(Placeholder::Env)
            .ok_or_else(|| unknown_placeholder(raw));
    }

    Err(unknown_placeholder(raw))
}

/// Parses `template` and returns the placeholders encountered, in order.
///
/// Error if a closed placeholder (`{{ ... }}`) has an unrecognized name (neither
/// `input`, nor `args.<name>`, nor `env.<NAME>`). An `{{` never closed is
/// not a placeholder: it is ignored here, just as it is by `render`.
pub fn placeholders(template: &str) -> crate::Result<Vec<Placeholder>> {
    scan(template)
        .into_iter()
        .filter_map(|token| match token {
            Token::Placeholder(raw) => Some(parse_placeholder(raw)),
            Token::Literal(_) | Token::Unclosed(_) => None,
        })
        .collect()
}

/// Statically checks that a template only references `input`, a DECLARED
/// argument (present in `declared_args`), or `env.X` (any syntactically
/// valid name: the PRESENCE of an environment variable is only
/// checked at render time, never here). Called when the command is loaded.
///
/// A declared argument never referenced in the prompt is not an
/// error: it remains a valid, documented CLI argument (npu-cli-spec.md
/// §11), simply unused by this particular prompt.
pub fn validate(template: &str, declared_args: &BTreeSet<String>) -> crate::Result<()> {
    for placeholder in placeholders(template)? {
        if let Placeholder::Arg(name) = placeholder
            && !declared_args.contains(&name)
        {
            return Err(crate::Error::Config(format!(
                "unknown argument \"{name}\" referenced by {{{{ args.{name} }}}}: \
                 declared arguments: {}",
                crate::error::format_available(declared_args.iter())
            )));
        }
    }
    Ok(())
}

/// Resolves the value of an argument referenced by `{{ args.NAME }}`, or the
/// configuration error naming the argument. Factored out between [`render`]
/// and [`preflight`]: both must produce EXACTLY the same message for
/// the same defect (defense in depth, cf. [`preflight`] doc).
fn resolve_arg<'a>(name: &str, args: &'a BTreeMap<String, String>) -> crate::Result<&'a String> {
    args.get(name).ok_or_else(|| {
        crate::Error::Config(format!(
            "argument \"{name}\" referenced by {{{{ args.{name} }}}} but missing from the \
             values provided"
        ))
    })
}

/// Resolves the value of an environment variable referenced by
/// `{{ env.NAME }}`, or the configuration error naming the variable. See
/// [`resolve_arg`] for the reason it is shared with [`render`]/[`preflight`].
fn resolve_env(name: &str, env: &dyn Fn(&str) -> Option<String>) -> crate::Result<String> {
    env(name).ok_or_else(|| {
        crate::Error::Config(format!(
            "environment variable \"{name}\" referenced by {{{{ env.{name} }}}} but not \
             defined"
        ))
    })
}

/// Checks, BEFORE any reading of the input, that everything the prompt
/// references and that is knowable WITHOUT the input (a declared argument
/// `{{ args.NAME }}`, an environment variable `{{ env.NAME }}`) is indeed
/// available.
///
/// INVARIANT (L3 review, fix 1): nothing that is knowable without
/// the input must be checked after the input has been read. `input::resolve`
/// may drain a non-replayable stream (a pipe, a one-shot command,
/// npu-cli-spec.md §22): if a missing optional argument or an undefined
/// environment variable only fail at render time, AFTER this read,
/// the work already produced upstream of the pipe is lost, and on a
/// non-replayable stream it is lost for good. The caller (`lib.rs::run`) must
/// therefore call `preflight` before `input::resolve`, never after.
///
/// `{{ input }}` itself is NOT checked here: by construction, its
/// value can only be known after the input has been read — that is
/// precisely not something "knowable without the input".
///
/// That [`render`] then rechecks the same presence is not a
/// duplication to remove but defense in depth: `preflight`
/// only guarantees that no check OCCURS after the input has been
/// read, not that its result remains valid until render time (an
/// environment variable could in theory disappear between the two
/// calls, although no code in this process modifies it).
pub fn preflight(
    template: &str,
    args: &BTreeMap<String, String>,
    env: &dyn Fn(&str) -> Option<String>,
) -> crate::Result<()> {
    for placeholder in placeholders(template)? {
        match placeholder {
            Placeholder::Input => {}
            Placeholder::Arg(name) => {
                resolve_arg(&name, args)?;
            }
            Placeholder::Env(name) => {
                resolve_env(&name, env)?;
            }
        }
    }
    Ok(())
}

/// Renders the template: substitutes each recognized placeholder with its value.
///
/// `env` is injected by the caller (rather than calling `std::env::var` here)
/// to stay testable without mutating the real environment — `unsafe` in
/// edition 2024 is forbidden by `unsafe_code = "forbid"`.
///
/// - `{{ input }}` → `input`.
/// - `{{ args.NAME }}` → `args[NAME]`. Missing from the map: `Error::Config`
///   naming the argument (shouldn't happen if `validate` has run and if
///   the caller correctly wires the CLI arguments into this map, but we
///   don't panic regardless).
/// - `{{ env.NAME }}` → `env(NAME)`. `None` (variable not defined):
///   `Error::Config` naming the variable. `Some(String::new())` (variable
///   defined but empty): substituted with an empty string, not an error.
///
/// The substitution is never reapplied to its own result: the
/// template is fully scanned BEFORE any substitution (`scan`), so
/// an argument value literally containing `{{ input }}` is never
/// reinterpreted.
pub fn render(
    template: &str,
    input: &str,
    args: &BTreeMap<String, String>,
    env: &dyn Fn(&str) -> Option<String>,
) -> crate::Result<String> {
    let mut result = String::new();

    for token in scan(template) {
        match token {
            Token::Literal(text) => result.push_str(text),
            Token::Unclosed(text) => {
                result.push_str("{{");
                result.push_str(text);
            }
            Token::Placeholder(raw) => match parse_placeholder(raw)? {
                Placeholder::Input => result.push_str(input),
                Placeholder::Arg(name) => {
                    let value = resolve_arg(&name, args)?;
                    result.push_str(value);
                }
                Placeholder::Env(name) => {
                    let value = resolve_env(&name, env)?;
                    result.push_str(&value);
                }
            },
        }
    }

    Ok(result)
}

#[cfg(test)]
#[allow(clippy::expect_used)] // tolerated in tests (cf. Cargo.toml [lints.clippy]).
mod tests {
    use super::*;

    // -- placeholders() -----------------------------------------------------

    #[test]
    fn placeholders_recognizes_input() {
        assert_eq!(
            placeholders("{{ input }}").expect("should parse"),
            vec![Placeholder::Input]
        );
    }

    #[test]
    fn placeholders_recognizes_arg() {
        assert_eq!(
            placeholders("{{ args.language }}").expect("should parse"),
            vec![Placeholder::Arg("language".to_string())]
        );
    }

    #[test]
    fn placeholders_recognizes_env() {
        assert_eq!(
            placeholders("{{ env.API_KEY }}").expect("should parse"),
            vec![Placeholder::Env("API_KEY".to_string())]
        );
    }

    #[test]
    fn placeholders_unknown_name_is_config_error_naming_accepted_forms() {
        let err = placeholders("{{ foo }}").expect_err("unknown name must fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn placeholders_variable_spacing() {
        assert_eq!(
            placeholders("{{args.x}}").expect("should parse"),
            vec![Placeholder::Arg("x".to_string())]
        );
        assert_eq!(
            placeholders("{{  args.x  }}").expect("should parse"),
            vec![Placeholder::Arg("x".to_string())]
        );
    }

    #[test]
    fn placeholders_unclosed_brace_yields_no_placeholder_and_no_error() {
        assert_eq!(
            placeholders("prefix {{ input, no closing brace").expect("should parse"),
            vec![]
        );
    }

    #[test]
    fn placeholders_empty_name_after_dot_is_error() {
        let err = placeholders("{{ args. }}").expect_err("empty name must fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn placeholders_name_with_space_is_error() {
        let err = placeholders("{{ args.foo bar }}").expect_err("name with space must fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn placeholders_multiple_forms_in_order() {
        let result =
            placeholders("{{ input }} {{ args.a }} {{ env.B }} {{ input }}").expect("should parse");
        assert_eq!(
            result,
            vec![
                Placeholder::Input,
                Placeholder::Arg("a".to_string()),
                Placeholder::Env("B".to_string()),
                Placeholder::Input,
            ]
        );
    }

    // -- validate() -----------------------------------------------------------

    #[test]
    fn validate_rejects_undeclared_arg_naming_it_and_declared_args() {
        let declared: BTreeSet<String> = BTreeSet::new();
        let err = validate("{{ args.language }}", &declared).expect_err("undeclared arg must fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn validate_accepts_declared_arg() {
        let declared: BTreeSet<String> = ["language".to_string()].into_iter().collect();
        assert!(validate("{{ args.language }}", &declared).is_ok());
    }

    #[test]
    fn validate_declared_but_unreferenced_arg_is_not_an_error() {
        let declared: BTreeSet<String> = ["language".to_string(), "unused".to_string()]
            .into_iter()
            .collect();
        assert!(validate("{{ args.language }}", &declared).is_ok());
    }

    #[test]
    fn validate_does_not_check_env_presence() {
        let declared: BTreeSet<String> = BTreeSet::new();
        assert!(validate("{{ env.NOT_SET_ANYWHERE }}", &declared).is_ok());
    }

    // -- render() ---------------------------------------------------------------

    #[test]
    fn render_nominal_with_input_args_env() {
        let mut args = BTreeMap::new();
        args.insert("language".to_string(), "french".to_string());
        let env = |name: &str| (name == "USER").then(|| "alice".to_string());

        let rendered = render(
            "Translate {{ input }} into {{ args.language }} for {{ env.USER }}.",
            "hello",
            &args,
            &env,
        )
        .expect("render should succeed");

        assert_eq!(rendered, "Translate hello into french for alice.");
    }

    #[test]
    fn render_missing_env_var_is_config_error_naming_it() {
        let args = BTreeMap::new();
        let env = |_: &str| None;

        let err = render("{{ env.API_KEY }}", "x", &args, &env).expect_err("must fail");

        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn render_env_var_defined_but_empty_is_not_an_error() {
        let args = BTreeMap::new();
        let env = |name: &str| (name == "EMPTY").then(String::new);

        let rendered =
            render("[{{ env.EMPTY }}]", "x", &args, &env).expect("empty value is not an error");

        assert_eq!(rendered, "[]");
    }

    #[test]
    fn render_missing_arg_value_is_config_error_not_panic() {
        let args = BTreeMap::new();
        let env = |_: &str| None;

        let err = render("{{ args.language }}", "x", &args, &env).expect_err("must fail");

        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn render_arg_value_containing_placeholder_syntax_is_not_reinterpreted() {
        let mut args = BTreeMap::new();
        args.insert("note".to_string(), "{{ input }}".to_string());
        let env = |_: &str| None;

        let rendered =
            render("{{ args.note }}", "real-input", &args, &env).expect("should succeed");

        assert_eq!(rendered, "{{ input }}");
    }

    #[test]
    fn render_multiple_occurrences_of_same_placeholder() {
        let mut args = BTreeMap::new();
        args.insert("x".to_string(), "V".to_string());
        let env = |_: &str| None;

        let rendered =
            render("{{ args.x }}-{{ args.x }}", "in", &args, &env).expect("should succeed");

        assert_eq!(rendered, "V-V");
    }

    #[test]
    fn render_unclosed_brace_is_preserved_literally() {
        let args = BTreeMap::new();
        let env = |_: &str| None;

        assert_eq!(
            render("{{input", "test", &args, &env).expect("should succeed"),
            "{{input"
        );
        assert_eq!(
            render("{{ input }text", "test", &args, &env).expect("should succeed"),
            "{{ input }text"
        );
    }

    #[test]
    fn render_template_with_accented_and_multibyte_characters_does_not_panic() {
        // `scan` only slices `template` at positions returned by
        // `str::find("{{"/"}}")`, so always on a valid character
        // boundary (guaranteed by `str::find`, never a manual byte
        // count) — but the L3 review of phases 1/2 asks for an
        // explicit test rather than implicit reasoning. This template places
        // multi-byte characters (accents, emoji) immediately touching the
        // `{{`/`}}` delimiters, with no space, the case most likely to
        // hit a character boundary if the splitting were done by
        // byte count rather than via `find`.
        let mut args = BTreeMap::new();
        args.insert("language".to_string(), "français".to_string());
        let env = |_: &str| None;

        let rendered = render(
            "Préparé{{ input }} : {{ args.language }}🎉 café",
            "☕é",
            &args,
            &env,
        )
        .expect("an accented/emoji template must never panic");

        assert_eq!(rendered, "Préparé☕é : français🎉 café");
    }

    #[test]
    fn scan_unclosed_brace_after_multibyte_text_is_preserved_literally_without_panicking() {
        let args = BTreeMap::new();
        let env = |_: &str| None;

        let rendered = render("café 🎉 {{ input not closed", "x", &args, &env)
            .expect("an unclosed {{ after multi-byte text must not panic");

        assert_eq!(rendered, "café 🎉 {{ input not closed");
    }

    #[test]
    fn render_unknown_placeholder_name_is_config_error() {
        let args = BTreeMap::new();
        let env = |_: &str| None;

        let err = render("{{ foo }}", "x", &args, &env).expect_err("must fail");

        assert!(matches!(err, crate::Error::Config(_)));
    }

    // -- preflight() (L3 review, fix 1) -----------------------------------

    #[test]
    fn preflight_detects_missing_env_var_without_needing_input() {
        // This is the check explicitly requested by the L3 review:
        // at the level of the preflight function itself, without going through
        // the full process (whose proof is the timing measurement on the
        // real binary, cf. report). A missing environment variable
        // must be detected WITHOUT any input needing to exist.
        let args = BTreeMap::new();
        let env = |_: &str| None;

        let err = preflight("{{ env.NPU_ABSENT }}: {{ input }}", &args, &env)
            .expect_err("a missing environment variable must fail in preflight");

        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn preflight_detects_missing_arg_without_needing_input() {
        let args = BTreeMap::new();
        let env = |_: &str| None;

        let err = preflight("the {{ args.tone }}: {{ input }}", &args, &env)
            .expect_err("a missing argument must fail in preflight");

        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn preflight_does_not_require_input_placeholder_to_be_resolved() {
        // `{{ input }}` is only checkable AFTER the input has been read: by
        // construction, `preflight` must never fail because of it
        // alone.
        let args = BTreeMap::new();
        let env = |_: &str| None;

        assert!(preflight("{{ input }}", &args, &env).is_ok());
    }

    #[test]
    fn preflight_accepts_present_args_and_env_vars() {
        let mut args = BTreeMap::new();
        args.insert("tone".to_string(), "formal".to_string());
        let env = |name: &str| (name == "USER").then(|| "alice".to_string());

        assert!(
            preflight(
                "{{ env.USER }} wants a {{ args.tone }} tone for {{ input }}",
                &args,
                &env,
            )
            .is_ok()
        );
    }

    #[test]
    fn preflight_and_render_agree_on_the_same_missing_env_var_message() {
        // Defense in depth (cf. `preflight` doc): both
        // functions share `resolve_env`, hence the same message.
        let args = BTreeMap::new();
        let env = |_: &str| None;

        let preflight_err =
            preflight("{{ env.API_KEY }}", &args, &env).expect_err("preflight must fail");
        let render_err = render("{{ env.API_KEY }}", "x", &args, &env)
            .expect_err("render must fail the same way");

        assert_eq!(preflight_err.to_string(), render_err.to_string());
    }
}
