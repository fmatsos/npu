//! Découverte et parsing des commandes définies sous `commands/**/*.md`.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

/// Mode de résolution de l'entrée d'une commande.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    #[default]
    Stdin,
    File,
    StdinOrFile,
}

/// Un argument CLI déclaré par une commande (npu-cli-spec.md §11).
///
/// Contrat d'API partagé : voir la doc de module pour la façon dont ce type
/// est peuplé. `short` est écrit en TOML comme une chaîne (`short = "l"`) ;
/// `ArgSpec` porte le `char` déjà validé, jamais la chaîne brute — la
/// conversion (avec vérification explicite qu'elle ne tronque rien) est
/// faite par [`convert_args`], pas par ce `#[derive(Deserialize)]`, qui
/// n'est jamais exercé directement sur du TOML (cf. [`RawArgSpec`]).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArgSpec {
    pub short: Option<char>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub description: String,
}

/// Une commande découverte : son chemin (dérivé de l'arborescence), le modèle
/// à utiliser, le mode d'entrée, le prompt (corps du fichier) et les
/// arguments CLI déclarés (`[args.*]`, phase 3).
#[derive(Debug)]
pub struct CommandSpec {
    pub path: Vec<String>,
    pub description: String,
    pub model: String,
    pub input: InputMode,
    pub prompt: String,
    pub args: BTreeMap<String, ArgSpec>,
}

/// Version brute d'`ArgSpec` telle qu'écrite en TOML : `short` y est une
/// chaîne, pas un `char`. `toml`/`serde` savent bien désérialiser une chaîne
/// TOML directement vers `char` (ils rejettent une chaîne de plusieurs
/// caractères plutôt que de la tronquer), mais le message qui en résulte ne
/// nomme pas l'argument fautif — seulement la ligne/colonne TOML. On
/// désérialise donc ici en `String`, et [`convert_args`] fait la conversion
/// avec un message qui nomme l'argument et la valeur, comme le reste du
/// module (cf. `validate_backend` dans `config.rs` pour le même idiome).
///
/// `deny_unknown_fields` doit être répété ici (et pas seulement sur
/// `ArgSpec`, jamais exercée par `serde` sur du TOML) : sinon une clé mal
/// orthographiée sous `[args.*]` (ex. `requred`) serait lue puis
/// silencieusement ignorée, exactement le défaut visé par la revue L3.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArgSpec {
    #[serde(default)]
    short: Option<String>,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    description: String,
}

/// Frontmatter TOML brut, avant résolution des valeurs par défaut.
///
/// `deny_unknown_fields` (revue L3, correctif 3) : sans lui, une clé mal
/// orthographiée au niveau racine du frontmatter (ex. `descripton` au lieu
/// de `description`) était lue puis silencieusement ignorée — exactement le
/// défaut que ce projet rejette depuis la phase 1 pour `[args.*]`
/// (`RawArgSpec`), désormais étendu au frontmatter entier.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frontmatter {
    #[serde(default)]
    description: String,
    model: String,
    #[serde(default)]
    input: InputSection,
    #[serde(default)]
    args: BTreeMap<String, RawArgSpec>,
    /// Section `[output]` (phase 4, npu-cli-spec.md — format de sortie,
    /// JSON Schema, `max_lines`), volontairement ACCEPTÉE mais PAS
    /// interprétée ici : aucune sémantique de sortie n'est implémentée par
    /// ce correctif. Typée en `toml::Value` (une table libre) plutôt qu'une
    /// struct dédiée, uniquement pour que `deny_unknown_fields` ci-dessus ne
    /// fasse pas échouer le chargement de la fixture versionnée
    /// `.npu/commands/commit-message.md`, qui déclare déjà `[output]`
    /// (`format = "text"`, `max_lines = 1`). La phase 4, qui suit ce
    /// correctif, remplacera ce champ par un vrai `OutputSpec` structuré et
    /// lui donnera un sens ; jusque-là, sa seule valeur est d'être accepté
    /// sans être lu.
    // Jamais lu ici par construction (cf. doc du champ juste au-dessus) :
    // seule sa PRÉSENCE dans le frontmatter compte pour `deny_unknown_fields`
    // (accepter la section sans lui donner de sens), pas sa valeur. La phase
    // 4 lira ce champ et cet `allow` disparaîtra alors.
    #[serde(default)]
    #[allow(dead_code)]
    output: Option<toml::Value>,
}

/// Section `[input]` du frontmatter.
///
/// `deny_unknown_fields` (revue L3, correctif 3) : une clé mal orthographiée
/// ici (ex. `moed` au lieu de `mode`) retombait silencieusement sur le mode
/// d'entrée par défaut (`InputMode::Stdin`) — le cas le plus dangereux des
/// trois relevés en revue, une commande à fichier lisant stdin sans un mot.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct InputSection {
    #[serde(default)]
    mode: Option<InputMode>,
}

/// Découvre les commandes sur plusieurs racines de scope (phase 2,
/// `IMPLEMENTATION.md` décision 3) et les fusionne par remplacement.
///
/// `roots` est ordonnée de la plus générale à la plus locale (cf.
/// `scope::roots`). La clé de commande (son chemin, ex. `"git/review"`) vient
/// du CHEMIN DU FICHIER, pas de son contenu : on peut donc résoudre le
/// gagnant de chaque chemin sur tous les scopes AVANT d'ouvrir le moindre
/// fichier (cf. revue L3, phase 2). Une commande dont le chemin complet a
/// déjà été vue dans un scope plus général est REMPLACÉE intégralement par
/// la version du scope plus local — pas de fusion champ par champ. Un
/// fichier cassé mais masqué par un override plus local n'est donc jamais
/// lu ni parsé. Une racine absente n'est pas une erreur : `collect_command_files`
/// renvoie déjà un `Vec` vide pour un dossier `commands/` manquant.
///
/// Le résultat est trié par chemin complet, pour que l'arbre clap et l'aide
/// soient déterministes quel que soit l'ordre de lecture du système de
/// fichiers.
pub fn discover_scopes(roots: &[std::path::PathBuf]) -> crate::Result<Vec<CommandSpec>> {
    let mut winners: std::collections::BTreeMap<String, (Vec<String>, std::path::PathBuf)> =
        std::collections::BTreeMap::new();

    for root in roots {
        for (path, file) in collect_command_files(root)? {
            let key = path.join("/");
            winners.insert(key, (path, file));
        }
    }

    let mut specs = Vec::with_capacity(winners.len());
    for (path, file) in winners.into_values() {
        specs.push(read_and_parse(path, &file)?);
    }

    Ok(specs)
}

