//! Interpolation du prompt (phase 3, npu-cli-spec.md §11/§12).
//!
//! Placeholders reconnus : `{{ input }}`, `{{ args.<nom> }}`, `{{ env.NOM }}`.
//! Un placeholder FERMÉ (`{{ ... }}`) dont le nom n'est aucune de ces trois
//! formes est une erreur de configuration, jamais recopié tel quel : un
//! `{{ args.langauge }}` mal orthographié doit échouer bruyamment plutôt que
//! d'être envoyé au modèle comme texte littéral (cf. revue L3 des phases 1 et
//! 2 : une clé lue puis ignorée est un défaut). Un `{{` jamais refermé reste
//! recopié tel quel, comme en phase 1 : on ne peut pas distinguer une
//! intention d'une faute de frappe (§12, §25 — pas d'heuristique).
//!
//! Les trois fonctions publiques ([`placeholders`], [`validate`], [`render`])
//! partagent un seul scanner (`scan`) plutôt que de dupliquer la boucle
//! `find("{{")` / `find("}}")` trois fois.

use std::collections::{BTreeMap, BTreeSet};

/// Ce à quoi un placeholder `{{ ... }}` peut se référer.
#[derive(Debug, PartialEq, Eq)]
pub enum Placeholder {
    Input,
    Arg(String),
    Env(String),
}

/// Formes de placeholder reconnues, pour les messages d'erreur.
const ACCEPTED_FORMS: &str = "\"input\", \"args.<nom>\" ou \"env.<NOM>\"";

/// Un fragment de template après un premier passage de scan.
///
/// `Placeholder` porte le contenu BRUT (non tronqué) entre `{{` et `}}` ;
/// son interprétation (nom reconnu ou non) est déléguée à `parse_placeholder`
/// pour que `placeholders`/`validate` (qui n'ont besoin que du nom) et
/// `render` (qui doit aussi recopier le texte littéral autour) partagent la
/// même passe.
enum Token<'a> {
    Literal(&'a str),
    Placeholder(&'a str),
    /// `{{` sans `}}` correspondant : le reste du template, à recopier tel
    /// quel avec le `{{` remis devant (cf. doc de module).
    Unclosed(&'a str),
}

/// Découpe `template` en fragments littéraux et en contenus bruts de
/// placeholders fermés. Généralise la boucle `find("{{")` / `find("}}")`
/// utilisée par les trois fonctions publiques du module.
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

/// Caractères acceptés dans un nom d'argument (`args.<nom>`) ou de variable
/// d'environnement (`env.<NOM>`), après trim et retrait du préfixe : ASCII
/// alphanumérique, `_` ou `-`.
///
/// Ce sont exactement les caractères qu'une clé TOML nue (`[args.foo-bar]`,
/// seule forme que `serde`/`toml` acceptent sans guillemets) et un nom de
/// variable d'environnement usuel peuvent tous deux porter sans ambiguïté.
/// Tout le reste (espace, point, accolade, guillemet, ...) est soit un
/// séparateur du gabarit, soit le signe d'une faute de frappe qu'on préfère
/// rejeter au chargement plutôt que d'accepter silencieusement.
///
/// `pub(crate)` : `command::validate_arg_name` réutilise EXACTEMENT cette
/// règle pour la clé `[args.<nom>]` elle-même, plutôt que d'en dupliquer une
/// divergente. TOML autorise une clé de table entre guillemets
/// (`[args."café"]`, `[args."foo.bar"]`) sur des caractères que ce module
/// n'accepte jamais dans un placeholder `{{ args.<nom> }}` : sans ce
/// partage, un tel argument chargeait silencieusement (nom jamais référencé
/// dans le prompt) ou échouait avec un message pointant sur le placeholder
/// plutôt que sur la déclaration fautive — exactement le défaut visé par la
/// règle d'architecture des revues L3 (« une clé lue puis silencieusement
/// ignorée est un défaut »), déplacé de la clé de placeholder vers le nom
/// d'argument déclaré.
pub(crate) fn is_valid_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Valide et renvoie le nom après un préfixe `args.`/`env.` : non vide, sans
/// espace, caractères acceptés uniquement. `{{ args. }}` (nom vide) est donc
/// rejeté ici, pas seulement une chaîne inconnue.
fn parse_named(rest: &str) -> Option<String> {
    if rest.is_empty() || !rest.chars().all(is_valid_name_char) {
        return None;
    }
    Some(rest.to_string())
}

/// Construit l'erreur de configuration pour un placeholder dont le nom (une
/// fois trimmé) ne correspond à aucune forme reconnue. `raw` est le contenu
/// brut, non trimmé, tel que trouvé entre `{{` et `}}`.
fn unknown_placeholder(raw: &str) -> crate::Error {
    let name = raw.trim();
    crate::Error::Config(format!(
        "placeholder inconnu « {{{{ {name} }}}} » : formes reconnues : {ACCEPTED_FORMS}"
    ))
}

/// Interprète le contenu brut d'un placeholder fermé (`raw`, entre `{{` et
/// `}}`, délimiteurs exclus) en `Placeholder`, ou renvoie l'erreur décrivant
/// le placeholder fautif et les formes acceptées.
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

/// Analyse `template` et renvoie les placeholders rencontrés, dans l'ordre.
///
/// Erreur si un placeholder fermé (`{{ ... }}`) porte un nom non reconnu (ni
/// `input`, ni `args.<nom>`, ni `env.<NOM>`). Un `{{` jamais refermé n'est
/// pas un placeholder : il est ignoré ici comme il l'est par `render`.
pub fn placeholders(template: &str) -> crate::Result<Vec<Placeholder>> {
    scan(template)
        .into_iter()
        .filter_map(|token| match token {
            Token::Placeholder(raw) => Some(parse_placeholder(raw)),
            Token::Literal(_) | Token::Unclosed(_) => None,
        })
        .collect()
}

/// Vérifie statiquement qu'un template ne référence que `input`, un argument
/// DÉCLARÉ (présent dans `declared_args`), ou `env.X` (n'importe quel nom
/// syntaxiquement valide : la PRÉSENCE d'une variable d'environnement n'est
/// vérifiée qu'au rendu, jamais ici). Appelée au chargement de la commande.
///
/// Un argument déclaré mais jamais référencé dans le prompt n'est pas une
/// erreur : il reste un argument CLI valide et documenté (npu-cli-spec.md
/// §11), simplement inutilisé par ce prompt-ci.
pub fn validate(template: &str, declared_args: &BTreeSet<String>) -> crate::Result<()> {
    for placeholder in placeholders(template)? {
        if let Placeholder::Arg(name) = placeholder
            && !declared_args.contains(&name)
        {
            return Err(crate::Error::Config(format!(
                "argument inconnu « {name} » référencé par {{{{ args.{name} }}}} : \
                 arguments déclarés : {}",
                crate::error::format_available(declared_args.iter())
            )));
        }
    }
    Ok(())
}

/// Résout la valeur d'un argument référencé par `{{ args.NOM }}`, ou l'erreur
/// de configuration nommant l'argument. Factorisé entre [`render`] et
/// [`preflight`] : les deux doivent produire EXACTEMENT le même message pour
/// le même défaut (défense en profondeur, cf. doc de [`preflight`]).
fn resolve_arg<'a>(name: &str, args: &'a BTreeMap<String, String>) -> crate::Result<&'a String> {
    args.get(name).ok_or_else(|| {
        crate::Error::Config(format!(
            "argument « {name} » référencé par {{{{ args.{name} }}}} mais absent des valeurs \
             fournies"
        ))
    })
}

