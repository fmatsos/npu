//! Contrat de sortie structurée (phase 4, npu-cli-spec.md §15).
//!
//! Pipeline (§15) : réponse brute du modèle → extraction d'une éventuelle
//! clôture Markdown (`strip_fences`, format JSON uniquement) → parsing →
//! validation JSON Schema si un schéma est déclaré → sérialisation compacte
//! → stdout. Pour le format texte : trim, puis vérification optionnelle de
//! `max_lines`.
//!
//! Distinction de code de sortie (règle 1 du contrat partagé, §23 — le
//! contrat machine dont un agent appelant dépend) : une réponse du modèle qui
//! ne respecte pas le contrat déclaré est une [`crate::Error::Output`] (code
//! 4, la configuration est valide, c'est le modèle qui a mal répondu) ; un
//! fichier de schéma introuvable, illisible ou syntaxiquement invalide est une
//! [`crate::Error::Config`] (code 2, c'est la configuration qui est cassée).
//! Une sortie invalide n'est JAMAIS réparée ni retentée ici (§15 : c'est un
//! échec d'exécution, pas quelque chose à rattraper — la reformulation/retry
//! appartient à la phase 5, hors périmètre).
//!
//! Résolution du chemin de schéma (règle 3 du contrat partagé) : `OutputSpec.
//! schema` porte un chemin DÉJÀ résolu (absolu, ou relatif au cwd) au moment
//! où il atteint ce module — la résolution relative à la racine de scope de
//! la commande (§4/§6 : `schemas/` est un dossier frère de `commands/`) est
//! la responsabilité de l'appelant (`command.rs`), pas de `output.rs`, qui ne
//! fait qu'ouvrir le chemin qu'on lui donne. Cette résolution (`command::
//! resolve_schema_path`) est PUREMENT SYNTAXIQUE (revue L3) : elle ne touche
//! jamais le disque. C'est donc CE module, dans `compile_schema`, qui
//! découvre en premier — et seulement au moment où la commande qui le
//! réclame est réellement invoquée — qu'un schéma est absent, illisible ou
//! syntaxiquement invalide, alignant l'existence sur la compilation, toutes
//! deux paresseuses.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Format de sortie déclaré par une commande (`[output].format`).
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    #[default]
    Text,
    Json,
}

/// Contrat de sortie résolu pour une commande.
///
/// Construit par l'appelant (`command.rs`) à partir du frontmatter `[output]`
/// — combinaisons interdites (règle 2 du contrat partagé : `schema` avec
/// `format = "text"`, ou `max_lines` avec `format = "json"`) déjà rejetées
/// avant que cette valeur n'existe. `finalize` ne revalide donc pas ces
/// combinaisons : elle lit uniquement le champ pertinent au `format` en
/// vigueur et ignore l'autre, ce qui reste sûr même si l'appelant ne
/// respectait pas l'invariant (aucun champ non pertinent n'est jamais lu).
#[derive(Debug, Default)]
pub struct OutputSpec {
    pub format: Format,
    pub schema: Option<PathBuf>,
    pub max_lines: Option<usize>,
}

/// Nombre maximal de caractères conservés dans l'extrait de réponse cité par
/// un message d'erreur de parsing JSON (règle 6 du contrat partagé : un
/// extrait tronqué, pas la réponse entière). Compté en caractères, pas en
/// octets, pour ne jamais couper au milieu d'un caractère multi-octets.
const EXCERPT_MAX_CHARS: usize = 200;

/// Tronque `text` à au plus [`EXCERPT_MAX_CHARS`] caractères pour un message
/// d'erreur, en ajoutant une ellipse si la réponse était plus longue.
/// Itère par `char`, jamais par indice d'octet brut : un slicing naïf sur un
/// texte accentué ou contenant un emoji paniquerait sur une frontière de
/// caractère multi-octets.
fn excerpt(text: &str) -> String {
    let mut truncated: String = text.chars().take(EXCERPT_MAX_CHARS).collect();
    if text.chars().count() > EXCERPT_MAX_CHARS {
        truncated.push('…');
    }
    truncated
}

