//! The TCP reachability probe `doctor`'s [`super::doctor::Probes::backend`]
//! injects, and the purely syntactic `base_url` parsing it is built on.

use std::net::ToSocketAddrs;

use crate::runtime::PROBE_TIMEOUT;

/// Extracts `(host, port)` from a base URL `http(s)://host[:port][/...]`.
/// Falls back to the scheme's implicit
/// port (80 for `http`, 443 for `https`) when no explicit port is
/// present. PURELY SYNTACTIC: never touches the network, only the
/// `base_url` string itself — it's [`tcp_probe`] that opens the
/// connection.
///
/// **Bracketed IPv6 notation** (`[::1]` or `[::1]:8000`, RFC 3986
/// section 3.2.2) handled separately, BEFORE the general `rsplit_once(':')`: a
/// bare IPv6 address itself contains `:` characters, so a plain
/// `rsplit_once(':')` would cut `[::1]:8000` on the last `:` inside the
/// brackets rather than on the host/port separator. `host` is returned
/// WITHOUT the brackets (`"::1"`, not `"[::1]"`): `Ipv6Addr::from_str`,
/// used by `ToSocketAddrs` in [`tcp_probe`], rejects the bracketed form
/// — keeping it would make any IPv6 resolution fail with a spurious DNS
/// error, never with the invalid-port message one would expect.
pub(crate) fn parse_host_port(base_url: &str) -> Result<(String, u16), String> {
    let Some((scheme, rest)) = base_url.split_once("://") else {
        return Err(format!(
            "base_url \"{base_url}\": missing scheme (expected \"http://\" or \"https://\")"
        ));
    };

    let default_port: u16 = match scheme {
        "http" => 80,
        "https" => 443,
        other => {
            return Err(format!(
                "base_url \"{base_url}\": unsupported scheme \"{other}\" (expected \"http\" or \
                 \"https\")"
            ));
        }
    };

    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if authority.is_empty() {
        return Err(format!("base_url \"{base_url}\": missing host"));
    }

    if let Some(after_bracket) = authority.strip_prefix('[') {
        return parse_ipv6_authority(base_url, after_bracket, default_port);
    }

    match authority.rsplit_once(':') {
        Some((host, port_text)) if !host.is_empty() => {
            let port: u16 = port_text
                .parse()
                .map_err(|_| format!("base_url \"{base_url}\": invalid port \"{port_text}\""))?;
            Ok((host.to_string(), port))
        }
        _ => Ok((authority.to_string(), default_port)),
    }
}

/// Complements [`parse_host_port`] for bracketed IPv6 notation:
/// `after_bracket` is what follows the opening `[` already consumed by
/// the caller (e.g. `"::1]:8000"` for `"[::1]:8000"`). Isolated in its
/// own function because [`parse_host_port`] already has two levels of
/// `match` / early-return; nesting it in place would hurt the
/// readability that `clippy::pedantic` (`too_many_lines`) would
/// otherwise penalize.
fn parse_ipv6_authority(
    base_url: &str,
    after_bracket: &str,
    default_port: u16,
) -> Result<(String, u16), String> {
    let Some(end) = after_bracket.find(']') else {
        return Err(format!(
            "base_url \"{base_url}\": unclosed opening IPv6 bracket \"[\""
        ));
    };
    // `end` points at `]` (ASCII, 1 byte): the two split bounds below
    // therefore always fall on a character boundary, whatever
    // `base_url`'s content around these brackets.
    let host = &after_bracket[..end];
    if host.is_empty() {
        return Err(format!(
            "base_url \"{base_url}\": empty IPv6 address between brackets"
        ));
    }
    let trailer = &after_bracket[end + 1..];

    match trailer.strip_prefix(':') {
        Some(port_text) if !port_text.is_empty() => port_text
            .parse()
            .map(|port| (host.to_string(), port))
            .map_err(|_| format!("base_url \"{base_url}\": invalid port \"{port_text}\"")),
        Some(_) => Err(format!("base_url \"{base_url}\": missing port after \":\"")),
        None if trailer.is_empty() => Ok((host.to_string(), default_port)),
        None => Err(format!(
            "base_url \"{base_url}\": unexpected characters after the IPv6 address \
             (\"{trailer}\")"
        )),
    }
}

/// Tests a backend's reachability with a TCP connection to its
/// `base_url`, with a short timeout
/// ([`PROBE_TIMEOUT`]), then closes it immediately. NO HTTP request: a
/// `POST` on the `chat` operation would actually invoke the model, an
/// unacceptable side effect for a diagnostic command — this function
/// therefore only opens and closes a socket, never writing or reading a
/// single byte on it. The error message says "unreachable" on the
/// `doctor` side (via the label `"reachable"` — cf.
/// `check_backends_reachable`) and never "available": only a
/// socket's acceptance is checked, not the model's ability to respond.
///
/// # Errors
///
/// Returns `Err` if `base_url` does not have the expected shape, if
/// resolving the host/port pair fails, or if the connection itself
/// fails or times out.
pub fn tcp_probe(base_url: &str) -> Result<(), String> {
    let (host, port) = parse_host_port(base_url)?;

    let mut addrs = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|err| format!("address resolution for \"{host}:{port}\" failed: {err}"))?;

    let addr = addrs
        .next()
        .ok_or_else(|| format!("address resolution for \"{host}:{port}\" produced no address"))?;

    std::net::TcpStream::connect_timeout(&addr, PROBE_TIMEOUT)
        .map(|_stream| ())
        .map_err(|err| format!("TCP connection to \"{host}:{port}\" failed: {err}"))
}
