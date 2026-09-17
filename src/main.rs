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

//! The application: what starts, in what order, and what runs forever.
//!
//! Everything Dom can *do* lives in the library ([`dom`]); this file is the
//! assembly. `main` opens the database, establishes a few facts that everything
//! else depends on, spawns the long-running tasks, and hands the terminal to
//! [`dom::tui::run`] — which may decline it, on a headless machine, without that
//! being an error.
//!
//! # The order at startup is load-bearing
//!
//! Three steps happen before anything is allowed to record a measurement, and
//! each is there because of a specific failure:
//!
//! - **`await_plausible_clock`.** A Raspberry Pi has no battery-backed clock: it
//!   boots at whatever was last written to disk and jumps when NTP catches up.
//!   A sample stamped days in the past is rolled up into the wrong day — or into
//!   a day already marked done, and so never rolled up at all, after which
//!   pruning removes it. The database is the only available evidence that time
//!   has passed, so the check is "is the clock at least as late as the newest
//!   row", and it is bounded: waiting forever would mean a machine with no
//!   network never starts.
//! - **The cluster identity and role.** The node id, keypair and peer address are
//!   read once here rather than by each of the three cluster tasks
//!   independently, and `App::cluster_role` is seeded *synchronously* before any
//!   of them spawn. The heartbeat listener answers from that field as soon as it
//!   accepts connections, so a paired node must never be observed at
//!   `Role::default()` — a peer heartbeating in that window would read a false
//!   `claims_active` and could wrongly demote itself.
//! - **`bootstrap_known_devices`**, so a restart resumes polling what it already
//!   knew about instead of waiting for a discovery pass.
//!
//! # The tasks
//!
//! Twelve are spawned here — nine for the house, three for the cluster — plus
//! one poll loop per configured device. They share nothing but the pool and the
//! one `RwLock` around [`App`](dom::app::App): there are no channels between
//! them, and a task that dies takes only its own job with it.
//!
//! Discovery is two tasks rather than one, on independent schedules: an ICMP
//! sweep of every local subnet, and a pass that reads the router's DHCP leases.
//! Both can be woken early by the interface's "rescan now" key through a shared
//! `Notify`. The rest keep something current on a timer — the charts, the
//! outdoor temperature, the production forecast, the daily rollups, the
//! retention tiers — and three more run the cluster.
//!
//! All but one are on a tokio `interval`, so a task's period is its period and
//! not its period plus however long the work took. `statistics_task` keeps a
//! trailing `sleep` deliberately: there the throttle exists to leave the disk
//! alone *between* passes, and an `interval` would hand it straight into the
//! next one.
//!
//! Release builds are compiled with `panic = "abort"`, which makes any reachable
//! panic in any of these the end of the whole process. That is the reason the
//! robustness work recorded in SPECS.md went where it did.
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{Timelike, Utc};
use sqlx::SqlitePool;
use tokio::sync::{Notify, Semaphore};

use dom::app::{SharedState, SwitchAutoMode};
use dom::{alarm, app, cluster, db, devices, fingerprint, logging, online, solar, stats, tui};

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
/// How long a fingerprint is reused before that device is probed over the
/// network again.
///
/// Learning what exists and re-confirming what is already known are two
/// different jobs that used to share one cost. The target list is rebuilt every
/// `DISCOVERY_INTERVAL_SECS` from the ping scan and the router's leases, which
/// is free — both are already in memory — so a new or moved address is still
/// noticed within five minutes. Re-running a twelve-port scan against a speaker
/// identified weeks ago is what does not need to happen at that rate: it is a
/// burst of TCP connects per device per cycle, and on 2.4 GHz that is airtime
/// taken from whatever else lives there. A cached fingerprint is dropped early
/// anyway whenever the MAC at the address changes, so a device swapped onto an
/// address is re-read on the next cycle rather than waiting out this interval.
const FINGERPRINT_TTL_SECS: u64 = 6 * 60 * 60;
/// How many addresses are fingerprinted at once.
///
/// Every target used to be spawned at once, so a cycle arrived as one
/// simultaneous burst across the whole house. The work is identical either way;
/// spreading it just stops discovery from competing with itself — and with the
/// devices it is scanning — for the air.
const FINGERPRINT_CONCURRENCY: usize = 12;
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
/// How many days of history Eco mode averages over to estimate a switch's
/// typical draw, and the household's typical load without it (see
/// `compute_eco_plan`). Long enough to smooth over one unusual day, short
/// enough to track a season change within a couple of weeks.
const ECO_HISTORY_DAYS: i64 = 14;

/// How many of today's 96 quarter-hour forecast steps must be on hand before
/// Eco will place a window against them, rather than deferring to the
/// configured timer. Open-Meteo serves the whole day at 15-minute resolution
/// in one document (see `online::forecast`), so a normal day has all 96 and
/// anything far short of that means the fetch has not landed — not that the
/// day is dark. Half a day is the loosest reading of "enough to tell morning
/// from afternoon".
const ECO_MIN_FORECAST_STEPS: usize = 48;
/// TCP port the cluster heartbeat listens on — see `cluster_heartbeat_listener_task` and
/// `cluster_task`.
const CLUSTER_HEARTBEAT_PORT: u16 = 7878;
/// How often a paired node attempts to reach its peer. Faster than `TIMER_TICK_SECS` because
/// failover latency (`CLUSTER_HEARTBEAT_INTERVAL_SECS * CLUSTER_PEER_LOST_AFTER_FAILURES`)
/// matters here in a way it doesn't for the other background tasks.
const CLUSTER_HEARTBEAT_INTERVAL_SECS: u64 = 5;
/// Consecutive heartbeat failures tolerated before a peer that was previously reachable is
/// declared `Unreachable`. Mirrors `devices::LOST_AFTER_FAILURES`'s reasoning: one dropped
/// packet must not trigger a failover on a healthy cluster.
const CLUSTER_PEER_LOST_AFTER_FAILURES: u32 = 3;
/// Per-operation timeout for a single heartbeat connect/write/read, on both the initiating and
/// the listening side. Short relative to `CLUSTER_HEARTBEAT_INTERVAL_SECS` so one hung attempt
/// cannot delay the next tick.
const CLUSTER_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(2);

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

    // Asked for by hand, and reported here rather than only to the log: it holds
    // the write lock over a whole-table index rebuild, so the startup it delays
    // is this one, and whoever set the variable is the one waiting.
    if std::env::var_os(db::MIGRATE_ENV).is_some() {
        println!("{}: repacking series indexes…", db::MIGRATE_ENV);
        match db::repack_series_indexes(&pool).await {
            Ok(done) if done.is_empty() => println!("  nothing to do — already repacked"),
            Ok(done) => println!("  rebuilt the index over {}", done.join(" and ")),
            // Not fatal. Every index is either old or new, both are correct, and
            // refusing to start over a space optimization would be the worse bug.
            Err(e) => eprintln!("  failed, leaving the indexes as they were — {e}"),
        }
    }

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

    // Background: prunes the 2s series and old network status events, hourly.
    tokio::spawn(prune_task(pool.clone()));

    // Background: keeps the chart data the views read current.
    tokio::spawn(chart_refresh_task(state.clone(), pool.clone()));

    // Background: outdoor temperature, if a location has been configured.
    tokio::spawn(weather_task(state.clone(), pool.clone()));

    // Background: solar production forecast, and the calibration behind it.
    tokio::spawn(solar_task(state.clone(), pool.clone()));

    // Background: keeps the daily energy rollup and the statistics view current.
    // The only one of these nine whose period drifts rather than being fixed —
    // see its doc for why that is deliberate.
    tokio::spawn(statistics_task(state.clone(), pool.clone()));

    // Background: fires scheduled switch timers every 30s.
    tokio::spawn(timer_job(state.clone()));

    // Background: computes and fires each Eco-mode switch's daily on/off plan.
    tokio::spawn(eco_job(state.clone(), pool.clone()));

    // This node's cluster identity, keypair, and peer address — read/generated once here,
    // ahead of all three tasks below, rather than each establishing them independently.
    let cluster_node_id = db::get_or_create_cluster_node_id(&pool).await?;
    let cluster_keypair = db::get_or_create_cluster_keypair(&pool).await?;
    let cluster_peer_addr = db::cluster_peer_addr(&pool).await?;

    // Seeded synchronously, before either cluster task spawns: the heartbeat
    // listener answers from this field the moment it starts accepting
    // connections, so it must never observe `Role::default()`'s `Active` for
    // a paired node even for the single tick it would take `cluster_task` to
    // correct it — a peer that heartbeats in that window would see a false
    // `claims_active: true` and could wrongly demote itself. See
    // `App::cluster_role`'s doc.
    state.write().unwrap().cluster_role = initial_cluster_role(cluster_peer_addr.is_some());

    // Background: this node's cluster role — solo/Active until pairing exists.
    tokio::spawn(cluster_task(
        cluster_node_id.clone(),
        cluster_peer_addr,
        state.clone(),
        pool.clone(),
    ));

    // Background: answers heartbeat/discovery connections. Spawned
    // unconditionally, like cluster_task — see cluster_heartbeat_listener_task.
    tokio::spawn(cluster_heartbeat_listener_task(
        cluster_node_id.clone(),
        cluster_keypair,
        state.clone(),
    ));

    // Background: looks for an unpaired (or re-identified) Dom peer on the LAN, reusing the same
    // scan infrastructure and rescan trigger as `discovery_task` rather than a second mechanism.
    tokio::spawn(cluster_discovery_task(
        cluster_node_id,
        state.clone(),
        pool.clone(),
        rescan_notify.clone(),
    ));

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

/// Background task: prunes the 2s measurement series and old network status
/// events once an hour, then hands back what that freed.
async fn prune_task(pool: SqlitePool) {
    let mut ticker = tokio::time::interval(Duration::from_secs(3600));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let _ = db::prune_raw_measurements(&pool).await;
        let _ = db::prune_network_status_events(&pool).await;
        // Without this the file only ever grows to its high-water mark — see
        // `db::reclaim_free_pages`.
        match db::reclaim_free_pages(&pool).await {
            Ok(0) => {}
            Ok(left) => log::info!("reclaimed free pages; {left} still on the free list"),
            Err(e) => log::warn!("reclaiming free pages failed: {e:#}"),
        }
    }
}

