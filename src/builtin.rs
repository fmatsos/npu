//! Built-ins du CLI : `doctor`, `models`, `describe` (phase 5,
//! npu-cli-spec.md §16, IMPLEMENTATION.md phase 5) et la sonde de
//! joignabilité TCP qu'ils partagent.
//!
//! Ce module ne fait JAMAIS d'écriture console lui-même : [`doctor`] renvoie
//! un rapport ([`Check`]) que l'appelant (`lib.rs::run`) formate avec
//! [`format_doctor`] avant de l'écrire sur stdout (règle du contrat : stdout
//! est réservé au RÉSULTAT d'un built-in, qui EST son rapport). C'est ce qui
//! rend [`doctor`] entièrement testable sans toucher au disque au-delà de la
//! vérification (e) ni au réseau : la sonde de joignabilité (`probe`) est
//! injectée par l'appelant plutôt qu'appelée en dur, exactement comme
//! `prompt::render`/`prompt::preflight` injectent leur résolveur de variable
//! d'environnement (cf. `lib.rs::run`).
//!
//! **Omission volontaire : « NPU available ».** L'exemple de rapport
//! `doctor` de la spec (§16) affiche cette ligne. Ce CLI est délibérément
//! agnostique du runtime d'inférence (npu-cli-spec.md §2 : « Backend-agnostic
//! architecture » ; §26 : le core ne connaît que des opérations backend
//! nommées, jamais un NPU au sens matériel) : il n'a donc AUCUN moyen de
//! vérifier la présence ou la disponibilité d'un NPU, contrairement à la
//! joignabilité d'un backend (une connexion TCP) ou à la validité d'un
//! schéma (une lecture de fichier disque). Afficher une coche pour une
//! vérification qu'on n'a pas réellement faite serait exactement le défaut
//! que la règle d'architecture des revues L3 interdit (« un rapport qui ment
//! est pire que pas de rapport ») : [`doctor`] ne produit donc JAMAIS de
//! [`Check`] pour cette ligne. Ce n'est pas un oubli.

use std::net::ToSocketAddrs;
use std::time::Duration;

use serde::Serialize;

/// Issue d'une vérification de [`doctor`].
#[derive(Debug)]
pub enum Status {
    /// La vérification a réussi.
    Ok,
    /// La vérification a échoué, avec un message actionnable.
    Failed(String),
}

/// Catégorie d'une [`Check`], au sens du code de sortie de [`doctor_exit_code`]
/// (point 5 du contrat partagé, §23 de la spec) : c'est cette valeur, et
/// JAMAIS le texte de [`Check::label`], qui distingue un échec de
/// configuration (« corrige tes fichiers ») d'un échec de joignabilité
/// (« démarre ton runtime ») pour un agent appelant. Un libellé est de
/// l'affichage — il peut être reformulé, traduit, ou recevoir un nouveau
/// suffixe sans préavis ; la catégorie est un contrat machine et doit y
/// survivre.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    /// Vérifications (a), (c), (d), (e) : configuration, modèles, commandes.
    Config,
    /// Vérification (b) : joignabilité TCP d'un backend.
    Reachability,
}

/// Une ligne du rapport de [`doctor`].
#[derive(Debug)]
pub struct Check {
    /// Catégorie de cette vérification, utilisée par [`doctor_exit_code`].
    pub kind: CheckKind,
    /// Ce qui a été vérifié (ex. « backend « ovms » joignable »).
    pub label: String,
    /// Le résultat de cette vérification.
    pub status: Status,
}

/// Les trois noms de built-ins exposés par ce CLI, plus `help`, réservé par
/// `clap` lui-même (chaque `clap::Command` reçoit un flag `-h`/`--help`
/// automatique). `command.rs` (`reject_reserved_path`) rejette au chargement
/// tout fichier de commande dont le PREMIER segment de chemin coïncide avec
/// l'une de ces valeurs (phase 5, point 2 du contrat partagé) : sans ce
/// rejet, `commands/doctor.md` serait silencieusement masqué par (ou
/// masquerait) le built-in `doctor` construit dans `lib.rs`.
pub const RESERVED: &[&str] = &["doctor", "models", "describe", "help"];

/// Délai maximal accordé à [`tcp_probe`] avant de considérer un backend
/// injoignable. Court par construction (point 4 du contrat partagé) :
/// `doctor` est une commande de diagnostic censée rester rapide même quand
/// plusieurs backends sont interrogés, jamais destinée à attendre un délai
/// d'expiration réseau complet.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Vérification (a) : la configuration a-t-elle été chargée avec succès ?
///
/// `load_error` porte l'erreur CONSERVÉE par le mode dégradé de `lib.rs::run`
/// (décision 1 du contrat partagé) plutôt que propagée immédiatement : c'est
/// précisément ce qui permet à `doctor` de tourner malgré une configuration
/// cassée. Le message de l'échec est celui de `crate::Error` lui-même (déjà
/// préfixé « erreur de configuration : », cf. `error.rs`), jamais reconstruit
/// à la main.
fn check_config_loaded(load_error: Option<&crate::Error>) -> Check {
    let status = match load_error {
        Some(err) => Status::Failed(err.to_string()),
        None => Status::Ok,
    };
    Check {
        kind: CheckKind::Config,
        label: "configuration chargée".to_string(),
        status,
    }
}

