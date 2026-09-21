//! Moteur générique d'exécution de commandes IA locales.
//!
//! Toute la logique vit ici : le binaire (`main.rs`) n'est qu'une coquille.
//! C'est ce qui rend le pipeline testable depuis `tests/`, qui ne peut importer
//! qu'une cible bibliothèque (cf. IMPLEMENTATION.md §4).

pub mod backend;
pub mod command;
pub mod config;
pub mod error;
pub mod input;
pub mod prompt;

pub use error::{Error, Result};

/// Racine de configuration de la phase 1 : uniquement `./.npu` (cf.
/// npu-cli-spec.md §5, IMPLEMENTATION.md §2 — pas de scopes `/etc` ni
/// `~/.config` avant la phase 2).
const CONFIG_ROOT: &str = ".npu";

/// Un nœud de l'arbre de commandes construit depuis les `CommandSpec`
/// découvertes. `spec` est renseigné sur une commande feuille ; un nœud
/// intermédiaire (ex. `git` avant `git review`) n'a que des enfants.
struct CommandNode<'a> {
    spec: Option<&'a command::CommandSpec>,
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

/// Regroupe les `CommandSpec` en arbre par préfixe de chemin : deux commandes
/// partageant un segment de tête (`git review`, `git commit`) fusionnent sous
/// le même nœud intermédiaire `git`.
fn build_command_tree(specs: &[command::CommandSpec]) -> CommandNode<'_> {
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

/// Construit récursivement le sous-arbre `clap` correspondant à `node`, nommé
/// `name`. Une commande feuille reçoit sa description et, si elle accepte un
/// fichier en entrée, un argument positionnel optionnel `FILE`. Un nœud
/// intermédiaire (pas de `CommandSpec` propre) reste utilisable seul : il
/// affiche alors son aide plutôt que d'échouer silencieusement.
fn build_clap_node(name: &str, node: &CommandNode<'_>) -> clap::Command {
    let mut cmd = clap::Command::new(name.to_string());

    match node.spec {
        Some(spec) => {
            cmd = cmd.about(spec.description.clone());
            if matches!(
                spec.input,
                command::InputMode::File | command::InputMode::StdinOrFile
            ) {
                cmd = cmd.arg(clap::Arg::new("FILE").required(false));
            }
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

/// Construit l'arbre `clap` complet (API builder, cf. IMPLEMENTATION.md
/// décision 1) depuis les commandes découvertes.
fn build_cli(specs: &[command::CommandSpec]) -> clap::Command {
    let tree = build_command_tree(specs);
    let mut root = clap::Command::new("npu").arg_required_else_help(true);
    for (name, node) in &tree.children {
        root = root.subcommand(build_clap_node(name, node));
    }
    root
}

/// Reconstitue le chemin de la commande sélectionnée en descendant la chaîne
/// de sous-commandes retenue par `clap`, et renvoie les `ArgMatches` de la
/// commande feuille avec le chemin parcouru.
fn selected_path(matches: &clap::ArgMatches) -> (Vec<String>, &clap::ArgMatches) {
    let mut path = Vec::new();
    let mut current = matches;
    while let Some((name, sub_matches)) = current.subcommand() {
        path.push(name.to_string());
        current = sub_matches;
    }
    (path, current)
}

/// Retrouve, parmi `specs`, la commande dont le chemin joint par `/` vaut
/// `key`. Renvoie une `Error::Config` listant les commandes disponibles si
/// aucune ne correspond (même style que `config::Config::resolve` et
/// `backend::chat` pour les identifiants inconnus).
fn find_command<'a>(
    specs: &'a [command::CommandSpec],
    key: &str,
) -> Result<&'a command::CommandSpec> {
    specs
        .iter()
        .find(|spec| spec.path.join("/") == key)
        .ok_or_else(|| {
            Error::Config(format!(
                "commande inconnue : « {key} » (commandes disponibles : {})",
                error::format_available(specs.iter().map(|s| s.path.join("/")))
            ))
        })
}

/// Point d'entrée de la bibliothèque, appelé par `main`.
///
/// Pipeline (npu-cli-spec.md §18, restreint à la phase 1) : chargement de la
/// configuration, découverte des commandes, construction de l'arbre `clap`,
/// résolution de la commande sélectionnée, du modèle, de l'entrée, rendu du
/// prompt, appel du backend, écriture du résultat sur stdout.
///
/// Une configuration invalide (TOML mal formé, frontmatter cassé) ne panique
/// jamais : `config::load` et `command::discover` remontent une
/// `Error::Config` (code de sortie 2) que `main` affiche sur stderr avant de
/// quitter — sans passer par la construction de l'arbre `clap`, puisque ce
/// dernier a justement besoin des commandes pour exister. C'est un choix
/// délibéré de la phase 1 : `--help` ne survit donc pas à une configuration
/// cassée (contrairement à l'objectif général d'IMPLEMENTATION.md décision 2,
/// qui vise les phases où `doctor` doit tourner malgré une config invalide).
pub fn run() -> Result<()> {
    let root = std::path::Path::new(CONFIG_ROOT);

    let config = config::load(root)?;
    let specs = command::discover(root)?;

    let cli = build_cli(&specs);
    let matches = cli.get_matches();

    let (path, leaf_matches) = selected_path(&matches);
    let key = path.join("/");

    let spec = find_command(&specs, &key)?;

    let (model, backend) = config.resolve(&spec.model)?;

    // L'argument `FILE` n'est déclaré (cf. `build_clap_node`) que pour les
    // modes qui acceptent un fichier : reproduire ici la même condition
    // évite d'appeler `get_one` sur un id absent (panique) et garde les deux
    // points en phase si une phase ultérieure change l'un sans l'autre.
    let file_arg = match spec.input {
        command::InputMode::File | command::InputMode::StdinOrFile => leaf_matches
            .get_one::<String>("FILE")
            .map(std::path::Path::new),
        command::InputMode::Stdin => None,
    };
    let input_text = input::resolve(&spec.input, file_arg)?;

    let prompt = prompt::render(&spec.prompt, &input_text);
    let output = backend::chat(backend, model, &prompt)?;

    println!("{output}");

    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)] // toléré dans les tests (cf. Cargo.toml [lints.clippy]).
