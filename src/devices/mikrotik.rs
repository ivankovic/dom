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

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};
use sqlx::{Row, SqlitePool};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{MissedTickBehavior, interval, timeout};

use crate::app::{ConnStatus, MikrotikReading, SharedState};
use crate::fingerprint::Fingerprint;

pub const NAME: &str = "MikroTik RouterOS";
pub const API_PORT: u16 = 80;
const DHCP_LEASE_PATH: &str = "/rest/ip/dhcp-server/lease";
const FIREWALL_FILTER_PATH: &str = "/rest/ip/firewall/filter";
const DEFAULT_POLL_SECS: i64 = 10;
/// The modem's Internet-facing interface, shown in the Network view as
/// "Internet traffic". Only the modem reports this name; fetches against
/// other MikroTik devices (router, APs) simply come back empty and are
/// skipped.
const INTERNET_INTERFACE: &str = "lte1";

/// RouterOS's webfig login page always embeds this copyright string, unique
/// enough among LAN devices that we don't need to also gate on the API ports.
pub fn detect(fp: &Fingerprint) -> bool {
    if !fp.open_ports.contains(&80) {
        return false;
    }
    fp.http.iter().any(|p| p.raw.contains("MikroTik"))
}

// ── API response ──────────────────────────────────────────────────────────────