/// Parcourt `<root>/commands/**/*.md` et renvoie, pour chaque fichier trouvé,
/// le chemin de commande dérivé de son emplacement (ex. `["git", "review"]`
/// pour `commands/git/review.md`) accompagné du chemin du fichier — sans
/// lire ni parser son contenu. Extrait de `discover` pour que
/// `discover_scopes` puisse résoudre l'override par chemin de fichier avant
/// d'ouvrir le moindre fichier (cf. revue L3, phase 2).
///
/// Renvoie un `Vec` vide si le dossier `commands/` n'existe pas.
fn collect_command_files(
    root: &std::path::Path,
) -> crate::Result<Vec<(Vec<String>, std::path::PathBuf)>> {
    let commands_root = root.join("commands");
    if !commands_root.is_dir() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    collect_markdown_files(&commands_root, &mut files)?;

    let mut result = Vec::with_capacity(files.len());
    for file in files {
        let relative = file.strip_prefix(&commands_root).map_err(|_| {
            crate::Error::Config(format!(
                "chemin de commande hors de commands/ : {}",
                file.display()
            ))
        })?;
        let path = relative
            .with_extension("")
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        result.push((path, file));
    }

    Ok(result)
}

/// Lit et parse le fichier de commande `file`, dont le chemin de commande
/// dérivé est `path`.
///
/// Enveloppe le MESSAGE d'une erreur de parsing avec le chemin du fichier
/// fautif, jamais l'erreur déjà formatée : `parse` renvoie une
/// `Error::Config` dont le `Display` porte déjà le préfixe « erreur de
/// configuration : » ; ré-envelopper cette erreur (plutôt que son message)
/// dans une nouvelle `Error::Config` dupliquerait ce préfixe (cf. revue L3,
/// phase 2).
fn read_and_parse(path: Vec<String>, file: &std::path::Path) -> crate::Result<CommandSpec> {
    let source = std::fs::read_to_string(file).map_err(|err| {
        crate::Error::Config(format!(
            "impossible de lire le fichier {} : {err}",
            file.display()
        ))
    })?;
    parse(&source, path).map_err(|err| match err {
        crate::Error::Config(msg) => crate::Error::Config(format!("{} : {msg}", file.display())),
        other => other,
    })
}

/// Parcourt `<root>/commands/**/*.md`.
///
/// Renvoie un `Vec` vide si le dossier `commands/` n'existe pas.
pub fn discover(root: &std::path::Path) -> crate::Result<Vec<CommandSpec>> {
    let mut specs = Vec::new();
    for (path, file) in collect_command_files(root)? {
        specs.push(read_and_parse(path, &file)?);
    }
    Ok(specs)
}

/// Parcourt récursivement `dir` et accumule dans `out` les chemins des
/// fichiers `.md` trouvés.
fn collect_markdown_files(
    dir: &std::path::Path,
    out: &mut Vec<std::path::PathBuf>,
) -> crate::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_markdown_files(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "md") {
            out.push(path);
        }
    }
    Ok(())
}

/// Noms d'argument réservés par `clap` ou par `lib.rs` : construire l'arbre
/// de commandes avec un argument nommé `help` ou `version` entrerait en
/// collision avec les flags que `clap` gère lui-même ; un argument nommé
/// `FILE` entrerait en collision avec l'argument positionnel `FILE` que
/// `lib.rs` (`build_clap_node`) ajoute déjà pour les commandes dont le mode
/// d'entrée accepte un fichier (`InputMode::File`/`StdinOrFile`, phase 3).
/// Refuser ici, au chargement, plutôt que de laisser l'erreur remonter (bien
/// moins clairement, voire faire paniquer `clap::Command::arg` sur un id
/// dupliqué) depuis la construction de l'arbre `clap` en aval, dans `lib.rs`.
const RESERVED_ARG_NAMES: [&str; 3] = ["help", "version", "FILE"];

/// Lettre courte réservée par `clap` : chaque `Command` reçoit un flag
/// `-h`/`--help` automatique, que `disable_help_flag` ne soit pas appelé ou
/// non — vérifié dans `lib.rs`, qui ne l'appelle pas. `-V`/`--version`
/// n'existe en revanche que si `Command::version(..)` est appelé, ce que
/// `lib.rs` ne fait pas non plus : on ne réserve donc pas `V` ici, pour ne
/// pas rejeter une configuration qui n'entre en collision avec rien de
/// construit. Si `lib.rs` se met un jour à appeler `.version(..)`, cette
/// liste devra suivre.
const RESERVED_SHORT_LETTERS: [char; 1] = ['h'];