/// Vérification (b) : chaque backend configuré est-il joignable ? Itère sur
/// `config.backends` triés par identifiant (`BTreeMap`/`HashMap` non
/// ordonnée sinon), pour que l'ordre du rapport soit déterministe quel que
/// soit l'ordre d'itération de la `HashMap` sous-jacente.
///
/// `probe` est injectée (jamais [`tcp_probe`] appelée en dur) : c'est ce qui
/// rend cette fonction, et donc [`doctor`] tout entier, testable sans jamais
/// ouvrir le moindre socket.
fn check_backends_reachable(
    config: &crate::config::Config,
    probe: &dyn Fn(&str) -> Result<(), String>,
) -> Vec<Check> {
    let mut ids: Vec<&String> = config.backends.keys().collect();
    ids.sort_unstable();

    ids.into_iter()
        .map(|id| {
            // `id` vient des clés de `config.backends` : l'entrée existe forcément.
            let backend = &config.backends[id];
            let status = match probe(&backend.base_url) {
                Ok(()) => Status::Ok,
                Err(message) => Status::Failed(message),
            };
            Check {
                kind: CheckKind::Reachability,
                label: format!("backend « {id} » joignable"),
                status,
            }
        })
        .collect()
}

/// Vérification (c) : pour chaque modèle configuré, son backend existe-t-il,
/// et expose-t-il l'opération que ce modèle déclare ? Une seule [`Check`] par
/// modèle, couvrant les deux — un modèle dont le backend est absent ne peut
/// de toute façon pas exposer d'opération à vérifier. Triés par identifiant
/// de modèle, même raison de déterminisme que [`check_backends_reachable`].
fn check_models(config: &crate::config::Config) -> Vec<Check> {
    let mut ids: Vec<&String> = config.models.keys().collect();
    ids.sort_unstable();

    ids.into_iter()
        .map(|id| {
            // `id` vient des clés de `config.models` : l'entrée existe forcément.
            let model = &config.models[id];
            let status = match config.backends.get(&model.backend) {
                None => Status::Failed(format!(
                    "backend « {} » introuvable (référencé par le modèle « {id} » ; backends \
                     disponibles : {})",
                    model.backend,
                    crate::error::format_available(config.backends.keys())
                )),
                Some(backend) => {
                    if backend.operations.contains_key(&model.operation) {
                        Status::Ok
                    } else {
                        Status::Failed(format!(
                            "le backend « {} » n'expose pas l'opération « {} » déclarée par le \
                             modèle « {id} » (opérations disponibles : {})",
                            backend.id,
                            model.operation,
                            crate::error::format_available(backend.operations.keys())
                        ))
                    }
                }
            };
            Check {
                kind: CheckKind::Config,
                label: format!("modèle « {id} »"),
                status,
            }
        })
        .collect()
}

/// Trie `commands` par chemin complet (`path.join("/")`), pour que l'ordre du
/// rapport de [`doctor`] soit déterministe quel que soit l'ordre de
/// découverte des fichiers sur le disque — même raison que le tri de
/// `command::discover_scopes` lui-même.
fn sorted_commands(commands: &[crate::command::CommandSpec]) -> Vec<&crate::command::CommandSpec> {
    let mut sorted: Vec<&crate::command::CommandSpec> = commands.iter().collect();
    sorted.sort_by_key(|spec| spec.path.join("/"));
    sorted
}

/// Vérification (d) : pour chaque commande découverte, dans TOUS les scopes
/// (pas seulement celle éventuellement invoquée), son modèle existe-t-il ?
fn check_commands_model(
    config: &crate::config::Config,
    commands: &[crate::command::CommandSpec],
) -> Vec<Check> {
    sorted_commands(commands)
        .into_iter()
        .map(|spec| {
            let path = spec.path.join("/");
            let status = if config.models.contains_key(&spec.model) {
                Status::Ok
            } else {
                Status::Failed(format!(
                    "modèle « {} » introuvable (référencé par la commande « {path} » ; modèles \
                     disponibles : {})",
                    spec.model,
                    crate::error::format_available(config.models.keys())
                ))
            };
            Check {
                kind: CheckKind::Config,
                label: format!("commande « {path} » : modèle"),
                status,
            }
        })
        .collect()
}

/// Vérification (e) : pour chaque commande déclarant un schéma de sortie
/// (`[output].schema`), dans TOUS les scopes, ce schéma est-il présent,
/// lisible, du JSON valide, et un schéma JSON Schema compilable ? C'est le
/// travail EXHAUSTIF que la phase 4 a délibérément différé à `doctor` (cf.
/// `output.rs`, `command::resolve_schema_path` : l'existence et la
/// compilation d'un schéma ne sont vérifiées, hors `doctor`, qu'au moment où
/// la commande qui le réclame est réellement invoquée). Réutilise
/// `output::compile_schema` — rendue `pub(crate)` pour cette phase (seul
/// changement autorisé dans `output.rs`, cf. le compte rendu) — plutôt que
/// d'écrire une seconde implémentation de la compilation de schéma : les
/// trois messages d'erreur distincts (absent/illisible, JSON invalide, schéma
/// syntaxiquement invalide) qu'elle produit déjà sont exactement ceux que
/// cette vérification doit rapporter.
///
/// Une commande sans `[output].schema` (format texte, ou JSON sans schéma —
/// les deux explicitement autorisés par `command::convert_output`) ne produit
/// aucune [`Check`] ici : il n'y a rien à vérifier.
fn check_commands_output_schema(commands: &[crate::command::CommandSpec]) -> Vec<Check> {
    sorted_commands(commands)
        .into_iter()
        .filter_map(|spec| {
            let schema_path = spec.output.schema.as_deref()?;
            let path = spec.path.join("/");
            let status = match crate::output::compile_schema(schema_path, &spec.file) {
                Ok(_validator) => Status::Ok,
                Err(err) => Status::Failed(err.to_string()),
            };
            Some(Check {
                kind: CheckKind::Config,
                label: format!("commande « {path} » : schéma de sortie"),
                status,
            })
        })
        .collect()
}

