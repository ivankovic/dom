/*  This file is part of the Dom smarthome app.
 *
 *  Copyright (C) 2026 Marko Ivankovic
 *
 *  Licensed under the Prosperity Public License 3.0.0: free to use and share
 *  for noncommercial purposes, and free to try for commercial purposes for
 *  thirty days. Continued commercial use requires a license negotiated with
 *  the contributor.
 *
 *  Contributor: Marko Ivankovic <marko@ivankovic.me>
 *  Source Code: https://github.com/ivankovic/dom
 *
 *  See the LICENSE file for the full terms.
 *
 *  As far as the law allows, this software comes as is, without any warranty
 *  or condition, and the contributor won't be liable to anyone for any
 *  damages related to this software or this license, under any kind of legal
 *  claim.
 */

use std::net::IpAddr;

use sqlx::{Row, SqlitePool};

pub const NAME: &str = "Dom";

#[derive(Debug, Clone)]
pub struct DomLocalDevice {
    pub ip: IpAddr,
    pub name: String,
}

/// Detect if this is the local Dom machine itself
pub fn detect_local_ips() -> Vec<IpAddr> {
    use if_addrs::{IfAddr, get_if_addrs};

    let mut local_ips = Vec::new();

    if let Ok(ifaces) = get_if_addrs() {
        for iface in ifaces {
            if iface.is_loopback() {
                continue;
            }
            match iface.addr {
                IfAddr::V4(v4) => {
                    if !v4.ip.is_loopback() {
                        local_ips.push(IpAddr::V4(v4.ip));
                    }
                }
                IfAddr::V6(v6) => {
                    if !v6.ip.is_loopback() {
                        local_ips.push(IpAddr::V6(v6.ip));
                    }
                }
            }
        }
    }

    local_ips
}

/// Save the local machine as a device with the name "Dom"
pub async fn save_device(pool: &SqlitePool, ip: IpAddr) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR IGNORE INTO Devices (type, name, ip, poll_interval_secs) VALUES (?, ?, ?, ?)",
    )
    .bind("dom_local")
    .bind(NAME)
    .bind(ip.to_string())
    .bind(10)
    .execute(pool)
    .await?;

    // Also update the name to "Dom" in case it already exists with a different name
    sqlx::query("UPDATE Devices SET name = ?, type = ? WHERE ip = ?")
        .bind(NAME)
        .bind("dom_local")
        .bind(ip.to_string())
        .execute(pool)
        .await?;

    Ok(())
}

/// Load all configured local devices
pub async fn load_all(pool: &SqlitePool) -> Result<Vec<DomLocalDevice>, sqlx::Error> {
    let rows = sqlx::query("SELECT ip, name FROM Devices WHERE type = 'dom_local'")
        .fetch_all(pool)
        .await?;

    let mut devices = Vec::new();
    for row in rows {
        let ip_str: String = row.get("ip");
        let name: String = row.get("name");
        if let Ok(ip) = ip_str.parse::<IpAddr>() {
            devices.push(DomLocalDevice { ip, name });
        }
    }
    Ok(devices)
}

/// Check if the given IP is one of our local IPs
pub fn is_local_ip(ip: IpAddr) -> bool {
    let local_ips = detect_local_ips();
    local_ips.contains(&ip)
}

/// Check if the given fingerprint represents the local machine
pub fn detect(_fp: &crate::fingerprint::Fingerprint) -> bool {
    // The local machine detection is handled separately via IP detection
    // rather than fingerprinting, since we know our own IPs
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_local_ips() {
        let local_ips = detect_local_ips();
        // Should return at least one non-loopback IP
        assert!(!local_ips.is_empty() || std::env::var("CI").is_ok()); // May be empty in CI environments

        // All IPs should be non-loopback
        for ip in &local_ips {
            assert!(!ip.is_loopback());
        }
    }

    #[test]
    fn test_is_local_ip() {
        let local_ips = detect_local_ips();

        // Test that our own IPs are detected as local
        for ip in local_ips {
            assert!(is_local_ip(ip));
        }

        // Test that loopback is not considered local
        assert!(!is_local_ip(IpAddr::from([127, 0, 0, 1])));
    }
}
