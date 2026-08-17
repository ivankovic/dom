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

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sqlx::SqlitePool;
use surge_ping::{Client, Config};
use tokio::sync::Notify;

use dom::app::{SharedState, SwitchAutoMode};
use dom::{app, db, devices, fingerprint, stats, tui};

/// How long after startup the ICMP ping scan's one-time early follow-up runs,
/// to catch devices that weren't up yet at the very first (t=0) scan.
const PING_SCAN_FOLLOWUP_SECS: u64 = 5 * 60;
/// Steady-state interval for the ICMP ping scan after the startup + follow-up
/// runs. Kept long because a full-subnet sweep triggers an ARP-broadcast burst
/// for every non-cached address — see `scan_all_networks`. Independent of
/// `DISCOVERY_INTERVAL_SECS` — a broken or slow ping scan (e.g. missing
/// CAP_NET_RAW) must never delay or block DHCP-lease-based device discovery,
/// and vice versa.
const PING_SCAN_STEADY_INTERVAL_SECS: u64 = 4 * 60 * 60;
/// How often devices are fingerprinted and the device list refreshed, from
/// the union of the most recent ping-scan results and the router's DHCP
/// leases (see `discovery_task`).
const DISCOVERY_INTERVAL_SECS: u64 = 5 * 60;
/// Cap on the in-memory (and bootstrap-loaded) network status event list
/// shown in the Network view.
const NETWORK_STATUS_HISTORY_LEN: i64 = 200;
/// How often the daily energy rollup is brought up to date. The current day's
/// totals only move as the day accrues, so this need not be frequent; the
/// statistics view also reloads immediately whenever the user changes period.
const ROLLUP_INTERVAL_SECS: u64 = 15 * 60;
/// Pause between days while rolling up. The first run has the whole history to
/// work through against a database that the poll loops are reading at the same
/// time; spreading the work out keeps it from starving them of I/O.
const ROLLUP_THROTTLE: Duration = Duration::from_millis(200);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let pool = db::init("sqlite://db.sqlite").await?;
    let state = app::new_shared();

    // Colour theme: an explicit choice the user made with 't' in a previous run
    // wins outright. Failing that, guess from the terminal, which frequently
    // tells us nothing (tmux, most modern terminals) and then falls back to the
    // dark palette — see tui::theme::detect_mode.
    let stored_theme = db::get_config(&pool, tui::theme::CONFIG_KEY)
        .await
        .ok()
        .flatten();
    state.write().unwrap().theme_mode = tui::theme::resolve_mode_from_env(stored_theme.as_deref());

    // Pre-populate device list and start poll loops for devices known from a prior run.
    bootstrap_known_devices(&state, &pool).await;

    // Woken by the TUI's manual "rescan now" key to run an out-of-schedule
    // ping scan + discovery pass immediately.
    let rescan_notify = Arc::new(Notify::new());

    // Background: ICMP ping scan and DHCP-lease-based device discovery run as
    // independent tasks on independent schedules — see their doc comments.
    tokio::spawn(ping_scan_task(state.clone(), rescan_notify.clone()));
    tokio::spawn(discovery_task(
        state.clone(),
        pool.clone(),
        rescan_notify.clone(),
    ));

    // Background: per-device infrastructure health checks at custom intervals.
    // Router and 5G modem: 3 packets every 10 seconds.
    // Access points: 3 packets every 30 seconds.
    // Other infrastructure devices: default intervals.
    tokio::spawn(ping_infrastructure_task(state.clone()));

    // Background: prune RawDeviceMeasurements and old network status events, hourly.
    {
        let pool = pool.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(3600));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let _ = db::prune_raw_measurements(&pool).await;
                let _ = db::prune_network_status_events(&pool).await;
            }
        });
    }

    // Background: refresh energy chart and Internet-traffic chart data —
    // fires immediately, then every 60s.
    {
        let pool = pool.clone();
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                if let Ok(data) = db::query_today_energy(&pool).await {
                    state.write().unwrap().energy_chart = data;
                }
                if let Ok(map) = db::query_device_energy_today(&pool).await {
                    state.write().unwrap().device_energy_today = map;
                }
                if let Ok(data) = db::query_internet_traffic_today(&pool).await {
                    state.write().unwrap().internet_traffic_chart = data;
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
    }

    // Background: keeps the daily energy rollup and the statistics view current.
    tokio::spawn(statistics_task(state.clone(), pool.clone()));

    // Background: fires scheduled switch timers every 30s.
    tokio::spawn(timer_job(state.clone()));

    tui::run(state, pool, rescan_notify).await
}

