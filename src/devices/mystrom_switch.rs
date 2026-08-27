/*  This file is part of the Dom smarthome app.
 *
 *  Copyright © 2026 Marko Ivankovic
 *
 *  This is anti-capitalist software, released for free use by individuals and
 *  organizations that do not operate by capitalist principles. Use is permitted
 *  by individuals working for themselves, non-profits, educational institutions,
 *  and organizations whose owners are all workers with equal equity and vote —
 *  and is not permitted to law enforcement or the military.
 *
 *  Licensed under the Anti-Capitalist Software License v1.4. See the LICENSE
 *  file for the full terms and conditions, which you must satisfy to have any
 *  licence at all.
 *
 *  Source Code: https://github.com/ivankovic/dom
 *
 *  THE SOFTWARE IS PROVIDED "AS IS", WITHOUT EXPRESS OR IMPLIED WARRANTY OF ANY
 *  KIND. IN NO EVENT SHALL THE AUTHORS BE LIABLE FOR ANY CLAIM, DAMAGES OR
 *  OTHER LIABILITY ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR
 *  THE USE OR OTHER DEALINGS IN THE SOFTWARE.
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
use crate::devices::ts;
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

/// Switches the relay, and confirms the device said it did.
///
/// The reply used to be read and discarded, so this returned `Ok(())` as long as
/// the TCP connect and write succeeded — a device that answered "500" or did not
/// answer at all reported success. That mattered: `timer_job` treats a scheduled
/// firing as done once this returns `Ok`, so a switch that refused the command
/// was silently skipped for the day.
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

    let mut buf = Vec::new();
    timeout(Duration::from_secs(3), stream.read_to_end(&mut buf))
        .await
        .context("read timeout")?
        .context("read failed")?;
    check_relay_reply(&String::from_utf8_lossy(&buf))
}

/// Accepts a 2xx reply and rejects anything else.
///
/// Split out so the response handling is testable without a socket, like
/// `parse_report`. An empty reply is a failure rather than a success: the device
/// answers this request, so nothing coming back means the command did not land.
fn check_relay_reply(raw: &str) -> anyhow::Result<()> {
    let status = raw
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .context("no HTTP status line in the reply")?;
    if (200..300).contains(&status) {
        Ok(())
    } else {
        anyhow::bail!("the switch answered HTTP {status}")
    }
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

    parse_report(&String::from_utf8_lossy(&buf))
}

/// Splits an HTTP response at the header/body separator and parses the body as
/// a `Report`. Kept separate from `fetch_report` so the response-shape handling
/// is testable without a socket. A payload with no separator is treated as a
/// bare body, so a device answering with just JSON still parses.
fn parse_report(raw: &str) -> anyhow::Result<Report> {
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or(raw);
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

async fn save_raw(
    tx: &mut sqlx::SqliteConnection,
    device_id: i64,
    t: &str,
    r: &Report,
) -> anyhow::Result<()> {
    for (metric, value) in [("power", r.power), ("temperature", r.temperature)] {
        sqlx::query(
            "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
             VALUES (?, ?, ?, ?)",
        )
        .bind(device_id)
        .bind(t)
        .bind(metric)
        .bind(value)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Trapezoidal integration of the interval [prev → curr] into an Energy row.
///
/// Returns `Ok(false)` when the interval was too long to integrate — see
/// `devices::max_integration_gap_ms`. A switch left unreachable for hours would
/// otherwise have its last known power multiplied across the whole absence.
async fn save_energy(
    tx: &mut sqlx::SqliteConnection,
    device_id: i64,
    prev_r: &Report,
    prev_t: DateTime<Utc>,
    curr_r: &Report,
    curr_t: DateTime<Utc>,
    poll_interval_secs: u64,
) -> anyhow::Result<bool> {
    let dt_ms = (curr_t - prev_t).num_milliseconds();
    if dt_ms <= 0 || dt_ms > crate::devices::max_integration_gap_ms(poll_interval_secs) {
        return Ok(false);
    }
    let dt = dt_ms as f64 / 1000.0;
    let energy_ws = (prev_r.power + curr_r.power) / 2.0 * dt;
    sqlx::query(
        "INSERT INTO Energy (device_id, timestamp, resolution, metric, energy_ws)
         VALUES (?, ?, '2s', 'power', ?)",
    )
    .bind(device_id)
    .bind(ts(curr_t))
    .bind(energy_ws)
    .execute(&mut *tx)
    .await?;
    Ok(true)
}

/// Writes everything one poll produced, as a single transaction.
///
/// Two raw readings and an energy row were three separate commits, and a commit
/// is a disk flush. See `db::init` and `sonnen_batterie::write_poll`.
async fn write_poll(
    pool: &SqlitePool,
    device_id: i64,
    t: &str,
    curr: &Report,
    prev: Option<(&Report, DateTime<Utc>)>,
    poll_time: DateTime<Utc>,
    poll_interval_secs: u64,
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    save_raw(&mut tx, device_id, t, curr).await?;
    if let Some((prev_r, prev_t)) = prev {
        save_energy(
            &mut tx,
            device_id,
            prev_r,
            prev_t,
            curr,
            poll_time,
            poll_interval_secs,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

// ── Poll loop ─────────────────────────────────────────────────────────────────

pub async fn poll_loop(pool: SqlitePool, device: DeviceRecord, state: SharedState) {
    let secs = device.poll_interval_secs.max(1) as u64;
    let mut ticker = interval(Duration::from_secs(secs));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut prev: Option<(Report, DateTime<Utc>)> = None;
    let mut failures: u32 = 0;

    loop {
        ticker.tick().await;
        let poll_time = Utc::now();

        match fetch_report(device.ip, device.port).await {
            Ok(curr) => {
                let t = ts(poll_time);
                if let Err(e) = write_poll(
                    &pool,
                    device.id,
                    &t,
                    &curr,
                    prev.as_ref().map(|(r, at)| (r, *at)),
                    poll_time,
                    secs,
                )
                .await
                {
                    log::warn!("myStrom {}: recording this poll failed: {e:#}", device.ip);
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

    fn fp(ports: Vec<u16>, probes: Vec<(&str, &str)>) -> Fingerprint {
        let ip = IpAddr::from([172, 16, 20, 7]);
        Fingerprint {
            ip,
            open_ports: ports,
            http: probes
                .into_iter()
                .map(|(url, raw)| HttpProbe {
                    ip,
                    port: 80,
                    url: url.to_string(),
                    raw: raw.to_string(),
                })
                .collect(),
        }
    }

    const REPORT_BODY: &str = r#"{"power":42.5,"relay":true,"temperature":21.5}"#;

    #[test]
    fn detect_accepts_a_report_containing_the_relay_field() {
        assert!(detect(&fp(vec![80], vec![("/report", REPORT_BODY)])));
    }

    #[test]
    fn detect_requires_port_80() {
        // Same telltale body, but the port isn't open: not a myStrom switch.
        assert!(!detect(&fp(vec![8080], vec![("/report", REPORT_BODY)])));
    }

    #[test]
    fn detect_requires_the_relay_field_on_the_report_path() {
        // "relay" seen on some other path doesn't count.
        assert!(!detect(&fp(vec![80], vec![("/", REPORT_BODY)])));
        // /report present but without the telltale field (e.g. another
        // device's 404 page, or a device exposing power only).
        assert!(!detect(&fp(
            vec![80],
            vec![("/report", r#"{"power":1.0}"#)]
        )));
        assert!(!detect(&fp(vec![80], vec![])));
    }

    #[test]
    fn parse_report_reads_the_body_after_the_header_separator() {
        let raw = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{REPORT_BODY}");
        let r = parse_report(&raw).unwrap();
        assert_eq!(r.power, 42.5);
        assert!(r.relay);
        assert_eq!(r.temperature, 21.5);
    }

    #[test]
    fn parse_report_accepts_a_bare_body_with_no_headers() {
        let r = parse_report(REPORT_BODY).unwrap();
        assert_eq!(r.power, 42.5);
    }

    #[test]
    fn parse_report_errors_on_a_non_json_payload() {
        assert!(parse_report("HTTP/1.1 404 Not Found\r\n\r\n<html>nope</html>").is_err());
        assert!(parse_report("").is_err());
    }

    // ── Relay command confirmation ────────────────────────────────────────────

    #[test]
    fn a_successful_relay_command_is_accepted() {
        assert!(check_relay_reply("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").is_ok());
        assert!(check_relay_reply("HTTP/1.0 204 No Content\r\n\r\n").is_ok());
    }

    #[test]
    fn a_refused_relay_command_is_an_error_rather_than_a_silent_success() {
        // `timer_job` marks a scheduled firing done when this returns Ok, so a
        // refusal that read as success skipped that timer for the day.
        for raw in [
            "HTTP/1.1 500 Internal Server Error\r\n\r\n",
            "HTTP/1.1 404 Not Found\r\n\r\n",
            "HTTP/1.1 401 Unauthorized\r\n\r\n",
        ] {
            assert!(check_relay_reply(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn a_reply_that_is_not_http_is_an_error() {
        // Including no reply at all: the device answers this request, so silence
        // means the command did not land.
        assert!(check_relay_reply("").is_err());
        assert!(check_relay_reply("garbage").is_err());
        assert!(check_relay_reply("HTTP/1.1 notanumber OK\r\n\r\n").is_err());
    }

    // ── Integration across gaps ───────────────────────────────────────────────

    async fn switch_pool() -> (SqlitePool, i64) {
        let pool = crate::db::init("sqlite://:memory:").await.unwrap();
        let ip = IpAddr::from([172, 16, 75, 4]);
        save_device(&pool, ip, "switch", None).await.unwrap();
        let id = load_all(&pool).await.unwrap()[0].id;
        (pool, id)
    }

    fn report(power: f64) -> Report {
        Report {
            power,
            relay: true,
            temperature: 24.0,
        }
    }

    async fn energy_rows(pool: &SqlitePool) -> Vec<f64> {
        sqlx::query_scalar::<_, f64>("SELECT energy_ws FROM Energy ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_gap_is_recorded_as_missing_rather_than_integrated() {
        // The same defect the battery had: a switch unreachable for hours would
        // have its last known power multiplied across the whole absence.
        let (pool, id) = switch_pool().await;
        let t0 = chrono::DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();

        write_poll(&pool, id, &ts(t0), &report(100.0), None, t0, 2)
            .await
            .unwrap();
        let hours_later = t0 + chrono::Duration::hours(3);
        write_poll(
            &pool,
            id,
            &ts(hours_later),
            &report(100.0),
            Some((&report(100.0), t0)),
            hours_later,
            2,
        )
        .await
        .unwrap();

        assert!(
            energy_rows(&pool).await.is_empty(),
            "a gap must leave a hole, not three hours of invented energy"
        );
    }

    #[tokio::test]
    async fn a_normal_interval_is_integrated() {
        let (pool, id) = switch_pool().await;
        let t0 = chrono::DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let t1 = t0 + chrono::Duration::seconds(2);

        write_poll(
            &pool,
            id,
            &ts(t1),
            &report(100.0),
            Some((&report(100.0), t0)),
            t1,
            2,
        )
        .await
        .unwrap();

        // 100 W held for 2 s is 200 Ws — the trapezoid, exactly.
        assert_eq!(energy_rows(&pool).await, vec![200.0]);
    }

    #[tokio::test]
    async fn a_poll_that_fails_partway_leaves_nothing_behind() {
        let (pool, _) = switch_pool().await;
        let t0 = chrono::DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let t1 = t0 + chrono::Duration::seconds(2);

        // No such device, so the foreign key rejects the first insert.
        let result = write_poll(
            &pool,
            9_999,
            &ts(t1),
            &report(100.0),
            Some((&report(100.0), t0)),
            t1,
            2,
        )
        .await;

        assert!(result.is_err());
        let raw: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM RawDeviceMeasurements")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(raw, 0, "a failed poll must leave nothing behind");
        assert!(energy_rows(&pool).await.is_empty());
    }
}
