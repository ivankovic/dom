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

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{Timelike, Utc};
use sqlx::SqlitePool;
use tokio::sync::Notify;

use dom::app::{SharedState, SwitchAutoMode};
use dom::{app, db, devices, fingerprint, logging, online, stats, tui};

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
///
/// This is how *every* device is found, energy hardware included — a battery or
/// wallbox that appears, or moves to a new address, only starts being polled once
/// discovery notices it. Deliberately unchanged while the infrastructure polling
/// around it was slowed down: this one is not chatter on behalf of a view, it is
/// how the app learns what exists.
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
/// Delete batches per pruning pass. The first prune after the rollup tiers
/// shipped has millions of rows to clear; capping the work per pass keeps each
/// one short, and the backlog drains over the following passes rather than
/// monopolising the database in one go.
const PRUNE_BATCHES_PER_PASS: usize = 25;
/// How many days of daily temperature history the Environment view shows. Bounded
/// so the range chart stays readable in a terminal rather than by what is stored —
/// `TemperatureDaily` is kept indefinitely.
const ENVIRONMENT_HISTORY_DAYS: i64 = 21;
/// How often the outdoor temperature is fetched. MeteoSwiss republishes its
/// 10-minute means on the same cadence, so polling faster only re-reads the same
/// figure — and the insert ignores a repeat anyway.
const WEATHER_INTERVAL_SECS: u64 = 10 * 60;
/// How often the solar forecast is fetched. Open-Meteo reruns the Swiss models
/// every three hours and republishes hourly, so half an hour keeps the outlook
/// current without asking the same question of the same model run repeatedly.
const SOLAR_INTERVAL_SECS: u64 = 30 * 60;
/// How often scheduled switch timers are checked.
const TIMER_TICK_SECS: u64 = 30;
/// How far back a timer sweep will look after a gap.
///
/// A tick that arrives late — a suspended laptop, a stopped process — should
/// still fire a timer it stepped over, but only a recent one: replaying a whole
/// day's schedule on resume gets every switch into the state it should have
/// reached hours ago, in the wrong order and all at once. Half an hour is enough
/// to cover a sleep or a slow round of unreachable switches, and short enough
/// that nothing surprising happens after a long absence.
const MAX_TIMER_CATCHUP_MINUTES: i64 = 30;
/// How many times a due timer is offered to a switch that will not take it.
///
/// Retrying matters — a switch briefly unreachable at the moment its timer came
/// due used to be skipped for the whole day. Retrying *forever* does not: each
/// attempt on an unreachable device costs a five-second connect timeout, and a
/// handful of switches that are simply switched off at the wall would then spend
/// most of every tick being asked again. After this many the timer is recorded
/// as done for the day, with the reason in the log.
const TIMER_MAX_ATTEMPTS: u32 = 5;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Before anything else that might have something to say. The TUI takes the
    // terminal a few lines below, after which nothing printed is visible, so a
    // failure to open the log is reported here or not at all.
    match dom::logging::init() {
        Ok(path) => println!("logging to {}", path.display()),
        Err(e) => eprintln!("warning: continuing without a log file — {e}"),
    }

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

    // Before anything records a measurement. A Raspberry Pi has no battery-backed
    // clock, so it boots at whatever was last written to disk and jumps when NTP
    // catches up — and a sample stamped with a date days in the past is rolled up
    // into the wrong day, or into a day already marked done and therefore never
    // rolled up at all, after which pruning removes it.
    await_plausible_clock(&pool).await;

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
                // Hand back what pruning freed. Without this the file only ever
                // grows to its high-water mark — see `db::reclaim_free_pages`.
                match db::reclaim_free_pages(&pool).await {
                    Ok(0) => {}
                    Ok(left) => log::info!("reclaimed free pages; {left} still on the free list"),
                    Err(e) => log::warn!("reclaiming free pages failed: {e:#}"),
                }
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
                // Daily temperature history for the Environment view. Read here
                // rather than during render so the view stays a pure function of
                // state, like every other one.
                let today = chrono::Local::now().date_naive();
                if let Ok(rows) = db::query_daily_temperature(
                    &pool,
                    today - chrono::Duration::days(ENVIRONMENT_HISTORY_DAYS),
                    today,
                )
                .await
                {
                    state.write().unwrap().temperature_history = rows;
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
    }

    // Background: outdoor temperature, if a location has been configured.
    tokio::spawn(weather_task(state.clone(), pool.clone()));

    // Background: solar production forecast, and the calibration behind it.
    tokio::spawn(solar_task(state.clone(), pool.clone()));

    // Background: keeps the daily energy rollup and the statistics view current.
    tokio::spawn(statistics_task(state.clone(), pool.clone()));

    // Background: fires scheduled switch timers every 30s.
    tokio::spawn(timer_job(state.clone()));

    match tui::run(state, pool, rescan_notify).await? {
        tui::Interface::Closed => Ok(()),
        tui::Interface::Unavailable(why) => {
            // Headless: keep doing the work, with no interface to show it in.
            // The background tasks are already running and are what actually
            // collects; the log is the only thing to watch. See `logging`.
            log::info!("no terminal to draw on ({why}); running headless");
            println!(
                "No terminal available ({why}) — running headless. Watch {}.",
                logging::LOG_FILE
            );
            run_headless().await
        }
    }
}

