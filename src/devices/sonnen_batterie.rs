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

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::{Row, SqlitePool};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{MissedTickBehavior, interval, timeout};

use crate::app::{ConnStatus, LiveReading, SharedState};
use crate::devices::ts;
use crate::fingerprint::Fingerprint;

pub const NAME: &str = "Sonnen Eco 8 Battery";
pub const API_PORT: u16 = 8080;
const API_STATUS_PATH: &str = "/api/v1/status";
const DEFAULT_POLL_SECS: i64 = 2;

pub fn detect(fp: &Fingerprint) -> bool {
    if !fp.open_ports.contains(&8080) || !fp.open_ports.contains(&8883) {
        return false;
    }
    fp.http.iter().any(|p| p.raw.contains("sonnenbatterie.de"))
}

// ── API response ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[allow(non_snake_case)]
pub struct Reading {
    pub Consumption_W: f64,
    pub Production_W: f64,
    /// Positive = discharging, negative = charging.
    pub Pac_total_W: f64,
    /// 0–100 %.
    pub RSOC: f64,
    /// Positive = export to grid, negative = import.
    pub GridFeedIn_W: f64,
    /// Current usable energy left in the battery, Wh.
    pub RemainingCapacity_Wh: f64,
}

pub async fn fetch_status(ip: IpAddr, port: u16, api_key: &str) -> anyhow::Result<Reading> {
    let addr = SocketAddr::new(ip, port);
    let mut stream = timeout(Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .context("connect timeout")?
        .context("connect failed")?;

    let req = format!(
        "GET {API_STATUS_PATH} HTTP/1.1\r\nHost: {ip}\r\nAuth-Token: {api_key}\r\nConnection: close\r\n\r\n"
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
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or(&raw);
    serde_json::from_str(body.trim()).context("parse JSON")
}

// ── Database ──────────────────────────────────────────────────────────────────

pub struct DeviceRecord {
    pub id: i64,
    pub name: String,
    pub ip: IpAddr,
    pub port: u16,
    pub api_key: Option<String>,
    pub poll_interval_secs: i64,
    pub label: Option<String>,
}

/// Registers a discovered device. Credentials are never known at scan time —
/// they're configured separately (see `db::set_device_api_key`) and preserved
/// across rescans; only name/poll_interval are touched here. `fingerprint`
/// (e.g. a MAC address) lets a device that changed IP be recognized and
/// migrated in place, credentials and all, rather than registered as a
/// second device with no API key — see `db::upsert_device`.
pub async fn save_device(
    pool: &SqlitePool,
    ip: IpAddr,
    name: &str,
    fingerprint: Option<&str>,
) -> anyhow::Result<()> {
    crate::db::upsert_device(
        pool,
        "sonnen_eco8",
        name,
        ip,
        DEFAULT_POLL_SECS,
        fingerprint,
    )
    .await
}

pub async fn load_all(pool: &SqlitePool) -> anyhow::Result<Vec<DeviceRecord>> {
    let rows = sqlx::query(
        "SELECT id, name, ip, api_key, poll_interval_secs, label
         FROM Devices WHERE type = 'sonnen_eco8'",
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
            name: row.get("name"),
            ip,
            port: API_PORT,
            api_key: row.get("api_key"),
            poll_interval_secs: row.get("poll_interval_secs"),
            label,
        });
    }
    Ok(out)
}

// ── Storage helpers ───────────────────────────────────────────────────────────

