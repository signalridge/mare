use super::*;

pub fn is_loopback_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    }
}

/// Returns true only for loopback hostnames/IPs (`127.0.0.0/8`, `::1`, `localhost`).
pub fn is_loopback_host(host: &str) -> bool {
    let trimmed = host.trim().trim_start_matches('[').trim_end_matches(']');
    if trimmed.eq_ignore_ascii_case("localhost") {
        return true;
    }
    trimmed.parse::<IpAddr>().is_ok_and(is_loopback_ip)
}

/// Parses and normalizes an HTTP `Host` header or `trusted_hosts` entry (`host` or `host:port`,
/// with bracketed IPv6 support). Returns `(normalized_host, optional_port)`.
pub fn parse_host_header(raw: &str) -> Option<(String, Option<u16>)> {
    let s = raw.trim();
    if s.is_empty() || s.chars().any(|c| c.is_control() || c.is_whitespace()) || s.contains(['/', '@', '?', '#']) {
        return None;
    }
    if let Some(rest) = s.strip_prefix('[') {
        let (addr, after) = rest.split_once(']')?;
        let port = if after.is_empty() {
            None
        } else {
            let p = after.strip_prefix(':')?.parse::<u16>().ok()?;
            Some(p)
        };
        let v6 = addr.parse::<Ipv6Addr>().ok()?;
        return Some((v6.to_string(), port));
    }
    let (host_part, port) = match s.rsplit_once(':') {
        Some((h, p)) => {
            if h.contains(':') {
                return None;
            }
            let port = p.parse::<u16>().ok()?;
            (h, Some(port))
        }
        None => (s, None),
    };
    if host_part.is_empty() {
        return None;
    }
    if let Ok(v4) = host_part.parse::<Ipv4Addr>() {
        return Some((v4.to_string(), port));
    }
    Some((host_part.to_ascii_lowercase(), port))
}

/// Validates an HTTP `Host` header value (with optional `:port`) as a loopback name or IP.
/// Rejects non-loopback hosts, malformed ports, and unbracketed IPv6 literals (DNS rebinding protection).
pub fn is_valid_loopback_host_header(raw: &str) -> bool {
    let Some((host, _)) = parse_host_header(raw) else {
        return false;
    };
    is_loopback_host(&host)
}

pub fn is_valid_host_header(raw: &str, trusted_hosts: &[(String, Option<u16>)]) -> bool {
    let Some((host, port)) = parse_host_header(raw) else {
        return false;
    };
    if is_loopback_host(&host) {
        return true;
    }
    trusted_hosts.iter().any(|(th, tp)| *th == host && (tp.is_none() || *tp == port))
}

/// Validate and normalize a list of `trusted_hosts` config entries.
pub fn parse_trusted_hosts(entries: &[String]) -> Result<Vec<(String, Option<u16>)>, String> {
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(parsed) = parse_host_header(entry) else {
            return Err(format!("trusted_hosts: invalid host entry '{entry}'"));
        };
        out.push(parsed);
    }
    Ok(out)
}