// ── Long-term statistics ──────────────────────────────────────────────────────

/// Keeps `EnergyDaily` up to date and reloads the statistics view from it.
///
/// The statistics view reads only the daily rollup, never the 2s `Energy` rows —
/// at 2s resolution even a single month is millions of rows. This task is what
/// makes that rollup exist.
///
/// Refreshes the view before rolling up as well as after, so an existing rollup
/// is on screen immediately rather than after the first pass completes: the very
/// first run has the entire recorded history to aggregate.
async fn statistics_task(state: SharedState, pool: SqlitePool) {
    stats::refresh(&pool, &state).await;
    loop {
        match db::rollup_history(&pool, ROLLUP_THROTTLE).await {
            Ok(0) => {}
            Ok(n) => log::info!("rolled up {n} device-days of energy"),
            Err(e) => log::warn!("energy rollup failed: {e:#}"),
        }
        stats::refresh(&pool, &state).await;
        tokio::time::sleep(Duration::from_secs(ROLLUP_INTERVAL_SECS)).await;
    }
}

// ── Ping scan ─────────────────────────────────────────────────────────────────

/// Runs the ICMP ping scan: immediately at startup, once more after
/// `PING_SCAN_FOLLOWUP_SECS` (to catch devices that weren't up yet at t=0),
/// then every `PING_SCAN_STEADY_INTERVAL_SECS` — or immediately whenever
/// `rescan` is notified (the TUI's manual "rescan now" key). Independent of
/// device discovery (`discovery_task`). Updates `ping_history` (used for the
/// Network view's OK/SLOW/DEGRADED/LOST status) and, on success, the latest
/// ping-reachable device list that `discovery_task` merges with DHCP leases.
/// A failure here (e.g. missing CAP_NET_RAW / ping_group_range on this host)
/// only records `last_scan_error` for visibility — it must never block or
/// delay DHCP-lease-based discovery, which needs only plain TCP, not ICMP.
async fn ping_scan_task(state: SharedState, rescan: Arc<Notify>) {
    let mut next_delay = Duration::ZERO;
    let mut did_followup = false;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(next_delay) => {}
            _ = rescan.notified() => {}
        }
        next_delay = if !did_followup {
            did_followup = true;
            Duration::from_secs(PING_SCAN_FOLLOWUP_SECS)
        } else {
            Duration::from_secs(PING_SCAN_STEADY_INTERVAL_SECS)
        };

        match devices::scan_all_networks().await {
            Ok(scan_result) => {
                let mut app = state.write().unwrap();
                app.last_scan_error = None;
                for device in &scan_result.successful {
                    let history = app.ping_history.entry(device.ip).or_default();
                    history.add(device.latency_ms, true);
                }
                for ip in &scan_result.failed {
                    let history = app.ping_history.entry(*ip).or_default();
                    history.add(0.0, false);
                }
                app.last_ping_devices = scan_result.successful;
            }
            Err(e) => {
                state.write().unwrap().last_scan_error = Some(e.to_string());
            }
        }
    }
}

// ── Per-device infrastructure ping ─────────────────────────────────────────────