/// Valide le nom d'un argument déclaré (la clé `[args.<nom>]`) : non vide,
/// composé uniquement des caractères qu'un placeholder `{{ args.<nom> }}`
/// peut porter, ne commençant pas par `-` (qui le ferait prendre pour un
/// flag par `clap`), et pas l'un des noms réservés ci-dessus.
///
/// La vérification de caractères réutilise `prompt::is_valid_name_char` (la
/// MÊME règle que celle qui reconnaît `{{ args.<nom> }}`) plutôt que d'en
/// écrire une seconde : TOML accepte une clé de table entre guillemets
/// (`[args."café"]`, `[args."foo.bar"]`) sur des caractères qu'un
/// placeholder n'accepte jamais. Sans ce partage, un tel nom passait la
/// validation ici (aucune de ces deux formes n'a d'espace ni ne commence par
/// `-`) puis échouait plus tard, au mieux avec un message pointant sur le
/// placeholder plutôt que sur la déclaration fautive (« placeholder inconnu
/// » au lieu de « nom d'argument invalide »), au pire jamais : un argument
/// déclaré mais non référencé dans le prompt chargeait alors silencieusement
/// avec un nom qu'aucun prompt ne peut jamais référencer validement.
fn validate_arg_name(name: &str) -> crate::Result<()> {
    if name.is_empty() {
        return Err(crate::Error::Config(
            "un argument a un nom vide (`[args.\"\"]`) : les arguments doivent être nommés"
                .to_string(),
        ));
    }
    if !name.chars().all(crate::prompt::is_valid_name_char) {
        return Err(crate::Error::Config(format!(
            "argument « {name} » : le nom d'un argument ne peut contenir que des lettres et \
             chiffres ASCII, « _ » ou « - » (mêmes caractères qu'un placeholder valide {{{{ \
             args.<nom> }}}} côté prompt)"
        )));
    }
    if name.starts_with('-') {
        return Err(crate::Error::Config(format!(
            "argument « {name} » : le nom d'un argument ne peut pas commencer par « - »"
        )));
    }
    if RESERVED_ARG_NAMES.contains(&name) {
        return Err(crate::Error::Config(format!(
            "argument « {name} » : nom réservé par clap ({}) ; choisissez un autre nom",
            RESERVED_ARG_NAMES.join("/")
        )));
    }
    Ok(())
}

/// Convertit un `short` brut (chaîne TOML, éventuellement absente) en `char`
/// validé, ou renvoie l'erreur de configuration nommant l'argument `name` et
/// la valeur fautive `raw`. Ne tronque jamais silencieusement : une chaîne
/// vide ou de plusieurs caractères est un rejet, pas une troncature au
/// premier caractère.
fn convert_short(name: &str, raw: Option<String>) -> crate::Result<Option<char>> {
    let Some(raw) = raw else {
        return Ok(None);
    };

    let mut chars = raw.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return Err(crate::Error::Config(format!(
            "argument « {name} » : « short » doit être un seul caractère, obtenu « {raw} » \
             ({} caractère(s))",
            raw.chars().count()
        )));
    };

    if RESERVED_SHORT_LETTERS.contains(&c) {
        return Err(crate::Error::Config(format!(
            "argument « {name} » : la lettre courte « {c} » est réservée par clap (aide, \
             version) ; choisissez-en une autre"
        )));
    }

    // `clap::Arg::short` refuse `-` : `debug_assert!(s != '-', "short option
    // name cannot be `-`")` (clap_builder 4.6.7, `builder/arg.rs`). Un
    // `debug_assert!` ne s'exécute qu'en profil debug — `cargo run`/`cargo
    // test`/`make test` paniquent (code de sortie 101, hors contrat) ; `make
    // qa`/`cargo build --release` (assertions de debug désactivées par
    // défaut) laisseraient passer silencieusement un flag court `-` inutile
    // (ambigu avec le préfixe d'option lui-même). Rejeter ici, à la
    // conversion, referme les deux : pas de dépendance au profil de
    // compilation pour un comportement correct.
    if c == '-' {
        return Err(crate::Error::Config(format!(
            "argument « {name} » : la lettre courte ne peut pas être « - » (confondue avec le \
             préfixe d'option lui-même) ; choisissez-en une autre"
        )));
    }

    Ok(Some(c))
}

/// Convertit les `[args.*]` bruts du frontmatter en `ArgSpec`, en validant :
/// le nom de chaque argument ([`validate_arg_name`]), la conversion de
/// `short` en `char` ([`convert_short`]), et l'absence de collision entre
/// deux arguments déclarant la même lettre `short` dans la même commande.
///
/// L'itération sur `raw` (un `BTreeMap`) est triée par nom d'argument, donc
/// le message de collision est déterministe : il nomme toujours l'argument
/// déjà vu (alphabétiquement antérieur) en premier.
fn convert_args(raw: BTreeMap<String, RawArgSpec>) -> crate::Result<BTreeMap<String, ArgSpec>> {
    let mut args = BTreeMap::new();
    let mut shorts_used: BTreeMap<char, String> = BTreeMap::new();

    for (name, raw_spec) in raw {
        validate_arg_name(&name)?;
        let short = convert_short(&name, raw_spec.short)?;

        if let Some(c) = short
            && let Some(existing) = shorts_used.insert(c, name.clone())
        {
            return Err(crate::Error::Config(format!(
                "arguments « {existing} » et « {name} » partagent la même lettre courte « {c} »"
            )));
        }

        args.insert(
            name,
            ArgSpec {
                short,
                required: raw_spec.required,
                description: raw_spec.description,
            },
        );
    }

    Ok(args)
}