/// Exécute toutes les vérifications de `npu doctor` (point 3 du contrat
/// partagé) et renvoie le rapport — sans jamais écrire sur la console ni
/// toucher au réseau (`probe` est injectée) ; seule la vérification (e) touche
/// au disque, en lisant les fichiers de schéma déclarés.
///
/// `config`/`commands` et `load_error` reflètent le mode dégradé de
/// `lib.rs::run` (décision 1 du contrat partagé, corollaire de la dette
/// tracée depuis la phase 1) : quand le chargement échoue, `run` CONSERVE
/// l'erreur au lieu de la propager, pour que `doctor` puisse quand même
/// tourner et la rapporter comme vérification (a) échouée. Cette fonction ne
/// suppose pas que `config`/`commands`/`load_error` varient en bloc pour
/// autant : chaque famille de vérifications (b/c, puis d/e) ne s'exécute que
/// si les données dont elle a besoin sont effectivement disponibles, ce qui
/// reste correct que l'appelant traite le chargement comme une seule
/// opération atomique (le cas attendu en pratique, cf. le compte rendu) ou
/// distingue un échec de configuration proprement dit d'un échec de
/// découverte des commandes.
#[must_use]
pub fn doctor(
    config: Option<&crate::config::Config>,
    commands: Option<&[crate::command::CommandSpec]>,
    load_error: Option<&crate::Error>,
    probe: &dyn Fn(&str) -> Result<(), String>,
) -> Vec<Check> {
    let mut checks = vec![check_config_loaded(load_error)];

    if let Some(config) = config {
        checks.extend(check_backends_reachable(config, probe));
        checks.extend(check_models(config));
    }

    if let (Some(config), Some(commands)) = (config, commands) {
        checks.extend(check_commands_model(config, commands));
        checks.extend(check_commands_output_schema(commands));
    }

    checks
}

/// Formate le rapport de `doctor` pour stdout (point 6 du contrat partagé) :
/// une coche (`✓`) suivie du libellé pour chaque vérification réussie, une
/// croix (`✗`) suivie du libellé PUIS du message pour chaque échec — jamais
/// l'inverse, sans quoi le message expliquant l'échec se retrouverait sans
/// contexte sur ce qu'il concerne.
#[must_use]
pub fn format_doctor(checks: &[Check]) -> String {
    checks
        .iter()
        .map(|check| match &check.status {
            Status::Ok => format!("✓ {}", check.label),
            Status::Failed(message) => format!("✗ {} : {message}", check.label),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Code de sortie du rapport de `doctor` (point 5 du contrat partagé) :
/// - `0` si toutes les vérifications passent ;
/// - `2` si au moins une vérification de CONFIGURATION (a, c, d, e) échoue —
///   la configuration prime, y compris quand une vérification de
///   joignabilité (b) échoue en même temps ;
/// - `3` si SEULE la joignabilité (b) échoue.
///
/// Distingue les deux familles d'échec via [`Check::kind`], jamais via le
/// texte de [`Check::label`] : un libellé est de l'affichage — il peut être
/// reformulé, traduit, ou recevoir un nouveau suffixe sans préavis — alors
/// que la catégorie est un contrat machine (§23 de la spec) que ce code de
/// sortie doit continuer à honorer quoi qu'il arrive au libellé.
#[must_use]
pub fn doctor_exit_code(checks: &[Check]) -> i32 {
    let mut config_failed = false;
    let mut reachability_failed = false;

    for check in checks {
        let Status::Failed(_) = &check.status else {
            continue;
        };
        match check.kind {
            CheckKind::Reachability => reachability_failed = true,
            CheckKind::Config => config_failed = true,
        }
    }

    if config_failed {
        2
    } else if reachability_failed {
        3
    } else {
        0
    }
}

/// Formate le tableau `npu models` (point 6 du contrat partagé, §16 de la
/// spec) : colonnes NAME/BACKEND/OPERATION, triées par nom pour rester
/// déterministes quel que soit l'ordre d'itération de la `HashMap`
/// sous-jacente, alignées sur la largeur RÉELLE du contenu (jamais une
/// largeur codée en dur : un nom de modèle plus long que « NAME » élargit sa
/// colonne). Une configuration sans aucun modèle produit quand même l'en-tête
/// — jamais un tableau vide ni un panic.
#[must_use]
pub fn format_models(config: &crate::config::Config) -> String {
    const NAME_HEADER: &str = "NAME";
    const BACKEND_HEADER: &str = "BACKEND";
    const OPERATION_HEADER: &str = "OPERATION";

    let mut rows: Vec<(&str, &str, &str)> = config
        .models
        .values()
        .map(|model| {
            (
                model.id.as_str(),
                model.backend.as_str(),
                model.operation.as_str(),
            )
        })
        .collect();
    rows.sort_unstable_by_key(|&(name, _backend, _operation)| name);

    let name_width = rows
        .iter()
        .map(|&(name, _backend, _operation)| name.len())
        .max()
        .unwrap_or(0)
        .max(NAME_HEADER.len());
    let backend_width = rows
        .iter()
        .map(|&(_name, backend, _operation)| backend.len())
        .max()
        .unwrap_or(0)
        .max(BACKEND_HEADER.len());

    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(format!(
        "{NAME_HEADER:<name_width$}  {BACKEND_HEADER:<backend_width$}  {OPERATION_HEADER}"
    ));
    for (name, backend, operation) in rows {
        lines.push(format!(
            "{name:<name_width$}  {backend:<backend_width$}  {operation}"
        ));
    }

    lines.join("\n")
}

/// Un argument déclaré tel que sérialisé par [`describe`] : mêmes champs que
/// `command::ArgSpec`, jamais celui-ci directement — `ArgSpec` ne dérive que
/// `Deserialize` (il n'est jamais écrit en sortie ailleurs dans ce crate), et
/// ce fichier n'a pas le droit de modifier `command.rs` pour y ajouter
/// `Serialize` (règle absolue de la tâche : propriétaire exclusif de
/// `builtin.rs`).
#[derive(Serialize)]
struct DescribeArg<'a> {
    short: Option<char>,
    required: bool,
    description: &'a str,
}

/// Le contrat de sortie tel que sérialisé par [`describe`] : mêmes données
/// que `output::OutputSpec`, mais avec `format` et `schema` déjà convertis en
/// types sérialisables (`&str`, `Option<String>`) plutôt que les types
/// internes (`output::Format`, `Option<PathBuf>`), pour la même raison que
/// [`DescribeArg`] ci-dessus.
#[derive(Serialize)]
struct DescribeOutput<'a> {
    format: &'a str,
    schema: Option<String>,
    max_lines: Option<usize>,
}

/// La description JSON complète d'une commande, telle que sérialisée par
/// [`describe`].
#[derive(Serialize)]
struct Describe<'a> {
    name: String,
    description: &'a str,
    model: &'a str,
    input: &'a str,
    args: std::collections::BTreeMap<&'a str, DescribeArg<'a>>,
    output: DescribeOutput<'a>,
}

