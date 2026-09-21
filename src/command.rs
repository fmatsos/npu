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

/// Parcourt `<root>/commands/**/*.md`.
///
/// Renvoie un `Vec` vide si le dossier `commands/` n'existe pas.
pub fn discover(root: &std::path::Path) -> crate::Result<Vec<CommandSpec>> {
    let commands_root = root.join("commands");
    if !commands_root.is_dir() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    collect_markdown_files(&commands_root, &mut files)?;

    let mut specs = Vec::with_capacity(files.len());
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

        let source = std::fs::read_to_string(&file)?;
        specs.push(parse(&source, path)?);
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
}
