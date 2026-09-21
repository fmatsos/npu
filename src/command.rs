//! Découverte et parsing des commandes définies sous `commands/**/*.md`.

use serde::Deserialize;

/// Mode de résolution de l'entrée d'une commande.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    #[default]
    Stdin,
    File,
    StdinOrFile,
}

/// Une commande découverte : son chemin (dérivé de l'arborescence), le modèle
/// à utiliser, le mode d'entrée et le prompt (corps du fichier).
#[derive(Debug)]
pub struct CommandSpec {
    pub path: Vec<String>,
    pub description: String,
    pub model: String,
    pub input: InputMode,
    pub prompt: String,
}

/// Frontmatter TOML brut, avant résolution des valeurs par défaut.
#[derive(Debug, Deserialize)]
struct Frontmatter {
    #[serde(default)]
    description: String,
    model: String,
    #[serde(default)]
    input: InputSection,
}

/// Section `[input]` du frontmatter.
#[derive(Debug, Deserialize, Default)]
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

    Ok(CommandSpec {
        path,
        description: frontmatter.description,
        model: frontmatter.model,
        input: frontmatter.input.mode.unwrap_or_default(),
        prompt,
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
}