/// Per-device ping health checks at custom intervals based on device role.
///
/// - Router and 5G Modem: pinged every 10 seconds with 3 packets
/// - Access Points: pinged every 30 seconds with 3 packets
/// - Other devices: uses default configuration
///
/// This task runs on a 10-second interval (the GCD of 10 and 30) and checks
/// which devices are due for pinging based on their individual intervals.
async fn ping_infrastructure_task(state: SharedState) {
    // Run on 10-second intervals (GCD of 10 and 30)
    let mut ticker = tokio::time::interval(Duration::from_secs(10));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);

    // Track the last ping time for each device
    let mut last_pinged: HashMap<IpAddr, chrono::DateTime<chrono::Utc>> = HashMap::new();

    loop {
        ticker.tick().await;

        let now = chrono::Utc::now();
        let devices: Vec<app::ScannedDevice>;

        {
            let app = state.read().unwrap();
            devices = app.devices.clone();
        }

        // Classify devices using the same logic as classify_network_devices
        let topology = app::classify_network_devices(&devices);
        let routers = topology.routers;
        let modems = topology.modems;
        let access_points = topology.access_points;

        // Build list of devices to ping with their configs
        let mut devices_to_ping: Vec<(IpAddr, u8)> = Vec::new(); // (ip, packet_count)

        // Router and 5G modem: every 10 seconds, 3 packets
        for ip in routers.iter().chain(modems.iter()) {
            if should_ping(*ip, 10, &last_pinged, now) {
                devices_to_ping.push((*ip, 3));
            }
        }

        // Access points: every 30 seconds, 3 packets
        for ip in &access_points {
            if should_ping(*ip, 30, &last_pinged, now) {
                devices_to_ping.push((*ip, 3));
            }
        }

        // Ping all due devices
        if !devices_to_ping.is_empty() {
            let client = match Client::new(&Config::default()) {
                Ok(c) => Arc::new(c),
                Err(_) => {
                    // Failed to create ping client, skip this cycle
                    continue;
                }
            };

            let mut set = tokio::task::JoinSet::new();
            for (ip, count) in devices_to_ping {
                let c = Arc::clone(&client);
                set.spawn(async move { (ip, devices::ping_device_multi(c, ip, count).await) });
            }

            // Collect results
            while let Some(result) = set.join_next().await {
                match result {
                    Ok((ip, Some((avg_latency, successes, total)))) => {
                        // All pings succeeded or partial success
                        let success = successes == total;
                        let mut app = state.write().unwrap();
                        let history = app.ping_history.entry(ip).or_default();
                        history.add(avg_latency, success);
                        last_pinged.insert(ip, now);
                    }
                    Ok((ip, None)) => {
                        // All pings failed
                        let mut app = state.write().unwrap();
                        let history = app.ping_history.entry(ip).or_default();
                        history.add(0.0, false);
                        last_pinged.insert(ip, now);
                    }
                    Err(_) => {
                        // Task failed
                    }
                }
            }
        }
    }
}