/// Retire une clôture Markdown entourant la réponse du modèle, si présente.
///
/// Les modèles entourent très souvent leur JSON de ` ``` ` ou de ` ```json `
/// (règle 5 du contrat partagé). Cette fonction retire au plus UNE clôture
/// ouvrante en tête et sa fermante en queue, avec une étiquette de langue
/// optionnelle sur la ligne d'ouverture, en tolérant les espaces/retours à la
/// ligne autour de l'ensemble (`str::trim` avant analyse). Elle ne touche à
/// rien :
/// - si le texte (une fois les espaces de tête/queue ignorés) ne commence pas
///   par ` ``` ` ;
/// - si la clôture d'ouverture n'a pas de fermante correspondante en fin de
///   texte (« clôture ouvrante sans fermante ») ;
/// - au milieu du texte : seules la première clôture (début) et la dernière
///   (fin) sont considérées, jamais une clôture interne, qui est du contenu.
///
/// Fonction pure et sans allocation : `raw.trim()` ne copie rien (il renvoie
/// une sous-tranche de `raw`), et toutes les découpes ultérieures portent sur
/// cette même tranche — le `&str` renvoyé est donc toujours emprunté à
/// `raw`, jamais une `String` neuve. Chaque point de découpe est ancré sur un
/// marqueur ASCII (` ``` `, `\n`) trouvé via `str::find`/`str::ends_with`,
/// qui renvoient toujours une frontière de caractère valide : aucun risque de
/// panique sur un contenu accentué ou contenant un emoji, où qu'il se trouve
/// dans le texte.
#[must_use]
pub fn strip_fences(raw: &str) -> &str {
    const FENCE: &str = "```";

    let s = raw.trim();
    if !s.starts_with(FENCE) {
        return raw;
    }

    // Ligne d'ouverture : `````` (étiquette de langue optionnelle) jusqu'au
    // premier retour à la ligne. Sans retour à la ligne, il n'y a pas de
    // corps distinct de la clôture d'ouverture elle-même : rien à retirer.
    let after_open = &s[FENCE.len()..];
    let Some(newline_offset) = after_open.find('\n') else {
        return raw;
    };
    let body_start = FENCE.len() + newline_offset + 1;

    if !s.ends_with(FENCE) || s.len() < body_start + FENCE.len() {
        return raw;
    }
    let close_start = s.len() - FENCE.len();

    // La clôture fermante doit être sur sa propre ligne : soit elle suit
    // immédiatement la ligne d'ouverture (corps vide, `body_start ==
    // close_start`), soit le caractère qui la précède est un `\n`. Sans cette
    // vérification, un texte se terminant par ``` littéral au milieu d'une
    // ligne de contenu serait pris pour une clôture.
    if close_start > body_start && !s[..close_start].ends_with('\n') {
        return raw;
    }

    let body_end = if close_start > body_start {
        close_start - 1 // exclut le `\n` qui précède la clôture fermante
    } else {
        close_start
    };

    s[body_start..body_end].trim()
}

/// Applique le contrat de sortie `spec` à la réponse brute `raw` du modèle et
/// renvoie le texte EXACT à écrire sur stdout, sans retour à la ligne final
/// (l'appelant l'ajoute — cf. `lib.rs`, `println!("{output}")`).
///
/// `command_file` est le chemin du fichier de commande (`CommandSpec.file`,
/// cf. `command.rs`) qui a produit `spec` — utilisé UNIQUEMENT pour nommer
/// la commande fautive dans un message d'erreur si le schéma qu'elle réclame
/// (branche JSON) s'avère absent, illisible ou syntaxiquement invalide au
/// moment de cette invocation (`compile_schema`, plus bas). Ignoré par la
/// branche texte, qui ne connaît pas de schéma.
pub fn finalize(spec: &OutputSpec, raw: &str, command_file: &Path) -> crate::Result<String> {
    match spec.format {
        Format::Text => finalize_text(spec.max_lines, raw),
        Format::Json => finalize_json(spec.schema.as_deref(), raw, command_file),
    }
}

/// Applique le contrat `format = "text"` (règle 7 du contrat partagé) :
/// aucune clôture retirée, aucun parsing. La réponse est trimée des espaces
/// en tête et en queue. Si `max_lines` est déclaré et que la réponse compte
/// plus de lignes NON VIDES (après trim) que cette limite, échec —
/// `Error::Output` indiquant le nombre attendu et le nombre reçu, jamais une
/// troncature silencieuse (§15 : échec, pas réparation). Les lignes vides
/// (uniquement des espaces, ou totalement vides) ne comptent pas dans le
/// total comparé à la limite, mais restent dans le texte renvoyé : seul le
/// trim global (tête/queue) modifie la réponse elle-même.
fn finalize_text(max_lines: Option<usize>, raw: &str) -> crate::Result<String> {
    let trimmed = raw.trim();

    if let Some(limit) = max_lines {
        let non_empty_lines = trimmed
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        if non_empty_lines > limit {
            return Err(crate::Error::Output(format!(
                "sortie texte : au plus {limit} ligne(s) non vide(s) attendue(s) (max_lines), \
                 {non_empty_lines} reçue(s)"
            )));
        }
    }

    Ok(trimmed.to_string())
}

