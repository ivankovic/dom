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

use sqlx::{Row, SqlitePool, sqlite::SqliteConnectOptions};
use std::collections::HashMap;
use std::net::IpAddr;
use std::str::FromStr;

pub async fn init(uri: &str) -> anyhow::Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(uri)
        .map_err(|e| anyhow::anyhow!(e))?
        .create_if_missing(true);
    let pool = if uri.contains(":memory:") {
        sqlx::pool::PoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?
    } else {
        SqlitePool::connect_with(opts).await?
    };

    sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&pool)
        .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS Devices (
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            type                TEXT    NOT NULL,
            name                TEXT    NOT NULL,
            ip                  TEXT    NOT NULL UNIQUE,
            poll_interval_secs  INTEGER NOT NULL DEFAULT 2,
            api_key             TEXT,
            username            TEXT,
            password            TEXT,
            label               TEXT,
            auto_mode           TEXT    NOT NULL DEFAULT 'disabled'
        )",
    )
    .execute(&pool)
    .await?;

    // Idempotent migrations: silently ignored if columns already exist.
    let _ = sqlx::query("ALTER TABLE Devices ADD COLUMN label TEXT")
        .execute(&pool)
        .await;
    let _ =
        sqlx::query("ALTER TABLE Devices ADD COLUMN auto_mode TEXT NOT NULL DEFAULT 'disabled'")
            .execute(&pool)
            .await;
    // Stable per-device identity (currently the LAN MAC address, from the ARP
    // cache) independent of IP, so a device that gets a new DHCP lease can be
    // recognized as the same physical device rather than showing up as a
    // second, permanently-unreachable entry — see `upsert_device`.
    let _ = sqlx::query("ALTER TABLE Devices ADD COLUMN fingerprint TEXT")
        .execute(&pool)
        .await;

    // NULLs are distinct in a SQLite unique index, so devices without a known
    // fingerprint yet (or types that don't support one) never collide here —
    // only two rows of the same type with the *same* non-null fingerprint do.
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_devices_type_fingerprint
         ON Devices (type, fingerprint)",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS SwitchTimers (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            device_id  INTEGER NOT NULL REFERENCES Devices(id),
            time_hhmm  TEXT    NOT NULL,
            relay_on   INTEGER NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS RawDeviceMeasurements (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            device_id   INTEGER NOT NULL REFERENCES Devices(id),
            timestamp   TEXT    NOT NULL,
            metric      TEXT    NOT NULL,
            value       REAL    NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_raw_metric_time
         ON RawDeviceMeasurements (device_id, metric, timestamp DESC)",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS Energy (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            device_id   INTEGER NOT NULL REFERENCES Devices(id),
            timestamp   TEXT    NOT NULL,
            resolution  TEXT    NOT NULL CHECK (resolution IN ('2s', '1min', '10min')),
            metric      TEXT    NOT NULL,
            energy_ws   REAL    NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_energy_metric_res_time
         ON Energy (device_id, metric, resolution, timestamp DESC)",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS EnergyStorage (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            device_id   INTEGER NOT NULL REFERENCES Devices(id),
            timestamp   TEXT    NOT NULL,
            resolution  TEXT    NOT NULL CHECK (resolution IN ('2s', '1min', '10min')),
            rsoc_avg    REAL    NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_energystorage_res_time
         ON EnergyStorage (device_id, resolution, timestamp DESC)",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS NetworkStatusEvents (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            ip                TEXT    NOT NULL,
            label             TEXT,
            timestamp         TEXT    NOT NULL,
            status            TEXT    NOT NULL,
            previous_status   TEXT    NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_networkstatusevents_time
         ON NetworkStatusEvents (timestamp DESC)",
    )
    .execute(&pool)
    .await?;

    // Application settings that outlive a run, e.g. the chosen colour theme.
    // A key/value table rather than a column per setting so adding a setting
    // needs no migration.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS Config (
            key    TEXT PRIMARY KEY,
            value  TEXT NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    // Per-local-day energy totals, rolled up from the 2s rows in `Energy`.
    //
    // The long-term statistics view cannot read `Energy` directly: at 2s
    // resolution a single month is already millions of rows (a bare COUNT over
    // 47 days measured at 16 seconds), and nothing prunes that table. Rolled up
    // by day, a year is a few thousand rows and every window is instant.
    //
    // `metric` here is *derived*, not a copy of `Energy.metric`: the signed
    // `grid` series is split into separate `grid_import` and `grid_export`
    // totals at rollup time, because summing a signed series per day would
    // collapse the two into a net figure that can't be separated afterwards.
    //
    // `device_id` is kept even though the statistics view sums over devices, so
    // the table stays a faithful aggregation of `Energy` and a per-device
    // breakdown needs no migration.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS EnergyDaily (
            device_id  INTEGER NOT NULL REFERENCES Devices(id),
            day        TEXT    NOT NULL,
            metric     TEXT    NOT NULL,
            energy_wh  REAL    NOT NULL,
            PRIMARY KEY (device_id, day, metric)
        )",
    )
    .execute(&pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_energydaily_day ON EnergyDaily (day, metric)")
        .execute(&pool)
        .await?;

    // Per-minute energy, rolled up from the 2s rows in `Energy`.
    //
    // The middle tier of `2s -> 1min -> daily`. Its purpose is to be the tier
    // that *survives*: the 2s series is only kept for a few days (see
    // `prune_energy_2s`), so anything a chart or a future historical view might
    // want has to be recoverable from here.
    //
    // A separate table rather than `Energy` rows with `resolution = '1min'`,
    // which the `resolution` column originally anticipated. Three reasons: the
    // aggregate-only columns below are meaningless for a 2s sample; a
    // (device, minute, metric) primary key gives idempotent upserts, which
    // `Energy` has no unique key for; and adding rows to `Energy` would grow
    // `idx_energy_metric_res_time`, already the single largest object in the
    // database at 914 MB against a 555 MB table.
    //
    // `energy_ws` stays the exact integral — summing 2s energies into a minute
    // loses no energy at all, only intra-minute shape. The positive and negative
    // parts are stored separately because a sign split is *not* tier-invariant: a
    // minute in which the battery both charged and discharged nets out, so
    // splitting after aggregation would understate both directions.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS EnergyMinute (
            device_id      INTEGER NOT NULL REFERENCES Devices(id),
            minute         TEXT    NOT NULL,
            metric         TEXT    NOT NULL,
            energy_ws      REAL    NOT NULL,
            energy_ws_pos  REAL    NOT NULL,
            energy_ws_neg  REAL    NOT NULL,
            span_secs      INTEGER NOT NULL,
            peak_w         REAL    NOT NULL,
            PRIMARY KEY (device_id, minute, metric)
        )",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_energyminute_time ON EnergyMinute (minute, metric)",
    )
    .execute(&pool)
    .await?;

    // Which local days each rollup tier has already processed.
    //
    // Recorded explicitly rather than inferred from whether a tier produced rows,
    // because "produced nothing" is a legitimate outcome — a day with no battery
    // samples yields no `StorageDaily` row — and inferring would leave such a day
    // looking unprocessed forever, re-scanning all history on every pass.
    //
    // Keyed by tier so that adding a tier later backfills across existing history
    // on its own: its marker set starts empty, so every day is outstanding for it
    // while the established tiers are skipped.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS RollupProgress (
            tier  TEXT NOT NULL,
            day   TEXT NOT NULL,
            PRIMARY KEY (tier, day)
        )",
    )
    .execute(&pool)
    .await?;

    // Per-local-day battery state of charge.
    //
    // `EnergyStorage` records RSOC every 2 seconds and nothing has ever read it —
    // 65 MB of rows plus a 124 MB index that no query touches. Rolled up to one
    // row per device-day so the 2s rows can be pruned without foreclosing
    // battery-health statistics later. RSOC is a state, not a flow, so min/max/avg
    // are the useful summaries rather than a sum.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS StorageDaily (
            device_id  INTEGER NOT NULL REFERENCES Devices(id),
            day        TEXT    NOT NULL,
            rsoc_min   REAL    NOT NULL,
            rsoc_max   REAL    NOT NULL,
            rsoc_avg   REAL    NOT NULL,
            samples    INTEGER NOT NULL,
            PRIMARY KEY (device_id, day)
        )",
    )
    .execute(&pool)
    .await?;

    Ok(pool)
}

/// Reads a setting, or `None` if it was never set. Callers are expected to have
/// a default rather than treating absence as an error — a fresh database has no
/// settings at all.
pub async fn get_config(pool: &SqlitePool, key: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT value FROM Config WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?)
}

