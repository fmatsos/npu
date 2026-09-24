//! Builds the `clap` command tree from the discovered `CommandSpec`s, and
//! the small helpers that walk `ArgMatches` back into business data
//! (`selected_path`, `find_command`, `collect_arg_values`).
//!
//! The built-ins (`backend …`, `config …`, `doctor`, `describe`, `update`)
//! live in [`builtins`], added on top of the tree built here.

pub(crate) mod builtins;

/// A node of the command tree built from the discovered `CommandSpec`s.
/// `spec` is set on a leaf command; an intermediate node (e.g. `git` before
/// `git review`) only has children.
struct CommandNode<'a> {
    spec: Option<&'a crate::command::CommandSpec>,
    children: std::collections::BTreeMap<String, CommandNode<'a>>,
}

impl CommandNode<'_> {
    fn new() -> Self {
        CommandNode {
            spec: None,
            children: std::collections::BTreeMap::new(),
        }
    }
}

/// Groups the `CommandSpec`s into a tree by path prefix: two commands
/// sharing a leading segment (`git review`, `git commit`) merge under the
/// same intermediate node `git`.
fn build_command_tree(specs: &[crate::command::CommandSpec]) -> CommandNode<'_> {
    let mut root = CommandNode::new();
    for spec in specs {
        let mut node = &mut root;
        for segment in &spec.path {
            node = node
                .children
                .entry(segment.clone())
                .or_insert_with(CommandNode::new);
        }
        node.spec = Some(spec);
    }
    root
}

/// Builds the `clap::Arg` corresponding to a declared argument
/// (`[args.*]`): `.long(name)`, `.short(letter)` if present,
/// `.required(required)`, `.help(description)` if non-empty, `.value_name(name
/// in UPPERCASE)` and the single-value action (`ArgAction::Set`, an
/// argument expects a single value — no multi-values, no boolean flag).
/// The argument's id is `name` itself: this is what
/// `collect_arg_values` reads back from the leaf command's `ArgMatches`.
fn build_declared_arg(name: &str, arg_spec: &crate::command::ArgSpec) -> clap::Arg {
    let mut arg = clap::Arg::new(name.to_string())
        .long(name.to_string())
        .required(arg_spec.required)
        .value_name(name.to_uppercase())
        .action(clap::ArgAction::Set);

    if let Some(short) = arg_spec.short {
        arg = arg.short(short);
    }
    if !arg_spec.description.is_empty() {
        arg = arg.help(arg_spec.description.clone());
    }

    arg
}

/// Recursively builds the `clap` subtree corresponding to `node`, named
/// `name`. A leaf command receives its description, an optional `FILE`
/// positional argument if it accepts a file as input, then a `clap::Arg`
/// per declared argument (`[args.*]` — cf. [`build_declared_arg`]).
/// Iterating over `spec.args` (a `BTreeMap`) is sorted by name, so the order
/// of arguments in `--help` is deterministic regardless of the order the
/// TOML frontmatter was written in. `command::parse` already reserves the
/// name `FILE` (cf. `RESERVED_ARG_NAMES`): no declared argument can
/// therefore collide with the positional argument added here. An
/// intermediate node (with no `CommandSpec` of its own) remains usable on
/// its own: it then shows its help instead of failing silently.
fn build_clap_node(name: &str, node: &CommandNode<'_>) -> clap::Command {
    let mut cmd = clap::Command::new(name.to_string());

    match node.spec {
        Some(spec) => {
            cmd = cmd.about(spec.description.clone());
            if matches!(
                spec.input,
                crate::command::InputMode::File | crate::command::InputMode::StdinOrFile
            ) {
                cmd = cmd.arg(clap::Arg::new("FILE").required(false));
            }
            for (arg_name, arg_spec) in &spec.args {
                cmd = cmd.arg(build_declared_arg(arg_name, arg_spec));
            }
            cmd = cmd.arg(
                clap::Arg::new("dry-run")
                    .long("dry-run")
                    .action(clap::ArgAction::SetTrue)
                    .help(
                        "Print the request that would be sent (url, headers, body) instead of \
                         sending it",
                    ),
            );
            cmd = cmd.arg(
                clap::Arg::new("model")
                    .long("model")
                    .value_name("ID")
                    .action(clap::ArgAction::Set)
                    .help("Use this model instead of the command's own, for this call only"),
            );
        }
        None => {
            cmd = cmd.arg_required_else_help(true);
        }
    }

    for (child_name, child_node) in &node.children {
        cmd = cmd.subcommand(build_clap_node(child_name, child_node));
    }

    cmd
}

