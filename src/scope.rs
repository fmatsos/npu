//! Resolution of layered configuration scopes (phase 2).
//!
//! General -> local precedence (npu-cli-spec.md §5, IMPLEMENTATION.md
//! decision 3): `/etc/npu`, then `$XDG_CONFIG_HOME/npu` (falling back to
//! `$HOME/.config/npu`), then `./.npu`. Consumers (`config.rs`,
//! `command.rs`) apply "last one wins" to this list.

use std::path::PathBuf;

/// `/etc/npu` root, hardcoded: this is not an environment variable
/// (see npu-cli-spec.md §5).
const ETC_ROOT: &str = "/etc/npu";

/// The environment inputs the resolution depends on, isolated so the
/// core stays a pure, testable function without touching real variables.
#[derive(Debug, Clone)]
pub(crate) struct ScopeEnv {
    /// `/etc/npu`.
    pub etc: PathBuf,
    /// `$XDG_CONFIG_HOME`, if set and non-empty.
    pub xdg_config_home: Option<PathBuf>,
    /// `$HOME`, if set and non-empty.
    pub home: Option<PathBuf>,
    /// Current directory.
    pub cwd: PathBuf,
}

/// Reduces an already-read environment variable value (`None` if absent)
/// to `None` as well when it is empty. Extracted from `non_empty_env_var`
/// to stay a pure, testable function: mutating real environment variables
/// to cover the "empty" case would require `std::env::set_var`,
/// `unsafe` since edition 2024 and therefore forbidden here (`unsafe_code =
/// "forbid"`, see Cargo.toml).
fn non_empty(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

/// Reads a system environment variable; returns `None` if it is
/// absent or empty.
fn non_empty_env_var(name: &str) -> Option<PathBuf> {
    non_empty(std::env::var_os(name))
}

impl ScopeEnv {
    /// Builds a `ScopeEnv` from the real environment variables and the
    /// process's current directory.
    ///
    /// Cannot fail: a failing `current_dir()` silently falls back to
    /// `PathBuf::from(".")` rather than panicking or returning an
    /// error — this function is not critical enough to fail the whole
    /// program.
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

/// Candidate roots, from MOST GENERAL to MOST LOCAL. Pure function:
/// reads no environment variable, everything comes from `env`.
///
/// Order:
/// 1. `env.etc` (typically `/etc/npu`);
/// 2. `env.xdg_config_home/npu` if `xdg_config_home` is `Some` (replaces the
///    derivation from `HOME`, does not add to it); otherwise `env.home/.config/npu`
///    if `home` is `Some`;
/// 3. `env.cwd/.npu`.
///
/// Deduplicates while preserving order: if two entries resolve to the same
/// path (e.g. `cwd` is `/etc`), only the first occurrence is kept.
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

/// Deduplicates `paths` while preserving the order of each path's
/// first occurrence.
fn dedup_preserve_order(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::with_capacity(paths.len());
    paths
        .into_iter()
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

/// Filters `candidates` to keep only the paths that actually exist on
/// disk as a directory, preserving order. Extracted from `roots()` to
/// stay testable without going through `ScopeEnv::from_env()`
/// (see tests below: no test should depend on the real environment
/// variables or on `/etc`/`~/.config`).
fn filter_existing_dirs(candidates: Vec<PathBuf>) -> Vec<PathBuf> {
    candidates.into_iter().filter(|p| p.is_dir()).collect()
}

/// Candidate roots that actually exist on disk, from most general
/// to most local (see `candidate_roots`).
///
/// A missing root is not an error: it is simply filtered out. Consumers
/// (`config::load_scopes`, `command::discover_scopes`) apply "last one
/// wins" to the returned list: a value defined in a more local root
/// (end of the list) replaces one from a more general root (start of
/// the list).
#[must_use]
pub fn roots() -> Vec<PathBuf> {
    filter_existing_dirs(candidate_roots(&ScopeEnv::from_env()))
}

#[cfg(test)]
#[allow(clippy::expect_used)] // tolerated in tests (see Cargo.toml [lints.clippy]).
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
        // HOME is also set, but XDG_CONFIG_HOME takes priority: only one
        // user path should appear, not both.
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
        // Direct collision: xdg_config_home already equals "/etc/npu",
        // identical to the etc root — a real case mentioned in the contract
        // (e.g. cwd = /etc).
        let e = env("/etc/npu", Some("/etc"), None, "/work");
        let roots = candidate_roots(&e);
        assert_eq!(
            roots,
            vec![PathBuf::from("/etc/npu"), PathBuf::from("/work/.npu")],
            "the second occurrence of /etc/npu (via xdg) must be deduplicated, the first kept"
        );

        // Collision between the etc root and the cwd root: cwd itself is
        // /etc/npu, so <cwd>/.npu remains distinct, but we verify that
        // the general->local order is preserved even in this edge case.
        let e2 = env("/etc/npu", None, None, "/etc/npu");
        let roots2 = candidate_roots(&e2);
        assert_eq!(
            roots2,
            vec![PathBuf::from("/etc/npu"), PathBuf::from("/etc/npu/.npu")]
        );
    }

    /// Creates a unique fixture directory under `target/`, so as not to
    /// pollute the repo or collide between tests run in parallel
    /// (same idiom as `config::tests::fixture_dir`).
    fn fixture_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!("scope-{name}-{n}"));
        std::fs::create_dir_all(&dir).expect("fixture directory creation");
        dir
    }

    #[test]
    fn filter_existing_dirs_keeps_only_directories_that_exist_and_preserves_order() {
        // No `ScopeEnv::from_env()` here: this test must depend neither on
        // real environment variables, nor on `/etc`, nor on `~/.config`
        // (see review L2), to stay safe under parallel execution.
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
        // This is the case a variable set but empty (e.g. `XDG_CONFIG_HOME=`)
        // must produce: treated as absent.
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