/// Writes a setting, replacing any previous value for the same key.
pub async fn set_config(pool: &SqlitePool, key: &str, value: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO Config (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

// ── Daily energy rollup ───────────────────────────────────────────────────────

/// Metrics written into `EnergyDaily`. The first two are copied straight from
/// `Energy`; the grid pair is derived by splitting the signed `grid` series.
pub const DAILY_METRICS: [&str; 4] = ["consumption", "production", "grid_import", "grid_export"];

/// How many of the most recent days with data are re-rolled on every pass.
///
/// Two, not one. Re-rolling only the current day leaves a gap: if the app is not
/// running at the moment a day ends — closed at 23:50, opened at 00:10 — that
/// day's last minutes are never aggregated, and because a row already exists it
/// would look complete and never be revisited. Re-rolling the previous day too
/// closes that without having to track a watermark.
const DAYS_ALWAYS_REROLLED: usize = 2;

/// The UTC instants bounding a local calendar day, as DB timestamp strings:
/// `[start, end)`.
///
/// `Energy.timestamp` holds naive UTC, so a local day has to be expressed as a
/// UTC half-open range before it can be compared against an indexed column —
/// `date(timestamp, 'localtime')` would work but is not indexable, and without
/// the index a single day's aggregation reads every row that device ever wrote.
///
/// The upper bound is the *next local midnight* rather than start plus 24 hours,
/// which is what makes a 23- or 25-hour DST day come out right.
fn local_day_bounds_utc(day: chrono::NaiveDate) -> Option<(String, String)> {
    use chrono::{Local, TimeZone};

    let local_midnight = |d: chrono::NaiveDate| -> Option<chrono::DateTime<chrono::Utc>> {
        let naive = d.and_hms_opt(0, 0, 0)?;
        // A spring-forward transition can make a local wall-clock time
        // non-existent; `earliest` then gives the first instant that does exist.
        Local
            .from_local_datetime(&naive)
            .earliest()
            .map(|dt| dt.with_timezone(&chrono::Utc))
    };

    let start = local_midnight(day)?;
    let end = local_midnight(day.succ_opt()?)?;
    Some((
        start
            .naive_utc()
            .format(crate::devices::DB_TIMESTAMP_FMT)
            .to_string(),
        end.naive_utc()
            .format(crate::devices::DB_TIMESTAMP_FMT)
            .to_string(),
    ))
}

/// Aggregates one local day of one device's 2s rows into `EnergyMinute`.
///
/// Only *complete* minutes are written: `cutoff` must be the start of the current
/// UTC minute, and rows at or after it are ignored. A partial minute row would
/// otherwise be indistinguishable from a whole one, and would then be treated as
/// final once the 2s rows behind it were pruned.
///
/// Idempotent by replacement rather than accumulation, so re-rolling a day that
/// has since gained more samples corrects it.
pub async fn rollup_energy_minute(
    pool: &SqlitePool,
    device_id: i64,
    day: chrono::NaiveDate,
    cutoff: &str,
) -> anyhow::Result<u64> {
    let Some((start, end)) = local_day_bounds_utc(day) else {
        return Ok(0);
    };
    // Never look past the last complete minute, even if the day extends beyond it.
    let end = if end.as_str() < cutoff {
        end
    } else {
        cutoff.to_string()
    };
    if start >= end {
        return Ok(0);
    }

    let result = sqlx::query(
        "INSERT INTO EnergyMinute
             (device_id, minute, metric, energy_ws, energy_ws_pos, energy_ws_neg,
              span_secs, peak_w)
         SELECT device_id,
                strftime('%Y-%m-%d %H:%M:00', timestamp),
                metric,
                SUM(energy_ws),
                SUM(CASE WHEN energy_ws > 0 THEN  energy_ws ELSE 0 END),
                SUM(CASE WHEN energy_ws < 0 THEN -energy_ws ELSE 0 END),
                -- Seconds actually covered, not a nominal 60: a device that was
                -- offline for part of the minute must not have its average power
                -- diluted across time it never reported.
                COUNT(*) * 2,
                -- Each 2s row's energy is already a 2-second average power once
                -- divided by its interval; the peak is the largest of those. This
                -- is the one quantity pruning the 2s rows makes unrecoverable.
                MAX(ABS(energy_ws)) / 2.0
         FROM Energy
         WHERE device_id = ?
           AND resolution = '2s'
           AND timestamp >= ?
           AND timestamp < ?
         GROUP BY device_id, strftime('%Y-%m-%d %H:%M:00', timestamp), metric
         ON CONFLICT(device_id, minute, metric) DO UPDATE SET
             energy_ws     = excluded.energy_ws,
             energy_ws_pos = excluded.energy_ws_pos,
             energy_ws_neg = excluded.energy_ws_neg,
             span_secs     = excluded.span_secs,
             peak_w        = excluded.peak_w",
    )
    .bind(device_id)
    .bind(&start)
    .bind(&end)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Start of the current UTC minute — the cutoff for `rollup_energy_minute`.
pub fn current_minute_start() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:00").to_string()
}

/// Summarises one local day of one device's `EnergyStorage` rows into
/// `StorageDaily`.
pub async fn rollup_storage_day(
    pool: &SqlitePool,
    device_id: i64,
    day: chrono::NaiveDate,
) -> anyhow::Result<()> {
    let Some((start, end)) = local_day_bounds_utc(day) else {
        return Ok(());
    };
    let day_str = day.format("%Y-%m-%d").to_string();

    let row = sqlx::query(
        "SELECT MIN(rsoc_avg) AS lo, MAX(rsoc_avg) AS hi, AVG(rsoc_avg) AS mean,
                COUNT(*) AS samples
         FROM EnergyStorage
         WHERE device_id = ? AND resolution = '2s' AND timestamp >= ? AND timestamp < ?",
    )
    .bind(device_id)
    .bind(&start)
    .bind(&end)
    .fetch_one(pool)
    .await?;

    let samples: i64 = row.get("samples");
    if samples == 0 {
        return Ok(());
    }
    let lo: f64 = row.get("lo");
    let hi: f64 = row.get("hi");
    let mean: f64 = row.get("mean");

    sqlx::query(
        "INSERT INTO StorageDaily (device_id, day, rsoc_min, rsoc_max, rsoc_avg, samples)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(device_id, day) DO UPDATE SET
             rsoc_min = excluded.rsoc_min,
             rsoc_max = excluded.rsoc_max,
             rsoc_avg = excluded.rsoc_avg,
             samples  = excluded.samples",
    )
    .bind(device_id)
    .bind(&day_str)
    .bind(lo)
    .bind(hi)
    .bind(mean)
    .bind(samples)
    .execute(pool)
    .await?;
    Ok(())
}

/// Aggregates one local day of one device's 2s rows into `EnergyDaily`.
///
/// Scoped to a single device because that is what the existing
/// `idx_energy_metric_res_time` index can serve: with `device_id` as its leading
/// column plus a bounded timestamp range this is an index range scan (measured
/// at ~0.16s per device-day against 12.7M rows), where the same aggregation
/// across all devices at once degrades to scanning the range for every device.
///
/// Idempotent: re-rolling a day replaces its totals rather than adding to them,
/// so a partially-elapsed day can be refreshed as often as needed.
pub async fn rollup_energy_day(
    pool: &SqlitePool,
    device_id: i64,
    day: chrono::NaiveDate,
) -> anyhow::Result<()> {
    let Some((start, end)) = local_day_bounds_utc(day) else {
        return Ok(());
    };
    let day_str = day.format("%Y-%m-%d").to_string();

    let row = sqlx::query(
        "SELECT
             SUM(CASE WHEN metric = 'consumption' THEN energy_ws ELSE 0 END) / 3600.0 AS consumption,
             SUM(CASE WHEN metric = 'production'  THEN energy_ws ELSE 0 END) / 3600.0 AS production,
             -- The signed grid series: negative is drawn from the grid,
             -- positive is fed back into it. Split here; a daily sum of the
             -- signed values could never be separated again.
             SUM(CASE WHEN metric = 'grid' AND energy_ws < 0 THEN -energy_ws ELSE 0 END) / 3600.0
                 AS grid_import,
             SUM(CASE WHEN metric = 'grid' AND energy_ws > 0 THEN  energy_ws ELSE 0 END) / 3600.0
                 AS grid_export,
             COUNT(*) AS samples
         FROM Energy
         WHERE device_id = ?
           AND resolution = '2s'
           AND timestamp >= ?
           AND timestamp < ?",
    )
    .bind(device_id)
    .bind(&start)
    .bind(&end)
    .fetch_one(pool)
    .await?;

    let samples: i64 = row.get("samples");
    if samples == 0 {
        // Nothing that day. Leave any existing rows alone rather than writing
        // zeros, so a device that was simply offline is distinguishable from one
        // that genuinely used nothing.
        return Ok(());
    }

    for metric in DAILY_METRICS {
        let wh: Option<f64> = row.get(metric);
        let wh = wh.unwrap_or(0.0);
        sqlx::query(
            "INSERT INTO EnergyDaily (device_id, day, metric, energy_wh)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(device_id, day, metric) DO UPDATE SET energy_wh = excluded.energy_wh",
        )
        .bind(device_id)
        .bind(&day_str)
        .bind(metric)
        .bind(wh)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// The rollup tiers, in the order they must run for a given day: each reads what
/// the one before it wrote, or the 2s series in the case of the first two.
const TIERS: [&str; 3] = ["minute", "storage", "daily"];

/// Brings every rollup tier up to date, returning how many local days it worked.
///
/// Advances `2s -> EnergyMinute`, `EnergyStorage -> StorageDaily` and
/// `2s -> EnergyDaily` together, so the tiers never drift apart: a coarser tier
/// must always cover at least as much history as the finer one, or pruning the 2s
/// series would drop data that never made it upwards.
///
/// Progress is tracked per tier in `RollupProgress`, so a newly added tier
/// backfills across history the others already cover, while they are skipped.
/// The `DAYS_ALWAYS_REROLLED` most recent days are always redone regardless.
///
/// `throttle` is awaited between days: the first run after a tier is added has the
/// whole history to work through against a live multi-gigabyte database, and
/// spreading it out keeps it from starving the poll loops of I/O.
pub async fn rollup_history(
    pool: &SqlitePool,
    throttle: std::time::Duration,
) -> anyhow::Result<usize> {
    let Some(first_day) = oldest_energy_sample_day(pool).await? else {
        return Ok(0);
    };
    let today = chrono::Local::now().date_naive();

    let mut days: Vec<chrono::NaiveDate> = Vec::new();
    let mut d = first_day;
    while d <= today {
        days.push(d);
        let Some(next) = d.succ_opt() else { break };
        d = next;
    }
    let always_from = days.len().saturating_sub(DAYS_ALWAYS_REROLLED);

    // For each day, which tiers still need to run on it.
    let mut plan: Vec<(chrono::NaiveDate, Vec<&'static str>)> = Vec::new();
    for tier in TIERS {
        let done = distinct_days(
            pool,
            "SELECT day FROM RollupProgress WHERE tier = ?",
            Some(tier),
        )
        .await?;
        for (i, day) in days.iter().enumerate() {
            let outstanding =
                i >= always_from || !done.contains(&day.format("%Y-%m-%d").to_string());
            if !outstanding {
                continue;
            }
            match plan.iter_mut().find(|(d, _)| d == day) {
                Some((_, tiers)) => tiers.push(tier),
                None => plan.push((*day, vec![tier])),
            }
        }
    }
    if plan.is_empty() {
        return Ok(0);
    }
    plan.sort_by_key(|(day, _)| *day);

    let device_ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM Devices")
        .fetch_all(pool)
        .await?;
    let cutoff = current_minute_start();

    let mut worked = 0usize;
    for (day, tiers) in plan {
        for &device_id in &device_ids {
            // Run in tier order so a day's minute rows exist before anything
            // that may later be derived from them.
            for tier in TIERS.iter().filter(|t| tiers.contains(t)) {
                match *tier {
                    "minute" => {
                        rollup_energy_minute(pool, device_id, day, &cutoff).await?;
                    }
                    "storage" => rollup_storage_day(pool, device_id, day).await?,
                    "daily" => rollup_energy_day(pool, device_id, day).await?,
                    _ => {}
                }
            }
        }
        // Mark the day done for every tier that ran, whether or not it produced
        // rows. Today is deliberately marked too: it is inside the
        // always-rerolled window, so the marker never stops it being redone.
        for tier in &tiers {
            sqlx::query(
                "INSERT INTO RollupProgress (tier, day) VALUES (?, ?)
                 ON CONFLICT(tier, day) DO NOTHING",
            )
            .bind(tier)
            .bind(day.format("%Y-%m-%d").to_string())
            .execute(pool)
            .await?;
        }
        worked += 1;
        if !throttle.is_zero() {
            tokio::time::sleep(throttle).await;
        }
    }
    Ok(worked)
}

/// Local day of the oldest 2s energy sample, or `None` when there are none.
async fn oldest_energy_sample_day(pool: &SqlitePool) -> anyhow::Result<Option<chrono::NaiveDate>> {
    let oldest: Option<Option<String>> = sqlx::query_scalar("SELECT MIN(timestamp) FROM Energy")
        .fetch_optional(pool)
        .await?;
    let Some(Some(oldest)) = oldest else {
        return Ok(None);
    };
    Ok(
        chrono::NaiveDateTime::parse_from_str(&oldest, crate::devices::DB_TIMESTAMP_FMT)
            .ok()
            .map(|ndt| {
                chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(ndt, chrono::Utc)
                    .with_timezone(&chrono::Local)
                    .date_naive()
            }),
    )
}

/// Runs a query returning one `YYYY-MM-DD` column and collects it into a set.
async fn distinct_days(
    pool: &SqlitePool,
    query: &str,
    bind: Option<&str>,
) -> anyhow::Result<std::collections::HashSet<String>> {
    let mut q = sqlx::query_scalar::<_, Option<String>>(query);
    if let Some(b) = bind {
        q = q.bind(b.to_string());
    }
    Ok(q.fetch_all(pool).await?.into_iter().flatten().collect())
}

/// One local day's energy totals, summed over every device, in kWh.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DailyEnergy {
    pub day: chrono::NaiveDate,
    pub consumption_kwh: f64,
    pub production_kwh: f64,
    pub grid_import_kwh: f64,
    pub grid_export_kwh: f64,
}

/// Daily totals for the inclusive local-date range `[from, to]`, oldest first.
///
/// Reads only `EnergyDaily`, so cost is proportional to the number of days asked
/// for rather than to the 2s rows behind them. Days with no rolled-up data are
/// absent from the result rather than returned as zeros — a gap in history is
/// not the same claim as a day of no usage, and the statistics view renders the
/// two differently.
pub async fn query_daily_energy(
    pool: &SqlitePool,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
) -> anyhow::Result<Vec<DailyEnergy>> {
    let rows = sqlx::query(
        "SELECT day, metric, SUM(energy_wh) / 1000.0 AS kwh
         FROM EnergyDaily
         WHERE day >= ? AND day <= ?
         GROUP BY day, metric
         ORDER BY day",
    )
    .bind(from.format("%Y-%m-%d").to_string())
    .bind(to.format("%Y-%m-%d").to_string())
    .fetch_all(pool)
    .await?;

    let mut by_day: std::collections::BTreeMap<chrono::NaiveDate, DailyEnergy> =
        std::collections::BTreeMap::new();
    for row in rows {
        let day_str: String = row.get("day");
        let Ok(day) = chrono::NaiveDate::parse_from_str(&day_str, "%Y-%m-%d") else {
            continue;
        };
        let metric: String = row.get("metric");
        let kwh: f64 = row.get("kwh");
        let entry = by_day.entry(day).or_insert_with(|| DailyEnergy {
            day,
            ..Default::default()
        });
        match metric.as_str() {
            "consumption" => entry.consumption_kwh = kwh,
            "production" => entry.production_kwh = kwh,
            "grid_import" => entry.grid_import_kwh = kwh,
            "grid_export" => entry.grid_export_kwh = kwh,
            _ => {}
        }
    }
    Ok(by_day.into_values().collect())
}

/// Oldest local day that has any rolled-up energy data, or `None` when the
/// rollup table is still empty. Bounds how far back the statistics view lets you
/// browse, so Left stops at the start of history rather than walking through
/// unbounded empty periods.
pub async fn oldest_energy_day(pool: &SqlitePool) -> anyhow::Result<Option<chrono::NaiveDate>> {
    let day: Option<Option<String>> = sqlx::query_scalar("SELECT MIN(day) FROM EnergyDaily")
        .fetch_optional(pool)
        .await?;
    Ok(day
        .flatten()
        .and_then(|d| chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok()))
}

/// Queries today's Energy table and returns per-minute kW averages for each metric.
/// x = hours since local midnight (e.g. 13.5 = 13:30), y = avg kW for that minute bucket.
///
/// Timestamps are stored as naive UTC strings, so "today" and the hour bucketing
/// both apply SQLite's `'localtime'` modifier — otherwise the day boundary drifts
/// by the local UTC offset (e.g. still showing "yesterday" just after local midnight).
pub async fn query_today_energy(pool: &SqlitePool) -> anyhow::Result<crate::app::EnergyChartData> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT
             (CAST(strftime('%H', timestamp, 'localtime') AS REAL) * 60
              + CAST(strftime('%M', timestamp, 'localtime') AS REAL)) / 60.0 AS t_hours,
             metric,
             SUM(energy_ws) / (COUNT(*) * 2.0) / 1000.0 AS avg_kw
         FROM Energy
         WHERE date(timestamp, 'localtime') = date('now', 'localtime')
           AND resolution = '2s'
           AND metric IN ('consumption', 'production', 'pac', 'grid')
         GROUP BY strftime('%Y-%m-%d %H:%M', timestamp, 'localtime'), metric
         ORDER BY t_hours",
    )
    .fetch_all(pool)
    .await?;

    let mut data = crate::app::EnergyChartData::default();
    for row in rows {
        let t: f64 = row.get("t_hours");
        let metric: String = row.get("metric");
        let kw: f64 = row.get("avg_kw");
        match metric.as_str() {
            "consumption" => data.consumption.push((t, kw)),
            "production" => data.production.push((t, kw)),
            "pac" => data.battery.push((t, kw)),
            "grid" => data.grid.push((t, kw)),
            _ => {}
        }
    }

    // Daily grid import/export totals.
    let totals = sqlx::query(
        "SELECT
             SUM(CASE WHEN energy_ws < 0 THEN ABS(energy_ws) ELSE 0 END) / 3600.0 / 1000.0 AS imported_kwh,
             SUM(CASE WHEN energy_ws > 0 THEN energy_ws          ELSE 0 END) / 3600.0 / 1000.0 AS exported_kwh
         FROM Energy
         WHERE date(timestamp, 'localtime') = date('now', 'localtime')
           AND resolution = '2s'
           AND metric = 'grid'",
    )
    .fetch_one(pool)
    .await?;
    let imported: Option<f64> = totals.get("imported_kwh");
    let exported: Option<f64> = totals.get("exported_kwh");
    data.grid_imported_kwh = imported.unwrap_or(0.0);
    data.grid_exported_kwh = exported.unwrap_or(0.0);

    Ok(data)
}

/// Returns per-device energy breakdown for today.
/// Battery: charged_kwh (pac < 0) and discharged_kwh (pac > 0) shown separately.
/// Switch: kwh from power metric (charged/discharged stay 0).
pub async fn query_device_energy_today(
    pool: &SqlitePool,
) -> anyhow::Result<std::collections::HashMap<std::net::IpAddr, crate::app::DeviceEnergyToday>> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT d.ip, d.type,
                SUM(CASE WHEN e.energy_ws < 0 THEN ABS(e.energy_ws) ELSE 0 END) / 3600.0 / 1000.0 AS kwh_charged,
                SUM(CASE WHEN e.energy_ws > 0 THEN e.energy_ws          ELSE 0 END) / 3600.0 / 1000.0 AS kwh_discharged,
                SUM(ABS(e.energy_ws))                                              / 3600.0 / 1000.0 AS kwh_total
         FROM Energy e
         JOIN Devices d ON e.device_id = d.id
         WHERE date(e.timestamp, 'localtime') = date('now', 'localtime')
           AND e.resolution = '2s'
           AND (
             (d.type = 'sonnen_eco8'    AND e.metric = 'pac')
             OR
             (d.type = 'mystrom_switch' AND e.metric = 'power')
             OR
             (d.type = 'keba'          AND e.metric = 'power')
           )
         GROUP BY e.device_id",
    )
    .fetch_all(pool)
    .await?;

    let mut out = std::collections::HashMap::new();
    for row in rows {
        let ip_str: String = row.get("ip");
        let Ok(ip) = ip_str.parse::<std::net::IpAddr>() else {
            continue;
        };
        let device_type: String = row.get("type");
        let entry = if device_type == "sonnen_eco8" {
            crate::app::DeviceEnergyToday {
                kwh: 0.0,
                charged_kwh: row.get("kwh_charged"),
                discharged_kwh: row.get("kwh_discharged"),
            }
        } else {
            crate::app::DeviceEnergyToday {
                kwh: row.get("kwh_total"),
                charged_kwh: 0.0,
                discharged_kwh: 0.0,
            }
        };
        out.insert(ip, entry);
    }
    Ok(out)
}

