//! Routes the selected command path to one of the built-ins, or to a
//! business command, through an exhaustive `match`: adding a route without
//! updating both `match`es in [`crate::run`] is a compile error, never a
//! silently unhandled path.

/// One selected command path, classified by [`route_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// `doctor` and `config check`: the same report under two names.
    Doctor,
    /// `help <path…>`.
    Help,
    /// `update`.
    Update,
    /// `model discover`.
    ModelDiscover,
    /// `describe <path…>`: a built-in first, a business command otherwise.
    Describe,
    /// `config models`.
    ConfigModels,
    /// `backend serve <MODEL>`.
    BackendServe,
    /// `backend stop <MODEL>`.
    BackendStop,
    /// `backend status`.
    BackendStatus,
    /// `backend logs <MODEL>`.
    BackendLogs,
    /// `backend tune`.
    BackendTune,
    /// Any other path: a configured business command.
    Business,
}

/// Classifies the selected command path exactly as the chain of `if route ==
/// [...]` comparisons it replaces did: same paths, same precedence (`describe`
/// only ever produces [`Route::Describe`], resolved between the built-in and
/// the business command by the caller).
pub(crate) fn route_for(path: &[&str]) -> Route {
    match path {
        ["doctor"] | ["config", "check"] => Route::Doctor,
        ["help"] => Route::Help,
        ["update"] => Route::Update,
        ["model", "discover"] => Route::ModelDiscover,
        ["describe"] => Route::Describe,
        ["config", "models"] => Route::ConfigModels,
        ["backend", "serve"] => Route::BackendServe,
        ["backend", "stop"] => Route::BackendStop,
        ["backend", "status"] => Route::BackendStatus,
        ["backend", "logs"] => Route::BackendLogs,
        ["backend", "tune"] => Route::BackendTune,
        _ => Route::Business,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_paths_map_to_their_route() {
        assert_eq!(route_for(&["doctor"]), Route::Doctor);
        assert_eq!(route_for(&["config", "check"]), Route::Doctor);
        assert_eq!(route_for(&["help"]), Route::Help);
        assert_eq!(route_for(&["update"]), Route::Update);
        assert_eq!(route_for(&["model", "discover"]), Route::ModelDiscover);
        assert_eq!(route_for(&["describe"]), Route::Describe);
        assert_eq!(route_for(&["config", "models"]), Route::ConfigModels);
        assert_eq!(route_for(&["backend", "serve"]), Route::BackendServe);
        assert_eq!(route_for(&["backend", "stop"]), Route::BackendStop);
        assert_eq!(route_for(&["backend", "status"]), Route::BackendStatus);
        assert_eq!(route_for(&["backend", "logs"]), Route::BackendLogs);
        assert_eq!(route_for(&["backend", "tune"]), Route::BackendTune);
    }

    #[test]
    fn an_unknown_or_configured_path_is_business() {
        assert_eq!(route_for(&["git", "review"]), Route::Business);
        assert_eq!(route_for(&[]), Route::Business);
    }
}