/// Décrit une commande dynamiquement configurée (point 6 du contrat partagé,
/// §16 de la spec) : produit du JSON sur stdout, sérialisé par `serde_json`
/// (jamais construit à la main — un `format!` manuel ne pourrait pas
/// échapper correctement une description ou un prompt contenant des
/// guillemets). Étend l'exemple de la §16 avec ce que les phases 3 et 4 ont
/// ajouté : les arguments déclarés (`args`) et le contrat de sortie
/// (`output`, avec son format, son schéma le cas échéant, et sa limite de
/// lignes le cas échéant).
///
/// Ne fait AUCUNE résolution par nom : le contrat partagé fixe cette
/// signature à un `CommandSpec` déjà résolu — c'est à l'appelant
/// (`lib.rs::run`) de retrouver ce `CommandSpec` (avec son propre
/// `find_command`, déjà écrit et testé là-bas) avant d'appeler cette
/// fonction. Voir le compte rendu pour la décision explicite derrière ce
/// choix : « describe sur une commande inconnue » n'est donc pas un
/// comportement que CE module peut produire ni tester, faute de recevoir un
/// nom à résoudre.
pub fn describe(spec: &crate::command::CommandSpec) -> crate::Result<String> {
    let input = match spec.input {
        crate::command::InputMode::Stdin => "stdin",
        crate::command::InputMode::File => "file",
        crate::command::InputMode::StdinOrFile => "stdin_or_file",
    };
    let format = match spec.output.format {
        crate::output::Format::Text => "text",
        crate::output::Format::Json => "json",
    };

    let args = spec
        .args
        .iter()
        .map(|(name, arg_spec)| {
            (
                name.as_str(),
                DescribeArg {
                    short: arg_spec.short,
                    required: arg_spec.required,
                    description: arg_spec.description.as_str(),
                },
            )
        })
        .collect();

    let dto = Describe {
        name: spec.path.join("/"),
        description: &spec.description,
        model: &spec.model,
        input,
        args,
        output: DescribeOutput {
            format,
            schema: spec
                .output
                .schema
                .as_ref()
                .map(|path| format!("{}", path.display())),
            max_lines: spec.output.max_lines,
        },
    };

    serde_json::to_string(&dto).map_err(|err| {
        crate::Error::Config(format!("échec de sérialisation de la description : {err}"))
    })
}

/// Extrait `(hôte, port)` d'une URL de base « http(s)://hôte[:port][/...] »
/// (point 4 du contrat partagé). Retombe sur le port implicite du schéma (80
/// pour `http`, 443 pour `https`) quand aucun port explicite n'est présent.
/// PUREMENT SYNTAXIQUE : ne touche jamais le réseau, seulement la chaîne
/// `base_url` elle-même — c'est [`tcp_probe`] qui ouvre la connexion.
///
/// **Notation IPv6 entre crochets** (`[::1]` ou `[::1]:8000`, RFC 3986
/// §3.2.2) traitée à part, AVANT le `rsplit_once(':')` général : une adresse
/// IPv6 nue contient elle-même des `:`, donc un simple `rsplit_once(':')`
/// couperait « `[::1]:8000` » sur le dernier `:` interne aux crochets plutôt
/// que sur le séparateur hôte/port. `hôte` est renvoyé SANS les crochets
/// (`"::1"`, pas `"[::1]"`) : `Ipv6Addr::from_str`, utilisée par
/// `ToSocketAddrs` dans [`tcp_probe`], rejette la forme entre crochets — la
/// conserver ferait échouer toute résolution IPv6 par une fausse erreur DNS,
/// jamais par le message de port invalide qu'on attendrait.
fn parse_host_port(base_url: &str) -> Result<(String, u16), String> {
    let Some((scheme, rest)) = base_url.split_once("://") else {
        return Err(format!(
            "base_url « {base_url} » : schéma manquant (attendu « http:// » ou « https:// »)"
        ));
    };

    let default_port: u16 = match scheme {
        "http" => 80,
        "https" => 443,
        other => {
            return Err(format!(
                "base_url « {base_url} » : schéma « {other} » non supporté (attendu « http » ou \
                 « https »)"
            ));
        }
    };

    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if authority.is_empty() {
        return Err(format!("base_url « {base_url} » : hôte manquant"));
    }

    if let Some(after_bracket) = authority.strip_prefix('[') {
        return parse_ipv6_authority(base_url, after_bracket, default_port);
    }

    match authority.rsplit_once(':') {
        Some((host, port_text)) if !host.is_empty() => {
            let port: u16 = port_text
                .parse()
                .map_err(|_| format!("base_url « {base_url} » : port « {port_text} » invalide"))?;
            Ok((host.to_string(), port))
        }
        _ => Ok((authority.to_string(), default_port)),
    }
}