/// Check if a device should be pinged based on its interval and last ping time.
fn should_ping(
    ip: IpAddr,
    interval_secs: u64,
    last_pinged: &HashMap<IpAddr, chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    match last_pinged.get(&ip) {
        Some(last) => {
            let elapsed = now.signed_duration_since(*last).num_seconds();
            elapsed >= interval_secs as i64
        }
        None => true, // Never pinged before
    }
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Fingerprints and registers devices from the union of the most recent
/// ping-scan results (`app.last_ping_devices`, written by `ping_scan_task`)
/// and the router's DHCP leases, then refreshes the device list. Runs on its
/// own schedule (unaffected by `ping_scan_task`'s much longer steady-state
/// interval) so a slow or failing ping scan never delays discovery of
/// devices the router already knows about (e.g. behind AP client isolation,
/// or while this host lacks ICMP permissions). Also wakes immediately when
/// `rescan` is notified, so a manual rescan's results (including any new
/// DHCP leases) are merged right away rather than waiting up to
/// `DISCOVERY_INTERVAL_SECS`.
async fn discovery_task(state: SharedState, pool: SqlitePool, rescan: Arc<Notify>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(DISCOVERY_INTERVAL_SECS));
    // Burst: first tick fires immediately; if a scan takes longer than the
    // interval the scheduler catches up with one extra run, not many.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = rescan.notified() => {}
        }

        let next_at = Utc::now()
            + chrono::Duration::try_seconds(DISCOVERY_INTERVAL_SECS as i64).unwrap_or_default();

        state.write().unwrap().scan = app::ScanPhase::Scanning;

        let mut devs: Vec<devices::Device> = {
            let app = state.read().unwrap();
            app.last_ping_devices.clone()
        };
        devs.sort_by_key(|d| ip_sort_key(d.ip));

        // Some devices (e.g. behind AP client isolation) never answer ICMP, but the
        // router already knows about them via DHCP — fingerprint those too so they
        // still show up as discovered devices.
        let lease_ips: Vec<IpAddr> = {
            let app = state.read().unwrap();
            app.mikrotik_readings
                .values()
                .flat_map(|r| r.leases.iter())
                .filter_map(|lease| lease.address.parse().ok())
                .collect()
        };
        let fingerprint_targets = merge_scan_targets(&devs, lease_ips);

        let mut set = tokio::task::JoinSet::new();
        for ip in fingerprint_targets {
            set.spawn(async move { fingerprint::fingerprint(ip).await });
        }
        let mut fps: Vec<fingerprint::Fingerprint> = Vec::new();
        while let Some(Ok(fp)) = set.join_next().await {
            fps.push(fp);
        }
        fps.sort_by_key(|fp| ip_sort_key(fp.ip));

        // Stable per-device identity (MAC address), independent of IP, so a
        // device with a new DHCP lease is recognized as the same device
        // rather than registered a second time — see db::upsert_device.
        let mac_by_ip = devices::arp_cache();
        let fingerprint_for = |ip: IpAddr| -> Option<String> {
            match ip {
                IpAddr::V4(v4) => mac_by_ip.get(&v4).cloned(),
                IpAddr::V6(_) => None,
            }
        };

        // Persist newly discovered devices and start poll loops. Each device is
        // classified once, by `devices::detect_type`, so the type stored here is
        // necessarily the same one shown in the UI below.
        for fp in &fps {
            let Some(device_type) = devices::detect_type(fp) else {
                continue;
            };
            let fingerprint = fingerprint_for(fp.ip);
            let name = device_type.display_name();
            match device_type {
                devices::DeviceType::SonnenBatterie => {
                    let _ = devices::sonnen_batterie::save_device(
                        &pool,
                        fp.ip,
                        name,
                        fingerprint.as_deref(),
                    )
                    .await;
                    maybe_spawn_poll_loop(fp.ip, &pool, &state).await;
                }
                devices::DeviceType::MystromSwitch => {
                    let _ = devices::mystrom_switch::save_device(
                        &pool,
                        fp.ip,
                        name,
                        fingerprint.as_deref(),
                    )
                    .await;
                    maybe_spawn_switch_poll_loop(fp.ip, &pool, &state).await;
                }
                devices::DeviceType::Mikrotik => {
                    let _ =
                        devices::mikrotik::save_device(&pool, fp.ip, name, fingerprint.as_deref())
                            .await;
                    maybe_spawn_mikrotik_poll_loop(fp.ip, &pool, &state).await;
                }
                devices::DeviceType::Keba => {
                    let _ = devices::keba::save_device(&pool, fp.ip, name, fingerprint.as_deref())
                        .await;
                    maybe_spawn_keba_poll_loop(fp.ip, &pool, &state).await;
                }
            }
        }

        // Automatically configure and add our own IP addresses as "Dom"
        let local_ips = devices::dom_local::detect_local_ips();
        for local_ip in local_ips {
            let _ = devices::dom_local::save_device(&pool, local_ip).await;
        }

        // Update the device list: scan results ∪ known polled devices (never drop them)
        // ∪ non-polled devices with custom labels (preserve user assignments).
        // Capture all existing devices and DHCP lease comments before acquiring the write lock.
        let all_existing_devices: Vec<app::ScannedDevice> = {
            let app = state.read().unwrap();
            app.devices.clone()
        };
        // Build a map from IP to DHCP lease comment/host-name for label fallback
        let dhcp_info: std::collections::HashMap<std::net::IpAddr, String> = {
            let app = state.read().unwrap();
            app.mikrotik_readings
                .values()
                .flat_map(|r| r.leases.iter())
                .filter_map(|lease| {
                    // Use comment if available, otherwise fall back to host-name
                    let label = lease
                        .comment
                        .clone()
                        .or_else(|| lease.host_name.clone())
                        .filter(|s| !s.is_empty());
                    lease.address.parse().ok().zip(label)
                })
                .collect()
        };

        // Load all labels from DB as a fallback
        let db_labels: std::collections::HashMap<std::net::IpAddr, Option<String>> =
            db::load_all_device_labels(&pool).await.unwrap_or_default();

        {
            let mut app = state.write().unwrap();
            let mut new_devices: Vec<app::ScannedDevice> = fps
                .iter()
                .map(|fp| {
                    let latency_ms = latency_for(fp.ip, &devs);
                    let label = resolve_label(fp.ip, &all_existing_devices, &db_labels, &dhcp_info);
                    app::ScannedDevice {
                        ip: fp.ip,
                        latency_ms,
                        open_ports: fp.open_ports.clone(),
                        // Same classification used when persisting above, so the
                        // displayed model can't disagree with the stored type.
                        name: devices::detect_type(fp)
                            .map(|t| t.display_name())
                            .or_else(|| {
                                devices::dom_local::is_local_ip(fp.ip)
                                    .then_some(devices::dom_local::NAME)
                            }),
                        label,
                    }
                })
                .collect();

            // Add our local IP addresses as "Dom" devices
            let local_ips = devices::dom_local::detect_local_ips();
            for local_ip in local_ips {
                if !new_devices.iter().any(|d| d.ip == local_ip) {
                    let latency_ms = latency_for(local_ip, &devs);
                    let label =
                        resolve_label(local_ip, &all_existing_devices, &db_labels, &dhcp_info);
                    new_devices.push(app::ScannedDevice {
                        ip: local_ip,
                        latency_ms,
                        open_ports: vec![],
                        name: Some(devices::dom_local::NAME),
                        label,
                    });
                }
            }

            // Add previously-polled devices not found in this scan.
            // Also add non-polled devices with labels that weren't found in this scan.
            for existing in &all_existing_devices {
                if !new_devices.iter().any(|d| d.ip == existing.ip)
                    && (app.polled_ips.contains(&existing.ip) || existing.label.is_some())
                {
                    new_devices.push(existing.clone());
                }
            }

            new_devices.sort_by_key(|d| ip_sort_key(d.ip));
            let max = new_devices.len().saturating_sub(1);
            app.selected = app.selected.min(max);
            app.devices = new_devices;
            app.scan = app::ScanPhase::Done {
                at: Utc::now(),
                next_at,
            };
        }

        // Re-read the switch auto-modes and timers every cycle, not just at
        // startup. These are keyed by IP but stored against a device id, so a
        // device that changes address loses its in-memory entry when the stale
        // poll loop clears it (`App::forget_device`) — the rows survive the
        // migration, and this is what puts them back under the new address
        // rather than leaving them missing from the UI until a restart.
        if let Ok((modes, timers)) = db::load_switch_configs(&pool).await {
            let mut app = state.write().unwrap();
            app.switch_auto_modes = modes;
            app.switch_timers = timers;
        }

        record_network_status_transitions(&state, &pool).await;
    }
}