/// Persists a network-infrastructure status transition (e.g. Router OK → LOST).
pub async fn record_network_status_event(
    pool: &SqlitePool,
    ip: std::net::IpAddr,
    label: Option<&str>,
    previous_status: &str,
    status: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO NetworkStatusEvents (ip, label, timestamp, status, previous_status)
         VALUES (?, ?, datetime('now'), ?, ?)",
    )
    .bind(ip.to_string())
    .bind(label)
    .bind(status)
    .bind(previous_status)
    .execute(pool)
    .await?;
    Ok(())
}

/// Loads the most recent network-infrastructure status events, newest first,
/// to repopulate the in-memory list shown in the Network view after a restart.
pub async fn query_recent_network_status_events(
    pool: &SqlitePool,
    limit: i64,
) -> anyhow::Result<Vec<crate::app::NetworkStatusEvent>> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT ip, label, timestamp, status, previous_status
         FROM NetworkStatusEvents
         ORDER BY timestamp DESC, id DESC
         LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::new();
    for row in rows {
        let ip_str: String = row.get("ip");
        let Ok(ip) = ip_str.parse::<std::net::IpAddr>() else {
            continue;
        };
        let ts_str: String = row.get("timestamp");
        let Some(at) =
            chrono::NaiveDateTime::parse_from_str(&ts_str, crate::devices::DB_TIMESTAMP_FMT)
                .ok()
                .map(|ndt| ndt.and_utc())
        else {
            continue;
        };
        out.push(crate::app::NetworkStatusEvent {
            ip,
            label: row.get("label"),
            previous: parse_network_status(&row.get::<String, _>("previous_status")),
            current: parse_network_status(&row.get::<String, _>("status")),
            at,
        });
    }
    Ok(out)
}