/// Waits for a shutdown signal, doing nothing else.
///
/// Everything Dom does is already running in background tasks by the time this
/// is reached; this only keeps the process alive and gives it somewhere to stop.
/// Ctrl-C and `SIGTERM` are both honoured, the latter because that is what a
/// service manager sends.
async fn run_headless() -> anyhow::Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => log::info!("interrupted; shutting down"),
        _ = term.recv() => log::info!("SIGTERM; shutting down"),
    }
    Ok(())
}

// ── Outdoor temperature ───────────────────────────────────────────────────────

/// Fetches the outdoor temperature from the nearest MeteoSwiss station and records
/// it, on `WEATHER_INTERVAL_SECS`.
///
/// Does nothing at all until the user configures a location — this is the only
/// part of Dom that talks to the internet, and it stays silent unless asked for.
/// It re-reads the location every tick rather than capturing it once, so setting
/// an address takes effect without a restart.
///
/// A failure is recorded for display rather than logged and forgotten: an outdoor
/// reading that silently stops updating would otherwise look like a stable
/// temperature.
async fn weather_task(state: SharedState, pool: SqlitePool) {
    let mut ticker = tokio::time::interval(Duration::from_secs(WEATHER_INTERVAL_SECS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        online::weather::refresh(&pool, &state).await;
    }
}

// ── Clock sanity ──────────────────────────────────────────────────────────────

/// Longest Dom will wait at startup for the clock to become plausible.
///
/// Bounded because waiting forever is worse than recording with a suspect clock:
/// a machine with no network would never start at all. On a Pi running
/// `fake-hwclock` the shortfall is usually seconds, since the clock is restored
/// from the last shutdown and only has to catch up to the last sample.
const CLOCK_WAIT_LIMIT: Duration = Duration::from_secs(120);

/// How often the clock is re-checked while waiting.
const CLOCK_POLL: Duration = Duration::from_secs(2);

/// Waits until the system clock is at least as late as the newest thing already
/// recorded.
///
/// The database is the only evidence available that time has passed: a row
/// stamped last Tuesday proves the clock once read last Tuesday. If it now reads
/// earlier than that, it is wrong, and anything recorded meanwhile lands in the
/// past — into a day the rollup has already marked done, which is then pruned
/// without ever being summarised.
///
/// Does nothing at all on a machine whose clock is fine, which is every machine
/// with a working RTC.
async fn await_plausible_clock(pool: &SqlitePool) {
    let newest = match db::newest_recorded_time(pool).await {
        Ok(Some(t)) => t,
        // Nothing recorded yet, so there is nothing to be inconsistent with.
        Ok(None) => return,
        Err(e) => {
            log::warn!("could not check the clock against recorded data: {e:#}");
            return;
        }
    };
    if Utc::now() >= newest {
        return;
    }

    log::warn!(
        "the clock reads {}, before the newest recorded sample at {}; waiting up to {}s for it \
         to be corrected",
        Utc::now(),
        newest,
        CLOCK_WAIT_LIMIT.as_secs()
    );
    let deadline = tokio::time::Instant::now() + CLOCK_WAIT_LIMIT;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(CLOCK_POLL).await;
        if Utc::now() >= newest {
            log::info!(
                "the clock is now {} and consistent with the record",
                Utc::now()
            );
            return;
        }
    }
    log::warn!(
        "the clock is still behind the record after {}s; continuing anyway — measurements \
         recorded now will be stamped in the past",
        CLOCK_WAIT_LIMIT.as_secs()
    );
}