/// Best-known display label for `ip`, in precedence order: a label already
/// attached to the device in memory (which is where a user's own rename lands),
/// then one persisted in the DB, then the router's DHCP lease comment or
/// host-name. A user-assigned label therefore always wins over anything
/// auto-derived from the network.
fn resolve_label(
    ip: IpAddr,
    existing: &[app::ScannedDevice],
    db_labels: &HashMap<IpAddr, Option<String>>,
    dhcp_info: &HashMap<IpAddr, String>,
) -> Option<String> {
    existing
        .iter()
        .find(|d| d.ip == ip)
        .and_then(|d| d.label.clone())
        .or_else(|| db_labels.get(&ip).and_then(|l| l.clone()))
        .or_else(|| dhcp_info.get(&ip).cloned())
}

/// Latency measured for `ip` in the most recent ping scan, or 0.0 when this
/// scan has none — a device discovered only through a DHCP lease (e.g. one
/// behind AP client isolation that never answers ICMP) has no measurement.
fn latency_for(ip: IpAddr, devs: &[devices::Device]) -> f64 {
    devs.iter()
        .find(|d| d.ip == ip)
        .map(|d| d.latency_ms)
        .unwrap_or(0.0)
}

/// Compares each network-infrastructure device's current status against its
/// last-observed status this run, logs any real transitions (both in-memory,
/// for immediate display, and to the DB, for durability across restarts), and
/// updates the last-observed map. A device's first observation this run has
/// no prior entry, so it is recorded but never itself logged as a transition
/// — see `app::status_transition`.
async fn record_network_status_transitions(state: &SharedState, pool: &SqlitePool) {
    let events: Vec<app::NetworkStatusEvent> = {
        let mut app = state.write().unwrap();
        let topology = app::classify_network_devices(&app.devices);
        let mut events = Vec::new();
        for ip in topology.iter_all() {
            let status = app.network_status(ip);
            let old = app.last_network_status.get(&ip).cloned();
            if let Some(previous) = app::status_transition(old.as_ref(), &status) {
                let label = app
                    .devices
                    .iter()
                    .find(|d| d.ip == ip)
                    .and_then(|d| d.label.clone());
                events.push(app::NetworkStatusEvent {
                    ip,
                    label,
                    previous,
                    current: status.clone(),
                    at: Utc::now(),
                });
            }
            app.last_network_status.insert(ip, status);
        }

        if !events.is_empty() {
            for event in events.iter().rev() {
                app.network_status_events.insert(0, event.clone());
            }
            app.network_status_events
                .truncate(NETWORK_STATUS_HISTORY_LEN as usize);
        }
        events
    };

    for event in &events {
        let _ = db::record_network_status_event(
            pool,
            event.ip,
            event.label.as_deref(),
            event.previous.display(),
            event.current.display(),
        )
        .await;
    }
}

