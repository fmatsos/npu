//! Resolution of layered configuration scopes.
//!
//! General -> local precedence: `/etc/npu`, then `$XDG_CONFIG_HOME/npu`
//! (falling back to
//! `$HOME/.config/npu`), then `./.npu`. Consumers (`config.rs`,
//! `command.rs`) apply "last one wins" to this list.

use std::path::PathBuf;

/// `/etc/npu` root, hardcoded: this is not an environment variable.
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
    /// `--config-dir`/`NPU_CONFIG_DIR`, when given: the project scope
    /// directly, bypassing the walk-up search entirely (see
    /// [`project_root`]). The CLI flag wins over the environment variable
    /// when both are set (`run` resolves that before building this
    /// struct).
    pub config_dir_override: Option<PathBuf>,
}

/// Reduces an already-read environment variable value (`None` if absent)
/// to `None` as well when it is empty. Extracted from `non_empty_env_var`
/// to stay a pure, testable function: mutating real environment variables
/// to cover the "empty" case would require `std::env::set_var`,
/// `unsafe` since edition 2024 and therefore forbidden here (`unsafe_code =
/// "forbid"`, see Cargo.toml).
pub(crate) fn non_empty(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

/// Reads a system environment variable; returns `None` if it is
/// absent or empty.
///
/// `pub(crate)`: `runtime::state` reads `$XDG_STATE_HOME` and `$HOME` under
/// exactly this rule, and a second copy of it would be a second place for
/// "set but empty" to be decided (same reason as `error::format_available`).
pub(crate) fn non_empty_env_var(name: &str) -> Option<PathBuf> {
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
    pub(crate) fn from_env(config_dir_override: Option<PathBuf>) -> Self {
        ScopeEnv {
            etc: PathBuf::from(ETC_ROOT),
            xdg_config_home: non_empty_env_var("XDG_CONFIG_HOME"),
            home: non_empty_env_var("HOME"),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            config_dir_override,
        }
    }
}

/// Reads `NPU_CONFIG_DIR`, `None` if absent or empty — same rule as every
/// other environment variable this module reads (`non_empty_env_var`).
#[must_use]
pub(crate) fn config_dir_env_var() -> Option<PathBuf> {
    non_empty_env_var("NPU_CONFIG_DIR")
}

/// Reads `--config-dir` from the RAW command line, before `clap` has parsed
/// anything — same idiom and same reason as `log::level_from_args` and
/// `error::error_format_from_args`: the project scope is resolved (to load
/// the configuration) before the `clap` tree is even built, so a declared
/// argument's value cannot be read back from `ArgMatches` yet.
#[must_use]
pub(crate) fn config_dir_from_args<I, S>(args: I) -> Option<PathBuf>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut value = None;
    let mut expecting_value = false;

    for arg in args {
        let arg = arg.as_ref();
        if expecting_value {
            value = Some(PathBuf::from(arg));
            expecting_value = false;
        } else if arg == "--config-dir" {
            expecting_value = true;
        } else if let Some(rest) = arg.strip_prefix("--config-dir=") {
            value = Some(PathBuf::from(rest));
        }
    }

    value
}

