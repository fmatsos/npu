//! The `port`/`{{ backend.port }}` machinery: an explicit number, the
//! `"auto"` keyword, and the substitution that resolves the placeholder
//! before anything downstream ever sees it.

use serde::Deserialize;
use std::path::Path;

use super::backend::Backend;
use super::runtime::{
    RUNTIME_DOCKER, docker_of, docker_of_mut, docker_reads_port, process_of, process_of_mut,
    process_reads_port,
};

/// The value of a backend's optional `port` key: an explicit number, or the
/// keyword `"auto"`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Port {
    Fixed(u16),
    /// Any string; only `"auto"` is accepted, checked by [`resolve_port`]
    /// so that a typo names its file instead of being treated as an unknown
    /// type by serde.
    Keyword(String),
}

/// The keyword accepted by `port`.
pub(crate) const PORT_AUTO: &str = "auto";

/// The value substituted for `{{ backend.port }}` in the `[docker]` lists of
/// a `port = "auto"` backend: Docker's own "pick a free one".
///
/// Allocation is delegated to the kernel rather than derived or probed
/// because `npu` keeps no state between processes: `npu serve` and the
/// `npu <command>` that follows minutes later, in another process, must
/// agree on a port. Deriving one (a hash of the id) agrees but can collide
/// with an unrelated service; probing for a free one does not agree at all,
/// since by request time the port is occupied — by us. Letting Docker
/// allocate and then ASKING it what it allocated is the only variant that is
/// both collision-free and reproducible, at the cost of making Docker a
/// prerequisite for executing commands on such a backend, not just for its
/// lifecycle.
pub(crate) const DOCKER_EPHEMERAL_PORT: u16 = 0;

/// The only `{{ backend.<name> }}` placeholder that exists.
const BACKEND_PLACEHOLDER: &str = "backend.port";

/// Replaces every `{{ backend.port }}` in `template` with `port`, and reports
/// whether it replaced anything.
///
/// Handled HERE rather than through `prompt.rs`: this placeholder has no
/// meaning in a command file, so the prompt engine stays unaware of it and
/// `validate_docker_template` stays as it is. A fixed `port` is substituted
/// at load time; a `port = "auto"` base URL keeps its placeholder until
/// `builtin::resolve_base_url` reads the real value back from Docker.
pub(crate) fn substitute_port(template: &str, port: u16) -> (String, bool) {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    let mut substituted = false;

    while let Some(open) = rest.find("{{") {
        let after_open = &rest[open + 2..];
        let Some(close) = after_open.find("}}") else {
            break;
        };
        if after_open[..close].trim() != BACKEND_PLACEHOLDER {
            out.push_str(&rest[..open + 2]);
            rest = after_open;
            continue;
        }
        out.push_str(&rest[..open]);
        out.push_str(&port.to_string());
        rest = &after_open[close + 2..];
        substituted = true;
    }

    out.push_str(rest);
    (out, substituted)
}

/// Substitutes `{{ backend.port }}` throughout a backend, and enforces that
/// `port` and the placeholder are declared together.
///
/// Both directions are configuration errors naming the file: a placeholder
/// without a `port` cannot be resolved, and a `port` no placeholder reads is
/// a key that would be silently ignored.
pub(crate) fn resolve_port(backend: &mut Backend, source: &Path) -> crate::Result<()> {
    let references_port = |backend: &Backend| {
        substitute_port(&backend.base_url, 0).1
            || docker_of(backend).is_some_and(docker_reads_port)
            || process_of(backend).is_some_and(process_reads_port)
    };

    let port = match &backend.port {
        None => {
            if references_port(backend) {
                return Err(crate::Error::Config(crate::error::ConfigError::in_file(
                    source,
                    Some(&backend.id),
                    format!(
                        "backend \"{}\": references {{{{ {BACKEND_PLACEHOLDER} }}}} but \
                         declares no \"port\" key",
                        backend.id
                    ),
                )));
            }
            return Ok(());
        }
        Some(Port::Fixed(0)) => {
            return Err(crate::Error::Config(crate::error::ConfigError::in_file(
                source,
                Some(&backend.id),
                format!(
                    "backend \"{}\": port 0 is not a port (\"{PORT_AUTO}\" lets Docker \
                     allocate one instead)",
                    backend.id
                ),
            )));
        }
        Some(Port::Fixed(number)) => *number,
        Some(Port::Keyword(keyword)) if keyword == PORT_AUTO => {
            // `auto` means "Docker allocates, npu asks it back", so both
            // halves must exist: something to start, and a base URL whose
            // port can be filled in afterwards.
            if docker_of(backend).is_none() {
                return Err(crate::Error::Config(crate::error::ConfigError::in_file(
                    source,
                    Some(&backend.id),
                    format!(
                        "backend \"{}\": port = \"{PORT_AUTO}\" requires a Docker runtime \
                         ([runtime] type = \"{RUNTIME_DOCKER}\") — npu can only read back a port it \
                         asked Docker to allocate",
                        backend.id
                    ),
                )));
            }
            // BOTH sides must read the placeholder, for the same reason:
            // without it in [docker] nothing is published, and without it in
            // base_url nothing reaches what was published. Either way the
            // allocated port is unreachable — a failure that would only
            // surface at the first command, as advice ("start it with npu
            // serve") that could never work.
            let in_docker = docker_of(backend).is_some_and(docker_reads_port);
            if !substitute_port(&backend.base_url, 0).1 || !in_docker {
                return Err(crate::Error::Config(crate::error::ConfigError::in_file(
                    source,
                    Some(&backend.id),
                    format!(
                        "backend \"{}\": port = \"{PORT_AUTO}\" requires \
                         {{{{ {BACKEND_PLACEHOLDER} }}}} in BOTH base_url and [docker], otherwise \
                         the allocated port is never published or never reached",
                        backend.id
                    ),
                )));
            }
            DOCKER_EPHEMERAL_PORT
        }
        Some(Port::Keyword(keyword)) => {
            return Err(crate::Error::Config(crate::error::ConfigError::in_file(
                source,
                Some(&backend.id),
                format!(
                    "backend \"{}\": port \"{keyword}\" is neither a number nor \
                     \"{PORT_AUTO}\"",
                    backend.id
                ),
            )));
        }
    };

    let mut used = false;

    // A `port = "auto"` base URL keeps its placeholder: the value is only
    // known once Docker has allocated it, so substituting 0 here would send
    // every request to port 0. Its `used` bookkeeping is already settled by
    // the stricter both-sides check above.
    if backend.uses_auto_port() {
        used = true;
    } else {
        let (base_url, hit) = substitute_port(&backend.base_url, port);
        backend.base_url = base_url;
        used |= hit;
    }

    if let Some(docker) = docker_of_mut(backend) {
        for template in std::iter::once(&mut docker.image)
            .chain(&mut docker.options)
            .chain(&mut docker.args)
        {
            let (resolved, hit) = substitute_port(template, port);
            *template = resolved;
            used |= hit;
        }
    }

    if let Some(process) = process_of_mut(backend) {
        for template in process.arguments.iter_mut().chain(process.env.values_mut()) {
            let (resolved, hit) = substitute_port(template, port);
            *template = resolved;
            used |= hit;
        }
    }

    if !used {
        return Err(crate::Error::Config(crate::error::ConfigError::in_file(
            source,
            Some(&backend.id),
            format!(
                "backend \"{}\": declares \"port\" but never references \
                 {{{{ {BACKEND_PLACEHOLDER} }}}}, so the value would be ignored",
                backend.id
            ),
        )));
    }

    Ok(())
}