/// Résout la valeur d'une variable d'environnement référencée par
/// `{{ env.NOM }}`, ou l'erreur de configuration nommant la variable. Voir
/// [`resolve_arg`] pour la raison du partage avec [`render`]/[`preflight`].
fn resolve_env(name: &str, env: &dyn Fn(&str) -> Option<String>) -> crate::Result<String> {
    env(name).ok_or_else(|| {
        crate::Error::Config(format!(
            "variable d'environnement « {name} » référencée par {{{{ env.{name} }}}} mais non \
             définie"
        ))
    })
}

/// Vérifie, AVANT toute lecture de l'entrée, que tout ce que le prompt
/// référence et qui est connaissable SANS l'entrée (un argument déclaré
/// `{{ args.NOM }}`, une variable d'environnement `{{ env.NOM }}`) est bien
/// disponible.
///
/// INVARIANT (revue L3, correctif 1) : rien de ce qui est connaissable sans
/// l'entrée ne doit être vérifié après avoir lu l'entrée. `input::resolve`
/// peut drainer un flux non rejouable (un pipe, une commande one-shot,
/// npu-cli-spec.md §22) : si un argument optionnel absent ou une variable
/// d'environnement non définie n'échouent qu'au rendu, APRÈS cette lecture,
/// le travail déjà produit en amont du pipe est perdu, et sur un flux non
/// rejouable il l'est définitivement. L'appelant (`lib.rs::run`) doit donc
/// appeler `preflight` avant `input::resolve`, jamais après.
///
/// `{{ input }}` lui-même n'est PAS vérifié ici : par construction, sa
/// valeur ne peut être connue qu'après avoir lu l'entrée — ce n'est
/// justement pas quelque chose de « connaissable sans l'entrée ».
///
/// Que [`render`] revérifie ensuite la même présence n'est pas une
/// duplication à supprimer mais de la défense en profondeur : `preflight`
/// garantit seulement qu'aucune vérification NE SE PRODUIT après la lecture
/// de l'entrée, pas que son résultat reste valable jusqu'au rendu (une
/// variable d'environnement pourrait en théorie disparaître entre les deux
/// appels, bien qu'aucun code de ce process ne la modifie).
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