/// The project scope directory `--config-dir`/`NPU_CONFIG_DIR` names, when
/// either is set (the CLI flag wins), OR the `.npu` directory found by
/// walking up from `env.cwd`: check the current directory's `.npu` FIRST,
/// then stop the climb (without going any higher) at a directory whose
/// `.git` `exists()` (a file in a worktree, a directory otherwise — either
/// way, that is this project's root, so no `.npu` above it belongs to it)
/// or at `env.home` — climbing PAST `$HOME` would make an unrelated
/// ancestor's `.npu` visible to every project on the machine, but `$HOME`
/// itself is still checked for its own `.npu` before the climb stops there.
///
/// `None` when no override is set and the walk-up finds nothing: exactly
/// the previous behaviour (only `cwd/.npu` — now the walk-up's very first
/// step) for a project with no parent `.npu` and no `.git` boundary.
#[must_use]
pub(crate) fn project_root(env: &ScopeEnv) -> Option<PathBuf> {
    if let Some(over) = &env.config_dir_override {
        return Some(over.clone());
    }

    let mut dir = env.cwd.clone();
    loop {
        // Checked BEFORE the `$HOME` boundary below: `cwd` starting exactly
        // at `$HOME` (or `$HOME` itself, reached while climbing) still gets
        // its own `.npu` looked up — "exclusive" means the climb never goes
        // ABOVE `$HOME`, not that `$HOME` itself is skipped.
        let candidate = dir.join(".npu");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if dir.join(".git").exists() || env.home.as_deref() == Some(dir.as_path()) {
            return None;
        }
        dir = dir.parent()?.to_path_buf();
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
/// 3. the project scope, if [`project_root`] finds one (an override, or a
///    `.npu` found by walking up from `env.cwd`).
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

    if let Some(project) = project_root(env) {
        candidates.push(project);
    }

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
pub fn roots(config_dir_override: Option<PathBuf>) -> Vec<PathBuf> {
    filter_existing_dirs(candidate_roots(&ScopeEnv::from_env(config_dir_override)))
}

/// The project scope `npu doctor` reports (an `Ok` check naming it, added
/// by `builtin::doctor::check_project_scope`), resolved exactly like
/// [`roots`] does — same override, same walk-up — but returned unfiltered
/// by existence: an override naming a directory that does not exist is
/// still worth telling the user about, since `roots()` would otherwise
/// just silently drop it.
#[must_use]
pub fn resolved_project_scope(config_dir_override: Option<PathBuf>) -> Option<PathBuf> {
    project_root(&ScopeEnv::from_env(config_dir_override))
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
            config_dir_override: None,
        }
    }

    /// A real `<dir>/.npu` directory on disk: `project_root` (unlike the
    /// old unconditional `cwd.join(".npu")`) must actually find it during
    /// the walk-up, so these tests can no longer use a purely notional
    /// path like the other `env(...)` fixtures below still do for
    /// `etc`/`xdg`/`home` (which `candidate_roots` never checks for
    /// existence itself — that is `filter_existing_dirs`' job downstream).
    fn work_dir_with_npu(name: &str) -> PathBuf {
        let work = fixture_dir(name);
        std::fs::create_dir_all(work.join(".npu")).expect("creating the fixture .npu directory");
        work
    }

    #[test]
    fn xdg_config_home_present_is_used_verbatim() {
        let work = work_dir_with_npu("xdg-present");
        let e = env(
            "/etc/npu",
            Some("/xdg"),
            Some("/home/alice"),
            &work.display().to_string(),
        );
        let roots = candidate_roots(&e);
        assert_eq!(
            roots,
            vec![
                PathBuf::from("/etc/npu"),
                PathBuf::from("/xdg/npu"),
                work.join(".npu"),
            ]
        );
    }

    #[test]
    fn xdg_absent_falls_back_to_home_dot_config() {
        let work = work_dir_with_npu("xdg-absent");
        let e = env(
            "/etc/npu",
            None,
            Some("/home/alice"),
            &work.display().to_string(),
        );
        let roots = candidate_roots(&e);
        assert_eq!(
            roots,
            vec![
                PathBuf::from("/etc/npu"),
                PathBuf::from("/home/alice/.config/npu"),
                work.join(".npu"),
            ]
        );
    }

    #[test]
    fn xdg_and_home_both_absent_yields_only_etc_and_cwd() {
        let work = work_dir_with_npu("xdg-and-home-absent");
        let e = env("/etc/npu", None, None, &work.display().to_string());
        let roots = candidate_roots(&e);
        assert_eq!(roots, vec![PathBuf::from("/etc/npu"), work.join(".npu")]);
    }

    #[test]
    fn xdg_present_replaces_home_derivation_rather_than_adding_to_it() {
        // HOME is also set, but XDG_CONFIG_HOME takes priority: only one
        // user path should appear, not both.
        let work = work_dir_with_npu("xdg-replaces-home");
        let e = env(
            "/etc/npu",
            Some("/xdg"),
            Some("/home/alice"),
            &work.display().to_string(),
        );
        let roots = candidate_roots(&e);
        assert!(!roots.contains(&PathBuf::from("/home/alice/.config/npu")));
        assert_eq!(roots.iter().filter(|p| p.ends_with("npu")).count(), 2);
    }

    #[test]
    fn order_is_general_to_local() {
        let work = work_dir_with_npu("order-general-to-local");
        let e = env(
            "/etc/npu",
            Some("/xdg"),
            Some("/home/alice"),
            &work.display().to_string(),
        );
        let roots = candidate_roots(&e);
        assert_eq!(roots[0], PathBuf::from("/etc/npu"));
        assert_eq!(roots[1], PathBuf::from("/xdg/npu"));
        assert_eq!(roots[2], work.join(".npu"));
    }

    #[test]
    fn duplicate_resolved_paths_keep_only_first_occurrence() {
        // Direct collision: xdg_config_home already equals the etc root —
        // a real case mentioned in the contract (e.g. cwd = /etc).
        let work = work_dir_with_npu("dup-xdg-collides-etc");
        let etc = PathBuf::from("/etc/npu");
        let e = env("/etc/npu", Some("/etc"), None, &work.display().to_string());
        let roots = candidate_roots(&e);
        assert_eq!(
            roots,
            vec![etc, work.join(".npu")],
            "the second occurrence of /etc/npu (via xdg) must be deduplicated, the first kept"
        );

        // Collision between the etc root and the cwd root itself: `etc`
        // and `cwd` resolve to the SAME directory, so `<cwd>/.npu` remains
        // distinct from it, but the general->local order must be preserved
        // even in this edge case.
        let dup_root = fixture_dir("dup-cwd-equals-etc");
        std::fs::create_dir_all(dup_root.join(".npu")).expect("creating .npu");
        let e2 = env(
            &dup_root.display().to_string(),
            None,
            None,
            &dup_root.display().to_string(),
        );
        let roots2 = candidate_roots(&e2);
        assert_eq!(roots2, vec![dup_root.clone(), dup_root.join(".npu")]);
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
        // real environment variables, nor on `/etc`, nor on `~/.config`,
        // to stay safe under parallel execution.
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

    // -- project_root: the walk-up search -----------------------------------

    #[test]
    fn project_root_finds_npu_at_cwd_itself() {
        let cwd = work_dir_with_npu("walkup-at-cwd");
        let e = env("/etc/npu", None, None, &cwd.display().to_string());
        assert_eq!(project_root(&e), Some(cwd.join(".npu")));
    }

    #[test]
    fn project_root_walks_up_to_a_parent_npu() {
        let root = fixture_dir("walkup-parent");
        std::fs::create_dir_all(root.join(".npu")).expect(".npu");
        let nested = root.join("src").join("deep");
        std::fs::create_dir_all(&nested).expect("nested dir");

        let e = env("/etc/npu", None, None, &nested.display().to_string());
        assert_eq!(project_root(&e), Some(root.join(".npu")));
    }

    #[test]
    fn project_root_stops_at_a_git_boundary_and_never_looks_above_it() {
        let outer = fixture_dir("walkup-git-boundary-outer");
        std::fs::create_dir_all(outer.join(".npu")).expect("outer .npu");
        let project = outer.join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        // `.git` as a FILE (the worktree case), not a directory: `exists()`
        // must still stop the climb here.
        std::fs::write(project.join(".git"), "gitdir: /elsewhere\n").expect(".git file");
        let nested = project.join("src");
        std::fs::create_dir_all(&nested).expect("nested dir");

        let e = env("/etc/npu", None, None, &nested.display().to_string());
        assert_eq!(
            project_root(&e),
            None,
            "the outer .npu must stay invisible past the .git boundary"
        );
    }

    #[test]
    fn project_root_never_searches_above_home() {
        // `home`'s PARENT is private to this test (a fresh fixture
        // directory, never `target/test-fixtures` itself, which every
        // fixture shares): writing `.npu` there must not leak into any
        // other test's walk-up.
        let isolated_parent = fixture_dir("walkup-home-exclusive");
        let home = isolated_parent.join("home");
        std::fs::create_dir_all(&home).expect("home dir");
        // `.npu` sits ABOVE home: only visible if the climb were allowed to
        // go past `$HOME`, which it must never do.
        std::fs::create_dir_all(isolated_parent.join(".npu")).expect("above-home .npu");
        let nested = home.join("projects").join("x");
        std::fs::create_dir_all(&nested).expect("nested dir");

        // Starting exactly AT home with no `.npu` there: the climb must
        // stop, never look at home's own parent.
        let at_home = env(
            "/etc/npu",
            None,
            Some(&home.display().to_string()),
            &home.display().to_string(),
        );
        assert_eq!(project_root(&at_home), None);

        // Starting below home, with no `.npu` between `nested` and `home`:
        // the climb must stop AT home, never search above it.
        let below_home = env(
            "/etc/npu",
            None,
            Some(&home.display().to_string()),
            &nested.display().to_string(),
        );
        assert_eq!(project_root(&below_home), None);
    }

    /// `$HOME` itself is not excluded from the search: only climbing PAST
    /// it is refused. A `cwd` that happens to equal `$HOME` (or reaches it
    /// while climbing) still gets its own `.npu` looked up there.
    #[test]
    fn project_root_still_finds_npu_at_home_itself() {
        let home = fixture_dir("walkup-home-not-skipped");
        std::fs::create_dir_all(home.join(".npu")).expect("home .npu");

        let at_home = env(
            "/etc/npu",
            None,
            Some(&home.display().to_string()),
            &home.display().to_string(),
        );
        assert_eq!(project_root(&at_home), Some(home.join(".npu")));
    }

    #[test]
    fn project_root_config_dir_override_wins_and_skips_the_walk() {
        let cwd = fixture_dir("walkup-override");
        // Deliberately no `.npu` anywhere near `cwd`: the override alone
        // decides the result.
        let mut e = env("/etc/npu", None, None, &cwd.display().to_string());
        let overridden = PathBuf::from("/somewhere/declared/.npu");
        e.config_dir_override = Some(overridden.clone());
        assert_eq!(project_root(&e), Some(overridden));
    }

    #[test]
    fn config_dir_from_args_reads_the_value() {
        assert_eq!(
            config_dir_from_args(["npu", "--config-dir", "/x", "y"]),
            Some(PathBuf::from("/x"))
        );
        assert_eq!(
            config_dir_from_args(["npu", "--config-dir=/x", "y"]),
            Some(PathBuf::from("/x"))
        );
        assert_eq!(config_dir_from_args(["npu", "y"]), None);
    }
}