/// The `--verbose <LEVEL>` argument, declared once on the root and marked
/// `global`, so it is accepted after any subcommand (`npu classify
/// --verbose info`) without being redeclared on each of them.
///
/// The accepted values come from `log::Level::NAMES`: the CLI cannot offer a
/// level the logger does not know, nor the reverse. `command.rs` reserves
/// the name `verbose` and the short letter `v` for the same reason it
/// reserves `help`/`h` — a command declaring them would collide with this
/// argument.
fn verbose_arg() -> clap::Arg {
    clap::Arg::new("verbose")
        .long("verbose")
        .short('v')
        .global(true)
        .value_name("LEVEL")
        .value_parser(crate::log::Level::NAMES)
        .default_value(crate::log::Level::DEFAULT)
        .help("Diagnostic verbosity on stderr; stdout always carries the result only")
}

/// Builds the complete `clap` tree (builder API) from the discovered
/// commands. Contains ONLY the business
/// commands: the built-ins (`backend …`, `config …`, `doctor`, `describe`, `update`, `--version`) are added
/// separately by [`builtins::add_builtins`], unconditionally — this function remains
/// usable with an empty `specs` (degraded mode, cf. `crate::run`).
pub(crate) fn build_cli(specs: &[crate::command::CommandSpec]) -> clap::Command {
    let tree = build_command_tree(specs);
    // `bin_name` pinned: clap would otherwise print `argv[0]` (`npu.exe` on
    // Windows) in usage lines, which `help` and the docs spell `npu`.
    let mut root = clap::Command::new("npu")
        .bin_name("npu")
        .styles(crate::style::clap_styles())
        .arg_required_else_help(true)
        .arg(verbose_arg());
    for (name, node) in &tree.children {
        root = root.subcommand(build_clap_node(name, node));
    }
    root
}

/// Splits the root help into `Commands:` (the configured commands) and
/// `Built-ins:`, through a `help_template`.
///
/// `clap` has no per-subcommand heading: the built-ins are HIDDEN from its
/// `{subcommands}` list — hidden, not removed, they parse exactly as
/// before — and rendered by hand in the template, in `clap`'s own palette
/// (`style.rs`), which `clap` strips when stdout is not a terminal. The `help`
/// subcommand `clap` would generate is disabled: `npu help` is a built-in
/// of ours, listed with the others (cf. [`builtins::help`]).
pub(crate) fn sectioned_help(cli: clap::Command, load_failed: bool) -> clap::Command {
    let names: Vec<String> = builtins::add_builtins(clap::Command::new("npu"))
        .get_subcommands()
        .map(|sub| sub.get_name().to_string())
        .collect();
    let configured = cli
        .get_subcommands()
        .filter(|sub| !names.iter().any(|name| name == sub.get_name()))
        .count();
    let width = names.iter().map(String::len).max().unwrap_or(0);

    let heading = |text: &str| crate::style::paint(crate::style::HEADER.underline(), text);
    let commands = if configured > 0 {
        "{subcommands}".to_string()
    } else if load_failed {
        "  none: the configuration failed to load; run \"npu doctor\"".to_string()
    } else {
        "  none configured yet".to_string()
    };
    let mut builtins = String::new();
    let mut cli = cli.disable_help_subcommand(true);
    for name in &names {
        let about = cli
            .find_subcommand(name)
            .and_then(clap::Command::get_about)
            .map(ToString::to_string)
            .unwrap_or_default();
        let padded = format!("{name:width$}");
        builtins = format!(
            "{builtins}\n  {}  {about}",
            crate::style::paint(crate::style::LITERAL, &padded)
        );
        cli = cli.mut_subcommand(name, |sub| sub.hide(true));
    }
    cli.help_template(format!(
        "{{about-with-newline}}{{usage-heading}} {{usage}}\n\n{}\n{commands}\n\n{}{builtins}\n\n\
         {}\n{{options}}{{after-help}}",
        heading("Commands:"),
        heading("Built-ins:"),
        heading("Options:"),
    ))
}

/// Reconstructs the path of the selected command by walking down the chain
/// of subcommands `clap` resolved, and returns the leaf command's
/// `ArgMatches` along with the path walked.
pub(crate) fn selected_path(matches: &clap::ArgMatches) -> (Vec<String>, &clap::ArgMatches) {
    let mut path = Vec::new();
    let mut current = matches;
    while let Some((name, sub_matches)) = current.subcommand() {
        path.push(name.to_string());
        current = sub_matches;
    }
    (path, current)
}

