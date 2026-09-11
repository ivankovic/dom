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

impl Reading {
    /// The four powers, in the form `crate::energy` works in.
    fn flows(&self) -> crate::energy::Flows {
        crate::energy::Flows {
            production_w: self.Production_W,
            consumption_w: self.Consumption_W,
            battery_w: self.Pac_total_W,
            grid_w: self.GridFeedIn_W,
        }
    }
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

    let buf = crate::devices::read_capped(&mut stream, Duration::from_secs(10))
        .await
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

async fn save_raw(
    tx: &mut sqlx::SqliteConnection,
    device_id: i64,
    t: &str,
    r: &Reading,
) -> anyhow::Result<()> {
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
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// How many polls between writes of the battery's grid share to the database.
/// About a minute at the default interval.
const GRID_ORIGIN_SAVE_EVERY: u32 = 30;

/// A reading and the instant it was taken, which only ever travel together.
#[derive(Clone, Copy)]
struct Sample<'a> {
    reading: &'a Reading,
    at: DateTime<Utc>,
}

/// Trapezoidal integration of the interval [prev → curr] into Energy rows.
///
/// Returns `Ok(false)` when the interval was too long to integrate; see
/// `devices::max_integration_gap_ms`.
async fn save_energy(
    tx: &mut sqlx::SqliteConnection,
    device_id: i64,
    prev: Sample<'_>,
    curr: Sample<'_>,
    poll_interval_secs: u64,
    origin: &mut crate::energy::GridOrigin,
) -> anyhow::Result<bool> {
    let (prev_r, prev_t, curr_r, curr_t) = (prev.reading, prev.at, curr.reading, curr.at);
    let dt_ms = (curr_t - prev_t).num_milliseconds();
    if dt_ms <= 0 || dt_ms > crate::devices::max_integration_gap_ms(poll_interval_secs) {
        // The battery kept working while Dom was not watching, so the provenance
        // has to cross the gap even though no energy is recorded for it.
        origin.resync(prev_r.RemainingCapacity_Wh, curr_r.RemainingCapacity_Wh);
        return Ok(false);
    }
    let dt = dt_ms as f64 / 1000.0;
    let t = ts(curr_t);

    // Where the grid's energy went, and where the battery's came from. Recorded
    // as ordinary metrics so the existing rollup tiers carry them upwards — see
    // `crate::energy` for why `consumption - grid_import` stops being
    // self-sufficiency once the battery charges overnight.
    let accounted = origin.account(
        prev_r.flows(),
        curr_r.flows(),
        dt,
        prev_r.RemainingCapacity_Wh,
        curr_r.RemainingCapacity_Wh,
        curr_r.RSOC,
    );
    for (metric, wh) in [
        ("grid_to_house", accounted.grid_to_house_wh),
        ("grid_to_battery", accounted.grid_to_battery_wh),
    ] {
        sqlx::query(
            "INSERT INTO Energy (device_id, timestamp, resolution, metric, energy_ws)
             VALUES (?, ?, '2s', ?, ?)",
        )
        .bind(device_id)
        .bind(&t)
        .bind(metric)
        .bind(wh * 3600.0)
        .execute(&mut *tx)
        .await?;
    }

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
        .execute(&mut *tx)
        .await?;
    }
    Ok(true)
}

async fn save_storage(
    tx: &mut sqlx::SqliteConnection,
    device_id: i64,
    t: &str,
    rsoc: f64,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO EnergyStorage (device_id, timestamp, resolution, rsoc_avg)
         VALUES (?, ?, '2s', ?)",
    )
    .bind(device_id)
    .bind(t)
    .bind(rsoc)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Writes everything one poll produced, as a single transaction.
