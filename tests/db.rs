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

mod common;

use std::net::IpAddr;

use dom::db;
use dom::devices::mystrom_switch;
use sqlx::Row;

#[tokio::test]
async fn schema_is_created_with_all_tables() {
    let pool = common::db().await;
    for table in [
        "Devices",
        "SwitchTimers",
        "RawDeviceMeasurements",
        "Energy",
        "EnergyStorage",
    ] {
        let count: i64 = sqlx::query(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap()
            .get(0);
        assert_eq!(count, 0, "table {table} should exist and be empty");
    }
}

#[tokio::test]
async fn update_device_label_sets_and_clears() {
    let pool = common::db().await;
    let ip: IpAddr = "192.168.1.10".parse().unwrap();
    mystrom_switch::save_device(&pool, ip, "Switch", None)
        .await
        .unwrap();

    db::update_device_label(&pool, ip, "Living Room")
        .await
        .unwrap();
    let label: Option<String> = sqlx::query("SELECT label FROM Devices WHERE ip = '192.168.1.10'")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("label");
    assert_eq!(label.as_deref(), Some("Living Room"));

    db::update_device_label(&pool, ip, "").await.unwrap();
    let label: Option<String> = sqlx::query("SELECT label FROM Devices WHERE ip = '192.168.1.10'")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("label");
    assert!(label.is_none());
}

#[tokio::test]
async fn switch_timer_roundtrip() {
    let pool = common::db().await;
    let ip: IpAddr = "10.0.0.1".parse().unwrap();
    mystrom_switch::save_device(&pool, ip, "Switch", None)
        .await
        .unwrap();

    let id = db::add_switch_timer(&pool, ip, "07:30", true)
        .await
        .unwrap();
    assert!(id > 0);

    let (_, timers) = db::load_switch_configs(&pool).await.unwrap();
    let ts = timers.get(&ip).unwrap();
    assert_eq!(ts.len(), 1);
    assert_eq!(ts[0].id, id);
    assert_eq!(ts[0].time_hhmm, "07:30");
    assert!(ts[0].relay_on);

    db::delete_switch_timer(&pool, id).await.unwrap();
    let (_, timers) = db::load_switch_configs(&pool).await.unwrap();
    assert!(timers.get(&ip).map(|v| v.is_empty()).unwrap_or(true));
}

#[tokio::test]
async fn set_switch_auto_mode_persists() {
    let pool = common::db().await;
    let ip: IpAddr = "10.0.0.2".parse().unwrap();
    mystrom_switch::save_device(&pool, ip, "Switch", None)
        .await
        .unwrap();

    db::set_switch_auto_mode(&pool, ip, &dom::app::SwitchAutoMode::Time)
        .await
        .unwrap();
    let (modes, _) = db::load_switch_configs(&pool).await.unwrap();
    assert!(matches!(
        modes.get(&ip),
        Some(dom::app::SwitchAutoMode::Time)
    ));

    db::set_switch_auto_mode(&pool, ip, &dom::app::SwitchAutoMode::Disabled)
        .await
        .unwrap();
    let (modes, _) = db::load_switch_configs(&pool).await.unwrap();
    assert!(matches!(
        modes.get(&ip),
        Some(dom::app::SwitchAutoMode::Disabled)
    ));
}

#[tokio::test]
async fn prune_raw_measurements_removes_old_rows() {
    let pool = common::db().await;
    sqlx::query(
        "INSERT INTO Devices (type, name, ip, poll_interval_secs) VALUES ('mystrom_switch', 'sw', '10.0.0.3', 2)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let device_id: i64 = sqlx::query("SELECT id FROM Devices WHERE ip = '10.0.0.3'")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get(0);

    sqlx::query(
        "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
         VALUES (?, datetime('now', '-2 days'), 'power', 100.0)",
    )
    .bind(device_id)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
         VALUES (?, datetime('now'), 'power', 200.0)",
    )
    .bind(device_id)
    .execute(&pool)
    .await
    .unwrap();

    let removed = db::prune_raw_measurements(&pool).await.unwrap();
    assert_eq!(removed, 1);

    let remaining: i64 = sqlx::query("SELECT COUNT(*) FROM RawDeviceMeasurements")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get(0);
    assert_eq!(remaining, 1);
}

#[tokio::test]
async fn query_today_energy_aggregates_energy_rows() {
    let pool = common::db().await;
    sqlx::query(
        "INSERT INTO Devices (type, name, ip, poll_interval_secs) VALUES ('sonnen_eco8', 'Battery', '10.0.0.4', 2)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let device_id: i64 = sqlx::query("SELECT id FROM Devices WHERE ip = '10.0.0.4'")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get(0);

    for metric in ["consumption", "production", "pac", "grid"] {
        sqlx::query(
            "INSERT INTO Energy (device_id, timestamp, resolution, metric, energy_ws)
             VALUES (?, datetime('now', '-1 minute'), '2s', ?, 360.0)",
        )
        .bind(device_id)
        .bind(metric)
        .execute(&pool)
        .await
        .unwrap();
    }

    let chart = db::query_today_energy(&pool).await.unwrap();
    assert_eq!(chart.consumption.len(), 1);
    assert_eq!(chart.production.len(), 1);
    assert_eq!(chart.battery.len(), 1);
    assert_eq!(chart.grid.len(), 1);
}

#[tokio::test]
async fn query_device_energy_today_returns_per_device_breakdown() {
    let pool = common::db().await;
    sqlx::query(
        "INSERT INTO Devices (type, name, ip, poll_interval_secs) VALUES ('mystrom_switch', 'SW', '10.0.0.5', 2)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let device_id: i64 = sqlx::query("SELECT id FROM Devices WHERE ip = '10.0.0.5'")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get(0);

    sqlx::query(
        "INSERT INTO Energy (device_id, timestamp, resolution, metric, energy_ws)
         VALUES (?, datetime('now'), '2s', 'power', 200.0)",
    )
    .bind(device_id)
    .execute(&pool)
    .await
    .unwrap();

    let map = db::query_device_energy_today(&pool).await.unwrap();
    let ip: IpAddr = "10.0.0.5".parse().unwrap();
    assert!(map.contains_key(&ip));
    assert!(map[&ip].kwh > 0.0);
}

#[tokio::test]
async fn query_recent_sonnen_avgs_filters_by_window_and_type_and_averages() {
    let pool = common::db().await;
    sqlx::query(
        "INSERT INTO Devices (type, name, ip, poll_interval_secs) VALUES ('sonnen_eco8', 'Battery', '10.0.0.6', 2)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let sonnen_id: i64 = sqlx::query("SELECT id FROM Devices WHERE ip = '10.0.0.6'")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get(0);

    // Two in-window samples per metric, from the device type this query
    // targets — averaged to 5000/1000/-500 respectively.
    for (metric, value, secs_ago) in [
        ("production", 4000.0, 30),
        ("production", 6000.0, 20),
        ("consumption", 800.0, 30),
        ("consumption", 1200.0, 20),
        ("pac", -400.0, 30),
        ("pac", -600.0, 20),
    ] {
        sqlx::query(&format!(
            "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
             VALUES (?, datetime('now', '-{secs_ago} seconds'), '{metric}', {value})"
        ))
        .bind(sonnen_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    // Out-of-window sample — must be excluded from the average.
    sqlx::query(
        "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
         VALUES (?, datetime('now', '-90 seconds'), 'production', 99999.0)",
    )
    .bind(sonnen_id)
    .execute(&pool)
    .await
    .unwrap();

    // A KEBA device's own rows on the same metric names (hypothetically) —
    // must be excluded by device type.
    sqlx::query(
        "INSERT INTO Devices (type, name, ip, poll_interval_secs) VALUES ('keba', 'Wallbox', '10.0.0.7', 5)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let keba_id: i64 = sqlx::query("SELECT id FROM Devices WHERE ip = '10.0.0.7'")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get(0);
    sqlx::query(
        "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
         VALUES (?, datetime('now', '-5 seconds'), 'production', 7000.0)",
    )
    .bind(keba_id)
    .execute(&pool)
    .await
    .unwrap();

    let avgs = db::query_recent_sonnen_avgs(&pool, 60)
        .await
        .unwrap()
        .expect("all three metrics have in-window samples");
    assert_eq!(avgs.production_w, 5000.0);
    assert_eq!(avgs.consumption_w, 1000.0);
    assert_eq!(avgs.pac_w, -500.0);
}

#[tokio::test]
async fn query_recent_sonnen_avgs_returns_none_when_a_metric_is_missing() {
    let pool = common::db().await;
    sqlx::query(
        "INSERT INTO Devices (type, name, ip, poll_interval_secs) VALUES ('sonnen_eco8', 'Battery', '10.0.0.8', 2)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let sonnen_id: i64 = sqlx::query("SELECT id FROM Devices WHERE ip = '10.0.0.8'")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get(0);

    // Only 'production' has a sample — 'consumption' and 'pac' don't.
    sqlx::query(
        "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
         VALUES (?, datetime('now', '-5 seconds'), 'production', 5000.0)",
    )
    .bind(sonnen_id)
    .execute(&pool)
    .await
    .unwrap();

    let avgs = db::query_recent_sonnen_avgs(&pool, 60).await.unwrap();
    assert!(avgs.is_none());
}

// ── Device merge on IP migration ──────────────────────────────────────────────

/// Two rows for one physical device: the known one at its old address, and the
/// one auto-discovered at the new address before the fingerprint match caught up.
async fn two_rows_for_one_device(pool: &sqlx::SqlitePool) -> (i64, i64) {
    let old: std::net::IpAddr = "172.16.1.10".parse().unwrap();
    let new: std::net::IpAddr = "172.16.1.11".parse().unwrap();
    dom::db::upsert_device(
        pool,
        "sonnen_eco8",
        "battery",
        old,
        2,
        Some("aa:bb:cc:dd:ee:ff"),
    )
    .await
    .unwrap();
    dom::db::upsert_device(pool, "sonnen_eco8", "battery", new, 2, None)
        .await
        .unwrap();
    let id = |ip: std::net::IpAddr| async move {
        sqlx::query_scalar::<_, i64>("SELECT id FROM Devices WHERE ip = ?")
            .bind(ip.to_string())
            .fetch_one(pool)
            .await
            .unwrap()
    };
    (id(old).await, id(new).await)
}

/// Completes the migration by re-upserting the new address *with* the fingerprint.
async fn migrate(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let new: std::net::IpAddr = "172.16.1.11".parse().unwrap();
    dom::db::upsert_device(
        pool,
        "sonnen_eco8",
        "battery",
        new,
        2,
        Some("aa:bb:cc:dd:ee:ff"),
    )
    .await
}

#[tokio::test]
async fn migrating_a_device_that_has_rolled_up_history_succeeds() {
    // Regression: the merge moved only the four original tables. The four rollup
    // tiers were added later, and since `sqlx` enforces foreign keys, deleting
    // the loser row failed outright — so the migration never completed and was
    // retried on every discovery pass.
    let pool = common::db().await;
    let (winner, loser) = two_rows_for_one_device(&pool).await;

    sqlx::query("INSERT INTO EnergyDaily (device_id, day, metric, energy_wh) VALUES (?,'2026-08-01','production',1000.0)")
        .bind(loser).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO StorageDaily (device_id, day, rsoc_min, rsoc_max, rsoc_avg, samples) VALUES (?,'2026-08-01',10.0,90.0,50.0,100)")
        .bind(loser).execute(&pool).await.unwrap();

    migrate(&pool).await.expect("the migration must complete");

    let devices: Vec<(i64, String)> = sqlx::query_as("SELECT id, ip FROM Devices ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(devices, vec![(winner, "172.16.1.11".to_string())]);

    // The history came with it rather than being deleted alongside the row.
    let owner: i64 = sqlx::query_scalar("SELECT device_id FROM EnergyDaily")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(owner, winner);
}

#[tokio::test]
async fn energy_is_summed_when_both_rows_covered_the_same_day() {
    // The transition day: the old address stopped answering partway through and
    // the new one took over, so both have a partial total for it.
    let pool = common::db().await;
    let (winner, loser) = two_rows_for_one_device(&pool).await;
    for (id, wh) in [(winner, 3000.0), (loser, 4000.0)] {
        sqlx::query("INSERT INTO EnergyDaily (device_id, day, metric, energy_wh) VALUES (?,'2026-08-01','production',?)")
            .bind(id).bind(wh).execute(&pool).await.unwrap();
    }
    // And a day only the loser saw, which must survive untouched.
    sqlx::query("INSERT INTO EnergyDaily (device_id, day, metric, energy_wh) VALUES (?,'2026-08-02','production',5000.0)")
        .bind(loser).execute(&pool).await.unwrap();

    migrate(&pool).await.unwrap();

    let rows: Vec<(String, f64)> =
        sqlx::query_as("SELECT day, energy_wh FROM EnergyDaily ORDER BY day")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![
            ("2026-08-01".to_string(), 7000.0),
            ("2026-08-02".to_string(), 5000.0),
        ]
    );
}

#[tokio::test]
async fn a_minute_that_both_rows_recorded_keeps_its_peak_and_its_parts() {
    let pool = common::db().await;
    let (winner, loser) = two_rows_for_one_device(&pool).await;
    let insert = |id: i64, ws: f64, pos: f64, neg: f64, span: i64, peak: f64| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO EnergyMinute
                     (device_id, minute, metric, energy_ws, energy_ws_pos, energy_ws_neg, span_secs, peak_w)
                 VALUES (?, '2026-08-01 12:00:00', 'grid', ?, ?, ?, ?, ?)")
                .bind(id).bind(ws).bind(pos).bind(neg).bind(span).bind(peak)
                .execute(&pool).await.unwrap();
        }
    };
    insert(winner, 100.0, 300.0, 200.0, 20, 900.0).await;
    insert(loser, 50.0, 80.0, 30.0, 40, 1500.0).await;

    migrate(&pool).await.unwrap();

    let (ws, pos, neg, span, peak): (f64, f64, f64, i64, f64) = sqlx::query_as(
        "SELECT energy_ws, energy_ws_pos, energy_ws_neg, span_secs, peak_w FROM EnergyMinute",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((ws, pos, neg, span), (150.0, 380.0, 230.0, 60));
    // A peak is the largest sample either side saw; summing it would be nonsense.
    assert_eq!(peak, 1500.0);
}

#[tokio::test]
async fn state_summaries_take_extremes_and_a_weighted_mean() {
    let pool = common::db().await;
    let (winner, loser) = two_rows_for_one_device(&pool).await;
    sqlx::query("INSERT INTO StorageDaily (device_id, day, rsoc_min, rsoc_max, rsoc_avg, samples) VALUES (?,'2026-08-01',20.0,80.0,40.0,300)")
        .bind(winner).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO StorageDaily (device_id, day, rsoc_min, rsoc_max, rsoc_avg, samples) VALUES (?,'2026-08-01',10.0,60.0,50.0,100)")
        .bind(loser).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO TemperatureDaily (device_id, day, temp_min, temp_max, temp_sum, samples, last_ts) VALUES (?,'2026-08-01',5.0,25.0,300.0,20,'2026-08-01 18:00:00')")
        .bind(winner).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO TemperatureDaily (device_id, day, temp_min, temp_max, temp_sum, samples, last_ts) VALUES (?,'2026-08-01',2.0,30.0,150.0,10,'2026-08-01 22:00:00')")
        .bind(loser).execute(&pool).await.unwrap();

    migrate(&pool).await.unwrap();

    let (lo, hi, avg, n): (f64, f64, f64, i64) =
        sqlx::query_as("SELECT rsoc_min, rsoc_max, rsoc_avg, samples FROM StorageDaily")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((lo, hi, n), (10.0, 80.0, 400));
    // Weighted by sample count, not a mean of means: (40*300 + 50*100) / 400.
    assert!((avg - 42.5).abs() < 1e-9, "got {avg}");

    let (tlo, thi, sum, tn, last): (f64, f64, f64, i64, String) = sqlx::query_as(
        "SELECT temp_min, temp_max, temp_sum, samples, last_ts FROM TemperatureDaily",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((tlo, thi, sum, tn), (2.0, 30.0, 450.0, 30));
    assert_eq!(last, "2026-08-01 22:00:00", "the later observation wins");
}

#[tokio::test]
async fn every_table_that_references_a_device_is_moved() {
    // The guard against this list going stale a second time. One row is written
    // into every table the schema says references Devices(id); after the merge
    // none may remain on the losing id, whatever that set happens to be.
    //
    // A table added to the schema and not added here fails the coverage
    // assertion below rather than passing quietly, which is the point.
    let pool = common::db().await;
    let (_, loser) = two_rows_for_one_device(&pool).await;

    let seed: &[(&str, &str)] = &[
        ("RawDeviceMeasurements",
         "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
          VALUES (?, '2026-08-01 12:00:00', 'production', 1.0)"),
        ("Energy",
         "INSERT INTO Energy (device_id, timestamp, resolution, metric, energy_ws)
          VALUES (?, '2026-08-01 12:00:00', '2s', 'production', 1.0)"),
        ("EnergyStorage",
         "INSERT INTO EnergyStorage (device_id, timestamp, resolution, rsoc_avg)
          VALUES (?, '2026-08-01 12:00:00', '2s', 50.0)"),
        ("SwitchTimers",
         "INSERT INTO SwitchTimers (device_id, time_hhmm, relay_on) VALUES (?, '07:00', 1)"),
        ("EnergyDaily",
         "INSERT INTO EnergyDaily (device_id, day, metric, energy_wh)
          VALUES (?, '2026-08-01', 'production', 1.0)"),
        ("EnergyMinute",
         "INSERT INTO EnergyMinute
              (device_id, minute, metric, energy_ws, energy_ws_pos, energy_ws_neg, span_secs, peak_w)
          VALUES (?, '2026-08-01 12:00:00', 'production', 1.0, 1.0, 0.0, 60, 1.0)"),
        ("StorageDaily",
         "INSERT INTO StorageDaily (device_id, day, rsoc_min, rsoc_max, rsoc_avg, samples)
          VALUES (?, '2026-08-01', 1.0, 2.0, 1.5, 10)"),
        ("TemperatureDaily",
         "INSERT INTO TemperatureDaily
              (device_id, day, temp_min, temp_max, temp_sum, samples, last_ts)
          VALUES (?, '2026-08-01', 1.0, 2.0, 15.0, 10, '2026-08-01 12:00:00')"),
    ];

    let referencing: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT m.name FROM sqlite_master m
         JOIN pragma_foreign_key_list(m.name) f
         WHERE m.type = 'table' AND f.\"table\" = 'Devices'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();

    for table in &referencing {
        assert!(
            seed.iter().any(|(t, _)| t == table),
            "{table} references Devices(id) but this test does not seed it — \
             add it here and to db::merge_device_history"
        );
    }

    for (_, sql) in seed {
        sqlx::query(sql).bind(loser).execute(&pool).await.unwrap();
    }

    migrate(&pool).await.expect("the migration must complete");

    for table in &referencing {
        let n: i64 =
            sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE device_id = ?"))
                .bind(loser)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 0, "{table} still holds rows for the merged-away device");
    }
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM Devices WHERE id = ?")
        .bind(loser)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0, "the losing device row must be gone");
}

// ── File permissions ──────────────────────────────────────────────────────────

#[tokio::test]
async fn the_database_is_readable_only_by_its_owner() {
    // `Devices` holds router, battery and wallbox credentials in plain text; the
    // ambient umask would leave them world-readable.
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");

    let pool = dom::db::init(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    // Write something, so the WAL sidecar exists to be checked as well.
    dom::db::set_config(&pool, "theme", "dark").await.unwrap();

    for suffix in ["", "-wal"] {
        let f = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
        if !f.exists() {
            continue;
        }
        let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{} is mode {mode:o}", f.display());
    }
}

#[tokio::test]
async fn an_existing_world_readable_database_is_corrected_on_startup() {
    // Every database written before this existed is mode 644 today, so opening
    // one has to fix it rather than only getting new ones right.
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");

    let uri = format!("sqlite://{}", path.display());
    drop(dom::db::init(&uri).await.unwrap());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );

    drop(dom::db::init(&uri).await.unwrap());

    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600,
        "an existing database must be tightened, not left as found"
    );
}

#[tokio::test]
async fn an_in_memory_database_needs_no_file_permissions() {
    // The path branch must not try to chmod ":memory:" and log a warning on
    // every single test that uses it.
    let pool = dom::db::init("sqlite://:memory:").await.unwrap();
    dom::db::set_config(&pool, "theme", "light").await.unwrap();
    assert!(!std::path::Path::new(":memory:").exists());
}

#[tokio::test]
async fn every_pooled_connection_gets_the_relaxed_sync_setting() {
    // `synchronous` is a property of a connection, not of the database file, so
    // issuing it as a query would set it on whichever connection served that one
    // query and leave the rest of the pool flushing the disk on every commit.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    let pool = dom::db::init(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();

    // Enough concurrent queries to be served by more than one connection.
    let mut checks = Vec::new();
    for _ in 0..8 {
        checks.push(sqlx::query_scalar::<_, i64>("PRAGMA synchronous").fetch_one(&pool));
    }
    for got in futures::future::join_all(checks).await {
        assert_eq!(got.unwrap(), 1, "1 is NORMAL; 2 would be FULL");
    }

    // ...and the journal mode is still WAL, which is what makes NORMAL safe.
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(mode, "wal");
}

// ── Today's charts, read from two tiers ───────────────────────────────────────

/// Writes a 2s sample at a local wall-clock time today.
async fn sample_today(
    pool: &sqlx::SqlitePool,
    device_id: i64,
    hhmmss: &str,
    metric: &str,
    ws: f64,
) {
    use chrono::TimeZone;
    let today = chrono::Local::now().date_naive();
    let naive =
        chrono::NaiveDateTime::parse_from_str(&format!("{today} {hhmmss}"), "%Y-%m-%d %H:%M:%S")
            .unwrap();
    let utc = chrono::Local
        .from_local_datetime(&naive)
        .earliest()
        .unwrap()
        .naive_utc();
    sqlx::query(
        "INSERT INTO Energy (device_id, timestamp, resolution, metric, energy_ws)
         VALUES (?, ?, '2s', ?, ?)",
    )
    .bind(device_id)
    .bind(utc.format("%Y-%m-%d %H:%M:%S").to_string())
    .bind(metric)
    .bind(ws)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn the_chart_reads_the_same_numbers_before_and_after_the_minute_tier_advances() {
    // The today-charts now take completed minutes from `EnergyMinute` and only
    // the unrolled tail from the raw rows — a full table scan became an index
    // range scan. The property that has to hold is that moving the boundary
    // changes nothing: the same minute must never be counted twice, nor dropped.
    let pool = common::db().await;
    dom::db::upsert_device(
        &pool,
        "sonnen_eco8",
        "battery",
        "10.0.0.1".parse().unwrap(),
        2,
        None,
    )
    .await
    .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM Devices WHERE ip = '10.0.0.1'")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Two minutes of samples, and a grid series that both imports and exports
    // inside one minute — the case a naive re-split would get wrong.
    for (at, metric, ws) in [
        ("01:00:00", "consumption", 2000.0),
        ("01:00:02", "consumption", 2200.0),
        ("01:01:00", "consumption", 1800.0),
        ("01:00:00", "production", 500.0),
        ("01:01:00", "production", 700.0),
        ("01:00:00", "pac", -300.0),
        ("01:01:00", "pac", 400.0),
        ("01:00:00", "grid", -900.0),
        ("01:00:02", "grid", 400.0),
        ("01:01:00", "grid", -100.0),
    ] {
        sample_today(&pool, id, at, metric, ws).await;
    }

    // Nothing rolled up yet: everything comes from the raw tail.
    let before = dom::db::query_today_energy(&pool).await.unwrap();
    assert!(!before.consumption.is_empty(), "should have chart points");

    // Advance the minute tier over the whole day.
    dom::db::rollup_history(&pool, std::time::Duration::ZERO)
        .await
        .unwrap();

    let after = dom::db::query_today_energy(&pool).await.unwrap();

    assert_eq!(before.consumption, after.consumption, "consumption");
    assert_eq!(before.production, after.production, "production");
    assert_eq!(before.battery, after.battery, "battery");
    assert_eq!(before.grid, after.grid, "grid");
    assert!(
        (before.grid_imported_kwh - after.grid_imported_kwh).abs() < 1e-9,
        "imported {} vs {}",
        before.grid_imported_kwh,
        after.grid_imported_kwh
    );
    assert!(
        (before.grid_exported_kwh - after.grid_exported_kwh).abs() < 1e-9,
        "exported {} vs {}",
        before.grid_exported_kwh,
        after.grid_exported_kwh
    );
    // ...and the import/export split really did survive a minute that did both.
    assert!(before.grid_imported_kwh > 0.0 && before.grid_exported_kwh > 0.0);
}

#[tokio::test]
async fn per_device_totals_survive_the_minute_tier_advancing_too() {
    // Same boundary, same hazard: a battery minute that both charged and
    // discharged must keep both halves rather than netting out.
    let pool = common::db().await;
    dom::db::upsert_device(
        &pool,
        "sonnen_eco8",
        "battery",
        "10.0.0.1".parse().unwrap(),
        2,
        None,
    )
    .await
    .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM Devices WHERE ip = '10.0.0.1'")
        .fetch_one(&pool)
        .await
        .unwrap();

    for (at, ws) in [
        ("01:00:00", -800.0),
        ("01:00:02", 500.0),
        ("01:01:00", -200.0),
    ] {
        sample_today(&pool, id, at, "pac", ws).await;
    }

    let before = dom::db::query_device_energy_today(&pool).await.unwrap();
    dom::db::rollup_history(&pool, std::time::Duration::ZERO)
        .await
        .unwrap();
    let after = dom::db::query_device_energy_today(&pool).await.unwrap();

    let ip: std::net::IpAddr = "10.0.0.1".parse().unwrap();
    let (b, a) = (&before[&ip], &after[&ip]);
    assert!(
        (b.charged_kwh - a.charged_kwh).abs() < 1e-9,
        "{} vs {}",
        b.charged_kwh,
        a.charged_kwh
    );
    assert!(
        (b.discharged_kwh - a.discharged_kwh).abs() < 1e-9,
        "{} vs {}",
        b.discharged_kwh,
        a.discharged_kwh
    );
    assert!((b.kwh - a.kwh).abs() < 1e-9, "{} vs {}", b.kwh, a.kwh);
    assert!(
        b.charged_kwh > 0.0 && b.discharged_kwh > 0.0,
        "the minute that did both must keep both halves"
    );
}

#[tokio::test]
async fn the_newest_recorded_time_is_the_clocks_lower_bound() {
    // A machine with no battery-backed clock boots at whatever was last written
    // to disk. The database is the only evidence that time has passed: a row
    // stamped at an instant proves the clock once read that instant.
    let pool = common::db().await;
    assert_eq!(
        dom::db::newest_recorded_time(&pool).await.unwrap(),
        None,
        "nothing recorded means nothing to be inconsistent with"
    );

    dom::db::upsert_device(
        &pool,
        "sonnen_eco8",
        "battery",
        "10.0.0.1".parse().unwrap(),
        2,
        None,
    )
    .await
    .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM Devices WHERE ip = '10.0.0.1'")
        .fetch_one(&pool)
        .await
        .unwrap();

    // The newest across every series, not just the first one looked at.
    sqlx::query(
        "INSERT INTO Energy (device_id, timestamp, resolution, metric, energy_ws)
         VALUES (?, '2026-08-01 10:00:00', '2s', 'production', 1.0)",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
         VALUES (?, '2026-08-03 07:30:00', 'production', 1.0)",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();

    let newest = dom::db::newest_recorded_time(&pool).await.unwrap().unwrap();
    assert_eq!(
        newest.format("%Y-%m-%d %H:%M:%S").to_string(),
        "2026-08-03 07:30:00"
    );
}

#[tokio::test]
async fn freed_pages_are_handed_back_rather_than_hoarded() {
    // `auto_vacuum=INCREMENTAL` only makes pages *available*; nothing shrinks the
    // file until something calls `incremental_vacuum`. That is how 265 MB of live
    // data came to occupy 1.29 GB.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    let pool = dom::db::init(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    dom::db::upsert_device(
        &pool,
        "sonnen_eco8",
        "battery",
        "10.0.0.1".parse().unwrap(),
        2,
        None,
    )
    .await
    .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM Devices WHERE ip = '10.0.0.1'")
        .fetch_one(&pool)
        .await
        .unwrap();

    for i in 0..4000 {
        sqlx::query(
            "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
             VALUES (?, '2020-01-01 00:00:00', 'production', ?)",
        )
        .bind(id)
        .bind(f64::from(i))
        .execute(&pool)
        .await
        .unwrap();
    }
    let grown = std::fs::metadata(&path).unwrap().len();

    // Old enough to prune, which frees the pages without shrinking the file.
    dom::db::prune_raw_measurements(&pool).await.unwrap();
    let freelist: i64 = sqlx::query_scalar("PRAGMA freelist_count")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(freelist > 0, "pruning should have freed pages");

    let left = dom::db::reclaim_free_pages(&pool).await.unwrap();
    assert!(left < freelist, "{freelist} pages before, {left} after");

    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&pool)
        .await
        .unwrap();
    let reclaimed = std::fs::metadata(&path).unwrap().len();
    assert!(reclaimed < grown, "{grown} bytes before, {reclaimed} after");
}