/// Applique le contrat `format = "json"` (règles 5/6 du contrat partagé) :
/// extraction d'une éventuelle clôture Markdown ([`strip_fences`]), parsing
/// (`serde_json` — échec => `Error::Output` citant l'erreur de parsing et un
/// extrait tronqué de la réponse), puis validation contre le schéma s'il y en
/// a un (échec => `Error::Output` listant CHAQUE violation, jamais seulement
/// la première). La valeur renvoyée sur stdout est la sérialisation COMPACTE
/// de la valeur parsée, pour que stdout reste du JSON valide quel que soit
/// l'emballage que le modèle a mis autour (§22 : `npu classify | jq .`).
fn finalize_json(schema: Option<&Path>, raw: &str, command_file: &Path) -> crate::Result<String> {
    let candidate = strip_fences(raw);

    let value: serde_json::Value = serde_json::from_str(candidate).map_err(|err| {
        crate::Error::Output(format!(
            "sortie JSON invalide : {err} ; réponse reçue (extrait) : « {} »",
            excerpt(candidate.trim())
        ))
    })?;

    if let Some(schema_path) = schema {
        // PARESSEUX PAR CONCEPTION (règle 4 du contrat partagé) : ce schéma
        // n'est compilé que parce que la commande réellement invoquée le
        // déclare, jamais au chargement de la configuration ni pour les
        // schémas d'autres commandes du même scope. Même leçon que la revue
        // L3 de la phase 2 (un backend/modèle cassé masqué par un scope plus
        // local n'est jamais lu) : un schéma cassé appartenant à une commande
        // que personne n'invoque ne doit pas rendre le CLI inutilisable. La
        // vérification exhaustive de tous les schémas est le travail de
        // `npu doctor` (phase 5, hors périmètre ici). Depuis la revue L3 de
        // CETTE phase, l'EXISTENCE du schéma est paresseuse au même titre que
        // sa compilation (cf. doc de `command::resolve_schema_path`) : c'est
        // ICI, et seulement ici, que `compile_schema` peut découvrir un
        // fichier absent, illisible ou syntaxiquement invalide.
        let validator = compile_schema(schema_path, command_file)?;
        validate_against_schema(&validator, &value)?;
    }

    serde_json::to_string(&value).map_err(|err| {
        crate::Error::Output(format!("échec de sérialisation de la sortie JSON : {err}"))
    })
}

/// Compile le schéma JSON situé à `path` en un validateur réutilisable.
///
/// Un fichier introuvable ou illisible, ou un JSON Schema syntaxiquement
/// invalide, est une `Error::Config` : la CONFIGURATION est cassée, pas la
/// réponse du modèle (règle 1 du contrat partagé). `path` est déjà résolu
/// par l'appelant (cf. doc de module) : ouvert tel quel, relatif au cwd du
/// processus s'il n'est pas absolu — comme le ferait n'importe quel autre
/// fichier lu par ce crate (`std::fs::read_to_string`).
///
/// `command_file` (revue L3, correctif 1) est le fichier de commande qui a
/// déclaré ce schéma (`CommandSpec.file`, cf. `command.rs`) : nommé dans
/// chacun des trois messages d'erreur ci-dessous, EN PLUS du chemin résolu
/// du schéma. Depuis que `command::resolve_schema_path` ne vérifie plus rien
/// au disque, c'est cette fonction, et seulement elle, qui découvre un
/// schéma absent, illisible ou cassé — au moment de l'exécution réelle de la
/// commande qui le réclame, jamais avant.
pub(crate) fn compile_schema(
    path: &Path,
    command_file: &Path,
) -> crate::Result<jsonschema::Validator> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        crate::Error::Config(format!(
            "schéma de sortie « {} », déclaré par le fichier de commande « {} », introuvable \
             ou illisible : {err}",
            path.display(),
            command_file.display()
        ))
    })?;

    let document: serde_json::Value = serde_json::from_str(&text).map_err(|err| {
        crate::Error::Config(format!(
            "schéma de sortie « {} », déclaré par le fichier de commande « {} » : JSON \
             invalide : {err}",
            path.display(),
            command_file.display()
        ))
    })?;

    jsonschema::validator_for(&document).map_err(|err| {
        crate::Error::Config(format!(
            "schéma de sortie « {} », déclaré par le fichier de commande « {} », invalide : \
             {err}",
            path.display(),
            command_file.display()
        ))
    })
}

