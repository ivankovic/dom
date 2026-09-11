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
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::timeout;

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

    let buf = crate::devices::read_capped(&mut stream, Duration::from_secs(3))
        .await
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

    let buf = crate::devices::read_capped(&mut stream, Duration::from_secs(10))
        .await
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
        // Skipped rather than `?`. Failing the whole query on one unreadable
        // row cost every device of this type its poll loop, because every
        // caller — `maybe_spawn_*_poll_loop`, `bootstrap_known_devices`,
        // `eco_job` — reads an `Err` as "there are none of these". The row is
        // named in the log so it can be found and fixed. `dom_local::load_all`
        // and `main::lease_addresses` have always done it this way.
        let Ok(ip) = ip_str.parse::<IpAddr>() else {
            log::warn!("ignoring a {NAME} row whose address does not parse: {ip_str:?}");
            continue;
        };
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
    let mut ticker = crate::devices::PollTicker::new(device.id, device.poll_interval_secs);

    let mut prev: Option<(Report, DateTime<Utc>)> = None;
    let mut failures: u32 = 0;

    loop {
        ticker.tick(&pool).await;
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
                    ticker.secs(),
                )
                .await
                {
                    crate::devices::note_write_failure(
                        &state,
                        &format!("myStrom {}", device.ip),
                        &e,
                    );
                } else {
                    crate::devices::note_write_ok(&state);
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

// ── Eco mode ──────────────────────────────────────────────────────────────────
//
// Unlike `keba::eco_loop`, which continuously varies a current, a relay is
// binary — the only decision is *when* to run a single on/off window each
// day. So Eco mode here computes a placement, once per local day, rather than
// running a tight control loop: `main::eco_job` calls `choose_window`, then
// fires the two resulting instants exactly as `main::timer_job` fires a
// `SwitchTimer`.
//
// Eco mode does not invent its own "how long" or "how much energy" config —
// it reads the duration implied by the switch's own two configured
// `SwitchTimers` (see `duration_steps_from_timers`), so enabling it requires
// an on/off pair to already exist. That pair's own clock time stops mattering
// once Eco is on; only its *length* does.

/// 15-minute steps in a local day. Matches `solar::STEPS_PER_HOUR`'s
/// resolution — see the test below asserting the two agree — since that is
/// the forecast's native granularity and there is no reason to invent a
/// different one for scoring against it.
pub const STEPS_PER_DAY: usize = 96;

/// How much a candidate window's predicted cost, in Wh, may differ from the
/// best one and still count as tied.
///
/// Without this, a day with no solar surplus at all — every window costing
/// exactly `typical_power_w * duration` — would almost never produce two
/// floating-point-identical sums, and the random tie-break the whole point of
/// asking for would never fire. The tolerance is deliberately generous: a
/// difference this small is not a meaningfully better window, it is rounding.
const TIE_EPSILON_WH: f64 = 0.5;

/// Quarter-hour-of-day index (0..[`STEPS_PER_DAY`]) for a local "HH:MM"
/// string, floored to the enclosing step. `None` for anything that is not a
/// 24-hour time.
pub fn step_of_hhmm(hhmm: &str) -> Option<usize> {
    let (h, m) = hhmm.split_once(':')?;
    let h: usize = h.parse().ok()?;
    let m: usize = m.parse().ok()?;
    (h < 24 && m < 60).then_some(h * 4 + m / 15)
}

/// The on-window length implied by a configured on/off timer pair, in
/// [`STEPS_PER_DAY`] steps — wrapping past midnight if `off_hhmm` reads
/// earlier than `on_hhmm`. `None` if either fails to parse, or the pair
/// implies a zero-length window (same time for both).
pub fn duration_steps_from_timers(on_hhmm: &str, off_hhmm: &str) -> Option<usize> {
    let on = step_of_hhmm(on_hhmm)?;
    let off = step_of_hhmm(off_hhmm)?;
    if on == off {
        return None;
    }
    Some(if off > on {
        off - on
    } else {
        off + STEPS_PER_DAY - on
    })
}

/// How many of the day's [`STEPS_PER_DAY`] steps must have real recorded
/// history before the baseline is worth scoring against.
///
/// `db::query_avg_power_by_local_quarter_hour` omits a step entirely when no
/// `EnergyMinute` row covers it in the whole lookback window — the device was
/// unreachable, or had not been added yet. That is *not* the same as a step
/// that averaged zero: a switch sitting off is still polled, and
/// `save_energy` writes its `energy_ws = 0` like any other reading. So a
/// missing step means "unknown", never "idle", and the two must not be
/// conflated — see [`baseline_from_curves`].
const ECO_MIN_KNOWN_STEPS: usize = STEPS_PER_DAY / 2;

/// Typical *other* household load per quarter-hour: whole-house consumption
/// with the switch's own historical draw subtracted out, which is what
/// [`window_cost_wh`] scores a candidate window against.
///
/// Both curves are sparse — see [`ECO_MIN_KNOWN_STEPS`] for why a step goes
/// missing. A step is only *known* here when both curves have it; anything
/// else is imputed with the mean of the known steps rather than left at zero.
/// Zero is the actively harmful choice: it claims the rest of the house draws
/// nothing at that hour, which inflates the apparent solar surplus and biases
/// the window straight toward whichever hours there is no data for. The mean
/// at least says "assume this hour looks like the ones we did see".
///
/// `None` when fewer than [`ECO_MIN_KNOWN_STEPS`] steps are known, which is
/// the caller's cue to fall back to the configured timer rather than plan
/// against a curve that is mostly guesswork.
pub fn baseline_from_curves(
    house: &std::collections::HashMap<usize, f64>,
    own: &std::collections::HashMap<usize, f64>,
) -> Option<[f64; STEPS_PER_DAY]> {
    let known: Vec<(usize, f64)> = (0..STEPS_PER_DAY)
        .filter_map(|step| {
            let house = house.get(&step).copied()?;
            let own = own.get(&step).copied()?;
            Some((step, (house - own).max(0.0)))
        })
        .collect();
    if known.len() < ECO_MIN_KNOWN_STEPS {
        return None;
    }
    let mean = known.iter().map(|(_, w)| *w).sum::<f64>() / known.len() as f64;
    let mut baseline = [mean; STEPS_PER_DAY];
    for (step, w) in known {
        baseline[step] = w;
    }
    Some(baseline)
}

/// Predicted marginal grid cost, in Wh, of running the switch for
/// `duration_steps` starting at quarter-hour `start` (wrapping past
/// midnight), given `typical_power_w` — what the switch draws while running —
/// and, for every quarter-hour of the local day, `production_w` (forecast
/// solar) and `baseline_w` (typical *other* household load, i.e. whole-house
/// consumption with this switch's own historical draw already subtracted
/// out).
///
/// Per step the marginal cost is the switch's own draw minus whatever solar
/// surplus is left after the rest of the house's typical load — floored at
/// zero (a step already covered by surplus costs nothing extra) and capped at
/// the switch's own draw (surplus beyond what it needs is not credited
/// against anything else, it is just unclaimed export). This is the same
/// "reconstruct what's actually available" idea as `keba::eco_decision`'s
/// `available_w`, applied to a fixed load instead of a controllable current —
/// see that function's docs for why the subtraction, not a live grid-export
/// reading, is what answers "is there really spare solar right now".
fn window_cost_wh(
    start: usize,
    duration_steps: usize,
    typical_power_w: f64,
    production_w: &[f64; STEPS_PER_DAY],
    baseline_w: &[f64; STEPS_PER_DAY],
) -> f64 {
    (0..duration_steps)
        .map(|i| {
            let step = (start + i) % STEPS_PER_DAY;
            let surplus = (production_w[step] - baseline_w[step]).max(0.0);
            (typical_power_w - surplus).max(0.0)
        })
        .sum::<f64>()
        / crate::solar::STEPS_PER_HOUR
}

/// Every quarter-hour start whose window cost is within [`TIE_EPSILON_WH`] of
/// the best. Always non-empty for `1..=STEPS_PER_DAY` — every start scores
/// something, so there is always a minimum to be within reach of.
pub fn best_window_starts(
    duration_steps: usize,
    typical_power_w: f64,
    production_w: &[f64; STEPS_PER_DAY],
    baseline_w: &[f64; STEPS_PER_DAY],
) -> Vec<usize> {
    if duration_steps == 0 || duration_steps > STEPS_PER_DAY {
        return Vec::new();
    }
    let scores: Vec<(usize, f64)> = (0..STEPS_PER_DAY)
        .map(|start| {
            (
                start,
                window_cost_wh(
                    start,
                    duration_steps,
                    typical_power_w,
                    production_w,
                    baseline_w,
                ),
            )
        })
        .collect();
    let best = scores
        .iter()
        .map(|(_, cost)| *cost)
        .fold(f64::INFINITY, f64::min);
    scores
        .into_iter()
        .filter(|(_, cost)| *cost <= best + TIE_EPSILON_WH)
        .map(|(start, _)| start)
        .collect()
}

/// Picks today's Eco window: a uniformly random pick among
/// [`best_window_starts`] — a tie is the expected outcome on a day with no
/// solar surplus at all (see that function's docs), and there is no
/// principled reason to prefer one tied hour over another, so this does not
/// invent one. Returns the chosen start and its predicted cost. `None` only
/// when `best_window_starts` is (an invalid `duration_steps`).
pub fn choose_window(
    duration_steps: usize,
    typical_power_w: f64,
    production_w: &[f64; STEPS_PER_DAY],
    baseline_w: &[f64; STEPS_PER_DAY],
) -> Option<(usize, f64)> {
    let candidates = best_window_starts(duration_steps, typical_power_w, production_w, baseline_w);
    if candidates.is_empty() {
        return None;
    }
    let start = candidates[rand::random::<usize>() % candidates.len()];
    Some((
        start,
        window_cost_wh(
            start,
            duration_steps,
            typical_power_w,
            production_w,
            baseline_w,
        ),
    ))
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

    // ── Eco mode ────────────────────────────────────────────────────────────

    fn flat(w: f64) -> [f64; STEPS_PER_DAY] {
        [w; STEPS_PER_DAY]
    }

    fn zeros() -> [f64; STEPS_PER_DAY] {
        flat(0.0)
    }

    #[test]
    fn steps_per_day_matches_the_forecast_resolution() {
        assert_eq!(
            STEPS_PER_DAY,
            (24.0 * crate::solar::STEPS_PER_HOUR) as usize
        );
    }

    #[test]
    fn duration_from_timers_handles_a_same_day_pair() {
        assert_eq!(duration_steps_from_timers("09:00", "18:00"), Some(36));
    }

    #[test]
    fn duration_from_timers_wraps_past_midnight() {
        // 22:00 to 06:00 is 8 hours = 32 steps, the long way round.
        assert_eq!(duration_steps_from_timers("22:00", "06:00"), Some(32));
    }

    #[test]
    fn duration_from_timers_rejects_an_empty_or_malformed_pair() {
        assert_eq!(duration_steps_from_timers("09:00", "09:00"), None);
        assert_eq!(duration_steps_from_timers("nope", "18:00"), None);
    }

    #[test]
    fn a_sunny_midday_window_is_picked_and_costs_almost_nothing() {
        let mut production = zeros();
        // Sun from 10:00 (step 40) to 14:00 (step 56), well above what the
        // switch needs.
        for slot in &mut production[40..56] {
            *slot = 3000.0;
        }
        let baseline = zeros();
        let starts = best_window_starts(8, 1000.0, &production, &baseline); // a 2h window
        assert!(starts.contains(&40), "{starts:?}");
        let cost = window_cost_wh(40, 8, 1000.0, &production, &baseline);
        assert!(cost < TIE_EPSILON_WH, "{cost}");
    }

    #[test]
    fn a_day_with_no_production_ties_every_start() {
        let starts = best_window_starts(8, 1000.0, &zeros(), &zeros());
        assert_eq!(
            starts.len(),
            STEPS_PER_DAY,
            "every start should be tied: {starts:?}"
        );
    }

    #[test]
    fn a_partial_overlap_scores_better_than_no_overlap_at_all() {
        let mut production = zeros();
        // A short sunny burst, 05:00-06:00.
        for slot in &mut production[20..24] {
            *slot = 2000.0;
        }
        let baseline = zeros();
        let overlapping = window_cost_wh(20, 16, 1000.0, &production, &baseline);
        let far_away = window_cost_wh(60, 16, 1000.0, &production, &baseline);
        assert!(overlapping < far_away, "{overlapping} vs {far_away}");
    }

    #[test]
    fn a_competing_baseline_load_eats_into_the_surplus() {
        let mut production = zeros();
        production[40] = 1500.0;
        let mut baseline = zeros();
        baseline[40] = 1000.0; // the house is already using most of it
        let with_baseline = window_cost_wh(40, 1, 1000.0, &production, &baseline);
        let without_baseline = window_cost_wh(40, 1, 1000.0, &production, &zeros());
        assert!(
            with_baseline > without_baseline,
            "{with_baseline} vs {without_baseline}"
        );
    }

    #[test]
    fn choose_window_always_returns_one_of_the_tied_candidates() {
        let (production, baseline) = (zeros(), zeros());
        for _ in 0..20 {
            let (start, _) = choose_window(4, 500.0, &production, &baseline).unwrap();
            assert!(
                best_window_starts(4, 500.0, &production, &baseline).contains(&start),
                "start {start} was not among the tied candidates"
            );
        }
    }

    #[test]
    fn choose_window_is_none_for_a_window_longer_than_a_day() {
        assert_eq!(
            choose_window(STEPS_PER_DAY + 1, 500.0, &zeros(), &zeros()),
            None
        );
    }

    /// A curve covering `steps`, every step holding `w`.
    fn curve(steps: std::ops::Range<usize>, w: f64) -> std::collections::HashMap<usize, f64> {
        steps.map(|step| (step, w)).collect()
    }

    #[test]
    fn a_baseline_is_the_house_minus_the_switchs_own_draw() {
        let baseline = baseline_from_curves(
            &curve(0..STEPS_PER_DAY, 800.0),
            &curve(0..STEPS_PER_DAY, 300.0),
        )
        .unwrap();
        assert!(baseline.iter().all(|w| (*w - 500.0).abs() < 1e-9));
    }

    #[test]
    fn a_baseline_never_goes_negative() {
        // The switch appearing to draw more than the whole house — clock skew
        // between two devices' rollups can do this — must not become a
        // negative "load" that invents surplus out of nothing.
        let baseline = baseline_from_curves(
            &curve(0..STEPS_PER_DAY, 100.0),
            &curve(0..STEPS_PER_DAY, 900.0),
        )
        .unwrap();
        assert!(baseline.iter().all(|w| *w == 0.0));
    }

    #[test]
    fn an_unrecorded_step_is_imputed_from_the_known_ones_not_zeroed() {
        // Three quarters of the day known at 600 W, the last quarter never
        // recorded. The gap must not read as "the house draws nothing then".
        let known = 0..(STEPS_PER_DAY * 3 / 4);
        let baseline =
            baseline_from_curves(&curve(known.clone(), 600.0), &curve(known, 0.0)).unwrap();
        assert!(
            baseline[STEPS_PER_DAY - 1] > 0.0,
            "an unknown step was zeroed: {}",
            baseline[STEPS_PER_DAY - 1]
        );
        assert!((baseline[STEPS_PER_DAY - 1] - 600.0).abs() < 1e-9);
    }

    #[test]
    fn a_zeroed_gap_would_have_biased_the_window_toward_the_hours_we_know_nothing_about() {
        // The regression this imputation exists for, and it bites precisely
        // where an unrecorded step still has sun on it: a weak-sun morning the
        // history does not cover, against a strong-sun midday it does, with a
        // heavy house load all day.
        //
        // Zeroing the morning credits the switch with every watt the panels
        // make then, because nothing else appears to be drawing — so 1200 W of
        // hazy morning reads as more spare than 2600 W of midday against a
        // real 2400 W load, and the window lands on the hours we know least
        // about. Imputing the known mean instead keeps the morning honest.
        let mut production = zeros();
        for slot in &mut production[24..40] {
            *slot = 1200.0; // hazy morning, unrecorded
        }
        for slot in &mut production[40..72] {
            *slot = 2600.0; // strong midday, recorded
        }
        let recorded = 40..STEPS_PER_DAY;
        let baseline =
            baseline_from_curves(&curve(recorded.clone(), 2400.0), &curve(recorded, 0.0)).unwrap();

        let mut zeroed = baseline;
        for slot in &mut zeroed[0..40] {
            *slot = 0.0;
        }

        let morning = window_cost_wh(24, 8, 1000.0, &production, &baseline);
        let midday = window_cost_wh(40, 8, 1000.0, &production, &baseline);
        assert!(
            midday < morning,
            "imputed: midday {midday} should beat the unrecorded morning {morning}"
        );
        assert!(
            window_cost_wh(24, 8, 1000.0, &production, &zeroed)
                < window_cost_wh(40, 8, 1000.0, &production, &zeroed),
            "the zeroed baseline was supposed to show the old bias"
        );
    }

    #[test]
    fn too_little_history_refuses_to_produce_a_baseline_at_all() {
        let sparse = 0..(ECO_MIN_KNOWN_STEPS - 1);
        assert_eq!(
            baseline_from_curves(&curve(sparse.clone(), 600.0), &curve(sparse, 0.0)),
            None
        );
    }

    #[test]
    fn a_step_missing_from_either_curve_alone_is_not_known() {
        // House recorded all day, switch only half — the steps where the
        // subtraction cannot be done are imputed, not treated as own = 0.
        let half = 0..STEPS_PER_DAY / 2;
        let baseline =
            baseline_from_curves(&curve(0..STEPS_PER_DAY, 900.0), &curve(half, 400.0)).unwrap();
        assert!(baseline.iter().all(|w| (*w - 500.0).abs() < 1e-9));
    }
}