/// Loads DB-known devices at startup, adds them to the TUI list, and starts poll loops
/// so the app shows live data immediately without waiting for the first network scan.
async fn bootstrap_known_devices(state: &SharedState, pool: &SqlitePool) {
    let mut initial: Vec<app::ScannedDevice> = Vec::new();

    if let Ok(rows) = devices::sonnen_batterie::load_all(pool).await {
        for d in &rows {
            initial.push(app::ScannedDevice {
                ip: d.ip,
                latency_ms: 0.0,
                open_ports: vec![],
                name: Some(devices::sonnen_batterie::NAME),
                label: d.label.clone(),
            });
        }
        for d in rows {
            maybe_spawn_poll_loop(d.ip, pool, state).await;
        }
    }
    if let Ok(rows) = devices::mystrom_switch::load_all(pool).await {
        for d in &rows {
            initial.push(app::ScannedDevice {
                ip: d.ip,
                latency_ms: 0.0,
                open_ports: vec![],
                name: Some(devices::mystrom_switch::NAME),
                label: d.label.clone(),
            });
        }
        for d in rows {
            maybe_spawn_switch_poll_loop(d.ip, pool, state).await;
        }
    }
    if let Ok(rows) = devices::mikrotik::load_all(pool).await {
        for d in &rows {
            initial.push(app::ScannedDevice {
                ip: d.ip,
                latency_ms: 0.0,
                open_ports: vec![],
                name: Some(devices::mikrotik::NAME),
                label: d.label.clone(),
            });
        }
        for d in rows {
            maybe_spawn_mikrotik_poll_loop(d.ip, pool, state).await;
        }
    }
    if let Ok(rows) = devices::keba::load_all(pool).await {
        for d in &rows {
            initial.push(app::ScannedDevice {
                ip: d.ip,
                latency_ms: 0.0,
                open_ports: vec![],
                name: Some(devices::keba::NAME),
                label: d.label.clone(),
            });
        }
        for d in rows {
            maybe_spawn_keba_poll_loop(d.ip, pool, state).await;
        }
    }

    // Add local machine as "Dom" devices
    if let Ok(rows) = devices::dom_local::load_all(pool).await {
        for d in &rows {
            initial.push(app::ScannedDevice {
                ip: d.ip,
                latency_ms: 0.0,
                open_ports: vec![],
                name: Some(devices::dom_local::NAME),
                label: None,
            });
        }
    }

    // Also add any local IPs that haven't been saved to DB yet
    let local_ips = devices::dom_local::detect_local_ips();
    for local_ip in local_ips {
        // Check if this IP is already in the initial list
        if !initial.iter().any(|d| d.ip == local_ip) {
            initial.push(app::ScannedDevice {
                ip: local_ip,
                latency_ms: 0.0,
                open_ports: vec![],
                name: Some(devices::dom_local::NAME),
                label: None,
            });
            // Save it to the database
            let _ = devices::dom_local::save_device(pool, local_ip).await;
        }
    }

    // Load switch auto-modes and scheduled timers.
    if let Ok((modes, timers)) = db::load_switch_configs(pool).await {
        let mut app = state.write().unwrap();
        app.switch_auto_modes = modes;
        app.switch_timers = timers;
    }

    // Load recent network status history from prior runs. Note: this only
    // seeds the *displayed* list — `last_network_status` (used to detect new
    // transitions) deliberately starts empty so a restart never re-logs the
    // device's first observation as a transition.
    if let Ok(events) =
        db::query_recent_network_status_events(pool, NETWORK_STATUS_HISTORY_LEN).await
    {
        state.write().unwrap().network_status_events = events;
    }

    if !initial.is_empty() {
        initial.sort_by_key(|d| ip_sort_key(d.ip));
        state.write().unwrap().devices = initial;
    }
}