/// Finds, among `specs`, the command whose path joined by `/` equals `key`.
/// Returns an `Error::Config` listing the available commands if none match
/// (same style as `config::Config::resolve` and `backend::chat` for unknown
/// identifiers).
pub(crate) fn find_command<'a>(
    specs: &'a [crate::command::CommandSpec],
    key: &str,
) -> crate::Result<&'a crate::command::CommandSpec> {
    specs
        .iter()
        .find(|spec| spec.path.join("/") == key)
        .ok_or_else(|| {
            crate::Error::Config(crate::error::ConfigError::bare(
                Some(key),
                format!(
                    "unknown command: \"{key}\" (available commands: {})",
                    crate::error::format_available(specs.iter().map(|s| s.path.join("/")))
                ),
            ))
        })
}

/// Collects, for the leaf command `spec`, the values of its declared
/// arguments (`[args.*]`) from the corresponding `ArgMatches`, into the
/// `BTreeMap<String, String>` expected by `prompt::render`.
///
/// A declared argument absent from `leaf_matches` (not required and not
/// supplied on the command line) is simply omitted from the map: there is
/// no default value. If the
/// prompt still references this argument via `{{ args.NAME }}`,
/// `prompt::render` fails with an `Error::Config` naming the argument
/// rather than substituting an unrequested empty
/// string.
pub(crate) fn collect_arg_values(
    spec: &crate::command::CommandSpec,
    leaf_matches: &clap::ArgMatches,
) -> std::collections::BTreeMap<String, String> {
    spec.args
        .keys()
        .filter_map(|name| {
            leaf_matches
                .get_one::<String>(name)
                .map(|value| (name.clone(), value.clone()))
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::expect_used)] // allowed in tests (cf. Cargo.toml [lints.clippy]).
mod tests {
    use super::*;
    use crate::command::{CommandSpec, InputMode};
    use builtins::POST_UPDATE_CHECK;

    fn spec(path: &[&str], input: InputMode) -> CommandSpec {
        spec_with_args(path, input, std::collections::BTreeMap::new())
    }

    fn spec_with_args(
        path: &[&str],
        input: InputMode,
        args: std::collections::BTreeMap<String, crate::command::ArgSpec>,
    ) -> CommandSpec {
        CommandSpec {
            path: path.iter().map(ToString::to_string).collect(),
            description: format!("desc {}", path.join("/")),
            model: "qwen-fast".to_string(),
            input,
            prompt: "{{ input }}".to_string(),
            args,
            // `command::CommandSpec.output`: these tests bear on the
            // construction of the `clap` tree, never on the output
            // contract — `OutputSpec::default()` (text format, no schema,
            // no limit) is neutral for them.
            output: crate::output::OutputSpec::default(),
            schemas: std::collections::BTreeMap::new(),
            system: None,
            examples: Vec::new(),
            generation: None,
            // Neutral for the same reasons as `output` above: these tests
            // never bear on the output contract nor on naming the command
            // file in a schema error.
            file: std::path::PathBuf::new(),
        }
    }

    fn arg_spec(short: Option<char>, required: bool, description: &str) -> crate::command::ArgSpec {
        crate::command::ArgSpec {
            short,
            required,
            description: description.to_string(),
        }
    }

    #[test]
    fn post_update_check_parses_against_the_real_tree() {
        let cli = sectioned_help(builtins::add_builtins(build_cli(&[])), false);
        assert!(
            cli.try_get_matches_from(
                std::iter::once("npu").chain(POST_UPDATE_CHECK.iter().copied())
            )
            .is_ok()
        );
    }

    #[test]
    fn build_cli_merges_commands_sharing_a_prefix() {
        let specs = vec![
            spec(&["commit-message"], InputMode::Stdin),
            spec(&["git", "review"], InputMode::StdinOrFile),
            spec(&["git", "commit"], InputMode::File),
        ];

        let cli = build_cli(&specs);

        let top_names: Vec<&str> = cli.get_subcommands().map(clap::Command::get_name).collect();
        assert!(top_names.contains(&"commit-message"));
        assert!(top_names.contains(&"git"));

        let git = cli
            .find_subcommand("git")
            .expect("git must exist as a merged subcommand");
        let git_children: Vec<&str> = git.get_subcommands().map(clap::Command::get_name).collect();
        assert!(git_children.contains(&"review"));
        assert!(git_children.contains(&"commit"));
    }

    #[test]
    fn leaf_command_gets_file_arg_only_when_input_accepts_a_file() {
        let specs = vec![
            spec(&["commit-message"], InputMode::Stdin),
            spec(&["classify"], InputMode::StdinOrFile),
            spec(&["summarize"], InputMode::File),
        ];

        let cli = build_cli(&specs);

        let stdin_only = cli
            .find_subcommand("commit-message")
            .expect("commit-message must exist");
        assert!(
            stdin_only
                .get_arguments()
                .all(|arg| matches!(arg.get_id().as_str(), "dry-run" | "model" | "help")),
            "a business leaf carries only its own args, plus dry-run and the automatic help flag"
        );

        let classify = cli
            .find_subcommand("classify")
            .expect("classify must exist");
        assert!(classify.get_arguments().any(|arg| arg.get_id() == "FILE"));

        let summarize = cli
            .find_subcommand("summarize")
            .expect("summarize must exist");
        assert!(summarize.get_arguments().any(|arg| arg.get_id() == "FILE"));
    }

    #[test]
    fn selected_path_descends_nested_subcommands() {
        let specs = vec![spec(&["git", "review"], InputMode::Stdin)];
        let cli = build_cli(&specs);
        let matches = cli
            .try_get_matches_from(["npu", "git", "review"])
            .expect("the command line must be accepted");

        let (path, leaf) = selected_path(&matches);

        assert_eq!(path, vec!["git".to_string(), "review".to_string()]);
        // The returned `ArgMatches` are indeed the leaf's, not the root's:
        // no subcommand remains to be descended into from `leaf`.
        assert!(leaf.subcommand().is_none());
    }

    #[test]
    fn selected_path_is_empty_when_no_subcommand_is_selected() {
        let specs = vec![spec(&["commit-message"], InputMode::Stdin)];
        let cli = build_cli(&specs);
        // `arg_required_else_help` normally prevents reaching this point
        // without a subcommand in real usage, but `selected_path` must
        // stay correct if it is called anyway on empty root `ArgMatches`.
        let matches = clap::Command::new("npu")
            .try_get_matches_from(["npu"])
            .expect("no subcommand required on this test instance");

        let (path, _leaf) = selected_path(&matches);

        assert!(path.is_empty());
        let _ = cli; // avoids a warning if `cli` is no longer used beyond this point
    }

    #[test]
    fn find_command_returns_matching_spec() {
        let specs = vec![
            spec(&["commit-message"], InputMode::Stdin),
            spec(&["git", "review"], InputMode::Stdin),
        ];
        let found = find_command(&specs, "git/review").expect("git/review must be found");
        assert_eq!(found.path, vec!["git".to_string(), "review".to_string()]);
    }

    #[test]
    fn find_command_unknown_key_lists_available_commands() {
        let specs = vec![
            spec(&["commit-message"], InputMode::Stdin),
            spec(&["git", "review"], InputMode::Stdin),
        ];
        let err =
            find_command(&specs, "does-not-exist").expect_err("the command must not be found");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("does-not-exist"));
        assert!(message.contains("commit-message"));
        assert!(message.contains("git/review"));
    }

    #[test]
    fn intermediate_node_without_its_own_spec_is_invocable_alone() {
        // An intermediate node (here `git`, which has no CommandSpec of its
        // own) must remain usable alone: `arg_required_else_help` rather
        // than a silent failure if `npu git` is invoked without a
        // subcommand.
        let specs = vec![spec(&["git", "review"], InputMode::Stdin)];
        let cli = build_cli(&specs);
        let git = cli.find_subcommand("git").expect("git must exist");
        assert!(git.is_arg_required_else_help_set());
    }

    // -- declared CLI arguments --------------------------------------------

    #[test]
    fn declared_arg_becomes_clap_arg_with_short_required_help_and_value_name() {
        let mut args = std::collections::BTreeMap::new();
        args.insert(
            "language".to_string(),
            arg_spec(Some('l'), true, "Target language"),
        );
        let specs = vec![spec_with_args(&["translate"], InputMode::StdinOrFile, args)];

        let cli = build_cli(&specs);
        let translate = cli
            .find_subcommand("translate")
            .expect("translate must exist");
        let language = translate
            .get_arguments()
            .find(|arg| arg.get_id() == "language")
            .expect("the 'language' argument must be present");

        assert_eq!(language.get_short(), Some('l'));
        assert!(language.is_required_set());
        assert_eq!(
            language.get_help().map(ToString::to_string).as_deref(),
            Some("Target language")
        );
        assert_eq!(
            language.get_value_names().map(|names| names[0].as_str()),
            Some("LANGUAGE")
        );
    }

    #[test]
    fn declared_arg_without_short_or_description_has_neither() {
        let mut args = std::collections::BTreeMap::new();
        args.insert("unused".to_string(), arg_spec(None, false, ""));
        let specs = vec![spec_with_args(&["x"], InputMode::Stdin, args)];

        let cli = build_cli(&specs);
        let x = cli.find_subcommand("x").expect("x must exist");
        let unused = x
            .get_arguments()
            .find(|arg| arg.get_id() == "unused")
            .expect("the 'unused' argument must be present");

        assert_eq!(unused.get_short(), None);
        assert!(!unused.is_required_set());
        assert!(unused.get_help().is_none());
    }

    #[test]
    fn declared_args_appear_in_deterministic_btreemap_order() {
        let mut args = std::collections::BTreeMap::new();
        args.insert("zebra".to_string(), arg_spec(None, false, ""));
        args.insert("alpha".to_string(), arg_spec(None, false, ""));
        let specs = vec![spec_with_args(&["x"], InputMode::Stdin, args)];

        let cli = build_cli(&specs);
        let x = cli.find_subcommand("x").expect("x must exist");
        let names: Vec<&str> = x
            .get_arguments()
            .map(|arg| arg.get_id().as_str())
            .filter(|id| !matches!(*id, "help" | "dry-run" | "model"))
            .collect();

        assert_eq!(names, vec!["alpha", "zebra"]);
    }

    #[test]
    fn file_positional_and_declared_args_coexist_without_id_collision() {
        let mut args = std::collections::BTreeMap::new();
        args.insert("language".to_string(), arg_spec(Some('l'), true, ""));
        let specs = vec![spec_with_args(&["translate"], InputMode::StdinOrFile, args)];

        // `build_cli` must not panic (duplicate id): the construction
        // itself is the assertion.
        let cli = build_cli(&specs);
        let translate = cli
            .find_subcommand("translate")
            .expect("translate must exist");
        assert!(translate.get_arguments().any(|arg| arg.get_id() == "FILE"));
        assert!(
            translate
                .get_arguments()
                .any(|arg| arg.get_id() == "language")
        );
    }

    #[test]
    fn collect_arg_values_reads_declared_arg_from_leaf_matches() {
        let mut args = std::collections::BTreeMap::new();
        args.insert("language".to_string(), arg_spec(Some('l'), true, ""));
        let specs = vec![spec_with_args(&["translate"], InputMode::Stdin, args)];
        let cli = build_cli(&specs);
        let matches = cli
            .try_get_matches_from(["npu", "translate", "--language", "french"])
            .expect("the command line must be accepted");
        let (_, leaf) = selected_path(&matches);

        let collected = collect_arg_values(&specs[0], leaf);

        assert_eq!(
            collected.get("language").map(String::as_str),
            Some("french")
        );
    }

    #[test]
    fn collect_arg_values_omits_unset_non_required_arg() {
        let mut args = std::collections::BTreeMap::new();
        args.insert("unused".to_string(), arg_spec(None, false, ""));
        let specs = vec![spec_with_args(&["x"], InputMode::Stdin, args)];
        let cli = build_cli(&specs);
        let matches = cli
            .try_get_matches_from(["npu", "x"])
            .expect("the command line must be accepted without the non-required argument");
        let (_, leaf) = selected_path(&matches);

        let collected = collect_arg_values(&specs[0], leaf);

        assert!(
            !collected.contains_key("unused"),
            "an argument that is not supplied and not required must not appear in the map, \
             no default value"
        );
    }

    #[test]
    fn missing_required_arg_is_rejected_by_clap() {
        let mut args = std::collections::BTreeMap::new();
        args.insert("language".to_string(), arg_spec(Some('l'), true, ""));
        let specs = vec![spec_with_args(&["translate"], InputMode::Stdin, args)];
        let cli = build_cli(&specs);

        let result = cli.try_get_matches_from(["npu", "translate"]);

        assert!(
            result.is_err(),
            "a missing required argument must be rejected by clap"
        );
    }
}