async fn save_raw(pool: &SqlitePool, device_id: i64, t: &str, r: &Reading) -> anyhow::Result<()> {
    for (metric, value) in [
        ("consumption", r.Consumption_W),
        ("production", r.Production_W),
        ("pac", r.Pac_total_W),
        ("grid", r.GridFeedIn_W),
    ] {
        sqlx::query(
            "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
             VALUES (?, ?, ?, ?)",
        )
        .bind(device_id)
        .bind(t)
        .bind(metric)
        .bind(value)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Trapezoidal integration of the interval [prev → curr] into Energy rows.
async fn save_energy(
    pool: &SqlitePool,
    device_id: i64,
    prev_r: &Reading,
    prev_t: DateTime<Utc>,
    curr_r: &Reading,
    curr_t: DateTime<Utc>,
) -> anyhow::Result<()> {
    let dt = (curr_t - prev_t).num_milliseconds() as f64 / 1000.0;
    if dt <= 0.0 {
        return Ok(());
    }
    let t = ts(curr_t);
    for (metric, p, c) in [
        ("consumption", prev_r.Consumption_W, curr_r.Consumption_W),
        ("production", prev_r.Production_W, curr_r.Production_W),
        ("pac", prev_r.Pac_total_W, curr_r.Pac_total_W),
        ("grid", prev_r.GridFeedIn_W, curr_r.GridFeedIn_W),
    ] {
        sqlx::query(
            "INSERT INTO Energy (device_id, timestamp, resolution, metric, energy_ws)
             VALUES (?, ?, '2s', ?, ?)",
        )
        .bind(device_id)
        .bind(&t)
        .bind(metric)
        .bind((p + c) / 2.0 * dt)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn save_storage(pool: &SqlitePool, device_id: i64, t: &str, rsoc: f64) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO EnergyStorage (device_id, timestamp, resolution, rsoc_avg)
         VALUES (?, ?, '2s', ?)",
    )
    .bind(device_id)
    .bind(t)
    .bind(rsoc)
    .execute(pool)
    .await?;
    Ok(())
}

// ── Poll loop ─────────────────────────────────────────────────────────────────

pub async fn poll_loop(pool: SqlitePool, device: DeviceRecord, state: SharedState) {
    let secs = device.poll_interval_secs.max(1) as u64;
    let mut ticker = interval(Duration::from_secs(secs));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let api_key = device.api_key.clone().unwrap_or_default();
    let mut prev: Option<(Reading, DateTime<Utc>)> = None;
    // Cached total capacity; updated whenever RSOC is reliable enough to derive it.
    let mut known_capacity_kwh: f64 = 0.0;
    let mut failures: u8 = 0;

    loop {
        ticker.tick().await;
        let poll_time = Utc::now();

        match fetch_status(device.ip, device.port, &api_key).await {
            Ok(curr) => {
                let t = ts(poll_time);

                if let Err(e) = save_raw(&pool, device.id, &t, &curr).await {
                    let _ = e;
                }
                if let Some((ref prev_r, prev_t)) = prev {
                    let _ = save_energy(&pool, device.id, prev_r, prev_t, &curr, poll_time).await;
                }
                let _ = save_storage(&pool, device.id, &t, curr.RSOC).await;

                if curr.RSOC > 5.0 {
                    known_capacity_kwh = curr.RemainingCapacity_Wh / (curr.RSOC / 100.0) / 1000.0;
                }

                failures = 0;
                let mut app = state.write().unwrap();
                app.readings.insert(
                    device.ip,
                    LiveReading {
                        consumption_w: curr.Consumption_W,
                        production_w: curr.Production_W,
                        pac_w: curr.Pac_total_W,
                        rsoc: curr.RSOC,
                        grid_w: curr.GridFeedIn_W,
                        remaining_kwh: curr.RemainingCapacity_Wh / 1000.0,
                        capacity_kwh: known_capacity_kwh,
                        updated_at: poll_time,
                    },
                );
                app.conn_status.insert(device.ip, ConnStatus::Online);
                app.last_error.remove(&device.ip);

                prev = Some((curr, poll_time));
            }
            Err(e) => {
                failures = failures.saturating_add(1);
                if crate::devices::handle_poll_failure(
                    &pool,
                    &state,
                    device.id,
                    device.ip,
                    failures,
                    format!("{e:#}"),
                )
                .await
                {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::HttpProbe;

    fn fp(ports: Vec<u16>, raws: Vec<&str>) -> Fingerprint {
        Fingerprint {
            ip: IpAddr::from([172, 16, 20, 8]),
            open_ports: ports,
            http: raws
                .into_iter()
                .map(|raw| HttpProbe {
                    port: 8080,
                    url: "/".to_string(),
                    raw: raw.to_string(),
                })
                .collect(),
        }
    }

    const VENDOR_PAGE: &str = "<html><a href=\"https://sonnenbatterie.de\">Sonnen</a></html>";

    #[test]
    fn detect_accepts_vendor_marker_with_both_ports_open() {
        assert!(detect(&fp(vec![8080, 8883], vec![VENDOR_PAGE])));
    }

    #[test]
    fn detect_requires_both_ports() {
        // The MQTT port (8883) alongside the API port (8080) is what separates a
        // Sonnen from any other device serving a page that mentions the vendor.
        assert!(!detect(&fp(vec![8080], vec![VENDOR_PAGE])));
        assert!(!detect(&fp(vec![8883], vec![VENDOR_PAGE])));
        assert!(!detect(&fp(vec![], vec![VENDOR_PAGE])));
    }

    #[test]
    fn detect_requires_the_vendor_marker() {
        assert!(!detect(&fp(vec![8080, 8883], vec!["<html>generic</html>"])));
        assert!(!detect(&fp(vec![8080, 8883], vec![])));
    }
}