///
/// Returns whether the interval since the previous reading was integrated;
/// `false` means it was treated as a gap. A failure rolls the whole poll back
/// rather than leaving a raw reading with no energy beside it — the tiers are
/// derived from these rows, and half a poll is worse than none.
#[allow(clippy::too_many_arguments)]
async fn write_poll(
    pool: &SqlitePool,
    device: &DeviceRecord,
    t: &str,
    curr: &Reading,
    prev: Option<Sample<'_>>,
    poll_time: DateTime<Utc>,
    poll_interval_secs: u64,
    origin: &mut crate::energy::GridOrigin,
) -> anyhow::Result<bool> {
    let mut tx = pool.begin().await?;

    save_raw(&mut tx, device.id, t, curr).await?;
    let integrated = match prev {
        Some(prev) => {
            save_energy(
                &mut tx,
                device.id,
                prev,
                Sample {
                    reading: curr,
                    at: poll_time,
                },
                poll_interval_secs,
                origin,
            )
            .await?
        }
        None => false,
    };
    save_storage(&mut tx, device.id, t, curr.RSOC).await?;

    tx.commit().await?;
    Ok(integrated)
}

// ── Poll loop ─────────────────────────────────────────────────────────────────

pub async fn poll_loop(pool: SqlitePool, device: DeviceRecord, state: SharedState) {
    let mut ticker = crate::devices::PollTicker::new(device.id, device.poll_interval_secs);

    let api_key = device.api_key.clone().unwrap_or_default();
    let mut prev: Option<(Reading, DateTime<Utc>)> = None;
    // Cached total capacity; updated whenever RSOC is reliable enough to derive it.
    let mut known_capacity_kwh: f64 = 0.0;
    // How much of what the battery holds came from the grid. Carried across
    // restarts because it is path-dependent and cannot be recomputed — see
    // `crate::energy::GridOrigin`.
    let mut origin = crate::energy::GridOrigin::new(
        crate::db::get_grid_origin_wh(&pool, device.id)
            .await
            .unwrap_or(0.0),
    );
    let mut polls_since_saved: u32 = 0;
    let mut failures: u32 = 0;

    loop {
        ticker.tick(&pool).await;
        let poll_time = Utc::now();

        match fetch_status(device.ip, device.port, &api_key).await {
            Ok(curr) => {
                let t = ts(poll_time);

                // Everything this poll writes goes in one transaction. Written
                // separately, each of the eleven inserts was its own commit, and
                // a commit is a disk flush: on this device alone that was five
                // and a half flushes a second, forever. One transaction makes it
                // one. See `db::init` for the other half of that.
                match write_poll(
                    &pool,
                    &device,
                    &t,
                    &curr,
                    prev.as_ref().map(|(r, at)| Sample {
                        reading: r,
                        at: *at,
                    }),
                    poll_time,
                    ticker.secs(),
                    &mut origin,
                )
                .await
                {
                    Ok(integrated) => {
                        crate::devices::note_write_ok(&state);
                        if let (false, Some((_, prev_t))) = (integrated, prev.as_ref()) {
                            // A skipped interval is a gap in the record, not an
                            // error: logged so a poll loop that keeps stalling is
                            // visible, but never integrated. See
                            // `MAX_INTEGRATION_GAP_MS`.
                            log::debug!(
                                "sonnen {}: {} ms since the previous reading, too long to \
                                 integrate; that interval is recorded as missing",
                                device.ip,
                                (poll_time - *prev_t).num_milliseconds()
                            );
                        }
                    }
                    Err(e) => crate::devices::note_write_failure(
                        &state,
                        &format!("sonnen {}", device.ip),
                        &e,
                    ),
                }

                // Persisted about once a minute rather than every poll: losing a
                // minute of it across an unclean shutdown costs nothing, since
                // the next `resync` scales whatever is stored to the battery's
                // actual state anyway.
                polls_since_saved += 1;
                if polls_since_saved >= GRID_ORIGIN_SAVE_EVERY {
                    polls_since_saved = 0;
                    if let Err(e) =
                        crate::db::set_grid_origin_wh(&pool, device.id, origin.stored_grid_wh())
                            .await
                    {
                        log::debug!("could not record the battery's grid share: {e:#}");
                    }
                }

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
        let ip = IpAddr::from([172, 16, 20, 8]);
        Fingerprint {
            ip,
            open_ports: ports,
            http: raws
                .into_iter()
                .map(|raw| HttpProbe {
                    ip,
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

    // ── Integration across gaps ───────────────────────────────────────────────

    fn reading(production_w: f64) -> Reading {
        Reading {
            Consumption_W: 0.0,
            Production_W: production_w,
            Pac_total_W: 0.0,
            GridFeedIn_W: 0.0,
            RSOC: 50.0,
            RemainingCapacity_Wh: 5_000.0,
        }
    }

    /// An in-memory database with one Sonnen row, so the `Energy` foreign key
    /// resolves. The real schema, per the project's no-mocks rule.
    async fn pool_with_device() -> (SqlitePool, i64) {
        let pool = crate::db::init("sqlite://:memory:").await.unwrap();
        let ip = IpAddr::from([172, 16, 20, 8]);
        save_device(&pool, ip, "battery", None).await.unwrap();
        let id = load_all(&pool).await.unwrap()[0].id;
        (pool, id)
    }

    /// The interval the battery is actually polled at.
    const POLL: u64 = DEFAULT_POLL_SECS as u64;

    /// A fresh provenance accumulator, for the tests that only care about the
    /// trapezoid and not about where the energy came from.
    fn origin() -> crate::energy::GridOrigin {
        crate::energy::GridOrigin::default()
    }

    /// Runs one interval through `save_energy` on a connection borrowed just for
    /// the call. The in-memory pool holds a single connection, so a test that
    /// kept one open while querying would deadlock against itself.
    async fn save_interval(
        pool: &SqlitePool,
        device_id: i64,
        prev: Sample<'_>,
        curr: Sample<'_>,
        poll_interval_secs: u64,
        origin: &mut crate::energy::GridOrigin,
    ) -> anyhow::Result<bool> {
        let mut conn = pool.acquire().await?;
        super::save_energy(&mut conn, device_id, prev, curr, poll_interval_secs, origin).await
    }

    async fn energy_rows(pool: &SqlitePool) -> Vec<f64> {
        sqlx::query_scalar::<_, f64>(
            "SELECT energy_ws FROM Energy WHERE metric = 'production' ORDER BY id",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// Row counts across the three tables a poll writes to.
    async fn row_counts(pool: &SqlitePool) -> (i64, i64, i64) {
        let one = |sql: &'static str| async move {
            sqlx::query_scalar::<_, i64>(sql)
                .fetch_one(pool)
                .await
                .unwrap()
        };
        (
            one("SELECT COUNT(*) FROM RawDeviceMeasurements").await,
            one("SELECT COUNT(*) FROM Energy").await,
            one("SELECT COUNT(*) FROM EnergyStorage").await,
        )
    }

    fn record(id: i64) -> DeviceRecord {
        DeviceRecord {
            id,
            name: "battery".into(),
            ip: IpAddr::from([172, 16, 20, 8]),
            port: 8080,
            api_key: None,
            poll_interval_secs: DEFAULT_POLL_SECS,
            label: None,
        }
    }

    #[tokio::test]
    async fn one_poll_is_one_transaction() {
        // Written separately, a poll's eleven inserts were eleven commits, and a
        // commit is a disk flush — five and a half a second, forever, which is
        // what wears out an SD card. They are now one transaction.
        let (pool, id) = pool_with_device().await;
        let t0 = chrono::DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let t1 = t0 + chrono::Duration::seconds(DEFAULT_POLL_SECS);
        let mut origin = origin();

        let integrated = write_poll(
            &pool,
            &record(id),
            &ts(t1),
            &reading(3_000.0),
            Some(Sample {
                reading: &reading(3_000.0),
                at: t0,
            }),
            t1,
            POLL,
            &mut origin,
        )
        .await
        .unwrap();

        assert!(integrated);
        let (raw, energy, storage) = row_counts(&pool).await;
        assert_eq!(raw, 4, "one per raw metric");
        assert_eq!(energy, 6, "four flows plus the two grid-origin series");
        assert_eq!(storage, 1);
    }

    #[tokio::test]
    async fn a_poll_that_fails_partway_leaves_nothing_behind() {
        // The property one transaction buys. The tiers are derived from these
        // rows, and a raw reading with no energy beside it is worse than nothing.
        let (pool, _) = pool_with_device().await;
        let before = row_counts(&pool).await;
        let t0 = chrono::DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let t1 = t0 + chrono::Duration::seconds(DEFAULT_POLL_SECS);

        // No such device, so the foreign key rejects the first insert.
        let result = write_poll(
            &pool,
            &record(9_999),
            &ts(t1),
            &reading(3_000.0),
            Some(Sample {
                reading: &reading(3_000.0),
                at: t0,
            }),
            t1,
            POLL,
            &mut origin(),
        )
        .await;

        assert!(result.is_err(), "the poll should have failed");
        assert_eq!(
            row_counts(&pool).await,
            before,
            "a failed poll must leave the database as it found it"
        );
    }

    #[tokio::test]
    async fn a_normal_interval_is_integrated() {
        let (pool, id) = pool_with_device().await;
        let mut origin = origin();
        let t0 = chrono::DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let t1 = t0 + chrono::Duration::seconds(DEFAULT_POLL_SECS);

        let integrated = save_interval(
            &pool,
            id,
            Sample {
                reading: &reading(3_000.0),
                at: t0,
            },
            Sample {
                reading: &reading(3_000.0),
                at: t1,
            },
            POLL,
            &mut origin,
        )
        .await
        .unwrap();

        assert!(integrated);
        // 3 kW held for 2 s is 6000 Ws — the trapezoid, exactly.
        assert_eq!(energy_rows(&pool).await, vec![6_000.0]);
    }

    #[tokio::test]
    async fn a_gap_is_recorded_as_missing_rather_than_integrated() {
        // The bug this guards: a stalled poll loop leaves `prev` hours old, and
        // integrating across it attributed 10.8 kWh to one row labelled `2s`,
        // inflating that day's total by 23% while looking entirely plausible.
        let (pool, id) = pool_with_device().await;
        let mut origin = origin();
        let t0 = chrono::DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let t1 = t0 + chrono::Duration::hours(3);

        let integrated = save_interval(
            &pool,
            id,
            Sample {
                reading: &reading(3_000.0),
                at: t0,
            },
            Sample {
                reading: &reading(3_000.0),
                at: t1,
            },
            POLL,
            &mut origin,
        )
        .await
        .unwrap();

        assert!(!integrated);
        assert!(
            energy_rows(&pool).await.is_empty(),
            "a gap must leave a hole, not invented energy"
        );
    }

    #[tokio::test]
    async fn the_gap_limit_is_a_boundary_not_a_range() {
        let (pool, id) = pool_with_device().await;
        let mut origin = origin();
        let t0 = chrono::DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let at = |ms| t0 + chrono::Duration::milliseconds(ms);
        let gap = crate::devices::max_integration_gap_ms(POLL);

        // Exactly at the limit still counts; one millisecond past it does not.
        assert!(
            save_interval(
                &pool,
                id,
                Sample {
                    reading: &reading(1.0),
                    at: t0
                },
                Sample {
                    reading: &reading(1.0),
                    at: at(gap)
                },
                POLL,
                &mut origin
            )
            .await
            .unwrap()
        );
        assert!(
            !save_interval(
                &pool,
                id,
                Sample {
                    reading: &reading(1.0),
                    at: t0
                },
                Sample {
                    reading: &reading(1.0),
                    at: at(gap + 1)
                },
                POLL,
                &mut origin
            )
            .await
            .unwrap()
        );
        // A device configured to poll more slowly still records. The limit is
        // derived from the device's own interval for exactly this reason: a fixed
        // one tuned to the two-second default would refuse every interval here
        // and silently stop recording all of its energy.
        let slow = 30;
        assert!(
            save_interval(
                &pool,
                id,
                Sample {
                    reading: &reading(1.0),
                    at: t0
                },
                Sample {
                    reading: &reading(1.0),
                    at: at(slow as i64 * 1_000)
                },
                slow,
                &mut origin
            )
            .await
            .unwrap()
        );

        // A clock that went backwards, and a repeated timestamp, are both refused.
        assert!(
            !save_interval(
                &pool,
                id,
                Sample {
                    reading: &reading(1.0),
                    at: t0
                },
                Sample {
                    reading: &reading(1.0),
                    at: t0
                },
                POLL,
                &mut origin
            )
            .await
            .unwrap()
        );
        assert!(
            !save_interval(
                &pool,
                id,
                Sample {
                    reading: &reading(1.0),
                    at: t0
                },
                Sample {
                    reading: &reading(1.0),
                    at: at(-1)
                },
                POLL,
                &mut origin
            )
            .await
            .unwrap()
        );
    }
}