/// Parse le contenu d'un fichier de commande.
///
/// Le frontmatter est délimité par des lignes `+++` ; l'en-tête est du TOML,
/// le corps (après le second délimiteur) est le prompt.
pub fn parse(source: &str, path: Vec<String>) -> crate::Result<CommandSpec> {
    let mut lines = source.lines();

    match lines.next() {
        Some("+++") => {}
        _ => {
            return Err(crate::Error::Config(
                "frontmatter manquant : le fichier doit commencer par une ligne '+++'".to_string(),
            ));
        }
    }

    let mut header_lines = Vec::new();
    let mut closed = false;
    let mut rest_lines: Vec<&str> = Vec::new();
    for line in lines.by_ref() {
        if line == "+++" {
            closed = true;
            break;
        }
        header_lines.push(line);
    }
    if !closed {
        return Err(crate::Error::Config(
            "frontmatter non fermé : ligne '+++' de fin manquante".to_string(),
        ));
    }
    rest_lines.extend(lines);

    let header = header_lines.join("\n");
    let frontmatter: Frontmatter = toml::from_str(&header)
        .map_err(|err| crate::Error::Config(format!("frontmatter invalide : {err}")))?;

    let prompt = rest_lines.join("\n");
    let prompt = prompt.trim_start_matches('\n').to_string();

    let args = convert_args(frontmatter.args)?;

    // Validation statique des placeholders (npu-cli-spec.md §12) : un prompt
    // référençant {{ args.inconnu }} doit échouer ICI, au chargement — pas
    // au moment de l'exécution. `parse` ne tourne que sur les fichiers
    // gagnants de la résolution de scopes (cf. `discover_scopes`), donc un
    // fichier masqué par un override local, même avec un placeholder cassé,
    // n'est toujours jamais ouvert ni validé (cf. revue L3, phase 2 ; test
    // `discover_scopes_broken_placeholder_fully_masked_by_local_scope_resolves_successfully`).
    let declared: BTreeSet<String> = args.keys().cloned().collect();
    crate::prompt::validate(&prompt, &declared)?;

    // Revue L3, correctif 2 : un argument référencé par le prompt via
    // {{ args.NOM }} mais déclaré `required = false` est une contradiction
    // dans le fichier de commande lui-même — un prompt qui interpole NOM ne
    // peut jamais se rendre sans NOM, quelle que soit la ligne de commande
    // effectivement tapée. On rejette ICI, au chargement, en nommant
    // l'argument (le fichier est ajouté par l'appelant, cf. `read_and_parse`)
    // plutôt que de laisser l'échec se produire au rendu (où il peut
    // survenir bien après que l'entrée a été consommée, cf. correctif 1) ou,
    // pire, de promouvoir silencieusement l'argument en `required = true` :
    // ce serait honorer la configuration autrement qu'elle n'est déclarée,
    // la même règle que ce projet applique depuis la phase 1 (une clé lue
    // puis ignorée, ou une valeur réinterprétée, est un défaut).
    //
    // La raison de fond : en phase 5, `npu doctor`/`npu describe` doivent
    // pouvoir dire qu'un fichier de commande est cassé SANS l'invoquer. Un
    // fichier dont le prompt ne peut jamais être rendu (quel que soit
    // l'appel) est cassé ; le rejeter au chargement le rend détectable
    // gratuitement, avant toute exécution.
    //
    // Contrainte délibérée de l'état ACTUEL, pas une vérité permanente : le
    // jour où les valeurs par défaut (`default = "..."`) existeront pour
    // `[args.*]`, un argument optionnel avec une valeur par défaut pourra de
    // nouveau être référencé par le prompt sans contradiction, et ce rejet
    // devra être assoupli en conséquence.
    for placeholder in crate::prompt::placeholders(&prompt)? {
        if let crate::prompt::Placeholder::Arg(name) = placeholder {
            // `declared` est garanti contenir `name` : `prompt::validate`
            // ci-dessus aurait déjà échoué sinon.
            let spec = &args[&name];
            if !spec.required {
                return Err(crate::Error::Config(format!(
                    "argument « {name} » référencé par {{{{ args.{name} }}}} mais déclaré \
                     required = false : un argument référencé par le prompt doit être \
                     required = true (aucune valeur par défaut n'existe encore pour \
                     `[args.*]`)"
                )));
            }
        }
    }

    Ok(CommandSpec {
        path,
        description: frontmatter.description,
        model: frontmatter.model,
        input: frontmatter.input.mode.unwrap_or_default(),
        prompt,
        args,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)] // toléré dans les tests (cf. Cargo.toml [lints.clippy]).
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Crée un dossier de fixture unique sous `target/`, pour ne pas polluer le
    /// dépôt ni entrer en collision entre tests exécutés en parallèle (même
    /// idiome que `config::tests::fixture_dir`).
    fn fixture_dir(name: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!("command-{name}-{n}"));
        std::fs::create_dir_all(&dir).expect("création du dossier de fixture");
        dir
    }

    #[test]
    fn collect_markdown_files_recurses_into_subdirectories() {
        let dir = fixture_dir("collect-nested");
        std::fs::create_dir_all(dir.join("git")).expect("création du sous-dossier");
        std::fs::write(dir.join("classify.md"), "top-level").expect("écriture fixture");
        std::fs::write(dir.join("git/review.md"), "nested").expect("écriture fixture");
        std::fs::write(dir.join("notes.txt"), "ignoré : pas .md").expect("écriture fixture");

        let mut found = Vec::new();
        collect_markdown_files(&dir, &mut found).expect("la collecte doit réussir");

        assert_eq!(
            found.len(),
            2,
            "seuls les .md doivent être collectés, à toute profondeur"
        );
        assert!(found.contains(&dir.join("classify.md")));
        assert!(found.contains(&dir.join("git/review.md")));
    }

    #[test]
    fn discover_finds_nested_command_path() {
        let root = fixture_dir("discover-nested");
        let commands_dir = root.join("commands").join("git");
        std::fs::create_dir_all(&commands_dir).expect("création du dossier commands/git");
        std::fs::write(
            commands_dir.join("review.md"),
            "+++\nmodel = \"qwen-fast\"\n+++\nprompt\n",
        )
        .expect("écriture fixture");

        let specs = discover(&root).expect("la découverte doit réussir");

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].path, vec!["git".to_string(), "review".to_string()]);
    }

    #[test]
    fn discover_reports_faulty_file_path_on_broken_frontmatter() {
        // Revue L3 (phase 1) : une config lue mais rejetée doit nommer le
        // fichier fautif. `parse` seul ne peut pas le faire (il ne connaît
        // pas le chemin) ; c'est `discover` qui doit l'ajouter.
        let root = fixture_dir("discover-broken-frontmatter");
        let commands_dir = root.join("commands");
        std::fs::create_dir_all(&commands_dir).expect("création du dossier commands");
        std::fs::write(
            commands_dir.join("broken.md"),
            "pas de frontmatter du tout\n",
        )
        .expect("écriture fixture");

        let err = discover(&root).expect_err("un frontmatter manquant doit échouer");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(
            message.contains("broken.md"),
            "le message d'erreur doit nommer le fichier fautif, obtenu : {message}"
        );
    }

    #[test]
    fn parses_nominal_command() {
        let source = "+++\ndescription = \"Classify\"\nmodel = \"qwen-fast\"\n\n[input]\nmode = \"stdin_or_file\"\n+++\nHello {{ input }}\n";
        let spec = parse(source, vec!["classify".to_string()]).expect("should parse");
        assert_eq!(spec.description, "Classify");
        assert_eq!(spec.model, "qwen-fast");
        assert!(matches!(spec.input, InputMode::StdinOrFile));
        assert_eq!(spec.prompt, "Hello {{ input }}");
        assert_eq!(spec.path, vec!["classify".to_string()]);
    }

    #[test]
    fn missing_closing_delimiter_is_config_error() {
        let source = "+++\nmodel = \"qwen-fast\"\nHello\n";
        let err = parse(source, vec!["x".to_string()]).expect_err("should fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn missing_frontmatter_is_config_error() {
        let source = "Hello {{ input }}\n";
        let err = parse(source, vec!["x".to_string()]).expect_err("should fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn default_input_mode_is_stdin() {
        let source = "+++\nmodel = \"qwen-fast\"\n+++\nprompt\n";
        let spec = parse(source, vec!["x".to_string()]).expect("should parse");
        assert!(matches!(spec.input, InputMode::Stdin));
    }

    #[test]
    fn missing_model_field_is_config_error() {
        let source = "+++\ndescription = \"no model\"\n+++\nprompt\n";
        let err = parse(source, vec!["x".to_string()]).expect_err("should fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn invalid_toml_is_config_error() {
        let source = "+++\nmodel = \n+++\nprompt\n";
        let err = parse(source, vec!["x".to_string()]).expect_err("should fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn nested_path_is_preserved() {
        let source = "+++\nmodel = \"qwen-fast\"\n+++\nprompt\n";
        let spec =
            parse(source, vec!["git".to_string(), "review".to_string()]).expect("should parse");
        assert_eq!(spec.path, vec!["git".to_string(), "review".to_string()]);
    }

    /// Écrit une commande `<root>/commands/<rel_path>.md` avec le frontmatter
    /// minimal donné, en créant les dossiers intermédiaires.
    fn write_command(root: &std::path::Path, rel_path: &str, model: &str, prompt: &str) {
        let file = root.join("commands").join(format!("{rel_path}.md"));
        std::fs::create_dir_all(file.parent().expect("le fichier a un parent"))
            .expect("création des dossiers intermédiaires");
        std::fs::write(&file, format!("+++\nmodel = \"{model}\"\n+++\n{prompt}\n"))
            .expect("écriture fixture");
    }

    #[test]
    fn discover_scopes_local_redefinition_wins() {
        let general = fixture_dir("scopes-general");
        let local = fixture_dir("scopes-local");
        write_command(&general, "classify", "qwen-general", "prompt général");
        write_command(&local, "classify", "qwen-local", "prompt local");

        let specs = discover_scopes(&[general, local]).expect("discover_scopes doit réussir");

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].model, "qwen-local");
        assert_eq!(specs[0].prompt, "prompt local");
    }

    #[test]
    fn discover_scopes_different_paths_accumulate() {
        let general = fixture_dir("scopes-accumulate-general");
        let local = fixture_dir("scopes-accumulate-local");
        write_command(&general, "classify", "qwen-general", "p1");
        write_command(&local, "summarize", "qwen-local", "p2");

        let specs = discover_scopes(&[general, local]).expect("discover_scopes doit réussir");

        let mut paths: Vec<_> = specs.iter().map(|s| s.path.join("/")).collect();
        paths.sort();
        assert_eq!(paths, vec!["classify".to_string(), "summarize".to_string()]);
    }

    #[test]
    fn discover_scopes_nested_command_replaced_by_full_path_no_collision_with_root_sibling() {
        let general = fixture_dir("scopes-nested-general");
        let local = fixture_dir("scopes-nested-local");
        write_command(&general, "git/review", "qwen-general", "p général");
        write_command(&general, "review", "qwen-root", "p racine");
        write_command(&local, "git/review", "qwen-local", "p local");

        let specs = discover_scopes(&[general, local]).expect("discover_scopes doit réussir");

        assert_eq!(specs.len(), 2);

        let nested = specs
            .iter()
            .find(|s| s.path == vec!["git".to_string(), "review".to_string()])
            .expect("git/review doit être présent");
        assert_eq!(nested.model, "qwen-local");
        assert_eq!(nested.prompt, "p local");

        let root_level = specs
            .iter()
            .find(|s| s.path == vec!["review".to_string()])
            .expect("review racine doit être présent, distinct de git/review");
        assert_eq!(root_level.model, "qwen-root");
        assert_eq!(root_level.prompt, "p racine");
    }

    #[test]
    fn discover_scopes_nonexistent_root_is_ignored() {
        let nonexistent = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join("command-scopes-does-not-exist");

        let specs = discover_scopes(&[nonexistent]).expect("discover_scopes doit réussir");

        assert!(specs.is_empty());
    }

    #[test]
    fn discover_scopes_result_is_sorted_deterministically() {
        let general = fixture_dir("scopes-sort-general");
        write_command(&general, "zebra", "m", "p");
        write_command(&general, "alpha", "m", "p");
        write_command(&general, "middle/child", "m", "p");

        let specs = discover_scopes(&[general]).expect("discover_scopes doit réussir");

        let paths: Vec<_> = specs.iter().map(|s| s.path.join("/")).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(
            paths, sorted,
            "le résultat doit être trié par chemin complet"
        );
    }

    #[test]
    fn discover_scopes_broken_frontmatter_fully_masked_by_local_scope_resolves_successfully() {
        // Revue L3 (phase 2) : la clé d'une commande vient du chemin du
        // fichier, pas de son contenu. Un override local valide doit donc
        // masquer un fichier général cassé SANS jamais l'ouvrir.
        let general = fixture_dir("scopes-broken-frontmatter-masked-general");
        let commands_dir = general.join("commands");
        std::fs::create_dir_all(&commands_dir).expect("création du dossier commands");
        std::fs::write(
            commands_dir.join("commit-message.md"),
            "pas de frontmatter\n",
        )
        .expect("écriture fixture");

        let local = fixture_dir("scopes-broken-frontmatter-masked-local");
        write_command(&local, "commit-message", "qwen-local", "prompt local");

        let specs = discover_scopes(&[general, local])
            .expect("la version locale doit masquer le fichier cassé du scope général");

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].model, "qwen-local");
        assert_eq!(specs[0].prompt, "prompt local");
    }

    #[test]
    fn discover_error_message_does_not_double_config_error_prefix() {
        // Revue L3 (phase 2) : `discover` enveloppait l'erreur déjà formatée
        // de `parse` (qui porte déjà « erreur de configuration : ») au lieu
        // de son message, doublant le préfixe.
        let root = fixture_dir("discover-double-prefix");
        let commands_dir = root.join("commands");
        std::fs::create_dir_all(&commands_dir).expect("création du dossier commands");
        std::fs::write(commands_dir.join("broken.md"), "pas de frontmatter\n")
            .expect("écriture fixture");

        let err = discover(&root).expect_err("un frontmatter manquant doit échouer");
        let msg = err.to_string();
        assert_eq!(
            msg.matches("erreur de configuration").count(),
            1,
            "le préfixe ne doit apparaître qu'une seule fois, obtenu : {msg}"
        );
        assert!(msg.contains("broken.md"));
    }

    // -- [args.*] (phase 3) -----------------------------------------------------

    #[test]
    fn parses_full_arg_spec() {
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.language]\nshort = \"l\"\n\
                       required = true\ndescription = \"Target language\"\n+++\nHello {{ args.language }}\n";
        let spec = parse(source, vec!["translate".to_string()]).expect("should parse");

        assert_eq!(spec.args.len(), 1);
        let arg = spec
            .args
            .get("language")
            .expect("l'argument doit être présent");
        assert_eq!(arg.short, Some('l'));
        assert!(arg.required);
        assert_eq!(arg.description, "Target language");
    }

    #[test]
    fn arg_short_absent_is_none() {
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.language]\nrequired = true\n+++\nHello {{ args.language }}\n";
        let spec = parse(source, vec!["translate".to_string()]).expect("should parse");

        assert_eq!(spec.args.get("language").expect("présent").short, None);
    }

    #[test]
    fn arg_short_multi_character_is_config_error() {
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.language]\nshort = \"lang\"\n+++\nHello {{ args.language }}\n";
        let err = parse(source, vec!["translate".to_string()])
            .expect_err("un short de plusieurs caractères doit échouer");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("language"), "obtenu : {message}");
        assert!(message.contains("lang"), "obtenu : {message}");
    }

    #[test]
    fn arg_short_dash_is_config_error_not_a_panic() {
        // `clap::Arg::short('-')` fait `debug_assert!(s != '-', ...)`
        // (clap_builder 4.6.7) : sans ce rejet à la conversion, un `short =
        // "-"` paniquerait (code de sortie 101, hors contrat) en profil
        // debug (`cargo run`/`cargo test`/`make test`), et passerait
        // silencieusement en profil release. Vérifié empiriquement sur les
        // deux profils avant ce correctif.
        let source =
            "+++\nmodel = \"qwen-fast\"\n\n[args.x]\nshort = \"-\"\n+++\nHello {{ args.x }}\n";
        let err = parse(source, vec!["x".to_string()])
            .expect_err("un short « - » doit échouer au chargement, jamais paniquer");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains('x'));
    }

    #[test]
    fn arg_required_defaults_to_false() {
        // Le prompt ne référence PAS `{{ args.language }}` : depuis le
        // correctif 2 (revue L3), un argument référencé par le prompt doit
        // être `required = true` (cf.
        // `referenced_arg_with_required_false_is_a_load_error` plus bas). Ce
        // test-ci vise uniquement la valeur par défaut de `required`, donc le
        // prompt ne peut pas référencer l'argument sans changer ce qu'il
        // teste.
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.language]\nshort = \"l\"\n+++\nHello {{ input }}\n";
        let spec = parse(source, vec!["translate".to_string()]).expect("should parse");

        assert!(!spec.args.get("language").expect("présent").required);
    }

    #[test]
    fn arg_name_with_accented_character_is_config_error_even_when_unreferenced() {
        // TOML autorise une clé de table entre guillemets sur des
        // caractères qu'un placeholder `{{ args.<nom> }}` n'accepte jamais
        // (cf. doc de `validate_arg_name`). Un tel argument, même jamais
        // référencé par le prompt, doit échouer AU CHARGEMENT — sinon il
        // charge silencieusement avec un nom qu'aucun prompt ne peut
        // jamais référencer validement, exactement le défaut visé par la
        // règle d'architecture des revues L3.
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.\"café\"]\nshort = \"c\"\n+++\nHello {{ input }}\n";
        let err = parse(source, vec!["x".to_string()])
            .expect_err("un nom d'argument accentué doit échouer au chargement");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("café"), "obtenu : {message}");
        assert!(
            !message.contains("placeholder inconnu"),
            "le message doit pointer sur la déclaration de l'argument (« nom d'un argument »), \
             pas reproduire l'erreur de placeholder inconnu de prompt.rs, obtenu : {message}"
        );
    }

    #[test]
    fn arg_name_with_dot_is_config_error() {
        // Même défaut que ci-dessus avec un caractère différent : `.` est
        // valide dans une clé TOML guillemetée mais délimite un préfixe de
        // placeholder (`args.`/`env.`) côté `prompt.rs`.
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.\"foo.bar\"]\n+++\nHello {{ input }}\n";
        let err = parse(source, vec!["x".to_string()])
            .expect_err("un nom d'argument contenant un point doit échouer au chargement");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("foo.bar"));
    }

    #[test]
    fn arg_named_help_is_config_error() {
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.help]\nshort = \"h\"\n+++\nHello {{ args.help }}\n";
        let err = parse(source, vec!["x".to_string()])
            .expect_err("un argument nommé « help » doit échouer");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("help"));
    }

    #[test]
    fn arg_named_file_is_config_error() {
        // `FILE` est l'id de l'argument positionnel que `lib.rs` ajoute pour
        // les commandes acceptant un fichier en entrée (phase 3) : un
        // argument déclaré du même nom entrerait en collision (id `clap`
        // dupliqué) et doit donc être rejeté ici, au chargement.
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.FILE]\nshort = \"f\"\n+++\nHello {{ args.FILE }}\n";
        let err = parse(source, vec!["x".to_string()])
            .expect_err("un argument nommé « FILE » doit échouer");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("FILE"));
    }

    #[test]
    fn two_args_sharing_the_same_short_letter_is_config_error() {
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.alpha]\nshort = \"x\"\n\n[args.beta]\n\
                       short = \"x\"\n+++\nHello {{ args.alpha }} {{ args.beta }}\n";
        let err = parse(source, vec!["x".to_string()])
            .expect_err("deux arguments partageant la même lettre short doivent échouer");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("alpha"), "obtenu : {message}");
        assert!(message.contains("beta"), "obtenu : {message}");
        assert!(message.contains('x'), "obtenu : {message}");
    }

    #[test]
    fn prompt_referencing_undeclared_arg_is_config_error_at_parse_time() {
        let source = "+++\nmodel = \"qwen-fast\"\n+++\nHello {{ args.language }}\n";
        let err = parse(source, vec!["translate".to_string()])
            .expect_err("un placeholder args.* non déclaré doit échouer au parsing");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("language"));
    }

    #[test]
    fn declared_but_unreferenced_arg_is_not_an_error() {
        let source =
            "+++\nmodel = \"qwen-fast\"\n\n[args.unused]\nshort = \"u\"\n+++\nHello {{ input }}\n";
        let spec = parse(source, vec!["x".to_string()]).expect("should parse");

        assert_eq!(spec.args.len(), 1);
        assert!(spec.args.contains_key("unused"));
    }

    #[test]
    fn command_without_args_section_has_empty_args() {
        let source = "+++\nmodel = \"qwen-fast\"\n+++\nprompt\n";
        let spec = parse(source, vec!["x".to_string()]).expect("should parse");

        assert!(spec.args.is_empty());
    }

    #[test]
    fn existing_commands_without_args_section_still_parse_non_regression() {
        let source = "+++\ndescription = \"Classify\"\nmodel = \"qwen-fast\"\n\n[input]\n\
                       mode = \"stdin_or_file\"\n+++\nHello {{ input }}\n";
        let spec = parse(source, vec!["classify".to_string()]).expect("should parse");

        assert_eq!(spec.description, "Classify");
        assert_eq!(spec.model, "qwen-fast");
        assert!(matches!(spec.input, InputMode::StdinOrFile));
        assert_eq!(spec.prompt, "Hello {{ input }}");
        assert!(spec.args.is_empty());
    }

    #[test]
    fn discover_scopes_broken_placeholder_fully_masked_by_local_scope_resolves_successfully() {
        // Même exigence que la revue L3 (phase 2) pour un frontmatter cassé,
        // appliquée à la nouvelle classe d'échec introduite en phase 3 : un
        // placeholder {{ args.inconnu }} non déclaré est désormais détecté
        // par `parse` elle-même. Un scope général portant ce défaut mais
        // intégralement masqué par un override local valide ne doit
        // toujours jamais être ouvert ni validé.
        let general = fixture_dir("scopes-broken-placeholder-masked-general");
        let commands_dir = general.join("commands");
        std::fs::create_dir_all(&commands_dir).expect("création du dossier commands");
        std::fs::write(
            commands_dir.join("translate.md"),
            "+++\nmodel = \"qwen-general\"\n+++\nHello {{ args.inconnu }}\n",
        )
        .expect("écriture fixture");

        let local = fixture_dir("scopes-broken-placeholder-masked-local");
        write_command(&local, "translate", "qwen-local", "prompt local");

        let specs = discover_scopes(&[general, local])
            .expect("la version locale doit masquer le fichier cassé du scope général");

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].model, "qwen-local");
        assert_eq!(specs[0].prompt, "prompt local");
    }

    #[test]
    fn discover_reports_faulty_file_path_and_declared_args_on_misspelled_placeholder() {
        // Symétrique de `discover_reports_faulty_file_path_on_broken_frontmatter`
        // (phase 1) et pendant NON masqué de
        // `discover_scopes_broken_placeholder_fully_masked_by_local_scope_...` :
        // c'est le mode de défaillance visé par la règle d'architecture des
        // revues L3 (« une clé lue puis silencieusement ignorée est un
        // défaut ») appliquée à la phase 3 — un placeholder mal orthographié
        // ({{ args.langauge }} au lieu de {{ args.language }}) doit échouer
        // AU CHARGEMENT avec un message nommant le fichier fautif, l'argument
        // manquant et les arguments déclarés (règle 3 du contrat). `parse`
        // seul ne connaît pas le chemin du fichier ; c'est `discover` (via
        // `read_and_parse`) qui doit l'ajouter, sans doubler le préfixe
        // « erreur de configuration : » (même invariant que
        // `discover_error_message_does_not_double_config_error_prefix`).
        let root = fixture_dir("discover-misspelled-placeholder");
        let commands_dir = root.join("commands");
        std::fs::create_dir_all(&commands_dir).expect("création du dossier commands");
        std::fs::write(
            commands_dir.join("translate.md"),
            "+++\nmodel = \"qwen-fast\"\n\n[args.language]\nshort = \"l\"\n+++\n\
             Translate into {{ args.langauge }}.\n",
        )
        .expect("écriture fixture");

        let err = discover(&root).expect_err("un placeholder mal orthographié doit échouer");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(
            message.contains("translate.md"),
            "le message doit nommer le fichier fautif, obtenu : {message}"
        );
        assert!(
            message.contains("langauge"),
            "le message doit nommer l'argument inconnu référencé, obtenu : {message}"
        );
        assert!(
            message.contains("language"),
            "le message doit lister les arguments déclarés (dont 'language'), obtenu : {message}"
        );
        assert_eq!(
            message.matches("erreur de configuration").count(),
            1,
            "le préfixe ne doit apparaître qu'une seule fois, obtenu : {message}"
        );
    }

    // -- revue L3, correctif 2 : required = false + référencé par le prompt --

    #[test]
    fn referenced_arg_with_required_false_is_a_load_error_naming_the_file() {
        // Reproduction exacte du cas 1 de la revue L3 : `[args.tone]` avec
        // `required = false`, référencé par `{{ args.tone }}`. Doit échouer
        // AU CHARGEMENT, pas seulement au rendu — et `discover` doit nommer
        // le fichier fautif (même contrat que `discover_reports_faulty_
        // file_path_and_declared_args_on_misspelled_placeholder`).
        let root = fixture_dir("referenced-arg-optional-is-load-error");
        let commands_dir = root.join("commands");
        std::fs::create_dir_all(&commands_dir).expect("création du dossier commands");
        std::fs::write(
            commands_dir.join("optarg.md"),
            "+++\nmodel = \"qwen-fast\"\n\n[args.tone]\nrequired = false\n+++\n\
             ton {{ args.tone }} : {{ input }}\n",
        )
        .expect("écriture fixture");

        let err = discover(&root)
            .expect_err("un argument référencé mais required = false doit échouer au chargement");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(
            message.contains("optarg.md"),
            "le message doit nommer le fichier fautif, obtenu : {message}"
        );
        assert!(message.contains("tone"), "obtenu : {message}");
        assert!(
            message.contains("required = true"),
            "le message doit expliquer qu'un argument référencé doit être required = true, \
             obtenu : {message}"
        );
    }

    #[test]
    fn referenced_arg_with_required_true_loads_correctly() {
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.tone]\nrequired = true\n+++\nton {{ args.tone }} : {{ input }}\n";
        let spec = parse(source, vec!["optarg".to_string()]).expect("should parse");

        assert!(spec.args.get("tone").expect("présent").required);
    }

    #[test]
    fn declared_but_unreferenced_arg_with_required_false_stays_valid() {
        // Non-régression explicitement demandée par la revue L3 : le
        // correctif 2 ne doit rejeter que les arguments RÉFÉRENCÉS par le
        // prompt. Un argument déclaré mais non référencé reste un argument
        // CLI valide et optionnel (cf. aussi
        // `declared_but_unreferenced_arg_is_not_an_error`, qui ne fixe pas
        // `required` explicitement).
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.unused]\nrequired = false\n+++\nHello {{ input }}\n";
        let spec = parse(source, vec!["x".to_string()]).expect("should parse");

        assert!(!spec.args.get("unused").expect("présent").required);
    }

    // -- revue L3, correctif 3 : clés inconnues du frontmatter -----------------

    #[test]
    fn unknown_root_level_frontmatter_key_is_config_error() {
        let source = "+++\nmodel = \"qwen-fast\"\ndescripton = \"faute de frappe\"\n+++\nprompt\n";
        let err = parse(source, vec!["x".to_string()])
            .expect_err("une clé racine inconnue doit échouer, pas être ignorée en silence");

        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn unknown_input_section_key_is_config_error() {
        let source = "+++\nmodel = \"qwen-fast\"\n\n[input]\nmoed = \"stdin\"\n+++\nprompt\n";
        let err = parse(source, vec!["x".to_string()]).expect_err(
            "une clé inconnue sous [input] doit échouer, pas retomber sur le mode \
                          par défaut en silence",
        );

        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn output_section_is_accepted_without_failing() {
        // Nuance de la revue L3 (correctif 3) : `[output]` existe déjà dans
        // la fixture versionnée `.npu/commands/commit-message.md` (format =
        // "text", max_lines = 1) et n'est pas encore interprétée (phase 4,
        // hors périmètre de ce correctif). `deny_unknown_fields` sur
        // `Frontmatter` ne doit donc pas la faire échouer.
        let source = "+++\nmodel = \"qwen-fast\"\n\n[output]\nformat = \"text\"\nmax_lines = 1\n\
                       +++\nprompt\n";
        let spec = parse(source, vec!["x".to_string()]).expect(
            "une section [output] présente doit être acceptée sans échouer (phase 4 non \
             implémentée par ce correctif)",
        );

        assert_eq!(spec.prompt, "prompt");
    }
}