fn parse_network_status(s: &str) -> crate::app::NetworkDeviceStatus {
    match s {
        "SLOW" => crate::app::NetworkDeviceStatus::Slow,
        "DEGRADED" => crate::app::NetworkDeviceStatus::Degraded,
        "LOST" => crate::app::NetworkDeviceStatus::Lost,
        "UNKNOWN" => crate::app::NetworkDeviceStatus::Unknown,
        _ => crate::app::NetworkDeviceStatus::Ok,
    }
}

/// Deletes NetworkStatusEvents rows older than 30 days. Returns rows deleted.
pub async fn prune_network_status_events(pool: &SqlitePool) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "DELETE FROM NetworkStatusEvents WHERE timestamp < datetime('now', '-30 days')",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Queries today's modem (lte1) traffic samples and buckets them at 2-minute
/// resolution. x = hours since local midnight, y = average throughput in kbps.
/// Samples are stored as per-poll byte deltas (see `devices::mikrotik`), so
/// summing a bucket and dividing by its 120s width gives average bytes/sec.
pub async fn query_internet_traffic_today(
    pool: &SqlitePool,
) -> anyhow::Result<crate::app::InternetTrafficChartData> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT
             bucket * 2.0 / 60.0 AS t_hours,
             metric,
             SUM(value) / 120.0 AS bytes_per_sec
         FROM (
             SELECT r.metric, r.value,
                    CAST((CAST(strftime('%H', r.timestamp, 'localtime') AS INTEGER) * 60
                          + CAST(strftime('%M', r.timestamp, 'localtime') AS INTEGER)) / 2 AS INTEGER) AS bucket
             FROM RawDeviceMeasurements r
             WHERE date(r.timestamp, 'localtime') = date('now', 'localtime')
               AND r.metric IN ('traffic_rx_bytes', 'traffic_tx_bytes')
         )
         GROUP BY bucket, metric
         ORDER BY bucket",
    )
    .fetch_all(pool)
    .await?;

    let mut data = crate::app::InternetTrafficChartData::default();
    for row in rows {
        let t: f64 = row.get("t_hours");
        let metric: String = row.get("metric");
        let bytes_per_sec: f64 = row.get("bytes_per_sec");
        let kbps = bytes_per_sec * 8.0 / 1000.0;
        match metric.as_str() {
            "traffic_rx_bytes" => data.rx_kbps.push((t, kbps)),
            "traffic_tx_bytes" => data.tx_kbps.push((t, kbps)),
            _ => {}
        }
    }
    Ok(data)
}