/// Complète [`parse_host_port`] pour la notation IPv6 entre crochets :
/// `after_bracket` est ce qui suit le `[` ouvrant déjà consommé par
/// l'appelant (ex. `"::1]:8000"` pour `"[::1]:8000"`). Isolée dans sa propre
/// fonction parce que [`parse_host_port`] a déjà deux niveaux de `match` /
/// early-return ; l'imbriquer sur place nuirait à la lisibilité que
/// `clippy::pedantic` (`too_many_lines`) sanctionnerait sinon.
fn parse_ipv6_authority(
    base_url: &str,
    after_bracket: &str,
    default_port: u16,
) -> Result<(String, u16), String> {
    let Some(end) = after_bracket.find(']') else {
        return Err(format!(
            "base_url « {base_url} » : crochet IPv6 ouvrant « [ » jamais refermé"
        ));
    };
    // `end` pointe sur `]` (ASCII, 1 octet) : les deux bornes de découpe
    // suivantes tombent donc toujours sur une frontière de caractère,
    // qu'importe le contenu de `base_url` autour de ces crochets.
    let host = &after_bracket[..end];
    if host.is_empty() {
        return Err(format!(
            "base_url « {base_url} » : adresse IPv6 vide entre crochets"
        ));
    }
    let trailer = &after_bracket[end + 1..];

    match trailer.strip_prefix(':') {
        Some(port_text) if !port_text.is_empty() => port_text
            .parse()
            .map(|port| (host.to_string(), port))
            .map_err(|_| format!("base_url « {base_url} » : port « {port_text} » invalide")),
        Some(_) => Err(format!(
            "base_url « {base_url} » : port manquant après « : »"
        )),
        None if trailer.is_empty() => Ok((host.to_string(), default_port)),
        None => Err(format!(
            "base_url « {base_url} » : caractères inattendus après l'adresse IPv6 (« {trailer} »)"
        )),
    }
}

/// Teste la joignabilité d'un backend par une connexion TCP à son `base_url`
/// (point 4 du contrat partagé), avec un timeout court ([`PROBE_TIMEOUT`]),
/// puis la referme aussitôt. PAS de requête HTTP : un `POST` sur l'opération
/// `chat` invoquerait réellement le modèle, un effet de bord inacceptable
/// pour une commande de diagnostic — cette fonction ne fait donc qu'ouvrir et
/// fermer un socket, jamais écrire ni lire le moindre octet dessus. Le
/// message d'erreur dit « injoignable » côté `doctor` (via le libellé
/// `« joignable »` — cf. [`check_backends_reachable`]) et jamais «
/// disponible » : seule l'acceptation d'un socket est vérifiée, pas la
/// capacité du modèle à répondre.
///
/// # Errors
///
/// Renvoie `Err` si `base_url` n'a pas la forme attendue, si la résolution du
/// couple hôte/port échoue, ou si la connexion elle-même échoue ou expire.
pub fn tcp_probe(base_url: &str) -> Result<(), String> {
    let (host, port) = parse_host_port(base_url)?;

    let mut addrs = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|err| format!("résolution de l'adresse « {host}:{port} » échouée : {err}"))?;

    let addr = addrs.next().ok_or_else(|| {
        format!("résolution de l'adresse « {host}:{port} » n'a produit aucune adresse")
    })?;

    std::net::TcpStream::connect_timeout(&addr, PROBE_TIMEOUT)
        .map(|_stream| ())
        .map_err(|err| format!("connexion TCP vers « {host}:{port} » échouée : {err}"))
}

