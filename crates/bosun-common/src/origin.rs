use std::net::IpAddr;

/// The origin at which a session's web UI and API are served, derived from
/// the control-plane URL. A session lives at
/// `<session-id>.<control-plane-host>` per
/// `docs/adrs/2026-08-22-session-subdomains.md`. Loopback hosts use the
/// `.localhost` domain, which every browser and operating system resolves to
/// 127.0.0.1 without DNS or a hosts file. Returns `None` when the control
/// plane is only reachable at a non-loopback IP address, where no wildcard DNS
/// exists and the session cannot have a subdomain.
pub fn session_origin(cp_url: &str, session_id: &str) -> Option<String> {
    let url = cp_url.trim_end_matches('/');
    let (scheme, authority) = url.split_once("://")?;
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    let base = match host {
        "localhost" | "127.0.0.1" | "[::1]" => "localhost",
        _ if host.parse::<IpAddr>().is_ok() => return None,
        _ => host,
    };
    let port_suffix = port.map(|port| format!(":{port}")).unwrap_or_default();
    Some(format!("{scheme}://{session_id}.{base}{port_suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_origin_for_dns_hosts() {
        assert_eq!(
            session_origin("https://bosun.on.21cs.biz", "abc-123").as_deref(),
            Some("https://abc-123.bosun.on.21cs.biz")
        );
    }

    #[test]
    fn session_origin_keeps_the_port() {
        assert_eq!(
            session_origin("http://bosun.example.com:8090", "abc-123").as_deref(),
            Some("http://abc-123.bosun.example.com:8090")
        );
    }

    #[test]
    fn session_origin_uses_localhost_for_loopback() {
        assert_eq!(
            session_origin("http://127.0.0.1:8090", "abc-123").as_deref(),
            Some("http://abc-123.localhost:8090")
        );
        assert_eq!(
            session_origin("http://localhost:8090", "abc-123").as_deref(),
            Some("http://abc-123.localhost:8090")
        );
    }

    #[test]
    fn session_origin_trims_a_trailing_slash() {
        assert_eq!(
            session_origin("https://bosun.on.21cs.biz/", "abc-123").as_deref(),
            Some("https://abc-123.bosun.on.21cs.biz")
        );
    }

    #[test]
    fn session_origin_is_none_for_non_loopback_ips() {
        assert_eq!(session_origin("http://192.168.1.10:8090", "abc-123"), None);
    }
}
