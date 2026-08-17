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