/// Updates (or clears) the user-assigned label for a device. Pass empty string to clear.
pub async fn update_device_label(
    pool: &SqlitePool,
    ip: std::net::IpAddr,
    label: &str,
) -> anyhow::Result<()> {
    let label_opt: Option<&str> = if label.is_empty() { None } else { Some(label) };
    sqlx::query("UPDATE Devices SET label = ? WHERE ip = ?")
        .bind(label_opt)
        .bind(ip.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Loads all devices with their labels from the database.
/// Returns a map from IP address to label.
pub async fn load_all_device_labels(
    pool: &SqlitePool,
) -> anyhow::Result<HashMap<IpAddr, Option<String>>> {
    let rows = sqlx::query("SELECT ip, label FROM Devices WHERE label IS NOT NULL")
        .fetch_all(pool)
        .await?;

    let mut labels = HashMap::new();
    for row in rows {
        let ip_str: String = row.get("ip");
        let ip: IpAddr = ip_str
            .parse()
            .map_err(|_| anyhow::anyhow!("bad IP in DB: {}", ip_str))?;
        let label: Option<String> = row.get("label");
        labels.insert(ip, label);
    }
    Ok(labels)
}

/// Updates the label for all NetworkStatusEvents entries for a given IP address.
/// Used when a device is renamed to update historical status events.
pub async fn update_network_status_events_label(
    pool: &SqlitePool,
    ip: IpAddr,
    label: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE NetworkStatusEvents SET label = ? WHERE ip = ?")
        .bind(label)
        .bind(ip.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Loads auto-mode settings and scheduled timers for all myStrom switch devices.
pub async fn load_switch_configs(
    pool: &SqlitePool,
) -> anyhow::Result<(
    std::collections::HashMap<std::net::IpAddr, crate::app::SwitchAutoMode>,
    std::collections::HashMap<std::net::IpAddr, Vec<crate::app::SwitchTimer>>,
)> {
    use sqlx::Row;
    let device_rows =
        sqlx::query("SELECT ip, auto_mode FROM Devices WHERE type = 'mystrom_switch'")
            .fetch_all(pool)
            .await?;

    let mut modes = std::collections::HashMap::new();
    for row in &device_rows {
        let ip_str: String = row.get("ip");
        let Ok(ip) = ip_str.parse::<std::net::IpAddr>() else {
            continue;
        };
        let mode_str: String = row.get("auto_mode");
        let mode = if mode_str == "time" {
            crate::app::SwitchAutoMode::Time
        } else {
            crate::app::SwitchAutoMode::Disabled
        };
        modes.insert(ip, mode);
    }

    let timer_rows = sqlx::query(
        "SELECT t.id, t.time_hhmm, t.relay_on, d.ip
         FROM SwitchTimers t
         JOIN Devices d ON t.device_id = d.id
         WHERE d.type = 'mystrom_switch'
         ORDER BY d.ip, t.time_hhmm",
    )
    .fetch_all(pool)
    .await?;

    let mut timers: std::collections::HashMap<std::net::IpAddr, Vec<crate::app::SwitchTimer>> =
        std::collections::HashMap::new();
    for row in timer_rows {
        let ip_str: String = row.get("ip");
        let Ok(ip) = ip_str.parse::<std::net::IpAddr>() else {
            continue;
        };
        let relay_on_i: i32 = row.get("relay_on");
        timers.entry(ip).or_default().push(crate::app::SwitchTimer {
            id: row.get("id"),
            time_hhmm: row.get("time_hhmm"),
            relay_on: relay_on_i != 0,
        });
    }

    Ok((modes, timers))
}

/// Persists the auto-mode setting for a switch device.
pub async fn set_switch_auto_mode(
    pool: &SqlitePool,
    ip: std::net::IpAddr,
    mode: &crate::app::SwitchAutoMode,
) -> anyhow::Result<()> {
    let mode_str = match mode {
        crate::app::SwitchAutoMode::Disabled => "disabled",
        crate::app::SwitchAutoMode::Time => "time",
    };
    sqlx::query("UPDATE Devices SET auto_mode = ? WHERE ip = ?")
        .bind(mode_str)
        .bind(ip.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Loads a KEBA wallbox's persisted charging mode. Reuses the same generic
/// `auto_mode` column switches use for their own, unrelated auto-mode setting —
/// safe since each device type is always queried by its own `type`, and the
/// column's schema default ('disabled') already matches ChargingMode::Disabled.
pub async fn load_keba_mode(
    pool: &SqlitePool,
    ip: std::net::IpAddr,
) -> anyhow::Result<crate::devices::keba::ChargingMode> {
    let mode_str: String = sqlx::query_scalar("SELECT auto_mode FROM Devices WHERE ip = ?")
        .bind(ip.to_string())
        .fetch_one(pool)
        .await?;
    Ok(crate::devices::keba::ChargingMode::from_db_str(&mode_str))
}

/// Persists a KEBA wallbox's charging mode.
pub async fn set_keba_mode(
    pool: &SqlitePool,
    ip: std::net::IpAddr,
    mode: crate::devices::keba::ChargingMode,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE Devices SET auto_mode = ? WHERE ip = ?")
        .bind(mode.as_db_str())
        .bind(ip.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

pub struct SonnenAvgs {
    pub production_w: f64,
    pub consumption_w: f64,
    /// Sonnen's own sign convention: positive = battery discharging, negative
    /// = charging.
    pub pac_w: f64,
}

/// Average production/consumption/battery-charge readings from the last
/// `window_secs` seconds of Sonnen samples. Used by KEBA Eco mode to estimate
/// how much power is left over for the car once the house and the battery
/// have taken their share. Returns `None` if any of the three metrics has no
/// samples in the window (e.g. right after startup).
pub async fn query_recent_sonnen_avgs(
    pool: &SqlitePool,
    window_secs: i64,
) -> anyhow::Result<Option<SonnenAvgs>> {
    use sqlx::Row;
    let row = sqlx::query(
        "SELECT
             AVG(CASE WHEN r.metric = 'production' THEN r.value END) AS production,
             AVG(CASE WHEN r.metric = 'consumption' THEN r.value END) AS consumption,
             AVG(CASE WHEN r.metric = 'pac' THEN r.value END) AS pac
         FROM RawDeviceMeasurements r
         JOIN Devices d ON r.device_id = d.id
         WHERE d.type = 'sonnen_eco8'
           AND r.timestamp >= datetime('now', ? || ' seconds')",
    )
    .bind(format!("-{window_secs}"))
    .fetch_one(pool)
    .await?;

    let (production, consumption, pac): (Option<f64>, Option<f64>, Option<f64>) = (
        row.get("production"),
        row.get("consumption"),
        row.get("pac"),
    );
    Ok(match (production, consumption, pac) {
        (Some(production_w), Some(consumption_w), Some(pac_w)) => Some(SonnenAvgs {
            production_w,
            consumption_w,
            pac_w,
        }),
        _ => None,
    })
}

/// Average of a single device's own metric over the last `window_secs`
/// seconds. Used by KEBA Eco mode to get the car's *windowed* power draw over
/// the same interval as `query_recent_sonnen_avgs`, rather than a single live
/// snapshot — averaging both over the same window is what makes the "add the
/// car back in" arithmetic in `eco_decision` a stable fixed point instead of
/// a moving target (see that function's docs). Returns `None` if there are no
/// samples for this device/metric in the window.
pub async fn query_recent_device_metric_avg(
    pool: &SqlitePool,
    device_id: i64,
    metric: &str,
    window_secs: i64,
) -> anyhow::Result<Option<f64>> {
    let avg: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(value)
         FROM RawDeviceMeasurements
         WHERE device_id = ?
           AND metric = ?
           AND timestamp >= datetime('now', ? || ' seconds')",
    )
    .bind(device_id)
    .bind(metric)
    .bind(format!("-{window_secs}"))
    .fetch_one(pool)
    .await?;
    Ok(avg)
}

/// Inserts a new timer for a switch device. Returns the new row's id.
pub async fn add_switch_timer(
    pool: &SqlitePool,
    ip: std::net::IpAddr,
    time_hhmm: &str,
    relay_on: bool,
) -> anyhow::Result<i64> {
    let result = sqlx::query(
        "INSERT INTO SwitchTimers (device_id, time_hhmm, relay_on)
         SELECT id, ?, ? FROM Devices WHERE ip = ?",
    )
    .bind(time_hhmm)
    .bind(relay_on as i32)
    .bind(ip.to_string())
    .execute(pool)
    .await?;
    Ok(result.last_insert_rowid())
}

/// Deletes a switch timer by its primary key id.
pub async fn delete_switch_timer(pool: &SqlitePool, timer_id: i64) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM SwitchTimers WHERE id = ?")
        .bind(timer_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Sets (or updates) the username/password login for a device by IP. Used for
/// devices authenticating with HTTP Basic Auth (e.g. MikroTik). Credentials
/// are configured here rather than passed through source code, so each
/// device can hold its own independent login.
pub async fn set_device_login(
    pool: &SqlitePool,
    ip: std::net::IpAddr,
    username: &str,
    password: &str,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE Devices SET username = ?, password = ? WHERE ip = ?")
        .bind(username)
        .bind(password)
        .bind(ip.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Sets (or updates) the API key for a device by IP (e.g. Sonnen's Auth-Token).
pub async fn set_device_api_key(
    pool: &SqlitePool,
    ip: std::net::IpAddr,
    api_key: &str,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE Devices SET api_key = ? WHERE ip = ?")
        .bind(api_key)
        .bind(ip.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Deletes RawDeviceMeasurements rows older than 24 hours. Returns rows deleted.
pub async fn prune_raw_measurements(pool: &SqlitePool) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "DELETE FROM RawDeviceMeasurements WHERE timestamp < datetime('now', '-1 day')",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Registers a discovered device, identified by `(device_type, ip)` as
/// before, but reconciled against `fingerprint` (e.g. a MAC address) when
/// one is known. If a device of the same type with the same fingerprint
/// already exists at a *different* ip, that device has moved — this moves
/// its row to the new `ip` in place (preserving its id, credentials, label
/// and history) instead of the new address being registered as an
/// unrelated, second device. Otherwise this behaves like a plain
/// upsert-by-ip, same as before fingerprinting existed.
pub async fn upsert_device(
    pool: &SqlitePool,
    device_type: &str,
    name: &str,
    ip: IpAddr,
    poll_interval_secs: i64,
    fingerprint: Option<&str>,
) -> anyhow::Result<()> {
    let ip_str = ip.to_string();

    if let Some(fp) = fingerprint {
        let existing: Option<(i64, String)> =
            sqlx::query_as("SELECT id, ip FROM Devices WHERE type = ? AND fingerprint = ?")
                .bind(device_type)
                .bind(fp)
                .fetch_optional(pool)
                .await?;
        if let Some((existing_id, existing_ip)) = existing
            && existing_ip != ip_str
        {
            migrate_device_ip(pool, existing_id, &ip_str).await?;
        }
    }

    sqlx::query(
        "INSERT INTO Devices (type, name, ip, poll_interval_secs, fingerprint)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(ip) DO UPDATE SET
             name               = excluded.name,
             poll_interval_secs = excluded.poll_interval_secs,
             fingerprint         = COALESCE(excluded.fingerprint, fingerprint)",
    )
    .bind(device_type)
    .bind(name)
    .bind(&ip_str)
    .bind(poll_interval_secs)
    .bind(fingerprint)
    .execute(pool)
    .await?;
    Ok(())
}

/// Moves device `id` to `new_ip`. If another row already occupies `new_ip`
/// (e.g. it was auto-discovered as a "new" device before the fingerprint
/// match caught up), that row's measurement/timer history is reassigned onto
/// `id` first and the now-empty row is removed — `id` (with its
/// credentials, label and history) is always the survivor, since it's the
/// one the fingerprint proves is the same physical device.
async fn migrate_device_ip(pool: &SqlitePool, id: i64, new_ip: &str) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;

    let collision: Option<i64> = sqlx::query_scalar("SELECT id FROM Devices WHERE ip = ?")
        .bind(new_ip)
        .fetch_optional(&mut *tx)
        .await?;

    if let Some(loser_id) = collision {
        for table in [
            "RawDeviceMeasurements",
            "Energy",
            "EnergyStorage",
            "SwitchTimers",
        ] {
            sqlx::query(&format!(
                "UPDATE {table} SET device_id = ? WHERE device_id = ?"
            ))
            .bind(id)
            .bind(loser_id)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("DELETE FROM Devices WHERE id = ?")
            .bind(loser_id)
            .execute(&mut *tx)
            .await?;
    }

    sqlx::query("UPDATE Devices SET ip = ? WHERE id = ?")
        .bind(new_ip)
        .bind(id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(())
}

/// True if device `id`'s address in the DB no longer matches `ip` (or the
/// row is gone entirely) — i.e. a fingerprint match moved this row out from
/// under a poll loop still bound to the old address. Poll loops check this
/// once they've gone `Lost`, so a stale loop chasing an address the device
/// left behind exits on its own rather than failing forever; a fresh loop
/// for the new address is already running, spawned by the next discovery
/// cycle.
pub async fn device_moved(pool: &SqlitePool, id: i64, ip: IpAddr) -> anyhow::Result<bool> {
    let current: Option<String> = sqlx::query_scalar("SELECT ip FROM Devices WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(current != Some(ip.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // These exercise the raw (non-macro) SQL in the new network-history/traffic
    // queries against a real in-memory schema, since sqlx::query isn't
    // compile-time checked the way sqlx::query! would be.

    // ── Daily energy rollup ───────────────────────────────────────────────────

    use chrono::NaiveDate;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    /// Inserts a 2s Energy sample at a given *local* wall-clock time.
    async fn sample(pool: &SqlitePool, device_id: i64, local: &str, metric: &str, ws: f64) {
        use chrono::{Local, TimeZone};
        let naive = chrono::NaiveDateTime::parse_from_str(local, "%Y-%m-%d %H:%M:%S").unwrap();
        let utc = Local
            .from_local_datetime(&naive)
            .earliest()
            .unwrap()
            .naive_utc();
        sqlx::query(
            "INSERT INTO Energy (device_id, timestamp, resolution, metric, energy_ws)
             VALUES (?, ?, '2s', ?, ?)",
        )
        .bind(device_id)
        .bind(utc.format(crate::devices::DB_TIMESTAMP_FMT).to_string())
        .bind(metric)
        .bind(ws)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn device(pool: &SqlitePool, ip: &str) -> i64 {
        upsert_device(pool, "sonnen_batterie", "b", ip.parse().unwrap(), 2, None)
            .await
            .unwrap();
        sqlx::query_scalar("SELECT id FROM Devices WHERE ip = ?")
            .bind(ip)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn local_day_bounds_span_exactly_one_local_day() {
        let (start, end) = local_day_bounds_utc(day("2026-03-15")).unwrap();
        assert!(start < end);
        // Whatever the offset, consecutive days must abut exactly: one day's end
        // is the next day's start, so no sample can fall in a gap or be counted
        // twice.
        let (next_start, _) = local_day_bounds_utc(day("2026-03-16")).unwrap();
        assert_eq!(end, next_start);
    }

    #[tokio::test]
    async fn rollup_on_an_empty_database_does_nothing_and_does_not_error() {
        // The first-launch path: no Energy rows at all. SELECT MIN() over an
        // empty table yields one NULL row, which must read as "nothing to do"
        // rather than failing to decode.
        let pool = init("sqlite::memory:").await.unwrap();
        let written = rollup_history(&pool, std::time::Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(written, 0);
        assert_eq!(oldest_energy_day(&pool).await.unwrap(), None);
        assert!(
            query_daily_energy(&pool, day("2026-01-01"), day("2026-12-31"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn rollup_with_devices_but_no_energy_rows_writes_nothing() {
        let pool = init("sqlite::memory:").await.unwrap();
        let _ = device(&pool, "10.0.0.9").await;
        assert_eq!(
            rollup_history(&pool, std::time::Duration::ZERO)
                .await
                .unwrap(),
            0
        );
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM EnergyDaily")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    // ── Minute and storage tiers ──────────────────────────────────────────────

    /// A cutoff far in the future, so tests roll up every minute they seeded.
    const NO_CUTOFF: &str = "2099-01-01 00:00:00";

    async fn storage_sample(pool: &SqlitePool, device_id: i64, local: &str, rsoc: f64) {
        use chrono::{Local, TimeZone};
        let naive = chrono::NaiveDateTime::parse_from_str(local, "%Y-%m-%d %H:%M:%S").unwrap();
        let utc = Local
            .from_local_datetime(&naive)
            .earliest()
            .unwrap()
            .naive_utc();
        sqlx::query(
            "INSERT INTO EnergyStorage (device_id, timestamp, resolution, rsoc_avg)
             VALUES (?, ?, '2s', ?)",
        )
        .bind(device_id)
        .bind(utc.format(crate::devices::DB_TIMESTAMP_FMT).to_string())
        .bind(rsoc)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn minute_rollup_preserves_energy_exactly_and_splits_by_sign() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.1.0.1").await;
        // Three samples inside one minute, two directions.
        sample(&pool, id, "2026-05-10 09:00:00", "grid", -3600.0).await;
        sample(&pool, id, "2026-05-10 09:00:02", "grid", -1800.0).await;
        sample(&pool, id, "2026-05-10 09:00:04", "grid", 7200.0).await;

        rollup_energy_minute(&pool, id, day("2026-05-10"), NO_CUTOFF)
            .await
            .unwrap();

        let row = sqlx::query(
            "SELECT energy_ws, energy_ws_pos, energy_ws_neg, span_secs, peak_w
             FROM EnergyMinute WHERE device_id = ? AND metric = 'grid'",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let net: f64 = row.get("energy_ws");
        let pos: f64 = row.get("energy_ws_pos");
        let neg: f64 = row.get("energy_ws_neg");
        let span: i64 = row.get("span_secs");
        let peak: f64 = row.get("peak_w");

        // Net is the exact integral: -3600 - 1800 + 7200.
        assert!((net - 1800.0).abs() < 1e-9, "{net}");
        // Both directions survive. Splitting after aggregation would have given
        // 1800 exported and nothing imported, losing 5400 Ws of import.
        assert!((pos - 7200.0).abs() < 1e-9, "{pos}");
        assert!((neg - 5400.0).abs() < 1e-9, "{neg}");
        assert_eq!(span, 6, "three 2s samples cover six seconds, not sixty");
        // Largest 2s average power in the minute: 7200 Ws over 2 s.
        assert!((peak - 3600.0).abs() < 1e-9, "{peak}");
    }

    #[tokio::test]
    async fn minute_rollup_buckets_by_minute() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.1.0.2").await;
        sample(&pool, id, "2026-05-10 09:00:58", "consumption", 3600.0).await;
        sample(&pool, id, "2026-05-10 09:01:00", "consumption", 7200.0).await;

        rollup_energy_minute(&pool, id, day("2026-05-10"), NO_CUTOFF)
            .await
            .unwrap();

        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM EnergyMinute")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            rows, 2,
            "samples either side of :00 belong to different minutes"
        );
    }

    #[tokio::test]
    async fn minute_rollup_skips_the_minute_still_in_progress() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.1.0.3").await;
        sample(&pool, id, "2026-05-10 09:00:00", "consumption", 3600.0).await;
        sample(&pool, id, "2026-05-10 09:01:00", "consumption", 3600.0).await;

        // Pretend "now" is inside 09:01, so only 09:00 is complete. Writing a
        // partial 09:01 row would look final once the 2s rows were pruned.
        let cutoff = {
            use chrono::{Local, TimeZone};
            let naive =
                chrono::NaiveDateTime::parse_from_str("2026-05-10 09:01:00", "%Y-%m-%d %H:%M:%S")
                    .unwrap();
            Local
                .from_local_datetime(&naive)
                .earliest()
                .unwrap()
                .naive_utc()
                .format(crate::devices::DB_TIMESTAMP_FMT)
                .to_string()
        };
        rollup_energy_minute(&pool, id, day("2026-05-10"), &cutoff)
            .await
            .unwrap();

        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM EnergyMinute")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "only the completed minute is written");
    }

    #[tokio::test]
    async fn minute_rollup_is_idempotent() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.1.0.4").await;
        sample(&pool, id, "2026-05-10 09:00:00", "consumption", 3600.0).await;

        for _ in 0..3 {
            rollup_energy_minute(&pool, id, day("2026-05-10"), NO_CUTOFF)
                .await
                .unwrap();
        }
        let (rows, total): (i64, f64) =
            sqlx::query_as("SELECT COUNT(*), SUM(energy_ws) FROM EnergyMinute")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rows, 1);
        assert!(
            (total - 3600.0).abs() < 1e-9,
            "replaced, not accumulated: {total}"
        );
    }

    #[tokio::test]
    async fn minute_tier_totals_match_the_daily_tier_exactly() {
        // The gate for pruning the 2s series: rolling a day up by minutes and
        // then summing those minutes must equal rolling the same day up directly.
        // Summing an integral in two stages is lossless, and this proves it on
        // real arithmetic rather than by assertion.
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.1.0.5").await;
        for (i, metric) in ["consumption", "production", "grid"].iter().enumerate() {
            for m in 0..90 {
                let ws = match *metric {
                    "grid" if m % 3 == 0 => -1234.5 - m as f64,
                    "grid" => 987.6 + m as f64,
                    _ => 1000.0 + (m as f64) * (i as f64 + 1.0),
                };
                sample(
                    &pool,
                    id,
                    &format!(
                        "2026-05-10 {:02}:{:02}:{:02}",
                        8 + m / 60,
                        m % 60,
                        (m * 7) % 60
                    ),
                    metric,
                    ws,
                )
                .await;
            }
        }

        rollup_energy_minute(&pool, id, day("2026-05-10"), NO_CUTOFF)
            .await
            .unwrap();
        rollup_energy_day(&pool, id, day("2026-05-10"))
            .await
            .unwrap();

        // From the daily tier.
        let daily = query_daily_energy(&pool, day("2026-05-10"), day("2026-05-10"))
            .await
            .unwrap();
        let d = &daily[0];

        // The same figures rebuilt from the minute tier.
        let row = sqlx::query(
            "SELECT
                 SUM(CASE WHEN metric='consumption' THEN energy_ws     ELSE 0 END)/3600.0/1000.0 AS c,
                 SUM(CASE WHEN metric='production'  THEN energy_ws     ELSE 0 END)/3600.0/1000.0 AS p,
                 SUM(CASE WHEN metric='grid'        THEN energy_ws_neg ELSE 0 END)/3600.0/1000.0 AS imp,
                 SUM(CASE WHEN metric='grid'        THEN energy_ws_pos ELSE 0 END)/3600.0/1000.0 AS exp
             FROM EnergyMinute WHERE device_id = ?",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let (c, p, imp, exp): (f64, f64, f64, f64) =
            (row.get("c"), row.get("p"), row.get("imp"), row.get("exp"));

        assert!(
            (c - d.consumption_kwh).abs() < 1e-9,
            "consumption {c} vs {:?}",
            d
        );
        assert!(
            (p - d.production_kwh).abs() < 1e-9,
            "production {p} vs {:?}",
            d
        );
        assert!(
            (imp - d.grid_import_kwh).abs() < 1e-9,
            "import {imp} vs {:?}",
            d
        );
        assert!(
            (exp - d.grid_export_kwh).abs() < 1e-9,
            "export {exp} vs {:?}",
            d
        );
    }

    #[tokio::test]
    async fn storage_rollup_summarises_state_of_charge() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.1.0.6").await;
        storage_sample(&pool, id, "2026-05-10 06:00:00", 20.0).await;
        storage_sample(&pool, id, "2026-05-10 12:00:00", 90.0).await;
        storage_sample(&pool, id, "2026-05-10 20:00:00", 40.0).await;
        // A different day must not bleed in.
        storage_sample(&pool, id, "2026-05-11 06:00:00", 5.0).await;

        rollup_storage_day(&pool, id, day("2026-05-10"))
            .await
            .unwrap();

        let row = sqlx::query(
            "SELECT rsoc_min, rsoc_max, rsoc_avg, samples FROM StorageDaily WHERE day = '2026-05-10'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let lo: f64 = row.get("rsoc_min");
        let hi: f64 = row.get("rsoc_max");
        let mean: f64 = row.get("rsoc_avg");
        let n: i64 = row.get("samples");
        assert_eq!((lo, hi, n), (20.0, 90.0, 3));
        assert!((mean - 50.0).abs() < 1e-9, "{mean}");
    }

    #[tokio::test]
    async fn storage_rollup_writes_nothing_when_there_are_no_samples() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.1.0.7").await;
        rollup_storage_day(&pool, id, day("2026-05-10"))
            .await
            .unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM StorageDaily")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn rollup_history_backfills_a_new_tier_across_days_the_old_one_already_covered() {
        // The migration case: EnergyDaily rows exist from before EnergyMinute
        // did. Those days must still be worked, or their minute rows would never
        // exist and pruning the 2s series would lose them permanently.
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.1.0.8").await;
        let today = chrono::Local::now().date_naive();
        for back in 2..6 {
            let d = today - chrono::Duration::days(back);
            sample(
                &pool,
                id,
                &format!("{} 09:00:00", d.format("%Y-%m-%d")),
                "consumption",
                3600.0,
            )
            .await;
            // Pretend the daily tier already covered this day.
            rollup_energy_day(&pool, id, d).await.unwrap();
        }
        let minutes_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM EnergyMinute")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(minutes_before, 0);

        rollup_history(&pool, std::time::Duration::ZERO)
            .await
            .unwrap();

        let minutes_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM EnergyMinute")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(minutes_after, 4, "every day with data gets minute rows");

        // And a second pass settles to just the re-rolled tail.
        let second = rollup_history(&pool, std::time::Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(second, DAYS_ALWAYS_REROLLED);
    }

    #[tokio::test]
    async fn local_day_bounds_are_converted_out_of_local_time() {
        use chrono::{Local, TimeZone};

        // The bug this guards against is treating the local date as if it were
        // already UTC: the bounds must land on local midnight, which in any
        // non-UTC zone is a different wall-clock time in UTC. Asserted by
        // converting back rather than against a fixed offset, so the test holds
        // in whatever zone it runs.
        let (start, _) = local_day_bounds_utc(day("2026-08-15")).unwrap();
        let start_utc =
            chrono::NaiveDateTime::parse_from_str(&start, crate::devices::DB_TIMESTAMP_FMT)
                .unwrap();
        let back = Local.from_utc_datetime(&start_utc);
        assert_eq!(
            back.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-08-15 00:00:00"
        );
    }

    #[tokio::test]
    async fn local_day_bounds_span_a_whole_day_across_a_dst_transition() {
        // In zones that observe it, the spring-forward day is 23 hours and the
        // autumn one 25. Taking the next local midnight rather than start+24h is
        // what keeps a day's samples from spilling into its neighbour.
        for d in ["2026-03-29", "2026-10-25", "2026-06-15"] {
            let (start, end) = local_day_bounds_utc(day(d)).unwrap();
            let s = chrono::NaiveDateTime::parse_from_str(&start, crate::devices::DB_TIMESTAMP_FMT)
                .unwrap();
            let e = chrono::NaiveDateTime::parse_from_str(&end, crate::devices::DB_TIMESTAMP_FMT)
                .unwrap();
            let hours = (e - s).num_hours();
            assert!(
                (23..=25).contains(&hours),
                "{d}: a local day should be 23-25 hours, got {hours}"
            );
        }
    }

    #[tokio::test]
    async fn rollup_sums_a_day_and_splits_grid_by_sign() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.0.0.1").await;

        // 3600 Ws = 1 Wh, so these are round numbers in Wh.
        sample(&pool, id, "2026-05-10 09:00:00", "consumption", 7200.0).await;
        sample(&pool, id, "2026-05-10 21:00:00", "consumption", 3600.0).await;
        sample(&pool, id, "2026-05-10 12:00:00", "production", 18000.0).await;
        // Negative grid is drawn from the grid, positive is fed back.
        sample(&pool, id, "2026-05-10 07:00:00", "grid", -3600.0).await;
        sample(&pool, id, "2026-05-10 08:00:00", "grid", -7200.0).await;
        sample(&pool, id, "2026-05-10 13:00:00", "grid", 10800.0).await;

        rollup_energy_day(&pool, id, day("2026-05-10"))
            .await
            .unwrap();

        let got = query_daily_energy(&pool, day("2026-05-10"), day("2026-05-10"))
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        let d = &got[0];
        assert!((d.consumption_kwh - 0.003).abs() < 1e-9, "{:?}", d);
        assert!((d.production_kwh - 0.005).abs() < 1e-9, "{:?}", d);
        // Import and export must stay separate, not collapse to a net figure.
        assert!((d.grid_import_kwh - 0.003).abs() < 1e-9, "{:?}", d);
        assert!((d.grid_export_kwh - 0.003).abs() < 1e-9, "{:?}", d);
    }

    #[tokio::test]
    async fn rollup_is_idempotent() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.0.0.2").await;
        sample(&pool, id, "2026-05-10 09:00:00", "consumption", 3600.0).await;

        for _ in 0..3 {
            rollup_energy_day(&pool, id, day("2026-05-10"))
                .await
                .unwrap();
        }

        let got = query_daily_energy(&pool, day("2026-05-10"), day("2026-05-10"))
            .await
            .unwrap();
        // Re-rolling replaces rather than accumulating.
        assert!((got[0].consumption_kwh - 0.001).abs() < 1e-9, "{got:?}");
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM EnergyDaily")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, DAILY_METRICS.len() as i64);
    }

    #[tokio::test]
    async fn rollup_keeps_days_separate() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.0.0.3").await;
        // Late on one day and early the next: the local-day boundary must put
        // these in different buckets.
        sample(&pool, id, "2026-05-10 23:59:00", "consumption", 3600.0).await;
        sample(&pool, id, "2026-05-11 00:01:00", "consumption", 7200.0).await;

        rollup_energy_day(&pool, id, day("2026-05-10"))
            .await
            .unwrap();
        rollup_energy_day(&pool, id, day("2026-05-11"))
            .await
            .unwrap();

        let got = query_daily_energy(&pool, day("2026-05-10"), day("2026-05-11"))
            .await
            .unwrap();
        assert_eq!(got.len(), 2);
        assert!((got[0].consumption_kwh - 0.001).abs() < 1e-9, "{got:?}");
        assert!((got[1].consumption_kwh - 0.002).abs() < 1e-9, "{got:?}");
    }

    #[tokio::test]
    async fn rollup_writes_nothing_for_a_day_with_no_samples() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.0.0.4").await;
        sample(&pool, id, "2026-05-10 09:00:00", "consumption", 3600.0).await;

        rollup_energy_day(&pool, id, day("2026-05-09"))
            .await
            .unwrap();

        // A day the device was offline stays absent, rather than being recorded
        // as a day of zero usage.
        let got = query_daily_energy(&pool, day("2026-05-09"), day("2026-05-09"))
            .await
            .unwrap();
        assert!(got.is_empty(), "{got:?}");
    }

    #[tokio::test]
    async fn query_daily_energy_sums_over_devices_and_skips_gaps() {
        let pool = init("sqlite::memory:").await.unwrap();
        let a = device(&pool, "10.0.0.5").await;
        let b = device(&pool, "10.0.0.6").await;
        sample(&pool, a, "2026-05-10 09:00:00", "consumption", 3600.0).await;
        sample(&pool, b, "2026-05-10 09:00:00", "consumption", 7200.0).await;
        // Nothing at all on the 11th; the 12th has data again.
        sample(&pool, a, "2026-05-12 09:00:00", "consumption", 3600.0).await;

        for d in ["2026-05-10", "2026-05-11", "2026-05-12"] {
            rollup_energy_day(&pool, a, day(d)).await.unwrap();
            rollup_energy_day(&pool, b, day(d)).await.unwrap();
        }

        let got = query_daily_energy(&pool, day("2026-05-10"), day("2026-05-12"))
            .await
            .unwrap();
        // Two days present, the empty one omitted rather than zero-filled.
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].day, day("2026-05-10"));
        assert!((got[0].consumption_kwh - 0.003).abs() < 1e-9, "{got:?}");
        assert_eq!(got[1].day, day("2026-05-12"));
    }

    #[tokio::test]
    async fn rollup_energy_daily_backfills_then_settles() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.0.0.7").await;
        let today = chrono::Local::now().date_naive();
        for back in 0..4 {
            let d = today - chrono::Duration::days(back);
            sample(
                &pool,
                id,
                &format!("{} 09:00:00", d.format("%Y-%m-%d")),
                "consumption",
                3600.0,
            )
            .await;
        }

        let first = rollup_history(&pool, std::time::Duration::ZERO)
            .await
            .unwrap();
        assert!(
            first >= 4,
            "backfill should cover every day with data: {first}"
        );

        // Second pass has nothing new, so it only re-rolls the most recent days.
        let second = rollup_history(&pool, std::time::Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(second, DAYS_ALWAYS_REROLLED);

        let got = query_daily_energy(&pool, today - chrono::Duration::days(3), today)
            .await
            .unwrap();
        assert_eq!(got.len(), 4);
        for d in &got {
            assert!((d.consumption_kwh - 0.001).abs() < 1e-9, "{got:?}");
        }
    }

    #[tokio::test]
    async fn config_returns_none_for_a_setting_never_written() {
        let pool = init("sqlite::memory:").await.unwrap();
        // A fresh DB has no settings; absence must be readable, not an error,
        // because callers fall back to a default on None.
        assert_eq!(get_config(&pool, "theme").await.unwrap(), None);
    }

    #[tokio::test]
    async fn config_round_trips_and_replaces_on_rewrite() {
        let pool = init("sqlite::memory:").await.unwrap();

        set_config(&pool, "theme", "light").await.unwrap();
        assert_eq!(
            get_config(&pool, "theme").await.unwrap().as_deref(),
            Some("light")
        );

        // Writing the same key again replaces rather than failing the PK or
        // leaving two rows — this is what toggling the theme twice does.
        set_config(&pool, "theme", "dark").await.unwrap();
        assert_eq!(
            get_config(&pool, "theme").await.unwrap().as_deref(),
            Some("dark")
        );
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM Config WHERE key = 'theme'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn config_keys_are_independent() {
        let pool = init("sqlite::memory:").await.unwrap();
        set_config(&pool, "theme", "light").await.unwrap();
        set_config(&pool, "other", "x").await.unwrap();
        assert_eq!(
            get_config(&pool, "theme").await.unwrap().as_deref(),
            Some("light")
        );
        assert_eq!(
            get_config(&pool, "other").await.unwrap().as_deref(),
            Some("x")
        );
    }

    #[tokio::test]
    async fn network_status_event_round_trip_is_newest_first() {
        let pool = init("sqlite::memory:").await.unwrap();
        let ip: std::net::IpAddr = "192.168.1.1".parse().unwrap();

        record_network_status_event(&pool, ip, Some("Router"), "OK", "LOST")
            .await
            .unwrap();
        record_network_status_event(&pool, ip, Some("Router"), "LOST", "OK")
            .await
            .unwrap();

        let events = query_recent_network_status_events(&pool, 10).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].previous, crate::app::NetworkDeviceStatus::Lost);
        assert_eq!(events[0].current, crate::app::NetworkDeviceStatus::Ok);
        assert_eq!(events[0].label.as_deref(), Some("Router"));
        assert_eq!(events[1].previous, crate::app::NetworkDeviceStatus::Ok);
        assert_eq!(events[1].current, crate::app::NetworkDeviceStatus::Lost);
    }

    #[tokio::test]
    async fn internet_traffic_query_buckets_and_converts_to_kbps() {
        let pool = init("sqlite::memory:").await.unwrap();
        sqlx::query(
            "INSERT INTO Devices (type, name, ip) VALUES ('mikrotik', 'modem', '10.0.0.1')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let device_id: i64 = sqlx::query_scalar("SELECT id FROM Devices WHERE ip = '10.0.0.1'")
            .fetch_one(&pool)
            .await
            .unwrap();

        // Two samples for the same instant (same 2-min bucket): 1000 rx bytes
        // and 200 tx bytes over the 120s bucket width.
        sqlx::query(
            "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
             VALUES (?, datetime('now'), 'traffic_rx_bytes', 1000)",
        )
        .bind(device_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
             VALUES (?, datetime('now'), 'traffic_tx_bytes', 200)",
        )
        .bind(device_id)
        .execute(&pool)
        .await
        .unwrap();

        let data = query_internet_traffic_today(&pool).await.unwrap();
        assert_eq!(data.rx_kbps.len(), 1);
        assert_eq!(data.tx_kbps.len(), 1);
        assert!((data.rx_kbps[0].1 - (1000.0 * 8.0 / 1000.0 / 120.0)).abs() < 1e-9);
        assert!((data.tx_kbps[0].1 - (200.0 * 8.0 / 1000.0 / 120.0)).abs() < 1e-9);
    }

    #[tokio::test]
    async fn upsert_device_without_fingerprint_behaves_like_plain_upsert_by_ip() {
        let pool = init("sqlite::memory:").await.unwrap();
        let ip: IpAddr = "10.1.0.1".parse().unwrap();

        upsert_device(&pool, "keba", "Wallbox v1", ip, 5, None)
            .await
            .unwrap();
        upsert_device(&pool, "keba", "Wallbox v2", ip, 5, None)
            .await
            .unwrap();

        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, name FROM Devices WHERE type = 'keba'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "Wallbox v2");
    }

    #[tokio::test]
    async fn upsert_device_migrates_ip_on_fingerprint_match() {
        let pool = init("sqlite::memory:").await.unwrap();
        let old_ip: IpAddr = "10.1.0.2".parse().unwrap();
        let new_ip: IpAddr = "10.1.0.3".parse().unwrap();

        upsert_device(
            &pool,
            "keba",
            "Wallbox",
            old_ip,
            5,
            Some("AA:BB:CC:DD:EE:FF"),
        )
        .await
        .unwrap();
        let id: i64 = sqlx::query_scalar("SELECT id FROM Devices WHERE type = 'keba'")
            .fetch_one(&pool)
            .await
            .unwrap();
        set_device_login(&pool, old_ip, "admin", "hunter2")
            .await
            .unwrap();
        update_device_label(&pool, old_ip, "Garage Wallbox")
            .await
            .unwrap();

        // Same fingerprint, new address: must move the *existing* row, not add one.
        upsert_device(
            &pool,
            "keba",
            "Wallbox",
            new_ip,
            5,
            Some("AA:BB:CC:DD:EE:FF"),
        )
        .await
        .unwrap();

        let rows: Vec<(i64, String, Option<String>, Option<String>)> =
            sqlx::query_as("SELECT id, ip, username, label FROM Devices WHERE type = 'keba'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 1, "must not create a second row");
        assert_eq!(rows[0].0, id, "must keep the original id");
        assert_eq!(rows[0].1, new_ip.to_string());
        assert_eq!(rows[0].2.as_deref(), Some("admin"), "credentials preserved");
        assert_eq!(
            rows[0].3.as_deref(),
            Some("Garage Wallbox"),
            "label preserved"
        );
    }

    #[tokio::test]
    async fn upsert_device_merges_history_when_new_ip_already_has_a_row() {
        let pool = init("sqlite::memory:").await.unwrap();
        let old_ip: IpAddr = "10.1.0.4".parse().unwrap();
        let new_ip: IpAddr = "10.1.0.5".parse().unwrap();

        // The real device, known by fingerprint, still at its old address.
        upsert_device(
            &pool,
            "keba",
            "Wallbox",
            old_ip,
            5,
            Some("11:22:33:44:55:66"),
        )
        .await
        .unwrap();
        let winner_id: i64 = sqlx::query_scalar("SELECT id FROM Devices WHERE ip = ?")
            .bind(old_ip.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
             VALUES (?, datetime('now'), 'power', 1.0)",
        )
        .bind(winner_id)
        .execute(&pool)
        .await
        .unwrap();

        // A second, un-fingerprinted row already discovered at the new address
        // (e.g. found by an earlier scan before this feature existed), with its
        // own accumulated history.
        upsert_device(&pool, "keba", "Wallbox", new_ip, 5, None)
            .await
            .unwrap();
        let loser_id: i64 = sqlx::query_scalar("SELECT id FROM Devices WHERE ip = ?")
            .bind(new_ip.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
             VALUES (?, datetime('now'), 'power', 2.0)",
        )
        .bind(loser_id)
        .execute(&pool)
        .await
        .unwrap();

        // Now the fingerprint match arrives for the new address: merge.
        upsert_device(
            &pool,
            "keba",
            "Wallbox",
            new_ip,
            5,
            Some("11:22:33:44:55:66"),
        )
        .await
        .unwrap();

        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, ip FROM Devices WHERE type = 'keba'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 1, "loser row must be removed");
        assert_eq!(rows[0].0, winner_id, "winner keeps its id");
        assert_eq!(rows[0].1, new_ip.to_string());

        let measurement_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM RawDeviceMeasurements")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            measurement_count, 2,
            "both devices' history survives, reassigned to the winner"
        );
        let orphaned: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM RawDeviceMeasurements WHERE device_id != ?")
                .bind(winner_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(orphaned, 0);
    }

    #[tokio::test]
    async fn device_moved_detects_ip_change_and_deletion() {
        let pool = init("sqlite::memory:").await.unwrap();
        let ip: IpAddr = "10.1.0.6".parse().unwrap();
        upsert_device(&pool, "keba", "Wallbox", ip, 5, None)
            .await
            .unwrap();
        let id: i64 = sqlx::query_scalar("SELECT id FROM Devices WHERE ip = ?")
            .bind(ip.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();

        assert!(!device_moved(&pool, id, ip).await.unwrap());

        sqlx::query("UPDATE Devices SET ip = '10.1.0.7' WHERE id = ?")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(device_moved(&pool, id, ip).await.unwrap());

        sqlx::query("DELETE FROM Devices WHERE id = ?")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(device_moved(&pool, id, ip).await.unwrap());
    }
}
