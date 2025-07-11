use anyhow::{Result, anyhow};
use ipnet::IpNet;
use std::net::IpAddr;

/// Parses the original client IP from X-Forwarded-For header
/// Format: "client, proxy1, proxy2, ..." - returns the leftmost (original client) IP
#[must_use]
pub fn parse_original_client_ip(xff_header: &str) -> Option<String> {
    xff_header
        .split(',')
        .next()
        .map(|ip| ip.trim().to_string())
        .filter(|ip| !ip.is_empty())
}

/// Checks if a proxy IP address is allowed based on the configured allowlist
/// Returns true if no allowlist is configured (allow all) or if IP matches any entry
pub fn is_proxy_ip_allowed(proxy_ip: IpAddr, allowed_ips: Option<&Vec<String>>) -> Result<bool> {
    let Some(allowed_list) = allowed_ips else {
        return Ok(true); // No restrictions configured
    };

    for allowed_entry in allowed_list {
        // Try parsing as individual IP address first
        if let Ok(allowed_ip) = allowed_entry.parse::<IpAddr>() {
            if proxy_ip == allowed_ip {
                return Ok(true);
            }
        }
        // Try parsing as CIDR subnet
        else if let Ok(allowed_net) = allowed_entry.parse::<IpNet>() {
            if allowed_net.contains(&proxy_ip) {
                return Ok(true);
            }
        } else {
            return Err(anyhow!(
                "Invalid IP address or CIDR in allowed_proxy_ips: {}",
                allowed_entry
            ));
        }
    }

    Ok(false)
}