/// Valide `value` contre `validator` et renvoie une `Error::Output` listant
/// CHAQUE violation (chemin JSON fautif + raison), jamais seulement la
/// première (règle 6 du contrat partagé) : un utilisateur doit pouvoir
/// corriger son prompt en une seule passe plutôt que de relancer la commande
/// à chaque violation découverte. Même style actionnable que
/// `config::validate_backend`/`error::format_available` : chemin entre
/// guillemets français, message d'erreur nommé.
fn validate_against_schema(
    validator: &jsonschema::Validator,
    value: &serde_json::Value,
) -> crate::Result<()> {
    let violations: Vec<String> = validator
        .iter_errors(value)
        .map(|error| {
            let path = error.instance_path();
            if path.is_empty() {
                format!("- (racine) : {error}")
            } else {
                format!("- {path} : {error}")
            }
        })
        .collect();

    if violations.is_empty() {
        return Ok(());
    }

    Err(crate::Error::Output(format!(
        "sortie JSON invalide au regard du schéma ({} violation(s)) :\n{}",
        violations.len(),
        violations.join("\n")
    )))
}

#[cfg(test)]
#[allow(clippy::expect_used)] // toléré dans les tests (cf. Cargo.toml [lints.clippy]).
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Crée un fichier de fixture unique sous `target/`, même idiome que
    /// `command::tests::fixture_dir` : pas de pollution du dépôt, pas de
    /// collision entre tests exécutés en parallèle.
    fn fixture_file(name: &str, contents: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join("output");
        std::fs::create_dir_all(&dir).expect("création du dossier de fixture");
        let file = dir.join(format!("{name}-{n}.json"));
        std::fs::write(&file, contents).expect("écriture fixture");
        file
    }

    /// Fichier de commande factice passé à `finalize` par les tests de ce
    /// module : sa seule contrainte est d'être un chemin stable, jamais lu
    /// ni ouvert par `finalize`/`finalize_json` elles-mêmes (seul
    /// `compile_schema`, sur la branche schéma cassé/absent, l'utilise — et
    /// uniquement pour le CITER dans le message d'erreur, jamais pour
    /// l'ouvrir).
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
        let text = "Voici le résultat : ``` pas une clôture d'enveloppe ``` fin.";
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
        let text = "Résumé : café à Montréal 🎉, sans clôture du tout.";
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
        let out = finalize(&spec, "  \n  bonjour le monde  \n\n", &test_command_file())
            .expect("doit réussir");
        assert_eq!(out, "bonjour le monde");
    }

    #[test]
    fn text_max_lines_respected() {
        let spec = OutputSpec {
            format: Format::Text,
            schema: None,
            max_lines: Some(2),
        };
        let out = finalize(&spec, "ligne 1\nligne 2", &test_command_file()).expect("doit réussir");
        assert_eq!(out, "ligne 1\nligne 2");
    }

    #[test]
    fn text_max_lines_exceeded_is_output_error() {
        let spec = OutputSpec {
            format: Format::Text,
            schema: None,
            max_lines: Some(1),
        };
        let err =
            finalize(&spec, "ligne 1\nligne 2", &test_command_file()).expect_err("doit échouer");
        assert!(matches!(err, crate::Error::Output(_)));
    }

    #[test]
    fn text_blank_lines_do_not_count_towards_max_lines() {
        let spec = OutputSpec {
            format: Format::Text,
            schema: None,
            max_lines: Some(2),
        };
        // 2 lignes non vides, 2 lignes vides (dont une avec seulement des
        // espaces) : ne doit pas dépasser max_lines = 2.
        let out = finalize(&spec, "ligne 1\n\nligne 2\n   \n", &test_command_file())
            .expect("doit réussir");
        assert_eq!(out, "ligne 1\n\nligne 2");
    }

    #[test]
    fn text_without_max_lines_has_no_limit() {
        let spec = OutputSpec {
            format: Format::Text,
            schema: None,
            max_lines: None,
        };
        let out =
            finalize(&spec, "l1\nl2\nl3\nl4\nl5", &test_command_file()).expect("doit réussir");
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
            finalize(&spec, "{\n  \"a\": 1\n}\n", &test_command_file()).expect("doit réussir");
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
        .expect("doit réussir");
        assert_eq!(out, "{\"a\":1,\"b\":2}");
    }

    #[test]
    fn json_invalid_is_output_error() {
        let spec = OutputSpec {
            format: Format::Json,
            schema: None,
            max_lines: None,
        };
        let err =
            finalize(&spec, "pas du JSON du tout", &test_command_file()).expect_err("doit échouer");
        assert!(matches!(err, crate::Error::Output(_)));
    }

    #[test]
    fn json_invalid_error_excerpt_truncates_long_multibyte_response_without_panicking() {
        // La branche de troncature d'`excerpt` (`chars().count() >
        // EXCERPT_MAX_CHARS`) n'était exercée par aucun test : le seul cas
        // existant (`json_invalid_is_output_error`) est une chaîne ASCII de
        // vingt caractères, bien en deçà de `EXCERPT_MAX_CHARS` (200). Ce
        // test force la troncature avec une réponse accentuée/emoji de plus
        // de 200 CARACTÈRES (mais bien plus de 200 OCTETS, chaque « é »
        // pesant deux octets et l'emoji quatre) : un slicing naïf sur un
        // indice d'octet paniquerait ici, alors qu'`excerpt` itère par
        // `char` (cf. sa doc). La réponse entière n'est délibérément pas du
        // JSON valide, pour emprunter le chemin d'erreur qui appelle
        // `excerpt`.
        let spec = OutputSpec {
            format: Format::Json,
            schema: None,
            max_lines: None,
        };
        // Des emojis (4 octets chacun) plutôt que des « é » (2 octets) :
        // avec un pas de 2 octets, une régression qui slicerait par indice
        // d'octet a une chance sur deux de retomber quand même sur une
        // frontière valide au décalage `EXCERPT_MAX_CHARS` précis et de ne
        // PAS paniquer malgré le bogue (vérifié empiriquement en mutant
        // `excerpt` pendant l'écriture de ce test) ; un pas de 4 octets
        // rend cette coïncidence bien moins probable.
        let long_response = format!("pas du JSON : {}", "🎉".repeat(250));
        assert!(
            long_response.chars().count() > EXCERPT_MAX_CHARS,
            "la fixture doit dépasser EXCERPT_MAX_CHARS pour exercer la troncature"
        );

        let err = finalize(&spec, &long_response, &test_command_file())
            .expect_err("doit échouer, ce n'est pas du JSON");
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
        .expect("doit réussir");
        assert!(
            !out.contains('\n'),
            "la sortie compacte ne doit pas contenir de retour à la ligne, obtenu : {out}"
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
        .expect("doit réussir contre un schéma satisfait");
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
        // « confidence » manquant (required) ET « category » du mauvais type :
        // deux violations distinctes.
        let err =
            finalize(&spec, "{\"category\": 42}", &test_command_file()).expect_err("doit échouer");
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
        let err = finalize(&spec, "{\"a\": 1}", &test_command_file()).expect_err("doit échouer");
        assert!(
            matches!(err, crate::Error::Config(_)),
            "un schéma introuvable est une erreur de CONFIGURATION, pas de sortie : {err:?}"
        );
    }

    #[test]
    fn schema_file_invalid_json_is_config_error() {
        let schema_path = fixture_file("broken-schema", "pas du JSON");
        let spec = OutputSpec {
            format: Format::Json,
            schema: Some(schema_path),
            max_lines: None,
        };
        let err = finalize(&spec, "{\"a\": 1}", &test_command_file()).expect_err("doit échouer");
        assert!(
            matches!(err, crate::Error::Config(_)),
            "un schéma syntaxiquement invalide est une erreur de CONFIGURATION : {err:?}"
        );
    }

    // -- code de sortie ---------------------------------------------------------

    #[test]
    fn output_error_exit_code_is_four() {
        let spec = OutputSpec {
            format: Format::Text,
            schema: None,
            max_lines: Some(0),
        };
        let err = finalize(&spec, "une ligne", &test_command_file()).expect_err("doit échouer");
        assert!(matches!(err, crate::Error::Output(_)));
        assert_eq!(err.exit_code(), 4);
    }
}
