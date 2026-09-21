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
pub mod scope;

pub use error::{Error, Result};

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

/// Construit le `clap::Arg` correspondant à un argument déclaré (`[args.*]`,
/// npu-cli-spec.md §11) : `.long(nom)`, `.short(lettre)` si présente,
/// `.required(required)`, `.help(description)` si non vide, `.value_name(nom
/// en MAJUSCULES)` et l'action de valeur simple (`ArgAction::Set`, un
/// argument attend une seule valeur — pas de multi-valeurs, pas de flag
/// booléen, cf. §11/§25). L'id de l'argument est `name` lui-même : c'est ce
/// que `collect_arg_values` relit depuis les `ArgMatches` de la commande
/// feuille.
fn build_declared_arg(name: &str, arg_spec: &command::ArgSpec) -> clap::Arg {
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

/// Construit récursivement le sous-arbre `clap` correspondant à `node`, nommé
/// `name`. Une commande feuille reçoit sa description, si elle accepte un
/// fichier en entrée un argument positionnel optionnel `FILE`, puis un
/// `clap::Arg` par argument déclaré (`[args.*]`, phase 3 — cf.
/// [`build_declared_arg`]). L'itération sur `spec.args` (un `BTreeMap`) est
/// triée par nom, donc l'ordre des arguments dans `--help` est déterministe
/// quel que soit l'ordre d'écriture du frontmatter TOML. `command::parse`
/// réserve déjà le nom `FILE` (cf. `RESERVED_ARG_NAMES`) : aucun argument
/// déclaré ne peut donc entrer en collision avec l'argument positionnel
/// ajouté ici. Un nœud intermédiaire (pas de `CommandSpec` propre) reste
/// utilisable seul : il affiche alors son aide plutôt que d'échouer
/// silencieusement.
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
            for (arg_name, arg_spec) in &spec.args {
                cmd = cmd.arg(build_declared_arg(arg_name, arg_spec));
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

/// Collecte, pour la commande feuille `spec`, les valeurs de ses arguments
/// déclarés (`[args.*]`) depuis les `ArgMatches` correspondantes, dans la
/// `BTreeMap<String, String>` attendue par `prompt::render`.
///
/// Un argument déclaré mais absent de `leaf_matches` (non requis et non
/// fourni sur la ligne de commande) est simplement omis de la map : la phase
/// 3 n'introduit pas de valeur par défaut (npu-cli-spec.md §11/§25). Si le
/// prompt référence quand même cet argument via `{{ args.NOM }}`,
/// `prompt::render` échoue avec une `Error::Config` nommant l'argument
/// (règle 6 du contrat partagé) plutôt que de substituer une chaîne vide non
/// demandée.
fn collect_arg_values(
    spec: &command::CommandSpec,
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

/// Point d'entrée de la bibliothèque, appelé par `main`.
///
/// Pipeline (npu-cli-spec.md §18, phase 2) : résolution des racines de scope
/// (`scope::roots()`, de la plus générale à la plus locale — `/etc/npu`, puis
/// `$XDG_CONFIG_HOME/npu` ou `$HOME/.config/npu`, puis `./.npu`), chargement
/// et fusion de la configuration sur ces racines (`config::load_scopes`),
/// découverte et fusion des commandes (`command::discover_scopes`),
/// construction de l'arbre `clap`, résolution de la commande sélectionnée, du
/// modèle, de l'entrée, rendu du prompt, appel du backend, écriture du
/// résultat sur stdout. La fusion entre scopes est un remplacement par
/// identifiant (backends/modèles) ou par chemin complet (commandes), jamais
/// une fusion champ par champ (IMPLEMENTATION.md décision 3) ; une racine
/// absente du disque n'est simplement pas prise en compte.
///
/// Une configuration invalide (TOML mal formé, frontmatter cassé) ne panique
/// jamais : `config::load_scopes` et `command::discover_scopes` remontent une
/// `Error::Config` (code de sortie 2) que `main` affiche sur stderr avant de
/// quitter — sans passer par la construction de l'arbre `clap`, puisque ce
/// dernier a justement besoin des commandes pour exister. C'est un choix
/// délibéré de la phase 1, toujours vrai en phase 2 : `--help` ne survit donc
/// pas à une configuration cassée (contrairement à l'objectif général
/// d'IMPLEMENTATION.md décision 2, qui vise les phases où `doctor` doit
/// tourner malgré une config invalide — dette explicitement tracée, non
/// résolue ici).
pub fn run() -> Result<()> {
    let roots = scope::roots();

    let config = config::load_scopes(&roots)?;
    let specs = command::discover_scopes(&roots)?;

    let cli = build_cli(&specs);
    let matches = cli.get_matches();

    let (path, leaf_matches) = selected_path(&matches);
    let key = path.join("/");

    let spec = find_command(&specs, &key)?;

    let (model, backend) = config.resolve(&spec.model)?;

    let args = collect_arg_values(spec, leaf_matches);
    // `std::env::var` renvoie `Err` aussi bien pour une variable absente que
    // pour une variable contenant de l'UTF-8 invalide ; `.ok()` réduit les
    // deux cas à `None`, exactement la sémantique attendue par
    // `prompt::render`/`prompt::preflight` (règle 5 du contrat : présence
    // vérifiée au rendu — et maintenant en préflight —, pas au chargement).
    // Une variable définie mais vide reste `Ok(String::new())` côté
    // `std::env::var` (elle n'est ni absente ni invalide), donc
    // `Some(String::new())` ici, jamais `None` — garanti par `std`, exercé
    // par les tests de `prompt::render` (`render_env_var_defined_but_empty_
    // is_not_an_error`) via cette même closure injectée. On ne peut pas
    // vérifier ce point directement par un test ICI sans muter l'environnement
    // réel, interdit par `unsafe_code = "forbid"` en édition 2024 (même
    // contrainte documentée sur `prompt::render`) : la closure elle-même
    // reste donc la plus petite unité non testable, tout le reste de la
    // sémantique est validé côté `prompt.rs`.
    let env = |name: &str| std::env::var(name).ok();

    // INVARIANT (revue L3, correctif 1) : rien de ce qui est connaissable
    // sans l'entrée ne doit être vérifié après avoir lu l'entrée. Un
    // argument référencé par le prompt et une variable d'environnement
    // référencée par le prompt sont tous deux connaissables avant même de
    // savoir ce que vaut `{{ input }}` : `prompt::preflight` le vérifie donc
    // AVANT `input::resolve`, qui est la seule étape de ce pipeline
    // susceptible de consommer une entrée non rejouable (un pipe, une
    // commande one-shot, cf. npu-cli-spec.md §22). Sans cet ordre, `git diff
    // | npu ...` lirait et jetterait tout le diff avant d'échouer sur un
    // argument optionnel absent ou une variable d'environnement non
    // définie — perte silencieuse et, sur un flux non rejouable,
    // irréversible du travail déjà produit en amont.
    prompt::preflight(&spec.prompt, &args, &env)?;

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

    let prompt = prompt::render(&spec.prompt, &input_text, &args, &env)?;
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
        spec_with_args(path, input, std::collections::BTreeMap::new())
    }

    fn spec_with_args(
        path: &[&str],
        input: InputMode,
        args: std::collections::BTreeMap<String, command::ArgSpec>,
    ) -> CommandSpec {
        CommandSpec {
            path: path.iter().map(ToString::to_string).collect(),
            description: format!("desc {}", path.join("/")),
            model: "qwen-fast".to_string(),
            input,
            prompt: "{{ input }}".to_string(),
            args,
        }
    }

    fn arg_spec(short: Option<char>, required: bool, description: &str) -> command::ArgSpec {
        command::ArgSpec {
            short,
            required,
            description: description.to_string(),
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

    // -- arguments CLI déclarés (phase 3) --------------------------------------

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
            .expect("translate doit exister");
        let language = translate
            .get_arguments()
            .find(|arg| arg.get_id() == "language")
            .expect("l'argument 'language' doit être présent");

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
        let x = cli.find_subcommand("x").expect("x doit exister");
        let unused = x
            .get_arguments()
            .find(|arg| arg.get_id() == "unused")
            .expect("l'argument 'unused' doit être présent");

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
        let x = cli.find_subcommand("x").expect("x doit exister");
        let names: Vec<&str> = x
            .get_arguments()
            .map(|arg| arg.get_id().as_str())
            .filter(|id| *id != "help")
            .collect();

        assert_eq!(names, vec!["alpha", "zebra"]);
    }

    #[test]
    fn file_positional_and_declared_args_coexist_without_id_collision() {
        let mut args = std::collections::BTreeMap::new();
        args.insert("language".to_string(), arg_spec(Some('l'), true, ""));
        let specs = vec![spec_with_args(&["translate"], InputMode::StdinOrFile, args)];

        // `build_cli` ne doit pas paniquer (id dupliqué) : la construction
        // seule est l'assertion.
        let cli = build_cli(&specs);
        let translate = cli
            .find_subcommand("translate")
            .expect("translate doit exister");
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
            .expect("la ligne de commande doit être acceptée");
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
            .expect("la ligne de commande doit être acceptée sans l'argument non requis");
        let (_, leaf) = selected_path(&matches);

        let collected = collect_arg_values(&specs[0], leaf);

        assert!(
            !collected.contains_key("unused"),
            "un argument non fourni et non requis ne doit pas apparaître dans la map, \
             pas de valeur par défaut (§11/§25)"
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
            "un argument requis manquant doit être rejeté par clap"
        );
    }
}