/// Rend le template : substitue chaque placeholder reconnu par sa valeur.
///
/// `env` est injectée par l'appelant (plutôt que `std::env::var` appelé ici)
/// pour rester testable sans muter le vrai environnement — `unsafe` en
/// édition 2024, interdit par `unsafe_code = "forbid"`.
///
/// - `{{ input }}` → `input`.
/// - `{{ args.NOM }}` → `args[NOM]`. Absent de la map : `Error::Config`
///   nommant l'argument (ne devrait pas arriver si `validate` a tourné et si
///   l'appelant câble correctement les arguments CLI vers cette map, mais on
///   ne panique pas pour autant).
/// - `{{ env.NOM }}` → `env(NOM)`. `None` (variable non définie) :
///   `Error::Config` nommant la variable. `Some(String::new())` (variable
///   définie mais vide) : substituée par une chaîne vide, pas une erreur.
///
/// La substitution ne se réapplique jamais à son propre résultat : le
/// template est entièrement scanné AVANT toute substitution (`scan`), donc
/// une valeur d'argument contenant littéralement `{{ input }}` n'est jamais
/// réinterprétée.
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
#[allow(clippy::expect_used)] // toléré dans les tests (cf. Cargo.toml [lints.clippy]).
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
        let message = err.to_string();
        assert!(message.contains("foo"), "obtenu : {message}");
        assert!(message.contains("args."), "obtenu : {message}");
        assert!(message.contains("env."), "obtenu : {message}");
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
        let message = err.to_string();
        assert!(message.contains("language"), "obtenu : {message}");
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
        assert!(err.to_string().contains("API_KEY"));
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
        assert!(err.to_string().contains("language"));
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
        // `scan` ne tranche `template` qu'aux positions renvoyées par
        // `str::find("{{"/"}}")`, donc toujours sur une frontière de
        // caractère valide (garanti par `str::find`, jamais un décompte
        // d'octets manuel) — mais la revue L3 des phases 1/2 demande un test
        // explicite plutôt qu'un raisonnement implicite. Ce gabarit place des
        // caractères multi-octets (accents, emoji) immédiatement collés aux
        // délimiteurs `{{`/`}}`, sans espace, le cas le plus susceptible de
        // heurter une frontière de caractère si le découpage était fait par
        // décompte d'octets plutôt que via `find`.
        let mut args = BTreeMap::new();
        args.insert("langue".to_string(), "français".to_string());
        let env = |_: &str| None;

        let rendered = render(
            "Préparé{{ input }} : {{ args.langue }}🎉 café",
            "☕é",
            &args,
            &env,
        )
        .expect("un template accentué/emoji ne doit jamais paniquer");

        assert_eq!(rendered, "Préparé☕é : français🎉 café");
    }

    #[test]
    fn scan_unclosed_brace_after_multibyte_text_is_preserved_literally_without_panicking() {
        let args = BTreeMap::new();
        let env = |_: &str| None;

        let rendered = render("café 🎉 {{ input non refermé", "x", &args, &env)
            .expect("un {{ non refermé après du multi-octet ne doit pas paniquer");

        assert_eq!(rendered, "café 🎉 {{ input non refermé");
    }

    #[test]
    fn render_unknown_placeholder_name_is_config_error() {
        let args = BTreeMap::new();
        let env = |_: &str| None;

        let err = render("{{ foo }}", "x", &args, &env).expect_err("must fail");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("foo"));
    }

    // -- preflight() (revue L3, correctif 1) -----------------------------------

    #[test]
    fn preflight_detects_missing_env_var_without_needing_input() {
        // C'est la vérification demandée explicitement par la revue L3 :
        // au niveau de la fonction de préflight elle-même, sans passer par
        // le processus complet (dont la preuve est la mesure de temps sur le
        // binaire réel, cf. rapport). Une variable d'environnement absente
        // doit être détectée SANS qu'aucune entrée n'ait besoin d'exister.
        let args = BTreeMap::new();
        let env = |_: &str| None;

        let err = preflight("{{ env.NPU_ABSENTE }} : {{ input }}", &args, &env)
            .expect_err("une variable d'environnement absente doit échouer en préflight");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("NPU_ABSENTE"));
    }

    #[test]
    fn preflight_detects_missing_arg_without_needing_input() {
        let args = BTreeMap::new();
        let env = |_: &str| None;

        let err = preflight("ton {{ args.tone }} : {{ input }}", &args, &env)
            .expect_err("un argument absent doit échouer en préflight");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("tone"));
    }

    #[test]
    fn preflight_does_not_require_input_placeholder_to_be_resolved() {
        // `{{ input }}` n'est vérifiable qu'APRÈS lecture de l'entrée : par
        // construction, `preflight` ne doit jamais échouer à cause de lui
        // seul.
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
        // Défense en profondeur (cf. doc de `preflight`) : les deux
        // fonctions partagent `resolve_env`, donc le même message.
        let args = BTreeMap::new();
        let env = |_: &str| None;

        let preflight_err =
            preflight("{{ env.API_KEY }}", &args, &env).expect_err("preflight doit échouer");
        let render_err = render("{{ env.API_KEY }}", "x", &args, &env)
            .expect_err("render doit échouer de la même façon");

        assert_eq!(preflight_err.to_string(), render_err.to_string());
    }
}
