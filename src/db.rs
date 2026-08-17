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