// ── Solar production forecast ─────────────────────────────────────────────────

/// Keeps the production forecast current, and the array calibration behind it.
///
/// Two jobs on one ticker because they are ordered: a calibration changes which
/// plane the forecast is requested for, so refitting first means the fetch that
/// follows already asks the right question.
///
/// The calibration is the expensive half and rarely needs doing —
/// `forecast::calibrate` decides for itself whether the stored fit is stale, and
/// how much of it to redo. Failures are logged and the loop continues: a fit that
/// cannot be improved today is not a reason to stop forecasting with yesterday's.
///
/// Like `weather_task`, this is idle until the user sets a location.
async fn solar_task(state: SharedState, pool: SqlitePool) {
    let mut ticker = tokio::time::interval(Duration::from_secs(SOLAR_INTERVAL_SECS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;

        // `calibrate` decides for itself whether a refit is owed and how much of
        // one, so this asks on every tick and usually gets an immediate no.
        if let Err(e) = online::forecast::calibrate(&pool).await {
            log::warn!("calibrating the solar array failed: {e:#}");
        }
        online::forecast::refresh(&pool, &state).await;
    }
}

// ── Long-term statistics ──────────────────────────────────────────────────────

/// Maintains the energy history: advances the rollup tiers, refreshes the
/// statistics view from them, and prunes raw data the tiers have superseded.
///
/// The tiers are `2s -> per-minute -> daily`, each retained for progressively
/// longer (four days, ninety days, forever). Nothing in the UI renders finer than
/// per-minute, and only for today, so keeping the 2s series indefinitely stored
/// roughly thirty times the resolution anything consumes — which is what made the
/// database grow about a gigabyte a month.
///
/// Order matters: roll up, then prune. Each prune additionally refuses on its own
/// to drop a day the next tier up has not processed, so a rollup failure cannot
/// turn into data loss.
///
/// Refreshes the view before rolling up as well as after, so existing figures are
/// on screen immediately rather than after the first pass completes: the very first
/// run has the entire recorded history to aggregate.
async fn statistics_task(state: SharedState, pool: SqlitePool) {
    stats::refresh(&pool, &state).await;
    loop {
        let rolled = db::rollup_history(&pool, ROLLUP_THROTTLE).await;
        // Recorded in `App` as well as logged. A failing rollup stops pruning
        // (below), and the database resumes growing by roughly a gigabyte a
        // month — which is much too consequential to leave in a file nobody is
        // watching. See `logging`.
        match &rolled {
            Ok(0) => state.write().unwrap().rollup_error = None,
            Ok(n) => {
                log::info!("rolled up {n} days of energy history");
                state.write().unwrap().rollup_error = None;
            }
            Err(e) => {
                log::warn!("energy rollup failed: {e:#}");
                state.write().unwrap().rollup_error = Some(format!("{e:#}"));
            }
        }
        stats::refresh(&pool, &state).await;

        // Prune only when the rollup pass that just ran was healthy.
        // `prune_energy_2s` independently refuses to delete days the minute tier
        // has not processed, so this is belt and braces rather than the only
        // safeguard — but there is no reason to spend I/O deleting while the tier
        // that has to outlive the raw samples is failing to advance.
        if rolled.is_ok() {
            for (what, result) in [
                (
                    "energy",
                    db::prune_energy_2s(&pool, PRUNE_BATCHES_PER_PASS).await,
                ),
                (
                    "storage",
                    db::prune_energy_storage_2s(&pool, PRUNE_BATCHES_PER_PASS).await,
                ),
                (
                    "per-minute",
                    db::prune_energy_minute(&pool, PRUNE_BATCHES_PER_PASS).await,
                ),
            ] {
                match result {
                    Ok(0) => {}
                    Ok(n) => log::info!("pruned {n} raw {what} samples"),
                    Err(e) => log::warn!("pruning raw {what} samples failed: {e:#}"),
                }
            }
        } else {
            log::warn!("skipping prune: the rollup pass did not complete");
        }

        tokio::time::sleep(Duration::from_secs(ROLLUP_INTERVAL_SECS)).await;
    }
}

// ── Ping scan ─────────────────────────────────────────────────────────────────

/// Runs the ICMP ping scan: immediately at startup, once more after
/// `PING_SCAN_FOLLOWUP_SECS` (to catch devices that weren't up yet at t=0),
/// then every `PING_SCAN_STEADY_INTERVAL_SECS` — or immediately whenever
/// `rescan` is notified (the TUI's manual "rescan now" key). Independent of
/// device discovery (`discovery_task`). On success it records the latest
/// ping-reachable device list, which `discovery_task` merges with the router's
/// DHCP leases; the round-trip time of each reply becomes that device's latency
/// in the Devices view.
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
                app.last_ping_devices = scan_result.successful;
            }
            Err(e) => {
                state.write().unwrap().last_scan_error = Some(e.to_string());
            }
        }
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

        // Automatically configure and add our own IP addresses as "Dom", and
        // drop any this machine no longer has — see `forget_stale_addresses`.
        let local_ips = devices::dom_local::detect_local_ips();
        for local_ip in &local_ips {
            let _ = devices::dom_local::save_device(&pool, *local_ip).await;
        }
        if let Err(e) = devices::dom_local::forget_stale_addresses(&pool, &local_ips).await {
            log::warn!("could not tidy the local machine's old addresses: {e:#}");
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
    let mut ticker = tokio::time::interval(Duration::from_secs(TIMER_TICK_SECS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // (timer_id, "YYYY-MM-DD") pairs that have fired, and ones that came due but
    // whose switch would not take the command. Both are cleared when the date
    // rolls over.
    let mut fired: std::collections::HashSet<(i64, String)> = std::collections::HashSet::new();
    // ...and how many attempts each pending one has already had, so a switch
    // that is simply off does not get talked to for the rest of the day.
    let mut pending: std::collections::HashMap<(i64, String), u32> =
        std::collections::HashMap::new();

    // Where the last sweep reached. Starts at "now" so timers earlier today do
    // not all fire at once on startup — Dom coming up at six in the evening must
    // not run the morning's schedule.
    let mut swept_to = chrono::Local::now();

    loop {
        ticker.tick().await;

        let now = chrono::Local::now();
        let today = now.format("%Y-%m-%d").to_string();

        // A timer fires when its time falls in the interval since the last
        // sweep, rather than when it equals the current minute. Equality needed
        // a tick to land inside the right minute, and a suspended machine or a
        // slow poll meant the minute could pass unvisited — after which that
        // firing was simply lost for the day.
        let window = elapsed_window(swept_to, now);

        let to_fire: Vec<(IpAddr, i64, bool)> = {
            let app = state.read().unwrap();
            let mut actions = Vec::new();
            for (ip, mode) in &app.switch_auto_modes {
                if *mode != SwitchAutoMode::Time {
                    continue;
                }
                for t in app.switch_timers.get(ip).into_iter().flatten() {
                    let key = (t.id, today.clone());
                    if fired.contains(&key) {
                        continue;
                    }
                    // Either newly due, or due earlier and not yet accepted.
                    if pending.contains_key(&key) || window.contains(&t.time_hhmm) {
                        actions.push((*ip, t.id, t.relay_on));
                    }
                }
            }
            actions
        };

        for (ip, timer_id, relay_on) in to_fire {
            let key = (timer_id, today.clone());
            // Marked done only once the switch confirms it. A device that was
            // briefly unreachable used to be recorded as fired and never retried.
            match devices::mystrom_switch::set_relay(
                ip,
                devices::mystrom_switch::API_PORT,
                relay_on,
            )
            .await
            {
                Ok(()) => {
                    log::info!(
                        "timer {timer_id} switched {ip} {}",
                        if relay_on { "on" } else { "off" }
                    );
                    fired.insert(key.clone());
                    pending.remove(&key);
                }
                Err(e) => {
                    let attempts = pending.entry(key.clone()).or_insert(0);
                    *attempts += 1;
                    if *attempts >= TIMER_MAX_ATTEMPTS {
                        log::warn!(
                            "timer {timer_id} could not switch {ip} after {attempts} attempts \
                             ({e:#}); giving up until tomorrow"
                        );
                        pending.remove(&key);
                        // Recorded as fired so it is not attempted again today.
                        // The switch never took the command, and the log says so;
                        // what this prevents is retrying a device that is simply
                        // off, every tick, for the rest of the day.
                        fired.insert(key);
                    } else {
                        log::warn!(
                            "timer {timer_id} could not switch {ip}: {e:#}; \
                             attempt {attempts}, will retry"
                        );
                    }
                }
            }
        }

        swept_to = now;
        fired.retain(|(_, date)| *date == today);
        pending.retain(|(_, date), _| *date == today);
    }
}

/// The `HH:MM` minutes strictly after `from` and up to and including `to`.
///
/// This is what a timer is matched against, so that a firing is caught by
/// whichever tick next runs rather than only by one that lands inside its own
/// minute. Returns nothing when the two instants are in the same minute, so a
/// timer fires once rather than on every tick of the minute it is due.
///
/// Bounded by `MAX_TIMER_CATCHUP_MINUTES`: after a long gap — a suspended
/// machine, a process that was stopped overnight — only the recent past is
/// swept. Firing a whole day of schedule at once on resume would be worse than
/// missing it, since the point of a timer is *when* it happens.
///
/// A backwards jump (`to` before `from`) yields nothing, which is also what a
/// clock stepped backwards by NTP produces.
///
/// Daylight saving is handled by the arithmetic rather than by this function:
/// the subtraction is on `DateTime<Local>`, so an autumn fall-back sweeps the
/// repeated hour's minutes a second time — but `fired` is keyed by timer and
/// date, so a timer inside it still fires only once. In spring the skipped hour
/// never occurs, and a timer set inside it does not fire that day; there is no
/// instant at which it was due.
///
/// Both instants are truncated to the minute before being compared, and that is
/// load-bearing rather than tidiness. Measuring the raw duration truncates
/// towards zero, so two sweeps thirty seconds apart across a minute boundary —
/// 07:00:30 to 07:01:00, which is exactly what a thirty-second tick produces —
/// measure as zero minutes and the window comes back empty. The 07:01 firing
/// would then be missed by the very mechanism meant to stop it being missed.
fn elapsed_window(
    from: chrono::DateTime<chrono::Local>,
    to: chrono::DateTime<chrono::Local>,
) -> Vec<String> {
    let truncate =
        |t: chrono::DateTime<chrono::Local>| t - chrono::Duration::seconds(i64::from(t.second()));
    let (from, to) = (truncate(from), truncate(to));

    let minutes = (to - from).num_minutes();
    if minutes <= 0 {
        return Vec::new();
    }
    let span = minutes.min(MAX_TIMER_CATCHUP_MINUTES);
    (0..span)
        .map(|back| {
            (to - chrono::Duration::minutes(back))
                .format("%H:%M")
                .to_string()
        })
        .collect()
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

/// Orders addresses so the device list reads the way a person reads a subnet.
///
/// IPv4 sorts numerically and ahead of IPv6, which is what someone scanning a
/// list of `172.16.x.y` expects. IPv6 sorts numerically among itself rather than
/// collapsing to one value: the previous key mapped every v6 address to
/// `u32::MAX`, so they all compared equal, and since the input arrives from a
/// `HashSet` — whose iteration order is deliberately not stable — their relative
/// order changed from run to run for no reason the user could see.
fn ip_sort_key(ip: IpAddr) -> (u8, u128) {
    match ip {
        IpAddr::V4(v4) => (0, u128::from(u32::from(v4))),
        IpAddr::V6(v6) => (1, u128::from(v6)),
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

    #[test]
    fn addresses_sort_numerically_with_ipv4_first() {
        let mut ips: Vec<IpAddr> = [
            "fe80::2",
            "192.168.1.10",
            "::1",
            "192.168.1.2",
            "fe80::1",
            "10.0.0.1",
        ]
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
        ips.sort_by_key(|ip| ip_sort_key(*ip));

        let as_text: Vec<String> = ips.iter().map(std::string::ToString::to_string).collect();
        assert_eq!(
            as_text,
            vec![
                "10.0.0.1",
                "192.168.1.2",
                "192.168.1.10",
                "::1",
                "fe80::1",
                "fe80::2"
            ],
            "v4 numerically and first, then v6 numerically — not v4 lexically, \
             and not every v6 address tied for last"
        );
    }

    #[test]
    fn ipv6_ordering_does_not_depend_on_the_order_it_was_discovered_in() {
        // The regression: every v6 address used to map to the same key, so the
        // stable sort preserved HashSet iteration order, which is not stable.
        let ips: Vec<IpAddr> = ["fe80::3", "fe80::1", "fe80::2"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let sorted = |mut v: Vec<IpAddr>| {
            v.sort_by_key(|ip| ip_sort_key(*ip));
            v
        };
        let mut reversed = ips.clone();
        reversed.reverse();
        assert_eq!(sorted(ips), sorted(reversed));
    }

    // ── Timer sweep window ────────────────────────────────────────────────────

    fn at(hhmm: &str) -> chrono::DateTime<chrono::Local> {
        at_sec(hhmm, 0)
    }

    fn at_sec(hhmm: &str, second: u32) -> chrono::DateTime<chrono::Local> {
        use chrono::TimeZone;
        let (h, m) = hhmm.split_once(':').unwrap();
        chrono::Local
            .with_ymd_and_hms(2026, 8, 23, h.parse().unwrap(), m.parse().unwrap(), second)
            .earliest()
            .unwrap()
    }

    #[test]
    fn consecutive_ticks_across_a_minute_boundary_still_fire() {
        // The case a raw duration gets wrong: thirty seconds apart is zero whole
        // minutes, so measuring the gap rather than the minutes crossed returns
        // an empty window and misses 07:01 entirely — which is the failure the
        // window was introduced to prevent.
        assert_eq!(
            elapsed_window(at_sec("07:00", 30), at_sec("07:01", 0)),
            vec!["07:01"]
        );
        // ...and two ticks inside one minute still fire nothing.
        assert!(elapsed_window(at_sec("07:00", 0), at_sec("07:00", 30)).is_empty());
    }

    #[test]
    fn every_minute_is_swept_exactly_once_over_a_run_of_ticks() {
        // Walk a thirty-second tick across several minutes and check each minute
        // is swept once and only once — no gaps, no repeats.
        let mut swept_to = at_sec("06:59", 45);
        let mut seen: Vec<String> = Vec::new();
        for _ in 0..10 {
            let now = swept_to + chrono::Duration::seconds(TIMER_TICK_SECS as i64);
            seen.extend(elapsed_window(swept_to, now));
            swept_to = now;
        }
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(seen.len(), unique.len(), "a minute fired twice: {seen:?}");
        assert_eq!(
            unique,
            vec!["07:00", "07:01", "07:02", "07:03", "07:04"],
            "every minute crossed must be swept: {seen:?}"
        );
    }

    #[test]
    fn a_sweep_within_the_same_minute_fires_nothing() {
        // Otherwise a 30-second tick would fire every timer twice.
        assert!(elapsed_window(at("07:00"), at("07:00")).is_empty());
    }

    #[test]
    fn a_normal_sweep_covers_the_minute_that_just_passed() {
        assert_eq!(elapsed_window(at("06:59"), at("07:00")), vec!["07:00"]);
    }

    #[test]
    fn a_tick_that_arrived_late_still_catches_the_minute_it_stepped_over() {
        // The regression: matching on the current minute alone meant a tick that
        // skipped 07:00 lost that firing for the whole day.
        let window = elapsed_window(at("06:58"), at("07:03"));
        assert!(window.contains(&"07:00".to_string()), "{window:?}");
        assert_eq!(window.len(), 5, "07:03 back to 06:59 inclusive: {window:?}");
    }

    #[test]
    fn a_long_absence_does_not_replay_the_whole_day() {
        // Coming back after a suspend should not run every switch through the
        // schedule it missed, all at once and out of order.
        let window = elapsed_window(at("07:00"), at("19:00"));
        assert_eq!(window.len(), MAX_TIMER_CATCHUP_MINUTES as usize);
        assert!(window.contains(&"19:00".to_string()));
        assert!(
            !window.contains(&"07:00".to_string()),
            "far past is not swept"
        );
    }

    #[test]
    fn a_clock_that_went_backwards_fires_nothing() {
        assert!(elapsed_window(at("07:00"), at("06:00")).is_empty());
    }

    #[test]
    fn the_sweep_always_reaches_at_least_as_far_back_as_one_tick() {
        // Whatever the tick interval, a timer due between two ticks has to land
        // inside the window the later one sweeps.
        let tick = chrono::Duration::seconds(TIMER_TICK_SECS as i64);
        let now = at("07:00");
        let window = elapsed_window(now - tick * 3, now);
        assert!(window.contains(&"07:00".to_string()), "{window:?}");
    }
}