/// Background task: every 30 s, checks wall-clock time against scheduled switch timers and fires
/// any that match the current HH:MM and haven't already fired today.
async fn timer_job(state: SharedState) {
    let mut ticker = tokio::time::interval(Duration::from_secs(30));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // (timer_id, "YYYY-MM-DD") pairs we've already fired — cleared when the date rolls over.
    let mut fired: std::collections::HashSet<(i64, String)> = std::collections::HashSet::new();

    loop {
        ticker.tick().await;

        let now = chrono::Local::now();
        let today = now.format("%Y-%m-%d").to_string();
        let now_hhmm = now.format("%H:%M").to_string();

        // Collect to-fire list under read lock, then drop before awaiting network calls.
        let to_fire: Vec<(IpAddr, i64, bool)> = {
            let app = state.read().unwrap();
            let mut actions = Vec::new();
            for (ip, mode) in &app.switch_auto_modes {
                if *mode != SwitchAutoMode::Time {
                    continue;
                }
                if let Some(timers) = app.switch_timers.get(ip) {
                    for t in timers {
                        let key = (t.id, today.clone());
                        if t.time_hhmm == now_hhmm && !fired.contains(&key) {
                            actions.push((*ip, t.id, t.relay_on));
                        }
                    }
                }
            }
            actions
        };

        for (ip, timer_id, relay_on) in to_fire {
            let key = (timer_id, today.clone());
            if fired.contains(&key) {
                continue;
            }
            let _ =
                devices::mystrom_switch::set_relay(ip, devices::mystrom_switch::API_PORT, relay_on)
                    .await;
            fired.insert(key);
        }

        // Drop yesterday's fire records.
        fired.retain(|(_, date)| *date == today);
    }
}

/// Returns true if `ip` already has a poll loop running.
fn already_polled(ip: IpAddr, state: &SharedState) -> bool {
    state.read().unwrap().polled_ips.contains(&ip)
}

/// Marks `ip` as having a poll loop running, so future scans don't spawn another.
fn mark_polled(ip: IpAddr, state: &SharedState) {
    state.write().unwrap().polled_ips.insert(ip);
}

/// Spawns a Sonnen battery poll loop for `ip` if one is not already running.
async fn maybe_spawn_poll_loop(ip: IpAddr, pool: &SqlitePool, state: &SharedState) {
    if already_polled(ip, state) {
        return;
    }
    let Ok(configured) = devices::sonnen_batterie::load_all(pool).await else {
        return;
    };
    let Some(device) = configured.into_iter().find(|d| d.ip == ip) else {
        return;
    };
    // Not yet configured with an API key (set via db::set_device_api_key) — skip
    // polling rather than hammer the device with requests we know will fail.
    if device.api_key.as_deref().unwrap_or("").is_empty() {
        return;
    }
    mark_polled(ip, state);
    let pool = pool.clone();
    let state = state.clone();
    tokio::spawn(async move {
        devices::sonnen_batterie::poll_loop(pool, device, state).await;
    });
}

/// Spawns a myStrom switch poll loop for `ip` if one is not already running.
async fn maybe_spawn_switch_poll_loop(ip: IpAddr, pool: &SqlitePool, state: &SharedState) {
    if already_polled(ip, state) {
        return;
    }
    let Ok(configured) = devices::mystrom_switch::load_all(pool).await else {
        return;
    };
    let Some(device) = configured.into_iter().find(|d| d.ip == ip) else {
        return;
    };
    mark_polled(ip, state);
    let pool = pool.clone();
    let state = state.clone();
    tokio::spawn(async move {
        devices::mystrom_switch::poll_loop(pool, device, state).await;
    });
}