/// RouterOS's REST API renders most numeric properties as JSON strings for
/// backwards compatibility with the classic API. Accept either form.
fn str_or_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = u64;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a string or integer")
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<u64, E> {
            Ok(v)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<u64, E> {
            Ok(v.max(0) as u64)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
            v.parse().map_err(de::Error::custom)
        }
    }
    deserializer.deserialize_any(V)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DhcpLease {
    pub address: String,
    #[serde(rename = "mac-address", default)]
    pub mac_address: String,
    #[serde(rename = "host-name", default)]
    pub host_name: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FirewallRule {
    #[serde(default)]
    pub chain: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(rename = "bytes", deserialize_with = "str_or_u64", default)]
    pub bytes: u64,
    #[serde(rename = "packets", deserialize_with = "str_or_u64", default)]
    pub packets: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InterfaceStats {
    #[serde(rename = "rx-byte", deserialize_with = "str_or_u64", default)]
    pub rx_byte: u64,
    #[serde(rename = "tx-byte", deserialize_with = "str_or_u64", default)]
    pub tx_byte: u64,
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = (b0 as u32) << 16 | (b1 as u32) << 8 | b2 as u32;
        out.push(TABLE[(n >> 18 & 0x3F) as usize] as char);
        out.push(TABLE[(n >> 12 & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6 & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

async fn rest_get(
    ip: IpAddr,
    port: u16,
    path: &str,
    username: &str,
    password: &str,
) -> anyhow::Result<String> {
    let addr = SocketAddr::new(ip, port);
    let mut stream = timeout(Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .context("connect timeout")?
        .context("connect failed")?;

    let auth = base64_encode(format!("{username}:{password}").as_bytes());
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {ip}\r\nAuthorization: Basic {auth}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .await
        .context("send request")?;

    let mut buf = Vec::new();
    timeout(Duration::from_secs(10), stream.read_to_end(&mut buf))
        .await
        .context("read timeout")?
        .context("read failed")?;

    let raw = String::from_utf8_lossy(&buf);
    let status = raw
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok());
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or(&raw).trim();

    if status != Some(200) {
        let code = status
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".to_string());
        anyhow::bail!("HTTP {code} from RouterOS: {body}");
    }
    Ok(body.to_string())
}

pub async fn fetch_dhcp_leases(
    ip: IpAddr,
    port: u16,
    username: &str,
    password: &str,
) -> anyhow::Result<Vec<DhcpLease>> {
    let body = rest_get(ip, port, DHCP_LEASE_PATH, username, password).await?;
    serde_json::from_str(&body).context("parse JSON")
}

pub async fn fetch_firewall_rules(
    ip: IpAddr,
    port: u16,
    username: &str,
    password: &str,
) -> anyhow::Result<Vec<FirewallRule>> {
    let body = rest_get(ip, port, FIREWALL_FILTER_PATH, username, password).await?;
    serde_json::from_str(&body).context("parse JSON")
}

/// Fetches rx/tx byte counters for `INTERNET_INTERFACE`. Returns `None` if
/// the device has no such interface (e.g. the router or an AP, rather than
/// the modem) rather than treating that as a poll failure.
pub async fn fetch_internet_traffic(
    ip: IpAddr,
    port: u16,
    username: &str,
    password: &str,
) -> anyhow::Result<Option<InterfaceStats>> {
    let path = format!("/rest/interface?name={INTERNET_INTERFACE}");
    let body = rest_get(ip, port, &path, username, password).await?;
    let mut stats: Vec<InterfaceStats> = serde_json::from_str(&body).context("parse JSON")?;
    Ok(if stats.is_empty() {
        None
    } else {
        Some(stats.remove(0))
    })
}

// ── Database ──────────────────────────────────────────────────────────────────

pub struct DeviceRecord {
    pub id: i64,
    pub ip: IpAddr,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub poll_interval_secs: i64,
    pub label: Option<String>,
}

/// Registers a discovered device. Credentials are never known at scan time —
/// they're configured separately (see `db::set_device_login`) and preserved
/// across rescans; only name/poll_interval are touched here. This also means
/// different MikroTik devices (router, APs, ...) can each hold their own
/// username/password without any of it passing through source code.
/// `fingerprint` (e.g. a MAC address) lets a device that changed IP be
/// recognized and migrated in place, credentials and all, rather than
/// registered as a second device with no login — see `db::upsert_device`.
pub async fn save_device(
    pool: &SqlitePool,
    ip: IpAddr,
    name: &str,
    fingerprint: Option<&str>,
) -> anyhow::Result<()> {
    crate::db::upsert_device(pool, "mikrotik", name, ip, DEFAULT_POLL_SECS, fingerprint).await
}

pub async fn load_all(pool: &SqlitePool) -> anyhow::Result<Vec<DeviceRecord>> {
    let rows = sqlx::query(
        "SELECT id, ip, username, password, poll_interval_secs, label
         FROM Devices WHERE type = 'mikrotik'",
    )
    .fetch_all(pool)
    .await?;

    let mut out = Vec::new();
    for row in rows {
        let ip_str: String = row.get("ip");
        let ip = ip_str
            .parse::<IpAddr>()
            .with_context(|| format!("bad IP in DB: {ip_str}"))?;
        let label: Option<String> = row.get("label");
        out.push(DeviceRecord {
            id: row.get("id"),
            ip,
            port: API_PORT,
            username: row.get::<Option<String>, _>("username").unwrap_or_default(),
            password: row.get::<Option<String>, _>("password").unwrap_or_default(),
            poll_interval_secs: row.get("poll_interval_secs"),
            label,
        });
    }
    Ok(out)
}

// ── Poll loop ─────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum PollOutcome {
    Online {
        leases: Vec<DhcpLease>,
        firewall_rules: Vec<FirewallRule>,
        /// Set when the (best-effort) firewall fetch failed even though
        /// leases succeeded — surfaced without downgrading connectivity.
        firewall_warning: Option<String>,
    },
    Disconnected {
        error: String,
    },
}

/// Combines the independently-fetched lease/firewall results into a single
/// outcome. Leases are load-bearing — the scanner fingerprints devices from
/// the DHCP lease table — so a lease-fetch success always reports Online,
/// carrying forward the previous poll's firewall data (rather than
/// discarding the working lease data) when the firewall fetch fails.
/// Requiring *both* to succeed, as this used to, meant a firewall-only
/// failure (e.g. a router without firewall filter rules configured) silently
/// discarded working lease data on every single poll.
fn decide_poll_outcome(
    leases: anyhow::Result<Vec<DhcpLease>>,
    rules: anyhow::Result<Vec<FirewallRule>>,
    prev_firewall_rules: Vec<FirewallRule>,
) -> PollOutcome {
    match leases {
        Ok(leases) => {
            let (firewall_rules, firewall_warning) = match rules {
                Ok(rules) => (rules, None),
                Err(e) => (
                    prev_firewall_rules,
                    Some(format!("firewall (non-critical): {e:#}")),
                ),
            };
            PollOutcome::Online {
                leases,
                firewall_rules,
                firewall_warning,
            }
        }
        Err(e) => PollOutcome::Disconnected {
            error: format!("leases: {e:#}"),
        },
    }
}

/// Persists the bytes transferred since the previous poll for a traffic
/// metric (`traffic_rx_bytes` / `traffic_tx_bytes`) in RawDeviceMeasurements,
/// alongside the rest of this device's raw measurements.
async fn save_traffic_delta(
    pool: &SqlitePool,
    device_id: i64,
    t: &str,
    metric: &str,
    delta_bytes: u64,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
         VALUES (?, ?, ?, ?)",
    )
    .bind(device_id)
    .bind(t)
    .bind(metric)
    .bind(delta_bytes as f64)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn poll_loop(pool: SqlitePool, device: DeviceRecord, state: SharedState) {
    let secs = device.poll_interval_secs.max(1) as u64;
    let mut ticker = interval(Duration::from_secs(secs));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut failures: u8 = 0;
    // Previous poll's raw counters, kept locally per poll loop (i.e. per
    // device) so a delta can be computed without a DB round-trip.
    let mut prev_traffic: Option<InterfaceStats> = None;

    loop {
        ticker.tick().await;

        let leases =
            fetch_dhcp_leases(device.ip, device.port, &device.username, &device.password).await;
        let rules =
            fetch_firewall_rules(device.ip, device.port, &device.username, &device.password).await;

        if let Ok(Some(traffic)) =
            fetch_internet_traffic(device.ip, device.port, &device.username, &device.password).await
        {
            if let Some(prev) = &prev_traffic {
                // Skip a sample straddling a counter reset/reboot (new < old)
                // rather than recording a bogus huge delta from wraparound.
                if traffic.rx_byte >= prev.rx_byte && traffic.tx_byte >= prev.tx_byte {
                    let t = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
                    let _ = save_traffic_delta(
                        &pool,
                        device.id,
                        &t,
                        "traffic_rx_bytes",
                        traffic.rx_byte - prev.rx_byte,
                    )
                    .await;
                    let _ = save_traffic_delta(
                        &pool,
                        device.id,
                        &t,
                        "traffic_tx_bytes",
                        traffic.tx_byte - prev.tx_byte,
                    )
                    .await;
                }
            }
            prev_traffic = Some(traffic);
        }

        let prev_firewall_rules = state
            .read()
            .unwrap()
            .mikrotik_readings
            .get(&device.ip)
            .map(|r| r.firewall_rules.clone())
            .unwrap_or_default();

        match decide_poll_outcome(leases, rules, prev_firewall_rules) {
            PollOutcome::Online {
                leases,
                firewall_rules,
                firewall_warning,
            } => {
                failures = 0;
                let mut app = state.write().unwrap();
                app.mikrotik_readings.insert(
                    device.ip,
                    MikrotikReading {
                        leases,
                        firewall_rules,
                        updated_at: chrono::Utc::now(),
                    },
                );
                app.conn_status.insert(device.ip, ConnStatus::Online);
                match firewall_warning {
                    Some(w) => {
                        app.last_error.insert(device.ip, w);
                    }
                    None => {
                        app.last_error.remove(&device.ip);
                    }
                }
            }
            PollOutcome::Disconnected { error } => {
                failures = failures.saturating_add(1);
                // First tick gone Lost: check whether a fingerprint match moved
                // this device to a new address (see db::upsert_device). If so, a
                // fresh loop is already running there — stop chasing the old one
                // rather than failing forever and leaving a dead entry in the UI.
                if failures == 4
                    && crate::db::device_moved(&pool, device.id, device.ip)
                        .await
                        .unwrap_or(false)
                {
                    let mut app = state.write().unwrap();
                    app.conn_status.remove(&device.ip);
                    app.last_error.remove(&device.ip);
                    app.mikrotik_readings.remove(&device.ip);
                    app.polled_ips.remove(&device.ip);
                    return;
                }
                let status = if failures <= 3 {
                    ConnStatus::Connecting
                } else {
                    ConnStatus::Lost
                };
                let mut app = state.write().unwrap();
                app.conn_status.insert(device.ip, status);
                app.last_error.insert(device.ip, error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(addr: &str) -> DhcpLease {
        DhcpLease {
            address: addr.to_string(),
            mac_address: "AA:BB:CC:DD:EE:FF".to_string(),
            host_name: None,
            comment: None,
            status: "bound".to_string(),
        }
    }

    fn rule() -> FirewallRule {
        FirewallRule {
            chain: "forward".to_string(),
            action: "accept".to_string(),
            comment: None,
            bytes: 100,
            packets: 1,
        }
    }

    /// The regression this guards: a firewall-only fetch failure used to be
    /// combined with leases via `match (leases, rules) { (Ok, Ok) => ..., _
    /// => disconnected }`, discarding a successful lease fetch — the data
    /// the scanner needs to discover DHCP-known devices — every single poll.
    #[test]
    fn leases_ok_firewall_err_reports_online_and_keeps_previous_firewall_data() {
        let leases = vec![lease("192.168.1.100")];
        let prev_firewall = vec![rule()];

        let outcome = decide_poll_outcome(
            Ok(leases.clone()),
            Err(anyhow::anyhow!("no such item (firewall)")),
            prev_firewall.clone(),
        );

        match outcome {
            PollOutcome::Online {
                leases: got_leases,
                firewall_rules,
                firewall_warning,
            } => {
                assert_eq!(got_leases, leases);
                assert_eq!(firewall_rules, prev_firewall);
                assert!(firewall_warning.unwrap().contains("firewall"));
            }
            PollOutcome::Disconnected { error } => {
                panic!("expected Online despite firewall failure, got Disconnected({error})")
            }
        }
    }

    #[test]
    fn leases_and_firewall_both_ok_reports_online_with_no_warning() {
        let leases = vec![lease("192.168.1.100")];
        let rules = vec![rule()];

        let outcome = decide_poll_outcome(Ok(leases.clone()), Ok(rules.clone()), vec![]);

        match outcome {
            PollOutcome::Online {
                leases: got_leases,
                firewall_rules,
                firewall_warning,
            } => {
                assert_eq!(got_leases, leases);
                assert_eq!(firewall_rules, rules);
                assert!(firewall_warning.is_none());
            }
            PollOutcome::Disconnected { error } => panic!("expected Online, got {error}"),
        }
    }

    #[test]
    fn leases_err_reports_disconnected_regardless_of_firewall() {
        let outcome = decide_poll_outcome(
            Err(anyhow::anyhow!("connection refused")),
            Ok(vec![rule()]),
            vec![],
        );

        match outcome {
            PollOutcome::Disconnected { error } => assert!(error.contains("leases")),
            PollOutcome::Online { .. } => panic!("expected Disconnected when leases fail"),
        }
    }
}
