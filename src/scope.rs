//! Résolution des scopes de configuration en couches (phase 2).
//!
//! Précédence générale -> locale (npu-cli-spec.md §5, IMPLEMENTATION.md
//! décision 3) : `/etc/npu`, puis `$XDG_CONFIG_HOME/npu` (à défaut
//! `$HOME/.config/npu`), puis `./.npu`. Les consommateurs (`config.rs`,
//! `command.rs`) appliquent « le dernier gagne » sur cette liste.

use std::path::PathBuf;

/// Racine `/etc/npu`, en dur : ce n'est pas une variable d'environnement
/// (cf. npu-cli-spec.md §5).
const ETC_ROOT: &str = "/etc/npu";

/// Les entrées d'environnement dont dépend la résolution, isolées pour que
/// le cœur reste une fonction pure testable sans toucher aux vraies variables.
#[derive(Debug, Clone)]
pub(crate) struct ScopeEnv {
    /// `/etc/npu`.
    pub etc: PathBuf,
    /// `$XDG_CONFIG_HOME`, si définie et non vide.
    pub xdg_config_home: Option<PathBuf>,
    /// `$HOME`, si définie et non vide.
    pub home: Option<PathBuf>,
    /// Répertoire courant.
    pub cwd: PathBuf,
}

/// Réduit une valeur de variable d'environnement déjà lue (`None` si absente)
/// à `None` également lorsqu'elle est vide. Extrait de `non_empty_env_var`
/// pour rester une fonction pure testable : muter les vraies variables
/// d'environnement pour couvrir le cas « vide » exigerait `std::env::set_var`,
/// `unsafe` depuis l'édition 2024 et donc interdit ici (`unsafe_code =
/// "forbid"`, cf. Cargo.toml).
fn non_empty(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

/// Lit une variable d'environnement système ; renvoie `None` si elle est
/// absente ou vide.
fn non_empty_env_var(name: &str) -> Option<PathBuf> {
    non_empty(std::env::var_os(name))
}

impl ScopeEnv {
    /// Construit un `ScopeEnv` depuis les vraies variables d'environnement et
    /// le répertoire courant du processus.
    ///
    /// Ne peut pas échouer : un `current_dir()` en échec replie silencieusement
    /// sur `PathBuf::from(".")` plutôt que de paniquer ou de renvoyer une
    /// erreur — cette fonction n'a rien d'assez critique pour faire échouer
    /// tout le programme.
    #[must_use]
    pub(crate) fn from_env() -> Self {
        ScopeEnv {
            etc: PathBuf::from(ETC_ROOT),
            xdg_config_home: non_empty_env_var("XDG_CONFIG_HOME"),
            home: non_empty_env_var("HOME"),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }
}

/// Racines candidates, de la PLUS GÉNÉRALE à la PLUS LOCALE. Fonction pure :
/// ne lit aucune variable d'environnement, tout vient de `env`.
///
/// Ordre :
/// 1. `env.etc` (typiquement `/etc/npu`) ;
/// 2. `env.xdg_config_home/npu` si `xdg_config_home` est `Some` (remplace la
///    dérivation depuis `HOME`, ne s'y ajoute pas) ; sinon `env.home/.config/npu`
///    si `home` est `Some` ;
/// 3. `env.cwd/.npu`.
///
/// Déduplique en préservant l'ordre : si deux entrées résolvent au même
/// chemin (ex. `cwd` vaut `/etc`), seule la première occurrence est gardée.
#[must_use]
pub(crate) fn candidate_roots(env: &ScopeEnv) -> Vec<PathBuf> {
    let mut candidates = vec![env.etc.clone()];

    if let Some(xdg) = &env.xdg_config_home {
        candidates.push(xdg.join("npu"));
    } else if let Some(home) = &env.home {
        candidates.push(home.join(".config").join("npu"));
    }

    candidates.push(env.cwd.join(".npu"));

    dedup_preserve_order(candidates)
}

/// Déduplique `paths` en préservant l'ordre de la première occurrence de
/// chaque chemin.
fn dedup_preserve_order(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::with_capacity(paths.len());
    paths
        .into_iter()
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

/// Filtre `candidates` pour ne garder que les chemins qui existent réellement
/// sur le disque en tant que dossier, en préservant l'ordre. Extrait de
/// `roots()` pour rester testable sans passer par `ScopeEnv::from_env()`
/// (cf. tests ci-dessous : aucun test ne doit dépendre des vraies variables
/// d'environnement ni de `/etc`/`~/.config`).
fn filter_existing_dirs(candidates: Vec<PathBuf>) -> Vec<PathBuf> {
    candidates.into_iter().filter(|p| p.is_dir()).collect()
}

/// Racines candidates qui existent réellement sur le disque, de la plus
/// générale à la plus locale (cf. `candidate_roots`).
///
/// Une racine absente n'est pas une erreur : elle est simplement filtrée. Les
/// consommateurs (`config::load_scopes`, `command::discover_scopes`)
/// appliquent « le dernier gagne » sur la liste renvoyée : une valeur définie
/// dans une racine plus locale (fin de liste) remplace celle d'une racine
/// plus générale (début de liste).
#[must_use]
pub fn roots() -> Vec<PathBuf> {
    filter_existing_dirs(candidate_roots(&ScopeEnv::from_env()))
}

#[cfg(test)]
#[allow(clippy::expect_used)] // toléré dans les tests (cf. Cargo.toml [lints.clippy]).
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn env(etc: &str, xdg_config_home: Option<&str>, home: Option<&str>, cwd: &str) -> ScopeEnv {
        ScopeEnv {
            etc: PathBuf::from(etc),
            xdg_config_home: xdg_config_home.map(PathBuf::from),
            home: home.map(PathBuf::from),
            cwd: PathBuf::from(cwd),
        }
    }

    #[test]
    fn xdg_config_home_present_is_used_verbatim() {
        let e = env("/etc/npu", Some("/xdg"), Some("/home/alice"), "/work");
        let roots = candidate_roots(&e);
        assert_eq!(
            roots,
            vec![
                PathBuf::from("/etc/npu"),
                PathBuf::from("/xdg/npu"),
                PathBuf::from("/work/.npu"),
            ]
        );
    }

    #[test]
    fn xdg_absent_falls_back_to_home_dot_config() {
        let e = env("/etc/npu", None, Some("/home/alice"), "/work");
        let roots = candidate_roots(&e);
        assert_eq!(
            roots,
            vec![
                PathBuf::from("/etc/npu"),
                PathBuf::from("/home/alice/.config/npu"),
                PathBuf::from("/work/.npu"),
            ]
        );
    }

    #[test]
    fn xdg_and_home_both_absent_yields_only_etc_and_cwd() {
        let e = env("/etc/npu", None, None, "/work");
        let roots = candidate_roots(&e);
        assert_eq!(
            roots,
            vec![PathBuf::from("/etc/npu"), PathBuf::from("/work/.npu")]
        );
    }

    #[test]
    fn xdg_present_replaces_home_derivation_rather_than_adding_to_it() {
        // HOME est renseigné aussi, mais XDG_CONFIG_HOME prime : un seul
        // chemin utilisateur doit apparaître, pas les deux.
        let e = env("/etc/npu", Some("/xdg"), Some("/home/alice"), "/work");
        let roots = candidate_roots(&e);
        assert!(!roots.contains(&PathBuf::from("/home/alice/.config/npu")));
        assert_eq!(roots.iter().filter(|p| p.ends_with("npu")).count(), 2);
    }

    #[test]
    fn order_is_general_to_local() {
        let e = env("/etc/npu", Some("/xdg"), Some("/home/alice"), "/work");
        let roots = candidate_roots(&e);
        assert_eq!(roots[0], PathBuf::from("/etc/npu"));
        assert_eq!(roots[1], PathBuf::from("/xdg/npu"));
        assert_eq!(roots[2], PathBuf::from("/work/.npu"));
    }

    #[test]
    fn duplicate_resolved_paths_keep_only_first_occurrence() {
        // Collision directe : xdg_config_home vaut déjà "/etc/npu", identique
        // à la racine etc — cas réel évoqué dans le contrat (ex. cwd = /etc).
        let e = env("/etc/npu", Some("/etc"), None, "/work");
        let roots = candidate_roots(&e);
        assert_eq!(
            roots,
            vec![PathBuf::from("/etc/npu"), PathBuf::from("/work/.npu")],
            "la seconde occurrence de /etc/npu (via xdg) doit être dédupliquée, la première conservée"
        );

        // Collision entre la racine etc et la racine cwd : cwd vaut /etc/npu
        // lui-même, donc <cwd>/.npu reste distinct, mais on vérifie que
        // l'ordre général->local est préservé même dans ce cas limite.
        let e2 = env("/etc/npu", None, None, "/etc/npu");
        let roots2 = candidate_roots(&e2);
        assert_eq!(
            roots2,
            vec![PathBuf::from("/etc/npu"), PathBuf::from("/etc/npu/.npu")]
        );
    }

    /// Crée un dossier de fixture unique sous `target/`, pour ne pas polluer
    /// le dépôt ni entrer en collision entre tests exécutés en parallèle
    /// (même idiome que `config::tests::fixture_dir`).
    fn fixture_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!("scope-{name}-{n}"));
        std::fs::create_dir_all(&dir).expect("création du dossier de fixture");
        dir
    }

    #[test]
    fn filter_existing_dirs_keeps_only_directories_that_exist_and_preserves_order() {
        // Pas de `ScopeEnv::from_env()` ici : ce test ne doit dépendre ni des
        // vraies variables d'environnement, ni de `/etc`, ni de `~/.config`
        // (cf. revue L2), pour rester sûr en exécution parallèle.
        let existing_a = fixture_dir("existing-a");
        let existing_b = fixture_dir("existing-b");
        let missing = existing_a.join("does-not-exist");

        let filtered = filter_existing_dirs(vec![existing_a.clone(), missing, existing_b.clone()]);

        assert_eq!(filtered, vec![existing_a, existing_b]);
    }

    #[test]
    fn non_empty_none_stays_none() {
        assert_eq!(non_empty(None), None);
    }

    #[test]
    fn non_empty_empty_string_becomes_none() {
        // C'est le cas qu'une variable d'environnement définie mais vide
        // (ex. `XDG_CONFIG_HOME=`) doit produire : traité comme absente.
        assert_eq!(non_empty(Some(std::ffi::OsString::new())), None);
    }

    #[test]
    fn non_empty_non_empty_string_is_kept() {
        assert_eq!(
            non_empty(Some(std::ffi::OsString::from("/xdg"))),
            Some(PathBuf::from("/xdg"))
        );
    }

    #[test]
    fn filter_existing_dirs_on_all_missing_yields_empty() {
        let missing = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join("scope-all-missing-does-not-exist");

        assert!(filter_existing_dirs(vec![missing]).is_empty());
    }
}