#[cfg(test)]
#[allow(clippy::expect_used)] // toléré dans les tests (cf. Cargo.toml [lints.clippy]).
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Crée un dossier de fixture unique sous `target/`, même idiome que les
    /// autres modules (`command::tests::fixture_dir`, `output::tests::
    /// fixture_file`) : pas de pollution du dépôt, pas de collision entre
    /// tests exécutés en parallèle.
    fn fixture_dir(name: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!("builtin-{name}-{n}"));
        std::fs::create_dir_all(&dir).expect("création du dossier de fixture");
        dir
    }

    fn backend(id: &str, base_url: &str, operations: &[&str]) -> crate::config::Backend {
        crate::config::Backend {
            id: id.to_string(),
            base_url: base_url.to_string(),
            kind: "openai-compatible".to_string(),
            operations: operations
                .iter()
                .map(|op| {
                    (
                        (*op).to_string(),
                        crate::config::Operation {
                            method: "POST".to_string(),
                            path: format!("/{op}"),
                        },
                    )
                })
                .collect(),
        }
    }

    fn model(id: &str, backend: &str, operation: &str) -> crate::config::Model {
        crate::config::Model {
            id: id.to_string(),
            backend: backend.to_string(),
            operation: operation.to_string(),
            model: format!("{id}-underlying"),
            generation: crate::config::Generation::default(),
        }
    }

    fn command_spec(path: &[&str], model: &str) -> crate::command::CommandSpec {
        crate::command::CommandSpec {
            path: path.iter().map(ToString::to_string).collect(),
            description: String::new(),
            model: model.to_string(),
            input: crate::command::InputMode::Stdin,
            prompt: "{{ input }}".to_string(),
            args: std::collections::BTreeMap::new(),
            output: crate::output::OutputSpec::default(),
            file: std::path::PathBuf::new(),
        }
    }

    // `unnecessary_wraps` : ces deux fonctions sont des sondes bouchonnées à
    // signature FIXE (`&dyn Fn(&str) -> Result<(), String>`, cf. `doctor`) ;
    // elles ne peuvent pas être simplifiées sans casser cette signature.
    #[allow(clippy::unnecessary_wraps)]
    fn always_ok(_base_url: &str) -> Result<(), String> {
        Ok(())
    }

    fn always_fails(_base_url: &str) -> Result<(), String> {
        Err("connexion refusée".to_string())
    }

    // -- doctor : scénario nominal -----------------------------------------

    #[test]
    fn doctor_all_checks_passing_yields_no_failure_and_exit_code_zero() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "ovms".to_string(),
            backend("ovms", "http://127.0.0.1:8000", &["chat"]),
        );
        config
            .models
            .insert("qwen-fast".to_string(), model("qwen-fast", "ovms", "chat"));
        let commands = vec![command_spec(&["classify"], "qwen-fast")];

        let checks = doctor(Some(&config), Some(&commands), None, &always_ok);

        assert!(
            checks.iter().all(|c| matches!(c.status, Status::Ok)),
            "obtenu : {checks:?}"
        );
        assert_eq!(doctor_exit_code(&checks), 0);
    }

    // -- (a) configuration chargée ------------------------------------------

    #[test]
    fn doctor_load_error_fails_configuration_check_only_and_exit_code_is_two() {
        let err = crate::Error::Config("fichier de commande cassé".to_string());

        let checks = doctor(None, None, Some(&err), &always_ok);

        assert_eq!(
            checks.len(),
            1,
            "config/commands absents : seule (a) doit être produite, obtenu : {checks:?}"
        );
        assert!(matches!(
            &checks[0].status,
            Status::Failed(message) if message.contains("fichier de commande cassé")
        ));
        assert_eq!(doctor_exit_code(&checks), 2);
    }

    // -- (c) modèles ----------------------------------------------------------

    #[test]
    fn doctor_model_referencing_unknown_backend_fails_check_c_and_exit_code_is_two() {
        let mut config = crate::config::Config::default();
        config
            .models
            .insert("gpt".to_string(), model("gpt", "does-not-exist", "chat"));

        let checks = doctor(Some(&config), Some(&[]), None, &always_ok);

        let model_check = checks
            .iter()
            .find(|c| c.label.contains("gpt"))
            .expect("une vérification pour le modèle « gpt » doit exister");
        assert!(matches!(&model_check.status, Status::Failed(_)));
        assert_eq!(doctor_exit_code(&checks), 2);
    }

    #[test]
    fn doctor_model_referencing_unexposed_operation_fails_check_c() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "ovms".to_string(),
            backend("ovms", "http://127.0.0.1:8000", &["chat"]),
        );
        config.models.insert(
            "whisper".to_string(),
            model("whisper", "ovms", "audio_transcriptions"),
        );

        let checks = doctor(Some(&config), Some(&[]), None, &always_ok);

        let model_check = checks
            .iter()
            .find(|c| c.label.contains("whisper"))
            .expect("une vérification pour le modèle « whisper » doit exister");
        assert!(matches!(
            &model_check.status,
            Status::Failed(message) if message.contains("audio_transcriptions")
        ));
        assert_eq!(doctor_exit_code(&checks), 2);
    }

    // -- (d) commandes / modèle ------------------------------------------------

    #[test]
    fn doctor_command_referencing_unknown_model_fails_check_d() {
        let config = crate::config::Config::default();
        let commands = vec![command_spec(&["classify"], "does-not-exist")];

        let checks = doctor(Some(&config), Some(&commands), None, &always_ok);

        let command_check = checks
            .iter()
            .find(|c| c.label.contains("classify") && c.label.contains("modèle"))
            .expect("une vérification (d) pour « classify » doit exister");
        assert!(matches!(&command_check.status, Status::Failed(_)));
        assert_eq!(doctor_exit_code(&checks), 2);
    }

    // -- (e) commandes / schéma de sortie ---------------------------------------

    #[test]
    fn doctor_command_with_missing_schema_file_fails_check_e() {
        let mut spec = command_spec(&["classify"], "qwen-fast");
        spec.output = crate::output::OutputSpec {
            format: crate::output::Format::Json,
            schema: Some(std::path::PathBuf::from(
                "/does/not/exist/schema-introuvable.json",
            )),
            max_lines: None,
        };
        let config = crate::config::Config::default();

        let checks = doctor(Some(&config), Some(&[spec]), None, &always_ok);

        let schema_check = checks
            .iter()
            .find(|c| c.label.contains("schéma"))
            .expect("une vérification (e) doit exister");
        assert!(matches!(
            &schema_check.status,
            Status::Failed(message) if message.contains("introuvable")
        ));
    }

    #[test]
    fn doctor_command_with_syntactically_invalid_schema_fails_check_e_with_distinct_message() {
        let dir = fixture_dir("broken-schema");
        let schema_path = dir.join("broken.json");
        std::fs::write(&schema_path, "ceci n'est pas du JSON").expect("écriture fixture");

        let mut spec = command_spec(&["classify"], "qwen-fast");
        spec.output = crate::output::OutputSpec {
            format: crate::output::Format::Json,
            schema: Some(schema_path),
            max_lines: None,
        };
        let config = crate::config::Config::default();

        let checks = doctor(Some(&config), Some(&[spec]), None, &always_ok);

        let schema_check = checks
            .iter()
            .find(|c| c.label.contains("schéma"))
            .expect("une vérification (e) doit exister");
        assert!(matches!(
            &schema_check.status,
            Status::Failed(message) if message.contains("JSON invalide")
        ));
    }

    #[test]
    fn doctor_command_without_schema_produces_no_check_e() {
        let spec = command_spec(&["commit-message"], "qwen-fast"); // format texte par défaut
        let config = crate::config::Config::default();

        let checks = doctor(Some(&config), Some(&[spec]), None, &always_ok);

        assert!(
            !checks.iter().any(|c| c.label.contains("schéma")),
            "aucune vérification (e) ne doit être produite en l'absence de schéma, obtenu : \
             {checks:?}"
        );
    }

    // -- code de sortie : priorité de la configuration sur la joignabilité -----

    #[test]
    fn doctor_reachability_failure_alone_yields_exit_code_three() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "ovms".to_string(),
            backend("ovms", "http://127.0.0.1:8000", &["chat"]),
        );

        let checks = doctor(Some(&config), Some(&[]), None, &always_fails);

        assert_eq!(doctor_exit_code(&checks), 3);
    }

    /// Régression : la classification doit venir de [`CheckKind`], jamais du
    /// texte du libellé. Un libellé de joignabilité délibérément reformulé,
    /// sans le mot « joignable » ni le suffixe historique, doit quand même
    /// produire le code de sortie 3 — la classification par texte reclassait
    /// silencieusement ce cas en échec de configuration (code 2).
    #[test]
    fn doctor_exit_code_uses_kind_not_label_text_for_reachability_failure() {
        let checks = vec![Check {
            kind: CheckKind::Reachability,
            label: "état du backend « ovms »".to_string(),
            status: Status::Failed("connexion refusée".to_string()),
        }];

        assert_eq!(doctor_exit_code(&checks), 3);
    }

    #[test]
    fn doctor_config_failure_takes_priority_over_reachability_failure() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "ovms".to_string(),
            backend("ovms", "http://127.0.0.1:8000", &["chat"]),
        );
        // (c) échoue : le modèle référence un backend inexistant.
        config.models.insert(
            "orphan".to_string(),
            model("orphan", "does-not-exist", "chat"),
        );

        // (b) échoue aussi : la sonde échoue pour tout backend interrogé.
        let checks = doctor(Some(&config), Some(&[]), None, &always_fails);

        let reachability_failed = checks
            .iter()
            .any(|c| c.kind == CheckKind::Reachability && matches!(c.status, Status::Failed(_)));
        let config_check_failed = checks
            .iter()
            .any(|c| c.label.contains("orphan") && matches!(c.status, Status::Failed(_)));
        assert!(
            reachability_failed,
            "précondition : la sonde doit avoir échoué"
        );
        assert!(
            config_check_failed,
            "précondition : la vérification (c) doit avoir échoué"
        );

        assert_eq!(
            doctor_exit_code(&checks),
            2,
            "la configuration doit primer sur la joignabilité (point 5 du contrat partagé)"
        );
    }

    // -- format_doctor -----------------------------------------------------------

    #[test]
    fn format_doctor_uses_checkmark_for_ok_and_cross_with_message_for_failed() {
        let checks = vec![
            Check {
                kind: CheckKind::Config,
                label: "a".to_string(),
                status: Status::Ok,
            },
            Check {
                kind: CheckKind::Config,
                label: "b".to_string(),
                status: Status::Failed("boum".to_string()),
            },
        ];

        assert_eq!(format_doctor(&checks), "✓ a\n✗ b : boum");
    }

    // -- format_models -------------------------------------------------------------

    #[test]
    fn format_models_aligns_backend_column_across_rows_of_differing_name_length() {
        let mut config = crate::config::Config::default();
        config
            .models
            .insert("a".to_string(), model("a", "ovms", "chat"));
        config.models.insert(
            "much-longer-model-name".to_string(),
            model("much-longer-model-name", "ovms", "chat"),
        );

        let out = format_models(&config);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);

        let header_backend_at = lines[0].find("BACKEND").expect("en-tête BACKEND");
        let row_a_backend_at = lines[1].find("ovms").expect("ligne « a »");
        let row_long_backend_at = lines[2].find("ovms").expect("ligne « much-longer… »");

        assert_eq!(header_backend_at, row_a_backend_at);
        assert_eq!(header_backend_at, row_long_backend_at);
    }

    #[test]
    fn format_models_sorts_deterministically_by_name() {
        let mut config = crate::config::Config::default();
        config
            .models
            .insert("zebra".to_string(), model("zebra", "ovms", "chat"));
        config
            .models
            .insert("alpha".to_string(), model("alpha", "ovms", "chat"));

        let out = format_models(&config);
        let lines: Vec<&str> = out.lines().collect();

        let alpha_at = lines
            .iter()
            .position(|l| l.starts_with("alpha"))
            .expect("« alpha » doit être présent");
        let zebra_at = lines
            .iter()
            .position(|l| l.starts_with("zebra"))
            .expect("« zebra » doit être présent");
        assert!(alpha_at < zebra_at);
    }

    #[test]
    fn format_models_with_no_models_prints_only_the_header_without_panicking() {
        let config = crate::config::Config::default();

        let out = format_models(&config);

        assert_eq!(out, "NAME  BACKEND  OPERATION");
    }

    // -- describe --------------------------------------------------------------

    fn sample_translate_spec() -> crate::command::CommandSpec {
        let mut args = std::collections::BTreeMap::new();
        args.insert(
            "language".to_string(),
            crate::command::ArgSpec {
                short: Some('l'),
                required: true,
                description: "Target language".to_string(),
            },
        );
        crate::command::CommandSpec {
            path: vec!["translate".to_string()],
            description: "Translate input text".to_string(),
            model: "qwen-fast".to_string(),
            input: crate::command::InputMode::StdinOrFile,
            prompt: "Translate {{ input }} to {{ args.language }}".to_string(),
            args,
            output: crate::output::OutputSpec {
                format: crate::output::Format::Json,
                schema: Some(std::path::PathBuf::from("schemas/translation.json")),
                max_lines: None,
            },
            file: std::path::PathBuf::from(".npu/commands/translate.md"),
        }
    }

    #[test]
    fn describe_produces_valid_json_with_declared_args_and_output_contract() {
        let spec = sample_translate_spec();

        let json_text = describe(&spec).expect("describe doit réussir");
        let value: serde_json::Value =
            serde_json::from_str(&json_text).expect("describe doit produire du JSON valide");

        assert_eq!(value["name"], "translate");
        assert_eq!(value["description"], "Translate input text");
        assert_eq!(value["model"], "qwen-fast");
        assert_eq!(value["input"], "stdin_or_file");
        assert_eq!(value["args"]["language"]["short"], "l");
        assert_eq!(value["args"]["language"]["required"], true);
        assert_eq!(value["args"]["language"]["description"], "Target language");
        assert_eq!(value["output"]["format"], "json");
        assert_eq!(value["output"]["schema"], "schemas/translation.json");
    }

    #[test]
    fn describe_text_output_without_schema_has_a_null_schema_field() {
        let mut spec = sample_translate_spec();
        spec.output = crate::output::OutputSpec {
            format: crate::output::Format::Text,
            schema: None,
            max_lines: Some(1),
        };

        let json_text = describe(&spec).expect("describe doit réussir");
        let value: serde_json::Value =
            serde_json::from_str(&json_text).expect("describe doit produire du JSON valide");

        assert_eq!(value["output"]["format"], "text");
        assert!(value["output"]["schema"].is_null());
        assert_eq!(value["output"]["max_lines"], 1);
    }

    // -- parse_host_port -------------------------------------------------------

    #[test]
    fn parse_host_port_defaults_http_to_port_80() {
        assert_eq!(
            parse_host_port("http://127.0.0.1").expect("doit parser"),
            ("127.0.0.1".to_string(), 80)
        );
    }

    #[test]
    fn parse_host_port_defaults_https_to_port_443() {
        assert_eq!(
            parse_host_port("https://example.com").expect("doit parser"),
            ("example.com".to_string(), 443)
        );
    }

    #[test]
    fn parse_host_port_explicit_port_is_used() {
        assert_eq!(
            parse_host_port("http://127.0.0.1:8000").expect("doit parser"),
            ("127.0.0.1".to_string(), 8000)
        );
    }

    #[test]
    fn parse_host_port_tolerates_a_path_after_the_authority() {
        assert_eq!(
            parse_host_port("http://127.0.0.1:8000/v3/chat").expect("doit parser"),
            ("127.0.0.1".to_string(), 8000)
        );
    }

    #[test]
    fn parse_host_port_missing_scheme_is_an_error() {
        assert!(parse_host_port("127.0.0.1:8000").is_err());
    }

    #[test]
    fn parse_host_port_unsupported_scheme_is_an_error() {
        assert!(parse_host_port("ftp://127.0.0.1:21").is_err());
    }

    // -- parse_host_port : IPv6 entre crochets ----------------------------------

    #[test]
    fn parse_host_port_ipv6_with_explicit_port_strips_brackets_from_host() {
        // Le host renvoyé doit être SANS crochets : `Ipv6Addr::from_str`
        // (utilisée par `ToSocketAddrs` dans `tcp_probe`) rejette la forme
        // entre crochets, cf. doc de `parse_ipv6_authority`.
        assert_eq!(
            parse_host_port("http://[::1]:8000").expect("doit parser"),
            ("::1".to_string(), 8000)
        );
    }

    #[test]
    fn parse_host_port_ipv6_without_port_defaults_to_scheme_port() {
        assert_eq!(
            parse_host_port("https://[2001:db8::1]").expect("doit parser"),
            ("2001:db8::1".to_string(), 443)
        );
    }

    #[test]
    fn parse_host_port_ipv6_tolerates_a_path_after_the_authority() {
        assert_eq!(
            parse_host_port("http://[::1]:8000/v3/chat").expect("doit parser"),
            ("::1".to_string(), 8000)
        );
    }

    #[test]
    fn parse_host_port_ipv6_unclosed_bracket_is_an_error_not_a_panic() {
        assert!(parse_host_port("http://[::1:8000").is_err());
    }

    #[test]
    fn parse_host_port_ipv6_empty_brackets_is_an_error() {
        assert!(parse_host_port("http://[]:8000").is_err());
    }

    #[test]
    fn parse_host_port_ipv6_trailing_colon_without_port_is_an_error() {
        assert!(parse_host_port("http://[::1]:").is_err());
    }

    #[test]
    fn parse_host_port_ipv6_garbage_after_bracket_is_an_error_not_a_panic() {
        assert!(parse_host_port("http://[::1]garbage").is_err());
    }

    #[test]
    fn parse_host_port_ipv6_non_numeric_port_is_an_error() {
        assert!(parse_host_port("http://[::1]:notaport").is_err());
    }

    // -- tcp_probe --------------------------------------------------------------

    #[test]
    fn tcp_probe_succeeds_against_a_locally_bound_listener() {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind du listener éphémère");
        let addr = listener.local_addr().expect("adresse locale du listener");
        let base_url = format!("http://{addr}");

        // Accepte la connexion entrante puis termine : aucun thread ni
        // socket ne doit survivre à ce test.
        let acceptor = std::thread::spawn(move || {
            let _ = listener.accept();
        });

        let result = tcp_probe(&base_url);

        acceptor
            .join()
            .expect("le thread accepteur ne doit pas paniquer");
        assert!(result.is_ok(), "obtenu : {result:?}");
    }

    #[test]
    fn tcp_probe_fails_against_a_closed_port() {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind du listener éphémère");
        let addr = listener.local_addr().expect("adresse locale du listener");
        drop(listener); // ferme immédiatement : plus personne n'écoute ici.

        let result = tcp_probe(&format!("http://{addr}"));

        assert!(
            result.is_err(),
            "un port fermé doit être signalé comme injoignable"
        );
    }
}
