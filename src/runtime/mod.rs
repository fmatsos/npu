//! Runtime families: how a backend's server is brought up, asked about and
//! torn down by `serve`, `stop`, `status` and `logs`.
//!
//! `builtin.rs` keeps the ORCHESTRATION — resolve the model, pick the
//! family, format the report — and this module keeps everything that
//! actually touches the outside world. `std::process::Command` appears
//! nowhere else in the crate.
//!
//! Dispatch is a `match` on [`crate::config::Runtime`], never a trait
//! object: the set of families is closed and declared in `config.rs`, there
//! is one call site per lifecycle command, and a `match` turns a new variant
//! into a compile error at every site that must handle it — which an
//! `Option<Box<dyn RuntimeDriver>>` would turn into a silent default.

use std::time::Duration;

pub mod docker;

/// Budget granted to a `doctor` probe: [`crate::builtin::tcp_probe`] before
/// it declares a backend unreachable, and [`docker::probe`] before it
/// declares the container runtime unusable.
///
/// Short by design (point 4 of the shared contract): `doctor` is a
/// diagnostic command meant to stay fast even when several backends are
/// queried, never meant to wait out a full network timeout — and an
/// unreachable backend or a wedged container daemon is precisely the case
/// `doctor` exists to REPORT, not to wait on.
///
/// ONE number, here, rather than one per probe: `doctor`'s responsiveness is
/// a single property, and two constants would drift apart.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Polling interval of the probes that have no blocking wait with a timeout
/// of their own (`std::process` offers none). Short enough to stay
/// imperceptible, long enough not to spin a core.
pub(crate) const PROBE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// The family name shown in the `RUNTIME` column of `npu status`, and the
/// value a backend writes as `type` in its `[runtime]` table.
#[must_use]
pub fn label(runtime: &crate::config::Runtime) -> &'static str {
    match runtime {
        crate::config::Runtime::Docker(_) => docker::NAME,
    }
}

/// Is `port` free to bind on the loopback interface?
///
/// ponytail: binds and drops rather than consulting the OS's socket table —
/// binding IS the question being asked, and it needs no dependency and no
/// injection, so `serve`'s tests exercise it without Docker.
#[must_use]
pub(crate) fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Completes `backend`'s `base_url` when its port was allocated by its
/// runtime, and hands back the declared one otherwise.
///
/// The dispatch point every caller goes through — `lib.rs` before a request,
/// `builtin::status` before a report — so that a backend with no runtime at
/// all (the common case: a server someone else started) costs nothing and
/// asks nobody.
///
/// # Errors
///
/// Whatever the family returns; `Error::Backend` for Docker, because a port
/// that cannot be read back means the runtime is not up, which is exit `3`,
/// never `2`.
pub fn resolve_base_url(
    backend: &crate::config::Backend,
    runner: &dyn Fn(&[String]) -> crate::Result<String>,
) -> crate::Result<String> {
    match backend.runtime() {
        Some(crate::config::Runtime::Docker(_)) => docker::resolve_base_url(backend, runner),
        None => Ok(backend.base_url.clone()),
    }
}
