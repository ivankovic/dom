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

use crate::app::{ConnStatus, SharedState, SwitchReading};
use crate::fingerprint::Fingerprint;

pub const NAME: &str = "myStrom WiFi Switch";
pub const API_PORT: u16 = 80;
const API_REPORT_PATH: &str = "/report";
const DEFAULT_POLL_SECS: i64 = 2;

pub fn detect(fp: &Fingerprint) -> bool {
    if !fp.open_ports.contains(&80) {
        return false;
    }
    // GET /report always returns {"power":…,"relay":…,"temperature":…} on a myStrom switch.
    // The "relay" field (boolean) doesn't appear in any other device's /report response.
    fp.http
        .iter()
        .any(|p| p.url == "/report" && p.raw.contains("\"relay\":"))
}

// ── API response ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Report {
    pub power: f64,
    pub relay: bool,
    pub temperature: f64,
}

pub async fn set_relay(ip: IpAddr, port: u16, on: bool) -> anyhow::Result<()> {
    let state = u8::from(on);
    let addr = SocketAddr::new(ip, port);
    let mut stream = timeout(Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .context("connect timeout")?
        .context("connect failed")?;
    let req =
        format!("GET /relay?state={state} HTTP/1.1\r\nHost: {ip}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.context("send")?;
    let mut buf = [0u8; 256];
    let _ = timeout(Duration::from_secs(3), stream.read(&mut buf)).await;
    Ok(())
}

pub async fn fetch_report(ip: IpAddr, port: u16) -> anyhow::Result<Report> {
    let addr = SocketAddr::new(ip, port);
    let mut stream = timeout(Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .context("connect timeout")?
        .context("connect failed")?;

    let req = format!("GET {API_REPORT_PATH} HTTP/1.1\r\nHost: {ip}\r\nConnection: close\r\n\r\n");
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
    pub ip: IpAddr,
    pub port: u16,
    pub poll_interval_secs: i64,
    pub label: Option<String>,
}

/// `fingerprint` (e.g. a MAC address) lets a device that changed IP be
/// recognized and migrated in place rather than registered as a second
/// device — see `db::upsert_device`.
pub async fn save_device(
    pool: &SqlitePool,
    ip: IpAddr,
    name: &str,
    fingerprint: Option<&str>,
) -> anyhow::Result<()> {
    crate::db::upsert_device(
        pool,
        "mystrom_switch",
        name,
        ip,
        DEFAULT_POLL_SECS,
        fingerprint,
    )
    .await
}

pub async fn load_all(pool: &SqlitePool) -> anyhow::Result<Vec<DeviceRecord>> {
    let rows = sqlx::query(
        "SELECT id, ip, poll_interval_secs, label FROM Devices WHERE type = 'mystrom_switch'",
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
            poll_interval_secs: row.get("poll_interval_secs"),
            label,
        });
    }
    Ok(out)
}

// ── Storage helpers ───────────────────────────────────────────────────────────

fn ts(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

async fn save_raw(pool: &SqlitePool, device_id: i64, t: &str, r: &Report) -> anyhow::Result<()> {
    for (metric, value) in [("power", r.power), ("temperature", r.temperature)] {
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

async fn save_energy(
    pool: &SqlitePool,
    device_id: i64,
    prev_r: &Report,
    prev_t: DateTime<Utc>,
    curr_r: &Report,
    curr_t: DateTime<Utc>,
) -> anyhow::Result<()> {
    let dt = (curr_t - prev_t).num_milliseconds() as f64 / 1000.0;
    if dt <= 0.0 {
        return Ok(());
    }
    let energy_ws = (prev_r.power + curr_r.power) / 2.0 * dt;
    sqlx::query(
        "INSERT INTO Energy (device_id, timestamp, resolution, metric, energy_ws)
         VALUES (?, ?, '2s', 'power', ?)",
    )
    .bind(device_id)
    .bind(ts(curr_t))
    .bind(energy_ws)
    .execute(pool)
    .await?;
    Ok(())
}

// ── Poll loop ─────────────────────────────────────────────────────────────────

pub async fn poll_loop(pool: SqlitePool, device: DeviceRecord, state: SharedState) {
    let secs = device.poll_interval_secs.max(1) as u64;
    let mut ticker = interval(Duration::from_secs(secs));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut prev: Option<(Report, DateTime<Utc>)> = None;
    let mut failures: u8 = 0;

    loop {
        ticker.tick().await;
        let poll_time = Utc::now();

        match fetch_report(device.ip, device.port).await {
            Ok(curr) => {
                let t = ts(poll_time);
                let _ = save_raw(&pool, device.id, &t, &curr).await;
                if let Some((ref prev_r, prev_t)) = prev {
                    let _ = save_energy(&pool, device.id, prev_r, prev_t, &curr, poll_time).await;
                }

                failures = 0;
                let mut app = state.write().unwrap();
                app.switch_readings.insert(
                    device.ip,
                    SwitchReading {
                        power_w: curr.power,
                        relay_on: curr.relay,
                        temperature_c: curr.temperature,
                        updated_at: poll_time,
                    },
                );
                app.conn_status.insert(device.ip, ConnStatus::Online);
                app.last_error.remove(&device.ip);

                prev = Some((curr, poll_time));
            }
            Err(e) => {
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
                    app.switch_readings.remove(&device.ip);
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
                app.last_error.insert(device.ip, format!("{e:#}"));
            }
        }
    }
}