/// Spawns a MikroTik poll loop for `ip` if one is not already running.
async fn maybe_spawn_mikrotik_poll_loop(ip: IpAddr, pool: &SqlitePool, state: &SharedState) {
    if already_polled(ip, state) {
        return;
    }
    let Ok(configured) = devices::mikrotik::load_all(pool).await else {
        return;
    };
    let Some(device) = configured.into_iter().find(|d| d.ip == ip) else {
        return;
    };
    // Not yet configured with a login (set via db::set_device_login) — skip
    // polling rather than hammer the device with requests we know will fail.
    if device.username.is_empty() || device.password.is_empty() {
        return;
    }
    mark_polled(ip, state);
    let pool = pool.clone();
    let state = state.clone();
    tokio::spawn(async move {
        devices::mikrotik::poll_loop(pool, device, state).await;
    });
}

/// Spawns a KEBA wallbox poll loop for `ip` if one is not already running.
async fn maybe_spawn_keba_poll_loop(ip: IpAddr, pool: &SqlitePool, state: &SharedState) {
    if already_polled(ip, state) {
        return;
    }
    let Ok(configured) = devices::keba::load_all(pool).await else {
        return;
    };
    let Some(device) = configured.into_iter().find(|d| d.ip == ip) else {
        return;
    };
    let mode = db::load_keba_mode(pool, ip)
        .await
        .unwrap_or(devices::keba::ChargingMode::Disabled);
    mark_polled(ip, state);
    state.write().unwrap().keba_modes.insert(ip, mode);
    let pool = pool.clone();
    let state = state.clone();
    let eco_device = device.clone();
    let eco_pool = pool.clone();
    let eco_state = state.clone();
    tokio::spawn(async move {
        devices::keba::poll_loop(pool, device, mode, state).await;
    });
    tokio::spawn(async move {
        devices::keba::eco_loop(eco_pool, eco_device, eco_state).await;
    });
}

fn ip_sort_key(ip: IpAddr) -> u32 {
    match ip {
        IpAddr::V4(v4) => u32::from(v4),
        IpAddr::V6(_) => u32::MAX,
    }
}

/// Merges ping/ARP-discovered devices with extra IPs from DHCP lease tables
/// (e.g. a device behind AP client isolation that never answers ICMP but is
/// known to the router). Returns a deduplicated, IP-sorted fingerprint list.
fn merge_scan_targets(
    devs: &[devices::Device],
    lease_ips: impl IntoIterator<Item = IpAddr>,
) -> Vec<IpAddr> {
    let mut set: std::collections::HashSet<IpAddr> = devs.iter().map(|d| d.ip).collect();
    set.extend(lease_ips);
    let mut ips: Vec<IpAddr> = set.into_iter().collect();
    ips.sort_by_key(|ip| ip_sort_key(*ip));
    ips
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_scan_targets_dedupes_and_sorts() {
        let devs = [
            devices::Device {
                ip: "192.168.1.5".parse().unwrap(),
                latency_ms: 1.0,
            },
            devices::Device {
                ip: "192.168.1.2".parse().unwrap(),
                latency_ms: 1.0,
            },
        ];
        let leases = vec![
            "192.168.1.2".parse().unwrap(), // duplicate of a pinged device
            "192.168.1.50".parse().unwrap(),
        ];

        let merged = merge_scan_targets(&devs, leases);

        assert_eq!(
            merged,
            vec![
                "192.168.1.2".parse::<IpAddr>().unwrap(),
                "192.168.1.5".parse().unwrap(),
                "192.168.1.50".parse().unwrap(),
            ]
        );
    }

    #[test]
    fn merge_scan_targets_with_no_leases_returns_just_devs() {
        let devs = [devices::Device {
            ip: "10.0.0.1".parse().unwrap(),
            latency_ms: 2.0,
        }];

        let merged = merge_scan_targets(&devs, std::iter::empty());

        assert_eq!(merged, vec!["10.0.0.1".parse::<IpAddr>().unwrap()]);
    }

    /// Regression test: a failed ping scan (e.g. missing CAP_NET_RAW) yields
    /// no `devs`, but DHCP-lease IPs from the router must still be merged in
    /// and fingerprinted — lease-based discovery must not depend on ICMP
    /// working on this host.
    #[test]
    fn merge_scan_targets_with_empty_devs_still_returns_leases() {
        let leases = vec![
            "192.168.1.100".parse().unwrap(),
            "192.168.1.101".parse().unwrap(),
        ];

        let merged = merge_scan_targets(&[], leases);

        assert_eq!(
            merged,
            vec![
                "192.168.1.100".parse::<IpAddr>().unwrap(),
                "192.168.1.101".parse().unwrap(),
            ]
        );
    }
}