mod tests {
    use super::*;
    use command::{CommandSpec, InputMode};

    fn spec(path: &[&str], input: InputMode) -> CommandSpec {
        CommandSpec {
            path: path.iter().map(ToString::to_string).collect(),
            description: format!("desc {}", path.join("/")),
            model: "qwen-fast".to_string(),
            input,
            prompt: "{{ input }}".to_string(),
        }
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
            .expect("git doit exister comme sous-commande fusionnée");
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
            .expect("commit-message doit exister");
        assert!(stdin_only.get_arguments().next().is_none());

        let classify = cli
            .find_subcommand("classify")
            .expect("classify doit exister");
        assert!(classify.get_arguments().any(|arg| arg.get_id() == "FILE"));

        let summarize = cli
            .find_subcommand("summarize")
            .expect("summarize doit exister");
        assert!(summarize.get_arguments().any(|arg| arg.get_id() == "FILE"));
    }

    #[test]
    fn selected_path_descends_nested_subcommands() {
        let specs = vec![spec(&["git", "review"], InputMode::Stdin)];
        let cli = build_cli(&specs);
        let matches = cli
            .try_get_matches_from(["npu", "git", "review"])
            .expect("la ligne de commande doit être acceptée");

        let (path, leaf) = selected_path(&matches);

        assert_eq!(path, vec!["git".to_string(), "review".to_string()]);
        // Les `ArgMatches` renvoyées sont bien celles de la feuille, pas de la racine :
        // aucune sous-commande ne reste à descendre depuis `leaf`.
        assert!(leaf.subcommand().is_none());
    }

    #[test]
    fn selected_path_is_empty_when_no_subcommand_is_selected() {
        let specs = vec![spec(&["commit-message"], InputMode::Stdin)];
        let cli = build_cli(&specs);
        // `arg_required_else_help` empêche normalement d'arriver ici sans
        // sous-commande en usage réel, mais `selected_path` doit rester correcte
        // si on l'appelle malgré tout sur des `ArgMatches` racine vides.
        let matches = clap::Command::new("npu")
            .try_get_matches_from(["npu"])
            .expect("aucune sous-commande requise sur cette instance de test");

        let (path, _leaf) = selected_path(&matches);

        assert!(path.is_empty());
        let _ = cli; // évite un warning si `cli` n'est plus utilisé au-delà
    }

    #[test]
    fn find_command_returns_matching_spec() {
        let specs = vec![
            spec(&["commit-message"], InputMode::Stdin),
            spec(&["git", "review"], InputMode::Stdin),
        ];
        let found = find_command(&specs, "git/review").expect("git/review doit être trouvée");
        assert_eq!(found.path, vec!["git".to_string(), "review".to_string()]);
    }

    #[test]
    fn find_command_unknown_key_lists_available_commands() {
        let specs = vec![
            spec(&["commit-message"], InputMode::Stdin),
            spec(&["git", "review"], InputMode::Stdin),
        ];
        let err = find_command(&specs, "does-not-exist")
            .expect_err("la commande ne doit pas être trouvée");

        assert!(matches!(err, Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("does-not-exist"));
        assert!(message.contains("commit-message"));
        assert!(message.contains("git/review"));
    }

    #[test]
    fn intermediate_node_without_its_own_spec_is_invocable_alone() {
        // Un nœud intermédiaire (ici `git`, qui n'a pas de CommandSpec propre)
        // doit rester utilisable seul : `arg_required_else_help` plutôt qu'un
        // échec silencieux si on invoque `npu git` sans sous-commande.
        let specs = vec![spec(&["git", "review"], InputMode::Stdin)];
        let cli = build_cli(&specs);
        let git = cli.find_subcommand("git").expect("git doit exister");
        assert!(git.is_arg_required_else_help_set());
    }
}