/// Background task: re-reads the chart data the views draw from, immediately and
/// then every 60s.
///
/// Read here rather than during render, so every view stays a pure function of
/// the state it is handed.
///
/// On an `interval` like the other tasks rather than a trailing `sleep`. The
/// difference is not cosmetic: a `sleep` at the foot of the loop makes the
/// period *sixty seconds plus however long the four queries took*, which on a
/// Pi is seconds and used to drift further as the database grew. An `interval`
/// measures from tick to tick, so the charts refresh on the minute whatever the
/// queries cost, and `Skip` means a pass that overran drops the tick it missed
/// instead of running twice back to back.
async fn chart_refresh_task(state: SharedState, pool: SqlitePool) {
    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        // The first tick completes immediately, so the charts are populated on
        // startup rather than a minute into it.
        ticker.tick().await;
        if let Ok(data) = db::query_today_energy(&pool).await {
            state.write().unwrap().energy_chart = data;
        }
        if let Ok(map) = db::query_device_energy_today(&pool).await {
            state.write().unwrap().device_energy_today = map;
        }
        if let Ok(data) = db::query_internet_traffic_today(&pool).await {
            state.write().unwrap().internet_traffic_chart = data;
        }
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
///
/// The one task that deliberately does *not* run on an `interval`, and the
/// reason is the throttle. `rollup_history` sleeps `ROLLUP_THROTTLE` between
/// days precisely so it does not starve the poll loops of a shared SD card, and
/// its first run has the whole history to work through — a pass can take
/// minutes. An `interval` measures tick to tick, so after a long pass the next
/// tick is already overdue and fires at once: the throttle would have spread
/// one pass out and then handed the disk straight to the next. The trailing
/// `sleep` instead guarantees a quiet gap *between* passes however long a pass
/// took, which is what the throttle is for in the first place.
///
/// The consequence, stated because it is otherwise invisible: this task's period
/// is `ROLLUP_INTERVAL_SECS` plus the length of a pass, and it drifts. Nothing
/// depends on it landing at a particular time — the statistics view also reloads
/// on its own whenever the user changes period.
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

    // Survives across cycles: the point of it is that an address identified on
    // an earlier pass is not probed again on this one.
    let mut fingerprints: HashMap<IpAddr, CachedFingerprint> = HashMap::new();

    loop {
        // "Rescan now" is a request to re-read the network, not to have the
        // last answer repeated back, so it empties the cache first.
        let forced = tokio::select! {
            _ = ticker.tick() => false,
            _ = rescan.notified() => true,
        };
        if forced {
            fingerprints.clear();
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
        let lease_ips = lease_addresses(&state.read().unwrap());
        let fingerprint_targets = merge_scan_targets(&devs, lease_ips);

        // Judged against what is at each address *now*: an entry survives only
        // while it is recent and the MAC there still matches. The ARP read
        // after the probes below is the one that stamps identity, and sees
        // whatever this cycle just resolved.
        let (mut fps, to_probe) = plan_fingerprints(
            &fingerprint_targets,
            &fingerprints,
            &devices::arp_cache(),
            Duration::from_secs(FINGERPRINT_TTL_SECS),
            Instant::now(),
        );

        // Bounded, so a cycle is a stream of probes rather than one burst
        // arriving at every address in the house simultaneously.
        let limit = Arc::new(Semaphore::new(FINGERPRINT_CONCURRENCY));
        let mut set = tokio::task::JoinSet::new();
        for ip in to_probe {
            let limit = Arc::clone(&limit);
            set.spawn(async move {
                // Held for the probe and released with the task. `acquire_owned`
                // fails only on a closed semaphore, and this one outlives the
                // set holding the handles.
                let _permit = limit.acquire_owned().await;
                fingerprint::fingerprint(ip).await
            });
        }
        let mut probed: Vec<fingerprint::Fingerprint> = Vec::new();
        while let Some(joined) = set.join_next().await {
            // One task that did not finish cleanly must not end the round. The
            // `while let Some(Ok(fp))` this replaces stopped draining at the
            // first `Err` and silently abandoned every result still in the set.
            // Release builds abort on panic, so that could not cost a live
            // cycle its discoveries — but it is the wrong shape, and it does
            // cost them under test. `cluster_discovery_task` already reads its
            // own set this way.
            match joined {
                Ok(fp) => probed.push(fp),
                Err(e) => log::warn!("a fingerprint did not complete: {e}"),
            }
        }

        // Stable per-device identity (MAC address), independent of IP, so a
        // device with a new DHCP lease is recognized as the same device
        // rather than registered a second time — see db::upsert_device.
        let mac_by_ip = devices::arp_cache();
        let fingerprint_for = |ip: IpAddr| -> Option<String> { mac_at(ip, &mac_by_ip) };

        // Keep what this cycle learned, against the MAC that was there when it
        // was read, and drop addresses that are no longer targets so the map
        // cannot grow without bound.
        let taken = Instant::now();
        for fp in &probed {
            fingerprints.insert(
                fp.ip,
                CachedFingerprint {
                    fp: fp.clone(),
                    mac: mac_at(fp.ip, &mac_by_ip),
                    at: taken,
                },
            );
        }
        let still_a_target: HashSet<IpAddr> = fingerprint_targets.iter().copied().collect();
        fingerprints.retain(|ip, _| still_a_target.contains(ip));

        // Everything downstream — the persisted types, the poll loops, the
        // device list — reads one list, whether an entry was probed just now or
        // carried over.
        fps.extend(probed);
        fps.sort_by_key(|fp| ip_sort_key(fp.ip));

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
        //
        // Read once and reused by `merge_device_list` below, which used to call
        // `detect_local_ips` again for itself. One reading of the interfaces per
        // cycle, so what is saved here and what is displayed cannot disagree.
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
        let dhcp_info = dhcp_labels(&state.read().unwrap());

        // Load all labels from DB as a fallback
        let db_labels: std::collections::HashMap<std::net::IpAddr, Option<String>> =
            db::load_all_device_labels(&pool).await.unwrap_or_default();

        {
            let mut app = state.write().unwrap();
            let new_devices = merge_device_list(
                &fps,
                &devs,
                &all_existing_devices,
                &app.polled_ips,
                &local_ips,
                &db_labels,
                &dhcp_info,
            );

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

/// Every address the router has handed out a DHCP lease for.
///
/// Some devices — behind AP client isolation, say — never answer ICMP, but the
/// router knows about them anyway, so these are worth reaching for by other
/// means.
fn lease_addresses(app: &app::App) -> Vec<IpAddr> {
    app.mikrotik_readings
        .values()
        .flat_map(|r| r.leases.iter())
        .filter_map(|lease| lease.address.parse().ok())
        .collect()
}

/// Names the router knows for the addresses it has leased: the lease comment
/// where one is set, otherwise the host-name the device gave for itself. Empty
/// strings are dropped rather than shown as a blank label.
fn dhcp_labels(app: &app::App) -> HashMap<IpAddr, String> {
    app.mikrotik_readings
        .values()
        .flat_map(|r| r.leases.iter())
        .filter_map(|lease| {
            let label = lease
                .comment
                .clone()
                .or_else(|| lease.host_name.clone())
                .filter(|s| !s.is_empty());
            lease.address.parse().ok().zip(label)
        })
        .collect()
}

/// The device list a finished scan should leave behind, in address order.
///
/// A scan is not the whole truth about what is on the network, so its results
/// are a union rather than a replacement: everything fingerprinted this round,
/// plus this machine's own addresses, plus anything already known that the scan
/// missed but which is either being polled or carries a label. Without those
/// last two a device would vanish from the list for a cycle because one probe
/// went unanswered.
#[allow(clippy::too_many_arguments)]
fn merge_device_list(
    fps: &[fingerprint::Fingerprint],
    devs: &[devices::Device],
    existing: &[app::ScannedDevice],
    polled_ips: &HashSet<IpAddr>,
    local_ips: &[IpAddr],
    db_labels: &HashMap<IpAddr, Option<String>>,
    dhcp_info: &HashMap<IpAddr, String>,
) -> Vec<app::ScannedDevice> {
    let mut new_devices: Vec<app::ScannedDevice> = fps
        .iter()
        .map(|fp| app::ScannedDevice {
            ip: fp.ip,
            latency_ms: latency_for(fp.ip, devs),
            open_ports: fp.open_ports.clone(),
            // The same classification used when persisting the device, so the
            // displayed model cannot disagree with the stored type.
            // Against the `local_ips` this call was handed, not against a
            // fresh `is_local_ip`, which re-reads the host's interfaces. The
            // caller's comment already says why one reading per cycle matters —
            // "what is saved here and what is displayed cannot disagree" — and
            // this was the one call site inside the function that comment is
            // about which still did its own. It also cost one `getifaddrs` per
            // unidentified address, on every cycle.
            name: devices::detect_type(fp)
                .map(|t| t.display_name())
                .or_else(|| {
                    local_ips
                        .contains(&fp.ip)
                        .then_some(devices::dom_local::NAME)
                }),
            label: resolve_label(fp.ip, existing, db_labels, dhcp_info),
        })
        .collect();

    for local_ip in local_ips {
        if !new_devices.iter().any(|d| d.ip == *local_ip) {
            new_devices.push(app::ScannedDevice {
                ip: *local_ip,
                latency_ms: latency_for(*local_ip, devs),
                open_ports: vec![],
                name: Some(devices::dom_local::NAME),
                label: resolve_label(*local_ip, existing, db_labels, dhcp_info),
            });
        }
    }

    for device in existing {
        if !new_devices.iter().any(|d| d.ip == device.ip)
            && (polled_ips.contains(&device.ip) || device.label.is_some())
        {
            new_devices.push(device.clone());
        }
    }

    new_devices.sort_by_key(|d| ip_sort_key(d.ip));
    new_devices
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
/// A device loaded from the database, presented as if a scan had just found it.
///
/// Zero latency and no open ports because nothing has been probed yet: the poll
/// loops spawned alongside are what fill those in, and until they do the device
/// still has to appear in the list.
fn known_device(ip: IpAddr, name: &'static str, label: Option<String>) -> app::ScannedDevice {
    app::ScannedDevice {
        ip,
        latency_ms: 0.0,
        open_ports: vec![],
        name: Some(name),
        label,
    }
}

async fn bootstrap_known_devices(state: &SharedState, pool: &SqlitePool) {
    let mut initial: Vec<app::ScannedDevice> = Vec::new();

    if let Ok(rows) = devices::sonnen_batterie::load_all(pool).await {
        for d in &rows {
            initial.push(known_device(
                d.ip,
                devices::sonnen_batterie::NAME,
                d.label.clone(),
            ));
        }
        for d in rows {
            maybe_spawn_poll_loop(d.ip, pool, state).await;
        }
    }
    if let Ok(rows) = devices::mystrom_switch::load_all(pool).await {
        for d in &rows {
            initial.push(known_device(
                d.ip,
                devices::mystrom_switch::NAME,
                d.label.clone(),
            ));
        }
        for d in rows {
            maybe_spawn_switch_poll_loop(d.ip, pool, state).await;
        }
    }
    if let Ok(rows) = devices::mikrotik::load_all(pool).await {
        for d in &rows {
            initial.push(known_device(d.ip, devices::mikrotik::NAME, d.label.clone()));
        }
        for d in rows {
            maybe_spawn_mikrotik_poll_loop(d.ip, pool, state).await;
        }
    }
    if let Ok(rows) = devices::keba::load_all(pool).await {
        for d in &rows {
            initial.push(known_device(d.ip, devices::keba::NAME, d.label.clone()));
        }
        for d in rows {
            maybe_spawn_keba_poll_loop(d.ip, pool, state).await;
        }
    }

    // Add local machine as "Dom" devices
    if let Ok(rows) = devices::dom_local::load_all(pool).await {
        for d in &rows {
            initial.push(known_device(d.ip, devices::dom_local::NAME, None));
        }
    }

    // Also add any local IPs that haven't been saved to DB yet
    let local_ips = devices::dom_local::detect_local_ips();
    for local_ip in local_ips {
        // Check if this IP is already in the initial list
        if !initial.iter().any(|d| d.ip == local_ip) {
            initial.push(known_device(local_ip, devices::dom_local::NAME, None));
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

/// A timer, identified by its id and the local day it is due on. Both halves
/// matter: the bookkeeping below is cleared when the date rolls over, so the
/// same timer is due again tomorrow.
type TimerKey = (i64, String);

/// The timers that should be commanded this tick.
///
/// A timer qualifies when its switch is in Time mode, it has not already fired
/// today, and it is either newly due — its time falls in the minutes swept since
/// the last tick — or was due earlier and has not been accepted yet.
fn timers_to_fire(
    app: &app::App,
    window: &[String],
    fired: &HashSet<TimerKey>,
    pending: &HashMap<TimerKey, u32>,
    today: &str,
) -> Vec<(IpAddr, i64, bool)> {
    let mut actions = Vec::new();
    for (ip, mode) in &app.switch_auto_modes {
        if *mode != SwitchAutoMode::Time {
            continue;
        }
        for t in app.switch_timers.get(ip).into_iter().flatten() {
            let key = (t.id, today.to_string());
            if fired.contains(&key) {
                continue;
            }
            if pending.contains_key(&key) || window.contains(&t.time_hhmm) {
                actions.push((*ip, t.id, t.relay_on));
            }
        }
    }
    actions
}

/// Records one failed attempt at a timer, returning how many it has now had and
/// whether it has been given up on for the day.
///
/// Giving up records it as fired, which is not a claim that it ran — the log
/// says otherwise. What it prevents is retrying a device that is simply off,
/// every tick, until midnight.
fn record_failed_attempt(
    fired: &mut HashSet<TimerKey>,
    pending: &mut HashMap<TimerKey, u32>,
    key: TimerKey,
) -> (u32, bool) {
    let attempts = pending.entry(key.clone()).or_insert(0);
    *attempts += 1;
    let attempts = *attempts;

    if attempts >= TIMER_MAX_ATTEMPTS {
        pending.remove(&key);
        fired.insert(key);
        (attempts, true)
    } else {
        (attempts, false)
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
    let mut fired: HashSet<TimerKey> = HashSet::new();
    // ...and how many attempts each pending one has already had, so a switch
    // that is simply off does not get talked to for the rest of the day.
    let mut pending: HashMap<TimerKey, u32> = HashMap::new();

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

        let to_fire = timers_to_fire(&state.read().unwrap(), &window, &fired, &pending, &today);

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
                Err(e) => match record_failed_attempt(&mut fired, &mut pending, key) {
                    (attempts, true) => log::warn!(
                        "timer {timer_id} could not switch {ip} after {attempts} attempts \
                         ({e:#}); giving up until tomorrow"
                    ),
                    (attempts, false) => log::warn!(
                        "timer {timer_id} could not switch {ip}: {e:#}; \
                         attempt {attempts}, will retry"
                    ),
                },
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

/// The UTC instant for a given quarter-hour-of-day step on a local calendar
/// day — the inverse of the local-time bucketing `db::
/// query_avg_power_by_local_quarter_hour` groups by.
fn datetime_from_step(day: chrono::NaiveDate, step: usize) -> Option<chrono::DateTime<Utc>> {
    use chrono::TimeZone;
    let step = u32::try_from(step).ok()?;
    let naive = day.and_hms_opt(step / 4, (step % 4) * 15, 0)?;
    chrono::Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Today's predicted production, in watts, for each of the day's 96 quarter-hour
/// steps — or `None` if too little of the forecast covers today to plan against.
///
/// A day the forecast barely covers scores as if the sun never rose, since every
/// unfilled step stays at zero production. That is indistinguishable from a
/// genuinely overcast day — which is a *legitimate* all-tied result that
/// `choose_window` is meant to break at random (see `TIE_EPSILON_WH`) — so the
/// two cannot be told apart after the fact, only here, by asking how many steps
/// actually arrived. Too few and there is no solar signal to place a window
/// against at all; the caller defers to the configured timer instead of rolling
/// dice.
fn production_curve(
    forecast: &[solar::ForecastPoint],
    calibration_k: f64,
    today: chrono::NaiveDate,
) -> Option<[f64; devices::mystrom_switch::STEPS_PER_DAY]> {
    let mut production_w = [0.0; devices::mystrom_switch::STEPS_PER_DAY];
    let mut forecast_steps = 0usize;
    for point in forecast {
        let local = point.valid_at.with_timezone(&chrono::Local);
        if local.date_naive() != today {
            continue;
        }
        let step = local.hour() as usize * 4 + local.minute() as usize / 15;
        if let Some(slot) = production_w.get_mut(step) {
            *slot = solar::predict_wh(point.gti_w_m2, point.temperature_c, calibration_k)
                * solar::STEPS_PER_HOUR;
            forecast_steps += 1;
        }
    }
    (forecast_steps >= ECO_MIN_FORECAST_STEPS).then_some(production_w)
}

/// What the switch typically draws while running, from its own recorded history.
///
/// Averaged over its historical on-window where there is data for it, falling
/// back to its overall average — a switch just moved into Eco from a differently
/// timed Time-mode schedule still has *some* history to start from. `None` when
/// that does not come out above zero, which is not a load worth planning around.
///
/// Written as `> 0.0` rather than the inlined version's `<= 0.0`, so an empty
/// curve — whose fallback average is `0.0 / 0.0` — is rejected instead of
/// carrying a NaN into `choose_window`. `baseline_from_curves` already rules
/// that input out in `compute_eco_plan`; this makes it safe here regardless.
fn typical_running_power(
    switch_curve: &HashMap<usize, f64>,
    on_step: usize,
    duration_steps: usize,
) -> Option<f64> {
    use devices::mystrom_switch::STEPS_PER_DAY;

    let in_window: Vec<f64> = (0..duration_steps)
        .filter_map(|i| switch_curve.get(&((on_step + i) % STEPS_PER_DAY)).copied())
        .collect();
    let typical_power_w = if in_window.is_empty() {
        switch_curve.values().sum::<f64>() / switch_curve.len() as f64
    } else {
        in_window.iter().sum::<f64>() / in_window.len() as f64
    };
    (typical_power_w > 0.0).then_some(typical_power_w)
}

/// The instants a window starting at `start` and running `duration_steps` opens
/// and closes, rolling into tomorrow when it runs past midnight.
fn window_instants(
    today: chrono::NaiveDate,
    start: usize,
    duration_steps: usize,
) -> Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)> {
    use devices::mystrom_switch::STEPS_PER_DAY;

    let on_at = datetime_from_step(today, start)?;
    let off_step_abs = start + duration_steps;
    let off_day = if off_step_abs >= STEPS_PER_DAY {
        today.succ_opt()?
    } else {
        today
    };
    let off_at = datetime_from_step(off_day, off_step_abs % STEPS_PER_DAY)?;
    Some((on_at, off_at))
}

/// Computes today's Eco-mode plan for one myStrom switch, or `None` if there
/// is not yet enough to plan from: no solar calibration, too little of today's
/// forecast, or too little recorded history for the switch's own draw and the
/// house around it. Called once per local day per Eco-mode device by
/// `eco_job`, which answers `None` with `timer_fallback_plan`.
///
/// `on_hhmm`/`off_hhmm` are the device's own configured timer pair — Eco mode
/// reads only their *length* (`duration_steps_from_timers`), not their clock
/// time, which is what it replaces each day. See
/// `devices::mystrom_switch`'s "Eco mode" section for the algorithm.
async fn compute_eco_plan(
    pool: &SqlitePool,
    switch_device_id: i64,
    on_hhmm: &str,
    off_hhmm: &str,
    today: chrono::NaiveDate,
) -> Option<app::EcoPlan> {
    use devices::mystrom_switch::{duration_steps_from_timers, step_of_hhmm};

    let duration_steps = duration_steps_from_timers(on_hhmm, off_hhmm)?;
    let on_step = step_of_hhmm(on_hhmm)?;

    let calibration = db::get_calibration(pool).await.ok().flatten()?;

    let forecast = db::query_forecast(pool, today, today).await.ok()?;
    let production_w = production_curve(&forecast, calibration.k, today)?;

    let house_id: Option<i64> =
        sqlx::query_scalar("SELECT id FROM Devices WHERE type = 'sonnen_eco8' LIMIT 1")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let house_id = house_id?;

    let switch_curve = db::query_avg_power_by_local_quarter_hour(
        pool,
        switch_device_id,
        "power",
        today,
        ECO_HISTORY_DAYS,
    )
    .await
    .ok()?;
    let house_curve = db::query_avg_power_by_local_quarter_hour(
        pool,
        house_id,
        "consumption",
        today,
        ECO_HISTORY_DAYS,
    )
    .await
    .ok()?;

    // The single gate on "is there enough history to plan against". It also
    // guarantees `switch_curve` is non-empty below, since it needs steps from
    // both curves to count one as known.
    let baseline_w = devices::mystrom_switch::baseline_from_curves(&house_curve, &switch_curve)?;

    let typical_power_w = typical_running_power(&switch_curve, on_step, duration_steps)?;

    let (start, predicted_grid_wh) = devices::mystrom_switch::choose_window(
        duration_steps,
        typical_power_w,
        &production_w,
        &baseline_w,
    )?;

    let (on_at, off_at) = window_instants(today, start, duration_steps)?;

    Some(app::EcoPlan {
        date: today,
        on_at,
        off_at,
        predicted_grid_wh: Some(predicted_grid_wh),
        on_fired: false,
        off_fired: false,
    })
}

/// Today's plan for a switch whose day Eco could not score: its own configured
/// timer pair, at the clock times the user set, fired through the same path an
/// Eco plan is.
///
/// Eco replaces the timer's *placement*, and it can only do that on a day it
/// can actually predict. Without a forecast or enough history it has no more
/// idea when the sun will shine than the timer does — and the timer at least
/// carries the user's own judgement about when the tank should be hot. Leaving
/// the relay untouched instead, which is what this used to do, is the one
/// option that serves nobody: the water goes cold for a reason nothing in the
/// house can act on.
///
/// This is not the "night fallback" SPECS.md rejected. That one would have
/// invented its own clock window out of a tariff signal that does not exist
/// here; this one defers to a schedule the user configured and can see.
fn timer_fallback_plan(
    on_hhmm: &str,
    off_hhmm: &str,
    today: chrono::NaiveDate,
) -> Option<app::EcoPlan> {
    use devices::mystrom_switch::{STEPS_PER_DAY, duration_steps_from_timers, step_of_hhmm};

    let on_step = step_of_hhmm(on_hhmm)?;
    let duration_steps = duration_steps_from_timers(on_hhmm, off_hhmm)?;

    let on_at = datetime_from_step(today, on_step)?;
    let off_step_abs = on_step + duration_steps;
    let off_day = if off_step_abs >= STEPS_PER_DAY {
        today.succ_opt()?
    } else {
        today
    };
    let off_at = datetime_from_step(off_day, off_step_abs % STEPS_PER_DAY)?;

    Some(app::EcoPlan {
        date: today,
        on_at,
        off_at,
        predicted_grid_wh: None,
        on_fired: false,
        off_fired: false,
    })
}

/// Sends the on/off command for one leg of an Eco plan, and records the
/// result — mirroring `timer_job`'s retry-then-give-up handling (see
/// `TIMER_MAX_ATTEMPTS`) so a switch that is briefly unreachable is retried
/// rather than skipped, but one that is simply off at the wall does not get
/// hammered for the rest of the day.
async fn fire_eco_leg(
    state: &SharedState,
    ip: IpAddr,
    relay_on: bool,
    attempts: &mut HashMap<IpAddr, u32>,
) {
    let leg = if relay_on { "on" } else { "off" };
    match devices::mystrom_switch::set_relay(ip, devices::mystrom_switch::API_PORT, relay_on).await
    {
        Ok(()) => {
            log::info!("eco {ip} switched {leg}");
            attempts.remove(&ip);
            let mut app = state.write().unwrap();
            if let Some(plan) = app.switch_eco_plans.get_mut(&ip) {
                if relay_on {
                    plan.on_fired = true;
                } else {
                    plan.off_fired = true;
                }
            }
        }
        Err(e) => {
            let count = attempts.entry(ip).or_insert(0);
            *count += 1;
            if *count >= TIMER_MAX_ATTEMPTS {
                log::warn!(
                    "eco could not switch {ip} {leg} after {count} attempts ({e:#}); giving up for today"
                );
                attempts.remove(&ip);
                let mut app = state.write().unwrap();
                if let Some(plan) = app.switch_eco_plans.get_mut(&ip) {
                    if relay_on {
                        plan.on_fired = true;
                    } else {
                        plan.off_fired = true;
                    }
                }
            } else {
                log::warn!("eco could not switch {ip} {leg}: {e:#}; attempt {count}, will retry");
            }
        }
    }
}

/// Background task: for every myStrom switch in Eco mode, keeps a plan for
/// today (recomputing it once the local day rolls over, or once one exists
/// at all — see `compute_eco_plan`) and fires its on/off instants as they
/// come due.
///
/// Unlike `timer_job`'s sweep, a plan has exactly one on and one off instant,
/// so there is no minute-window to catch: "now is at or past `on_at` and it
/// has not fired yet" is its own catch-up, self-healing after any gap.
/// `MAX_TIMER_CATCHUP_MINUTES` (shared with `timer_job`) only bounds how late
/// a catch-up is still worth attempting, so a multi-hour outage does not fire
/// an hours-old "on" with almost none of its window left.
/// The Eco-mode switches with a usable timer pair, and the pair that gives each
/// one's window its length — see `compute_eco_plan`.
///
/// If a switch has more than one timer of either direction, the first found
/// (earliest, since `switch_timers` is loaded in time order) is the one read:
/// Eco plans exactly one window a day, so a device with several pairs configured
/// only lends its first pair's length. A switch missing either direction is not
/// a candidate at all — there is no duration to plan against.
fn eco_candidates(app: &app::App) -> Vec<(IpAddr, String, String)> {
    app.switch_auto_modes
        .iter()
        .filter(|(_, mode)| matches!(mode, SwitchAutoMode::Eco))
        .filter_map(|(ip, _)| {
            let timers = app.switch_timers.get(ip)?;
            let on = timers.iter().find(|t| t.relay_on)?;
            let off = timers.iter().find(|t| !t.relay_on)?;
            Some((*ip, on.time_hhmm.clone(), off.time_hhmm.clone()))
        })
        .collect()
}

/// Whether one leg of a plan should fire now: it has not fired yet, its time has
/// come, and it came recently enough to still be worth acting on.
///
/// The upper bound is what stops a laptop resumed in the evening from switching
/// a tank on for a window that ended hours ago.
fn leg_is_due(
    fired: bool,
    at: chrono::DateTime<Utc>,
    now: chrono::DateTime<Utc>,
    catchup: chrono::Duration,
) -> bool {
    !fired && now >= at && now - at <= catchup
}

async fn eco_job(state: SharedState, pool: SqlitePool) {
    let mut ticker = tokio::time::interval(Duration::from_secs(TIMER_TICK_SECS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut on_attempts: HashMap<IpAddr, u32> = HashMap::new();
    let mut off_attempts: HashMap<IpAddr, u32> = HashMap::new();

    loop {
        ticker.tick().await;
        let now = Utc::now();
        let today = now.with_timezone(&chrono::Local).date_naive();

        let candidates = eco_candidates(&state.read().unwrap());
        if candidates.is_empty() {
            continue;
        }

        let Ok(switches) = devices::mystrom_switch::load_all(&pool).await else {
            continue;
        };

        for (ip, on_hhmm, off_hhmm) in candidates {
            let Some(device_id) = switches.iter().find(|d| d.ip == ip).map(|d| d.id) else {
                continue;
            };

            let needs_plan = !matches!(
                state.read().unwrap().switch_eco_plans.get(&ip),
                Some(p) if p.date == today
            );
            if needs_plan {
                on_attempts.remove(&ip);
                off_attempts.remove(&ip);
                // A day Eco cannot score falls back to the device's own timer
                // pair rather than leaving the relay untouched — see
                // `timer_fallback_plan`. Either way the day now has a plan, so
                // `needs_plan` goes false and this is not recomputed until
                // tomorrow. That stickiness is what makes the fallback safe:
                // re-deciding mid-day would build a fresh plan with
                // `on_fired: false` and switch a tank that had already been
                // heated back on.
                let plan =
                    match compute_eco_plan(&pool, device_id, &on_hhmm, &off_hhmm, today).await {
                        Some(plan) => Some(plan),
                        None => timer_fallback_plan(&on_hhmm, &off_hhmm, today),
                    };
                let mut app = state.write().unwrap();
                match plan {
                    Some(plan) => {
                        app.switch_eco_plans.insert(ip, plan);
                    }
                    None => {
                        app.switch_eco_plans.remove(&ip);
                    }
                }
            }

            let Some(plan) = state.read().unwrap().switch_eco_plans.get(&ip).cloned() else {
                continue;
            };
            let catchup = chrono::Duration::minutes(MAX_TIMER_CATCHUP_MINUTES);

            if leg_is_due(plan.on_fired, plan.on_at, now, catchup) {
                fire_eco_leg(&state, ip, true, &mut on_attempts).await;
            }
            if leg_is_due(plan.off_fired, plan.off_at, now, catchup) {
                fire_eco_leg(&state, ip, false, &mut off_attempts).await;
            }
        }
    }
}

/// Whether a heartbeat reply's claimed identity should be trusted: always, when nothing is pinned
/// yet (`cluster_task` warns about that state once at startup); otherwise only when it matches.
fn identity_ok(pinned_peer_key: &Option<String>, replied_key: &str) -> bool {
    match pinned_peer_key {
        None => true,
        Some(pin) => pin == replied_key,
    }
}

/// The `PeerStatus` for one failed-or-untrusted heartbeat attempt — mirrors
/// `devices::ConnStatus::Connecting`'s reasoning: report the last known claim until
/// `CLUSTER_PEER_LOST_AFTER_FAILURES` consecutive attempts have failed, rather than flipping
/// straight to `Unreachable` on a single dropped or mismatched heartbeat.
fn degrade(
    consecutive_failures: u32,
    ever_reachable: bool,
    last_claims_active: bool,
) -> cluster::PeerStatus {
    if consecutive_failures >= CLUSTER_PEER_LOST_AFTER_FAILURES {
        cluster::PeerStatus::Unreachable
    } else if ever_reachable {
        cluster::PeerStatus::Reachable {
            claims_active: last_claims_active,
        }
    } else {
        cluster::PeerStatus::Establishing
    }
}

/// Background task: decides and maintains this node's cluster role — see
/// `cluster::decide_role` and SPECS.md, "High availability: a two-node
/// active/standby cluster", for the design.
///
/// With no peer configured (`peer_addr` is `None`, true of every existing
/// single-node install), this runs exactly as before: 30s tick,
/// `PeerStatus::NotPaired`, always `Active`, nothing observable beyond the
/// one startup leadership epoch. A configured peer switches this to a faster
/// heartbeat cadence (`CLUSTER_HEARTBEAT_INTERVAL_SECS`) and drives
/// `peer`/`self_is_designated_primary` from real state — see `heartbeat_once`
/// for the wire exchange and `db::cluster_is_primary` for the designation.
///
/// `peer_addr` is the *starting* value, read once in `main()` because
/// `state.cluster_role` has to be seeded from the same value *before*
/// `cluster_heartbeat_listener_task` starts answering connections (see
/// `App::cluster_role`'s doc) — but pairing/re-pairing now happens live (a
/// TUI keypress, or `DOM_CLUSTER_AUTO_PAIR`), so every tick re-reads
/// `cluster_peer_addr`/`cluster_peer_pubkey`/`cluster_is_primary` from
/// `Config` rather than trusting the value this task was handed at startup —
/// otherwise a node paired after boot would never notice and would sit in
/// `NotPaired` forever until restarted. An address change specifically (not
/// just a pin update at the same address) resets this task's local tracking
/// to the same cold-start-safe values a process boot would use — the peer
/// relationship is effectively new, and carrying over a stale `role`/
/// `currently_active` from whatever this node was doing before pairing
/// existed would reopen the exact cold-start hazard `PeerStatus::Establishing`
/// exists to close.
///
/// Gateway reachability reuses `App::network_status` (already computed for
/// the Network view) against whichever device `classify_network_devices`
/// names as the router, rather than adding a second ping mechanism.
/// The role a node takes before it has heard anything from a peer.
///
/// A configured peer starts it at `Standby` rather than `Role::default()`
/// (`Active`): this is the other half of the cold-start fix alongside
/// `PeerStatus::Establishing`. `currently_active` must start honest, not
/// optimistic, or two nodes booting together could both feed `true` into
/// `decide_role` before either has heard from the other. See SPECS.md.
///
/// `main` seeds `App::cluster_role` with this synchronously, before either
/// cluster task spawns, so the heartbeat listener can never answer from
/// `Active` for a paired node — a peer heartbeating in that window would read a
/// false `claims_active: true` and could wrongly demote itself.
fn initial_cluster_role(paired: bool) -> cluster::Role {
    if paired {
        cluster::Role::Standby
    } else {
        cluster::Role::Active
    }
}

/// Everything this node has learned about its current pairing.
///
/// Grouped because they are reset together: when the configured peer changes,
/// none of it describes the new machine, and unpicking the fields one at a time
/// is how one gets forgotten.
struct PeerLink {
    role: cluster::Role,
    /// Forces the very first tick to log and act even when `decide_role`'s
    /// answer matches `role`'s initial value — a solo node starting and staying
    /// `Active`, say. Distinct from "has this node ever opened a leadership
    /// epoch", which for a node that starts and stays `Standby` would never
    /// become true and would re-log every tick forever.
    started: bool,
    consecutive_failures: u32,
    ever_reachable: bool,
    last_claims_active: bool,
}

impl PeerLink {
    /// A link that has learned nothing yet — see `initial_cluster_role` for why
    /// a paired node starts at `Standby`.
    fn new(paired: bool) -> Self {
        Self {
            role: initial_cluster_role(paired),
            started: false,
            consecutive_failures: 0,
            ever_reachable: false,
            last_claims_active: false,
        }
    }

    /// How an unanswered — or wrongly answered — heartbeat leaves the peer.
    fn degraded_status(&self) -> cluster::PeerStatus {
        degrade(
            self.consecutive_failures,
            self.ever_reachable,
            self.last_claims_active,
        )
    }
}

/// Whether this node can still reach the gateway, which is what tells an
/// `Active` node it is on the useful side of a network split.
///
/// The first router in the classified topology is the one asked; `Slow` counts
/// as reachable, since a slow answer is still an answer.
fn gateway_is_reachable(app: &app::App) -> bool {
    app::classify_network_devices(&app.devices)
        .routers
        .first()
        .is_some_and(|ip| {
            matches!(
                app.network_status(*ip),
                app::NetworkDeviceStatus::Ok | app::NetworkDeviceStatus::Slow
            )
        })
}

async fn cluster_task(
    node_id: String,
    mut peer_addr: Option<String>,
    state: SharedState,
    pool: SqlitePool,
) {
    use alarm::AlarmSink;

    // Both assigned unconditionally at the top of every loop iteration below, not just once here
    // — see this function's doc for why pairing can no longer be treated as fixed for the task's
    // whole lifetime.
    let mut self_is_designated_primary;
    let mut pinned_peer_key: Option<String>;
    // Logged once, the first tick a peer address exists without a pinned identity — not on every
    // tick, which would just be noise for a state that (once it's true) tends to stay true until
    // a human pairs. See `db::cluster_peer_pubkey`'s doc for why this fallback exists at all.
    let mut warned_no_pin = false;

    let alarm_sink = alarm::LogAlarm;
    let mut alarmed = false;
    // A configured peer starts this node at `Standby`, not `Role::default()`
    // (`Active`) — this is the other half of the cold-start fix alongside
    // `PeerStatus::Establishing`: `currently_active` must start honest, not
    // optimistic, or two nodes booting together could both feed `true` into
    // `decide_role` before either has heard from the other. See SPECS.md.
    let mut link = PeerLink::new(peer_addr.is_some());

    let tick_secs = |paired: bool| {
        Duration::from_secs(if paired {
            CLUSTER_HEARTBEAT_INTERVAL_SECS
        } else {
            TIMER_TICK_SECS
        })
    };
    let mut ticker = tokio::time::interval(tick_secs(peer_addr.is_some()));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        let fresh_peer_addr = db::cluster_peer_addr(&pool).await.unwrap_or(None);
        if fresh_peer_addr != peer_addr {
            log::info!(
                "cluster: peer configuration changed ({peer_addr:?} -> {fresh_peer_addr:?})"
            );
            peer_addr = fresh_peer_addr;
            // Everything learned about the old peer is about a different
            // machine now, so the whole link starts again rather than being
            // unpicked field by field.
            link = PeerLink::new(peer_addr.is_some());
            state.write().unwrap().cluster_role = link.role;
            ticker = tokio::time::interval(tick_secs(peer_addr.is_some()));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        }
        self_is_designated_primary = match &peer_addr {
            Some(_) => db::cluster_is_primary(&pool).await.unwrap_or(false),
            None => true,
        };
        pinned_peer_key = match &peer_addr {
            Some(_) => db::cluster_peer_pubkey(&pool).await.unwrap_or(None),
            None => None,
        };
        if peer_addr.is_some() && pinned_peer_key.is_none() && !warned_no_pin {
            warned_no_pin = true;
            log::warn!(
                "cluster: a peer address is configured but no identity is pinned — heartbeats \
                 will trust whoever answers at that address. Pair via the TUI to pin an identity."
            );
        } else if pinned_peer_key.is_some() {
            warned_no_pin = false;
        }

        let currently_active = matches!(link.role, cluster::Role::Active);
        let gateway_reachable = gateway_is_reachable(&state.read().unwrap());

        let peer = match &peer_addr {
            None => cluster::PeerStatus::NotPaired,
            Some(addr) => match heartbeat_once(addr, &node_id).await {
                Ok(reply) if identity_ok(&pinned_peer_key, &reply.public_key) => {
                    log::debug!(
                        "cluster: heartbeat to {addr} ok, peer {} claims_active={}",
                        reply.node_id,
                        reply.claims_active
                    );
                    link.consecutive_failures = 0;
                    link.ever_reachable = true;
                    link.last_claims_active = reply.claims_active;
                    cluster::PeerStatus::Reachable {
                        claims_active: link.last_claims_active,
                    }
                }
                Ok(reply) => {
                    // Internally valid, signed reply — just not from the key that's pinned.
                    // Never trust its claim, but this is a distinct, actionable situation from an
                    // ordinary network failure (most plausibly: the peer was reinstalled and
                    // generated a new keypair), so it gets its own alarm and a re-pair prompt
                    // rather than silently degrading like a dropped connection would.
                    link.consecutive_failures += 1;
                    log::warn!(
                        "cluster: heartbeat to {addr} answered by an identity that doesn't match \
                         the pin (got {}) — treating as unreachable until re-paired",
                        reply.public_key
                    );
                    alarm_sink.raise(cluster::AlarmCondition::PeerIdentityMismatch);
                    state.write().unwrap().cluster_pairing_prompt =
                        Some(app::ClusterPairingPrompt {
                            addr: addr.clone(),
                            node_id: reply.node_id,
                            public_key: reply.public_key,
                            replaces_pin: true,
                        });
                    link.degraded_status()
                }
                Err(e) => {
                    link.consecutive_failures += 1;
                    log::debug!(
                        "cluster: heartbeat to {addr} failed ({} in a row): {e:#}",
                        link.consecutive_failures
                    );
                    link.degraded_status()
                }
            },
        };

        let decision = cluster::decide_role(
            currently_active,
            self_is_designated_primary,
            gateway_reachable,
            peer,
        );

        if let Some(condition) = decision.alarm {
            alarm_sink.raise(condition);
            alarmed = true;
        } else if alarmed {
            alarm_sink.clear();
            alarmed = false;
        }

        if decision.role != link.role || !link.started {
            link.started = true;
            log::info!("cluster: role is now {:?}", decision.role);
            link.role = decision.role;
            state.write().unwrap().cluster_role = link.role;
            if link.role == cluster::Role::Active
                && let Err(e) = db::open_leadership_epoch(&pool, &node_id, Utc::now()).await
            {
                log::warn!("cluster: could not record a leadership epoch: {e:#}");
            }
        }
    }
}

/// One heartbeat round: connects to `addr`, sends a fresh-nonce request, and returns the peer's
/// signed reply — used both for an ongoing paired heartbeat and for an unpaired discovery probe
/// (`cluster_discovery_task`); the only difference is what the caller does with the identity in
/// the result. Verifies the reply is internally consistent (`cluster::verify_reply`) and actually
/// answers *this* request (`request_nonce` matches the nonce just sent — without that check, a
/// captured old reply could be replayed forever to mask a dead peer, which would be worse than no
/// authentication at all) before returning it; a caller that needs the identity to match a pin
/// still has to check that itself, since this function has no pin to check against for a
/// discovery probe. Every I/O step is individually timeout-bound (`CLUSTER_HEARTBEAT_TIMEOUT`) so
/// one unresponsive peer cannot stall a whole tick.
async fn heartbeat_once(addr: &str, node_id: &str) -> anyhow::Result<cluster::HeartbeatReply> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let stream = tokio::time::timeout(
        CLUSTER_HEARTBEAT_TIMEOUT,
        tokio::net::TcpStream::connect(addr),
    )
    .await??;

    let nonce = cluster::random_nonce_hex();
    let mut outgoing = serde_json::to_string(&cluster::HeartbeatRequest {
        node_id: node_id.to_string(),
        nonce: nonce.clone(),
    })?;
    outgoing.push('\n');

    let mut reader = BufReader::new(stream);
    tokio::time::timeout(
        CLUSTER_HEARTBEAT_TIMEOUT,
        reader.get_mut().write_all(outgoing.as_bytes()),
    )
    .await??;

    let mut line = String::new();
    tokio::time::timeout(CLUSTER_HEARTBEAT_TIMEOUT, reader.read_line(&mut line)).await??;

    let reply: cluster::HeartbeatReply = serde_json::from_str(&line)?;
    anyhow::ensure!(
        reply.request_nonce == nonce,
        "reply did not echo this request's nonce"
    );
    anyhow::ensure!(
        cluster::verify_reply(&reply),
        "reply signature did not verify"
    );
    Ok(reply)
}

/// Background task: answers heartbeat connections — from a paired peer, or from any node running
/// `cluster_discovery_task`'s probe — with this node's signed identity and current role claim.
/// Always spawned, even before any peer is configured: a probe that arrives before pairing is
/// mutual still gets a truthful, self-signed (but not yet *trusted* by anyone) answer, which is
/// exactly what makes discovery possible in the first place. See `cluster_task`/
/// `cluster_discovery_task` for the initiating half, and `cluster::verify_reply`'s doc for why an
/// unpinned reply proves less than it might look like it does.
async fn cluster_heartbeat_listener_task(
    node_id: String,
    keypair: ring::signature::Ed25519KeyPair,
    state: SharedState,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", CLUSTER_HEARTBEAT_PORT)).await {
        Ok(l) => l,
        Err(e) => {
            log::warn!(
                "cluster: could not bind the heartbeat port ({CLUSTER_HEARTBEAT_PORT}), a \
                 configured peer will always see this node as unreachable: {e:#}"
            );
            return;
        }
    };
    let keypair = std::sync::Arc::new(keypair);

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                log::debug!("cluster: heartbeat accept failed: {e:#}");
                continue;
            }
        };
        let node_id = node_id.clone();
        let keypair = keypair.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            if tokio::time::timeout(CLUSTER_HEARTBEAT_TIMEOUT, reader.read_line(&mut line))
                .await
                .is_err()
            {
                return;
            }
            let Ok(request) = serde_json::from_str::<cluster::HeartbeatRequest>(&line) else {
                return;
            };
            let claims_active = state.read().unwrap().cluster_role == cluster::Role::Active;
            let reply = cluster::sign_reply(&keypair, &node_id, claims_active, &request.nonce);
            let Ok(mut reply_line) = serde_json::to_string(&reply) else {
                return;
            };
            reply_line.push('\n');
            let _ = tokio::time::timeout(
                CLUSTER_HEARTBEAT_TIMEOUT,
                reader.get_mut().write_all(reply_line.as_bytes()),
            )
            .await;
        });
    }
}

/// Background task: looks for a Dom peer on the LAN by probing every candidate IP the existing
/// ping-scan/DHCP-lease discovery already gathers — the same candidate source `discovery_task`
/// uses, reused rather than adding a second discovery mechanism (SPECS.md explains why mDNS was
/// rejected for this). Not folded into `discovery_task`/`fingerprint.rs`/`devices::detect_type`:
/// a peer isn't a sensor to add to `Devices` and poll, it's a cluster relationship that needs a
/// human's explicit confirmation before anything trusts it (`App::cluster_pairing_prompt`) —
/// except in the Docker test harness (`DOM_CLUSTER_AUTO_PAIR=true`), which has no TTY to confirm
/// anything with.
///
/// A probe reply means one of two things: an identity that isn't the pinned one (or nothing is
/// pinned yet) becomes a pairing prompt; the *already-pinned* identity answering at a new address
/// updates `cluster_peer_addr` on its own, no human involved — the pinned key, not the address,
/// is the trust anchor, exactly like existing MAC-based device IP-migration
/// (`db::upsert_device`).
async fn cluster_discovery_task(
    node_id: String,
    state: SharedState,
    pool: SqlitePool,
    rescan: Arc<Notify>,
) {
    let auto_pair = std::env::var("DOM_CLUSTER_AUTO_PAIR").as_deref() == Ok("true");

    let mut ticker = tokio::time::interval(Duration::from_secs(DISCOVERY_INTERVAL_SECS));
    // Burst, matching discovery_task: if a probe round takes longer than the interval, the
    // scheduler catches up with one extra run rather than skipping it — a missed candidate
    // is a missed pairing opportunity, not just a stale reading.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);

    loop {
        // Also wakes on a manual rescan ('s'), same as discovery_task — a person triggering a
        // rescan is very plausibly doing it because they just brought the peer online.
        tokio::select! {
            _ = ticker.tick() => {}
            _ = rescan.notified() => {}
        }

        let paired_addr = db::cluster_peer_addr(&pool).await.unwrap_or(None);
        let pinned_key = db::cluster_peer_pubkey(&pool).await.unwrap_or(None);

        let devs: Vec<devices::Device> = {
            let app = state.read().unwrap();
            app.last_ping_devices.clone()
        };
        let lease_ips = lease_addresses(&state.read().unwrap());
        let candidates = merge_scan_targets(&devs, lease_ips);

        let mut set = tokio::task::JoinSet::new();
        for ip in candidates {
            let addr = format!("{ip}:{CLUSTER_HEARTBEAT_PORT}");
            if Some(&addr) == paired_addr.as_ref() {
                // Already covered by cluster_task's own heartbeat every tick — no reason to
                // double-probe the peer's listener from two tasks every cycle.
                continue;
            }
            let node_id = node_id.clone();
            set.spawn(async move { (addr.clone(), heartbeat_once(&addr, &node_id).await) });
        }

        while let Some(res) = set.join_next().await {
            let Ok((addr, Ok(reply))) = res else {
                continue;
            };
            if reply.node_id == node_id {
                // This node's own IP was among the candidates — not a peer.
                continue;
            }

            if pinned_key.as_deref() == Some(reply.public_key.as_str()) {
                if paired_addr.as_deref() != Some(addr.as_str()) {
                    log::info!("cluster: peer's address changed to {addr}");
                    if let Err(e) = db::set_cluster_peer_addr(&pool, &addr).await {
                        log::warn!("cluster: could not record the peer's new address: {e:#}");
                    }
                }
                continue;
            }

            if auto_pair {
                log::info!(
                    "cluster: DOM_CLUSTER_AUTO_PAIR set — pinning discovered peer at {addr} \
                     automatically (test harness only; a real install confirms in the TUI)"
                );
                if let Err(e) = db::set_cluster_peer_addr(&pool, &addr).await {
                    log::warn!("cluster: could not record the discovered peer's address: {e:#}");
                }
                if let Err(e) = db::set_cluster_peer_pubkey(&pool, &reply.public_key).await {
                    log::warn!("cluster: could not pin the discovered peer's identity: {e:#}");
                }
            } else {
                log::info!(
                    "cluster: found a candidate peer at {addr} — awaiting pairing confirmation"
                );
                state.write().unwrap().cluster_pairing_prompt = Some(app::ClusterPairingPrompt {
                    addr,
                    node_id: reply.node_id,
                    public_key: reply.public_key,
                    replaces_pin: pinned_key.is_some(),
                });
            }
        }
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

/// A fingerprint carried over from an earlier discovery cycle.
struct CachedFingerprint {
    fp: fingerprint::Fingerprint,
    /// The MAC at that address when the fingerprint was taken. Identity is the
    /// MAC rather than the address (see `db::upsert_device`), so a different one
    /// now means a different device holds the address and this entry describes
    /// something that is no longer there.
    mac: Option<String>,
    at: Instant,
}

/// The MAC the kernel's ARP cache holds for an address, if any.
///
/// IPv6 has no entry there, so those are always `None` — the same rule
/// `discovery_task` applies when it stamps identity onto a discovered device.
fn mac_at(ip: IpAddr, macs: &HashMap<Ipv4Addr, String>) -> Option<String> {
    match ip {
        IpAddr::V4(v4) => macs.get(&v4).cloned(),
        IpAddr::V6(_) => None,
    }
}

/// Split discovery's targets into the fingerprints that can be reused as they
/// are and the addresses that have to go back on the wire.
///
/// An entry is reused only while it is both recent and still about the same
/// device: taken within `ttl`, and with the MAC at that address unchanged
/// since. Anything else — never seen, aged out, or a different device now
/// answering at a familiar address — is probed again.
fn plan_fingerprints(
    targets: &[IpAddr],
    cache: &HashMap<IpAddr, CachedFingerprint>,
    macs: &HashMap<Ipv4Addr, String>,
    ttl: Duration,
    now: Instant,
) -> (Vec<fingerprint::Fingerprint>, Vec<IpAddr>) {
    let mut reused = Vec::new();
    let mut probe = Vec::new();
    for &ip in targets {
        match cache.get(&ip) {
            Some(entry) if now.duration_since(entry.at) < ttl && entry.mac == mac_at(ip, macs) => {
                reused.push(entry.fp.clone());
            }
            _ => probe.push(ip),
        }
    }
    (reused, probe)
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

    /// The local day the fallback tests place their plans in.
    fn a_day() -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2026, 3, 14).unwrap()
    }

    fn local_hhmm(t: chrono::DateTime<Utc>) -> String {
        t.with_timezone(&chrono::Local).format("%H:%M").to_string()
    }

    // ── Cluster: starting state and gateway reachability ──────────────────────

    #[test]
    fn a_paired_node_starts_on_standby_and_a_solo_one_active() {
        // A paired node must not start optimistic: two booting together would
        // both feed `claims_active: true` into the other's `decide_role`.
        assert_eq!(initial_cluster_role(true), cluster::Role::Standby);
        assert_eq!(initial_cluster_role(false), cluster::Role::Active);
    }

    #[test]
    fn a_fresh_link_has_learned_nothing() {
        let link = PeerLink::new(true);
        assert_eq!(link.role, cluster::Role::Standby);
        assert!(!link.started, "the first tick must still log and act");
        assert_eq!(link.consecutive_failures, 0);
        assert!(!link.ever_reachable);
        assert!(!link.last_claims_active);
    }

    #[test]
    fn a_peer_never_yet_heard_from_is_establishing_not_unreachable() {
        // The distinction is what stops a node promoting itself during the
        // seconds before the peer's listener is up.
        let mut link = PeerLink::new(true);
        link.consecutive_failures = 1;
        assert_eq!(link.degraded_status(), cluster::PeerStatus::Establishing);
    }

    #[test]
    fn a_peer_that_was_reachable_keeps_its_last_claim_while_it_is_missed() {
        let mut link = PeerLink::new(true);
        link.ever_reachable = true;
        link.last_claims_active = true;
        link.consecutive_failures = 1;
        assert_eq!(
            link.degraded_status(),
            cluster::PeerStatus::Reachable {
                claims_active: true
            },
            "one missed beat is not a lost peer"
        );
    }

    #[test]
    fn enough_missed_beats_make_a_peer_unreachable_however_well_it_was_known() {
        let mut link = PeerLink::new(true);
        link.ever_reachable = true;
        link.last_claims_active = true;
        link.consecutive_failures = CLUSTER_PEER_LOST_AFTER_FAILURES;
        assert_eq!(link.degraded_status(), cluster::PeerStatus::Unreachable);
    }

    /// An app holding one device labelled `label`, with the given poll state.
    fn app_with_router(label: &str, status: Option<(app::ConnStatus, f64)>) -> app::App {
        let mut app = app::App::default();
        let router = ip(1);
        app.devices.push(app::ScannedDevice {
            ip: router,
            latency_ms: 1.0,
            open_ports: vec![],
            name: None,
            label: Some(label.to_string()),
        });
        if let Some((conn, latency)) = status {
            app.conn_status.insert(router, conn);
            app.poll_latency_ms.insert(router, latency);
        }
        app
    }

    #[test]
    fn a_router_answering_normally_counts_as_a_reachable_gateway() {
        let app = app_with_router("router", Some((app::ConnStatus::Online, 10.0)));
        assert!(gateway_is_reachable(&app));
    }

    #[test]
    fn a_slow_router_is_still_a_reachable_gateway() {
        // A slow answer is still an answer; treating it as a split would demote
        // a node over nothing more than a loaded router.
        let app = app_with_router("router", Some((app::ConnStatus::Online, app::SLOW_POLL_MS)));
        assert!(gateway_is_reachable(&app));
    }

    #[test]
    fn a_lost_or_connecting_router_is_not_a_reachable_gateway() {
        for conn in [app::ConnStatus::Lost, app::ConnStatus::Connecting] {
            let app = app_with_router("router", Some((conn, 10.0)));
            assert!(!gateway_is_reachable(&app));
        }
    }

    #[test]
    fn a_router_nothing_has_polled_yet_is_not_a_reachable_gateway() {
        // `Unknown` is not evidence of reachability, and treating it as such
        // would make every startup look like a healthy gateway.
        let app = app_with_router("router", None);
        assert!(!gateway_is_reachable(&app));
    }

    #[test]
    fn a_node_that_knows_of_no_router_has_no_reachable_gateway() {
        assert!(!gateway_is_reachable(&app::App::default()));
        let app = app_with_router("kitchen lamp", Some((app::ConnStatus::Online, 10.0)));
        assert!(!gateway_is_reachable(&app), "not classified as a router");
    }

    // ── Reading the router's lease table ──────────────────────────────────────

    fn app_with_leases(leases: &[(&str, Option<&str>, Option<&str>)]) -> app::App {
        let mut app = app::App::default();
        app.mikrotik_readings.insert(
            IpAddr::V4(std::net::Ipv4Addr::new(172, 16, 0, 1)),
            app::MikrotikReading {
                leases: leases
                    .iter()
                    .map(|(addr, comment, host)| devices::mikrotik::DhcpLease {
                        address: (*addr).to_string(),
                        mac_address: "AA:BB:CC:DD:EE:FF".to_string(),
                        host_name: host.map(|h| h.to_string()),
                        comment: comment.map(|c| c.to_string()),
                        status: "bound".to_string(),
                    })
                    .collect(),
                firewall_rules: vec![],
                updated_at: Utc::now(),
            },
        );
        app
    }

    #[test]
    fn every_leased_address_is_worth_reaching_for() {
        let app = app_with_leases(&[("172.16.0.20", None, None), ("172.16.0.21", None, None)]);
        let mut got = lease_addresses(&app);
        got.sort_by_key(|ip| ip_sort_key(*ip));
        assert_eq!(
            got,
            vec![
                "172.16.0.20".parse::<IpAddr>().unwrap(),
                "172.16.0.21".parse::<IpAddr>().unwrap()
            ]
        );
    }

    #[test]
    fn a_lease_with_an_unparseable_address_is_skipped_not_fatal() {
        let app = app_with_leases(&[("not-an-address", None, None), ("172.16.0.20", None, None)]);
        assert_eq!(lease_addresses(&app).len(), 1);
    }

    #[test]
    fn a_lease_comment_is_preferred_over_the_host_name() {
        // The comment is what a person typed on the router; the host-name is
        // whatever the device called itself.
        let app = app_with_leases(&[("172.16.0.20", Some("Kitchen"), Some("esp-1a2b"))]);
        let got = dhcp_labels(&app);
        assert_eq!(
            got.get(&"172.16.0.20".parse().unwrap()).map(String::as_str),
            Some("Kitchen")
        );
    }

    #[test]
    fn the_host_name_is_used_when_no_comment_was_set() {
        let app = app_with_leases(&[("172.16.0.20", None, Some("esp-1a2b"))]);
        let got = dhcp_labels(&app);
        assert_eq!(
            got.get(&"172.16.0.20".parse().unwrap()).map(String::as_str),
            Some("esp-1a2b")
        );
    }

    #[test]
    fn a_blank_name_is_no_name_at_all() {
        // An empty comment would otherwise show as a device with a blank label,
        // which reads as a bug rather than as "unnamed".
        let app = app_with_leases(&[
            ("172.16.0.20", Some(""), None),
            ("172.16.0.21", None, Some("")),
            ("172.16.0.22", None, None),
        ]);
        assert!(dhcp_labels(&app).is_empty());
    }

    #[test]
    fn an_empty_comment_masks_the_host_name_rather_than_falling_through() {
        // `comment.or_else(host_name)` picks the comment because `Some("")` is
        // `Some`, and only then is the blank dropped — so a device whose lease
        // has an empty comment shows unnamed even though it gave a host-name.
        // Recorded as the behaviour it is; moving the emptiness check ahead of
        // the fallback would change what such devices are called.
        let app = app_with_leases(&[("172.16.0.20", Some(""), Some("esp-1a2b"))]);
        assert_eq!(dhcp_labels(&app).get(&"172.16.0.20".parse().unwrap()), None);
    }

    // ── Merging a scan into the device list ───────────────────────────────────

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::new(172, 16, 0, last))
    }

    fn fp_at(last: u8) -> fingerprint::Fingerprint {
        fingerprint::Fingerprint {
            ip: ip(last),
            open_ports: vec![80],
            http: vec![],
        }
    }

    fn seen(last: u8, label: Option<&str>) -> app::ScannedDevice {
        app::ScannedDevice {
            ip: ip(last),
            latency_ms: 5.0,
            open_ports: vec![],
            name: None,
            label: label.map(|l| l.to_string()),
        }
    }

    fn merged(
        fps: &[fingerprint::Fingerprint],
        existing: &[app::ScannedDevice],
        polled: &[u8],
    ) -> Vec<IpAddr> {
        let polled_ips: HashSet<IpAddr> = polled.iter().map(|l| ip(*l)).collect();
        merge_device_list(
            fps,
            &[],
            existing,
            &polled_ips,
            &[],
            &HashMap::new(),
            &HashMap::new(),
        )
        .into_iter()
        .map(|d| d.ip)
        .collect()
    }

    #[test]
    fn a_scan_that_missed_a_polled_device_does_not_drop_it() {
        // One unanswered probe must not make a device Dom is actively polling
        // disappear from the list for a cycle.
        assert_eq!(
            merged(&[fp_at(20)], &[seen(20, None), seen(30, None)], &[30]),
            vec![ip(20), ip(30)]
        );
    }

    #[test]
    fn a_scan_that_missed_a_labelled_device_does_not_drop_it_either() {
        // A label is a person's own assignment; losing it to a missed probe
        // would lose their work.
        assert_eq!(
            merged(
                &[fp_at(20)],
                &[seen(20, None), seen(30, Some("Kitchen"))],
                &[]
            ),
            vec![ip(20), ip(30)]
        );
    }

    #[test]
    fn an_unlabelled_device_that_is_not_polled_does_drop_out() {
        // Nothing is holding on to it, so a scan that no longer sees it is the
        // truth about the network.
        assert_eq!(
            merged(&[fp_at(20)], &[seen(20, None), seen(30, None)], &[]),
            vec![ip(20)]
        );
    }

    #[test]
    fn a_device_found_by_the_scan_is_not_added_twice() {
        assert_eq!(
            merged(&[fp_at(20)], &[seen(20, Some("Kitchen"))], &[20]),
            vec![ip(20)],
            "polled and labelled and scanned, but still one device"
        );
    }

    #[test]
    fn the_merged_list_comes_back_in_address_order() {
        let got = merged(&[fp_at(30), fp_at(9), fp_at(200)], &[], &[]);
        assert_eq!(
            got,
            vec![ip(9), ip(30), ip(200)],
            "numeric, not lexicographic"
        );
    }

    #[test]
    fn this_machines_own_addresses_are_added_once_and_named() {
        let local = [ip(50)];
        let devices_out = merge_device_list(
            &[fp_at(20)],
            &[],
            &[],
            &HashSet::new(),
            &local,
            &HashMap::new(),
            &HashMap::new(),
        );
        let dom: Vec<&app::ScannedDevice> = devices_out.iter().filter(|d| d.ip == ip(50)).collect();
        assert_eq!(dom.len(), 1);
        assert_eq!(dom[0].name, Some(devices::dom_local::NAME));
    }

    #[test]
    fn a_label_from_the_routers_lease_reaches_a_freshly_scanned_device() {
        let dhcp: HashMap<IpAddr, String> = [(ip(20), "Kitchen".to_string())].into_iter().collect();
        let devices_out = merge_device_list(
            &[fp_at(20)],
            &[],
            &[],
            &HashSet::new(),
            &[],
            &HashMap::new(),
            &dhcp,
        );
        assert_eq!(devices_out[0].label.as_deref(), Some("Kitchen"));
    }

    // ── Timer selection and retry bookkeeping ─────────────────────────────────

    /// An app with one Time-mode switch holding the given timers.
    fn timer_app(mode: SwitchAutoMode, timers: &[(i64, &str, bool)]) -> app::App {
        let mut app = app::App::default();
        let ip = IpAddr::V4(std::net::Ipv4Addr::new(172, 16, 0, 1));
        app.switch_auto_modes.insert(ip, mode);
        app.switch_timers.insert(
            ip,
            timers
                .iter()
                .map(|(id, hhmm, relay_on)| app::SwitchTimer {
                    id: *id,
                    time_hhmm: (*hhmm).to_string(),
                    relay_on: *relay_on,
                })
                .collect(),
        );
        app
    }

    fn win(minutes: &[&str]) -> Vec<String> {
        minutes.iter().map(|m| (*m).to_string()).collect()
    }

    const DAY: &str = "2026-08-03";

    #[test]
    fn a_timer_fires_when_its_minute_falls_in_the_swept_window() {
        let app = timer_app(SwitchAutoMode::Time, &[(1, "07:00", true)]);
        let got = timers_to_fire(
            &app,
            &win(&["07:01", "07:00"]),
            &HashSet::new(),
            &HashMap::new(),
            DAY,
        );
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].1, got[0].2), (1, true));
    }

    #[test]
    fn a_timer_outside_the_window_is_left_alone() {
        let app = timer_app(SwitchAutoMode::Time, &[(1, "07:00", true)]);
        assert!(
            timers_to_fire(
                &app,
                &win(&["09:00", "08:59"]),
                &HashSet::new(),
                &HashMap::new(),
                DAY
            )
            .is_empty()
        );
    }

    #[test]
    fn only_time_mode_switches_run_their_timers() {
        // Eco reads the same timers for its window's *length* only; it must not
        // fire them itself, or the day would be switched twice.
        for mode in [SwitchAutoMode::Eco, SwitchAutoMode::Disabled] {
            let app = timer_app(mode.clone(), &[(1, "07:00", true)]);
            assert!(
                timers_to_fire(
                    &app,
                    &win(&["07:00"]),
                    &HashSet::new(),
                    &HashMap::new(),
                    DAY
                )
                .is_empty()
            );
        }
    }

    #[test]
    fn a_timer_that_already_fired_today_does_not_fire_again() {
        let app = timer_app(SwitchAutoMode::Time, &[(1, "07:00", true)]);
        let fired: HashSet<TimerKey> = [(1, DAY.to_string())].into_iter().collect();
        assert!(timers_to_fire(&app, &win(&["07:00"]), &fired, &HashMap::new(), DAY).is_empty());
    }

    #[test]
    fn the_same_timer_is_due_again_the_next_day() {
        // The key carries the date, so yesterday's firing does not suppress it.
        let app = timer_app(SwitchAutoMode::Time, &[(1, "07:00", true)]);
        let fired: HashSet<TimerKey> = [(1, "2026-08-02".to_string())].into_iter().collect();
        assert_eq!(
            timers_to_fire(&app, &win(&["07:00"]), &fired, &HashMap::new(), DAY).len(),
            1
        );
    }

    #[test]
    fn a_timer_awaiting_a_retry_stays_due_after_its_window_has_passed() {
        // This is what makes a briefly unreachable switch recoverable: the
        // minute is long gone, but the command was never accepted.
        let app = timer_app(SwitchAutoMode::Time, &[(1, "07:00", true)]);
        let pending: HashMap<TimerKey, u32> = [((1, DAY.to_string()), 2)].into_iter().collect();
        assert_eq!(
            timers_to_fire(&app, &win(&["09:00"]), &HashSet::new(), &pending, DAY).len(),
            1,
            "still pending, so still due"
        );
    }

    #[test]
    fn every_due_timer_on_a_switch_is_returned() {
        let app = timer_app(
            SwitchAutoMode::Time,
            &[(1, "07:00", true), (2, "07:00", false), (3, "22:00", false)],
        );
        let got = timers_to_fire(
            &app,
            &win(&["07:00"]),
            &HashSet::new(),
            &HashMap::new(),
            DAY,
        );
        let mut ids: Vec<i64> = got.iter().map(|(_, id, _)| *id).collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn a_failed_attempt_is_counted_and_retried() {
        let mut fired = HashSet::new();
        let mut pending = HashMap::new();
        let key = (1i64, DAY.to_string());

        for expected in 1..TIMER_MAX_ATTEMPTS {
            assert_eq!(
                record_failed_attempt(&mut fired, &mut pending, key.clone()),
                (expected, false)
            );
            assert!(fired.is_empty(), "not given up on yet");
        }
        assert_eq!(pending.get(&key), Some(&(TIMER_MAX_ATTEMPTS - 1)));
    }

    #[test]
    fn a_switch_that_never_answers_is_given_up_on_for_the_day() {
        // Recorded as fired, which is not a claim that it ran — it stops Dom
        // talking to a device that is simply off, every tick, until midnight.
        let mut fired = HashSet::new();
        let mut pending = HashMap::new();
        let key = (1i64, DAY.to_string());

        let mut last = (0, false);
        for _ in 0..TIMER_MAX_ATTEMPTS {
            last = record_failed_attempt(&mut fired, &mut pending, key.clone());
        }
        assert_eq!(last, (TIMER_MAX_ATTEMPTS, true));
        assert!(
            fired.contains(&key),
            "recorded as fired so it is not retried"
        );
        assert!(!pending.contains_key(&key), "and no longer pending");
    }

    #[test]
    fn giving_up_on_one_timer_leaves_the_others_alone() {
        let mut fired = HashSet::new();
        let mut pending = HashMap::new();
        let dead = (1i64, DAY.to_string());
        let live = (2i64, DAY.to_string());

        for _ in 0..TIMER_MAX_ATTEMPTS {
            record_failed_attempt(&mut fired, &mut pending, dead.clone());
        }
        record_failed_attempt(&mut fired, &mut pending, live.clone());

        assert!(fired.contains(&dead) && !fired.contains(&live));
        assert_eq!(pending.get(&live), Some(&1));
    }

    // ── Eco planning: the pure pieces ─────────────────────────────────────────

    fn forecast_point(at: chrono::DateTime<chrono::Local>, gti: f64) -> solar::ForecastPoint {
        solar::ForecastPoint {
            valid_at: at.with_timezone(&Utc),
            gti_w_m2: gti,
            temperature_c: 20.0,
            cloud_cover_pct: 0.0,
            precipitation_mm: 0.0,
        }
    }

    /// A forecast covering `steps` quarter-hours from midnight local, all with
    /// the same irradiance.
    fn forecast_covering(
        day: chrono::NaiveDate,
        steps: usize,
        gti: f64,
    ) -> Vec<solar::ForecastPoint> {
        use chrono::TimeZone;
        (0..steps)
            .map(|i| {
                let naive = day
                    .and_hms_opt((i as u32 / 4) % 24, (i as u32 % 4) * 15, 0)
                    .unwrap();
                let local = chrono::Local
                    .from_local_datetime(&naive)
                    .earliest()
                    .unwrap();
                forecast_point(local, gti)
            })
            .collect()
    }

    #[test]
    fn a_forecast_covering_too_little_of_the_day_is_not_planned_against() {
        // Below the gate there is no solar signal to place a window against;
        // every unfilled step reads as zero production, which is indistinguishable
        // from an overcast day.
        let day = a_day();
        let thin = forecast_covering(day, ECO_MIN_FORECAST_STEPS - 1, 500.0);
        assert!(production_curve(&thin, 1.0, day).is_none());

        let enough = forecast_covering(day, ECO_MIN_FORECAST_STEPS, 500.0);
        assert!(production_curve(&enough, 1.0, day).is_some());
    }

    #[test]
    fn only_todays_forecast_points_count_towards_the_coverage_gate() {
        // `query_forecast` is asked for one day, but a point on either side of
        // the local midnight boundary can still come back; it must not be
        // counted as coverage of today.
        let day = a_day();
        let mut points = forecast_covering(day, ECO_MIN_FORECAST_STEPS - 1, 500.0);
        points.extend(forecast_covering(day.succ_opt().unwrap(), 8, 500.0));
        assert!(
            production_curve(&points, 1.0, day).is_none(),
            "tomorrow's points must not make today look covered"
        );
    }

    #[test]
    fn the_production_curve_lands_each_point_on_its_own_quarter_hour() {
        let day = a_day();
        let curve = production_curve(&forecast_covering(day, 96, 800.0), 1.0, day).unwrap();
        assert_eq!(curve.len(), devices::mystrom_switch::STEPS_PER_DAY);
        // Uniform irradiance in, so every step should hold the same figure, and
        // it should be a real one.
        assert!(curve[0] > 0.0);
        assert!(curve.iter().all(|w| (w - curve[0]).abs() < 1e-9));
    }

    #[test]
    fn a_dark_forecast_still_produces_a_curve_to_plan_against() {
        // All-zero production is a legitimate result — an overcast day — and is
        // deliberately distinct from "not enough forecast".
        let day = a_day();
        let curve = production_curve(&forecast_covering(day, 96, 0.0), 1.0, day).unwrap();
        assert!(curve.iter().all(|w| *w == 0.0));
    }

    #[test]
    fn typical_power_averages_the_switchs_own_on_window() {
        // Steps 8..12 draw 2000 W, the rest 100 W. A window over 8..12 should
        // read the former, not the blend.
        let mut curve: HashMap<usize, f64> = (0..96).map(|i| (i, 100.0)).collect();
        for i in 8..12 {
            curve.insert(i, 2000.0);
        }
        assert_eq!(typical_running_power(&curve, 8, 4), Some(2000.0));
    }

    #[test]
    fn typical_power_falls_back_to_the_overall_average_off_window() {
        // A switch just moved into Eco from a differently timed schedule has no
        // history in the new window, but still has history.
        let curve: HashMap<usize, f64> = [(40, 300.0), (41, 500.0)].into_iter().collect();
        assert_eq!(
            typical_running_power(&curve, 8, 4),
            Some(400.0),
            "no data in 8..12, so the whole curve's average"
        );
    }

    #[test]
    fn typical_power_wraps_a_window_that_runs_past_midnight() {
        let mut curve: HashMap<usize, f64> = (0..96).map(|i| (i, 100.0)).collect();
        curve.insert(95, 1000.0);
        curve.insert(0, 1000.0);
        assert_eq!(
            typical_running_power(&curve, 95, 2),
            Some(1000.0),
            "steps 95 and 0, not 95 and 96"
        );
    }

    #[test]
    fn a_switch_that_never_draws_anything_is_not_worth_planning_for() {
        let curve: HashMap<usize, f64> = (0..96).map(|i| (i, 0.0)).collect();
        assert_eq!(typical_running_power(&curve, 0, 4), None);
        // An empty curve averages to NaN; rejecting it here is what keeps that
        // out of `choose_window`. Unreachable via `compute_eco_plan`, which
        // gates on `baseline_from_curves` first.
        assert_eq!(typical_running_power(&HashMap::new(), 0, 4), None);
    }

    #[test]
    fn a_window_reads_back_as_the_wall_clock_times_it_covers() {
        let day = a_day();
        // Step 32 is 08:00; four steps is an hour.
        let (on, off) = window_instants(day, 32, 4).unwrap();
        assert_eq!(local_hhmm(on), "08:00");
        assert_eq!(local_hhmm(off), "09:00");
    }

    #[test]
    fn a_window_that_runs_past_midnight_closes_on_the_next_day() {
        let day = a_day();
        // Step 92 is 23:00; eight steps carries it two hours into tomorrow.
        let (on, off) = window_instants(day, 92, 8).unwrap();
        assert_eq!(local_hhmm(on), "23:00");
        assert_eq!(local_hhmm(off), "01:00");
        assert!(off > on, "the close must not land before the open");
        assert_eq!(
            (off - on),
            chrono::Duration::hours(2),
            "eight quarter-hours, across the boundary"
        );
    }

    // ── Eco candidates ────────────────────────────────────────────────────────

    /// One switch to seed the app with: its last IP octet, its auto mode (or
    /// none configured), and its timers as (time, turns-on).
    type EcoEntry<'a> = (u8, Option<SwitchAutoMode>, &'a [(&'a str, bool)]);

    fn eco_app(entries: &[EcoEntry]) -> app::App {
        let mut app = app::App::default();
        for (last_octet, mode, timers) in entries {
            let ip: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(172, 16, 0, *last_octet));
            if let Some(mode) = mode {
                app.switch_auto_modes.insert(ip, mode.clone());
            }
            app.switch_timers.insert(
                ip,
                timers
                    .iter()
                    .enumerate()
                    .map(|(i, (hhmm, relay_on))| app::SwitchTimer {
                        id: i as i64,
                        time_hhmm: (*hhmm).to_string(),
                        relay_on: *relay_on,
                    })
                    .collect(),
            );
        }
        app
    }

    #[test]
    fn only_eco_switches_with_a_complete_timer_pair_are_candidates() {
        let app = eco_app(&[
            (
                1,
                Some(SwitchAutoMode::Eco),
                &[("06:00", true), ("08:00", false)],
            ),
            (
                2,
                Some(SwitchAutoMode::Time),
                &[("06:00", true), ("08:00", false)],
            ),
            (3, Some(SwitchAutoMode::Eco), &[("06:00", true)]),
            (4, Some(SwitchAutoMode::Eco), &[("08:00", false)]),
            (5, None, &[("06:00", true), ("08:00", false)]),
        ]);
        let got = eco_candidates(&app);
        assert_eq!(got.len(), 1, "only .1 qualifies: {got:?}");
        assert_eq!(got[0].0, IpAddr::V4(std::net::Ipv4Addr::new(172, 16, 0, 1)));
        assert_eq!((got[0].1.as_str(), got[0].2.as_str()), ("06:00", "08:00"));
    }

    #[test]
    fn a_switch_with_several_pairs_lends_only_its_first() {
        // Eco plans exactly one window a day, so the extra pairs are ignored
        // rather than producing extra candidates.
        let app = eco_app(&[(
            1,
            Some(SwitchAutoMode::Eco),
            &[
                ("06:00", true),
                ("08:00", false),
                ("18:00", true),
                ("20:00", false),
            ],
        )]);
        let got = eco_candidates(&app);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].1.as_str(), got[0].2.as_str()), ("06:00", "08:00"));
    }

    #[test]
    fn no_eco_switches_means_no_candidates() {
        assert!(eco_candidates(&app::App::default()).is_empty());
    }

    // ── Firing a leg ──────────────────────────────────────────────────────────

    #[test]
    fn a_leg_fires_once_its_time_has_come_and_not_before() {
        let catchup = chrono::Duration::minutes(MAX_TIMER_CATCHUP_MINUTES);
        let at = at("07:00").with_timezone(&Utc);

        assert!(!leg_is_due(
            false,
            at,
            at - chrono::Duration::minutes(1),
            catchup
        ));
        assert!(leg_is_due(false, at, at, catchup));
        assert!(leg_is_due(
            false,
            at,
            at + chrono::Duration::minutes(5),
            catchup
        ));
    }

    #[test]
    fn a_leg_that_already_fired_does_not_fire_again() {
        let catchup = chrono::Duration::minutes(MAX_TIMER_CATCHUP_MINUTES);
        let at = at("07:00").with_timezone(&Utc);
        assert!(!leg_is_due(
            true,
            at,
            at + chrono::Duration::minutes(5),
            catchup
        ));
    }

    #[test]
    fn a_leg_missed_by_more_than_the_catchup_is_left_alone() {
        // Resuming a suspended machine in the evening must not switch a tank on
        // for a window that closed hours ago.
        let catchup = chrono::Duration::minutes(MAX_TIMER_CATCHUP_MINUTES);
        let at = at("07:00").with_timezone(&Utc);

        let just_inside = at + chrono::Duration::minutes(MAX_TIMER_CATCHUP_MINUTES);
        assert!(
            leg_is_due(false, at, just_inside, catchup),
            "the bound is inclusive"
        );

        let just_outside = just_inside + chrono::Duration::seconds(1);
        assert!(!leg_is_due(false, at, just_outside, catchup));
        assert!(!leg_is_due(
            false,
            at,
            at + chrono::Duration::hours(12),
            catchup
        ));
    }

    #[test]
    fn the_timer_fallback_runs_at_the_users_own_clock_times() {
        let plan = timer_fallback_plan("09:00", "18:00", a_day()).unwrap();
        assert_eq!(local_hhmm(plan.on_at), "09:00");
        assert_eq!(local_hhmm(plan.off_at), "18:00");
        assert_eq!(plan.date, a_day());
        // Nothing was predicted — this window was placed by the clock, not
        // scored, and the TUI reads exactly this to say so.
        assert_eq!(plan.predicted_grid_wh, None);
        assert!(!plan.on_fired && !plan.off_fired);
    }

    #[test]
    fn a_timer_fallback_across_midnight_lands_on_the_next_day() {
        let plan = timer_fallback_plan("22:00", "05:00", a_day()).unwrap();
        assert_eq!(local_hhmm(plan.on_at), "22:00");
        assert_eq!(local_hhmm(plan.off_at), "05:00");
        assert!(
            plan.off_at > plan.on_at,
            "the off leg should be the following morning, not the same one"
        );
        assert_eq!(
            plan.off_at - plan.on_at,
            chrono::Duration::hours(7),
            "a wrapping pair should keep the length its timers imply"
        );
    }

    #[test]
    fn a_zero_length_timer_pair_has_no_fallback_to_offer() {
        // `duration_steps_from_timers` rejects it, and there is nothing
        // sensible to invent — this is the one case that still leaves the
        // relay alone.
        assert!(timer_fallback_plan("09:00", "09:00", a_day()).is_none());
        assert!(timer_fallback_plan("not a time", "18:00", a_day()).is_none());
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

    const TEST_TTL: Duration = Duration::from_secs(60);

    fn a_fingerprint(ip: IpAddr) -> fingerprint::Fingerprint {
        fingerprint::Fingerprint {
            ip,
            open_ports: vec![80, 443],
            http: vec![],
        }
    }

    fn arp(entries: &[(&str, &str)]) -> HashMap<Ipv4Addr, String> {
        entries
            .iter()
            .map(|(ip, mac)| (ip.parse().unwrap(), (*mac).to_string()))
            .collect()
    }

    /// A cache holding one address, fingerprinted `age` before `now`.
    fn cache_of(
        ip: IpAddr,
        mac: Option<&str>,
        now: Instant,
        age: Duration,
    ) -> HashMap<IpAddr, CachedFingerprint> {
        HashMap::from([(
            ip,
            CachedFingerprint {
                fp: a_fingerprint(ip),
                mac: mac.map(str::to_string),
                at: now - age,
            },
        )])
    }

    #[test]
    fn a_fresh_entry_for_the_same_device_is_reused_instead_of_probed() {
        let ip: IpAddr = "192.168.1.10".parse().unwrap();
        // `now` is offset from the real clock so subtracting an age cannot
        // underflow the monotonic instant on a machine that just booted.
        let now = Instant::now() + Duration::from_secs(3600);
        let cache = cache_of(ip, Some("aa:bb:cc:dd:ee:ff"), now, Duration::from_secs(10));
        let macs = arp(&[("192.168.1.10", "aa:bb:cc:dd:ee:ff")]);

        let (reused, probe) = plan_fingerprints(&[ip], &cache, &macs, TEST_TTL, now);

        assert_eq!(reused.len(), 1, "the cached fingerprint should be reused");
        assert_eq!(reused[0].ip, ip);
        assert!(probe.is_empty(), "nothing should go back on the wire");
    }

    #[test]
    fn an_entry_older_than_the_ttl_is_probed_again() {
        let ip: IpAddr = "192.168.1.10".parse().unwrap();
        let now = Instant::now() + Duration::from_secs(3600);
        let cache = cache_of(ip, Some("aa:bb:cc:dd:ee:ff"), now, Duration::from_secs(600));
        let macs = arp(&[("192.168.1.10", "aa:bb:cc:dd:ee:ff")]);

        let (reused, probe) = plan_fingerprints(&[ip], &cache, &macs, TEST_TTL, now);

        assert!(reused.is_empty());
        assert_eq!(probe, vec![ip]);
    }

    #[test]
    fn a_different_device_on_a_familiar_address_is_probed_again() {
        // The entry is well within the TTL, but the MAC there has changed, so it
        // describes a device that no longer holds the address.
        let ip: IpAddr = "192.168.1.10".parse().unwrap();
        let now = Instant::now() + Duration::from_secs(3600);
        let cache = cache_of(ip, Some("aa:bb:cc:dd:ee:ff"), now, Duration::from_secs(1));
        let macs = arp(&[("192.168.1.10", "11:22:33:44:55:66")]);

        let (reused, probe) = plan_fingerprints(&[ip], &cache, &macs, TEST_TTL, now);

        assert!(reused.is_empty(), "a new MAC invalidates the entry");
        assert_eq!(probe, vec![ip]);
    }

    #[test]
    fn an_address_that_has_never_been_seen_is_probed() {
        let known: IpAddr = "192.168.1.10".parse().unwrap();
        let fresh: IpAddr = "192.168.1.11".parse().unwrap();
        let now = Instant::now() + Duration::from_secs(3600);
        let cache = cache_of(
            known,
            Some("aa:bb:cc:dd:ee:ff"),
            now,
            Duration::from_secs(1),
        );
        let macs = arp(&[("192.168.1.10", "aa:bb:cc:dd:ee:ff")]);

        let (reused, probe) = plan_fingerprints(&[known, fresh], &cache, &macs, TEST_TTL, now);

        assert_eq!(reused.len(), 1);
        assert_eq!(probe, vec![fresh], "only the unknown address is probed");
    }

    #[test]
    fn an_entry_taken_before_the_address_had_an_arp_record_still_matches() {
        // Both sides are `None`: no ARP entry then, none now. That is the same
        // device as far as this can tell, so it must not re-probe every cycle.
        let ip: IpAddr = "192.168.1.10".parse().unwrap();
        let now = Instant::now() + Duration::from_secs(3600);
        let cache = cache_of(ip, None, now, Duration::from_secs(1));

        let (reused, probe) = plan_fingerprints(&[ip], &cache, &HashMap::new(), TEST_TTL, now);

        assert_eq!(reused.len(), 1);
        assert!(probe.is_empty());
    }

    #[test]
    fn ipv6_has_no_arp_identity() {
        let v6: IpAddr = "fe80::1".parse().unwrap();
        assert_eq!(
            mac_at(v6, &arp(&[("192.168.1.10", "aa:bb:cc:dd:ee:ff")])),
            None
        );
    }
}
