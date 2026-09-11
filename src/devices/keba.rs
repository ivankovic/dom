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

//! A KEBA wallbox: charging a car from the sun, over UDP.
//!
//! Two things make this the most involved device module. It is the only one that
//! does not speak HTTP — the wallbox's protocol is line-oriented UDP on port
//! 7090, where a request is a word (`report 2`) and the reply is JSON — and it
//! is the only one whose actuation is *continuous*. A switch is on or off; a
//! wallbox is told a current, in milliamps, and changing it changes how fast a
//! car charges right now.
//!
//! The polling half reads two reports: [`Report2`] for state, plug and hardware
//! current limit, [`Report3`] for power and energy counters. Note the units —
//! milliwatts and deci-watt-hours — which is where the conversions in the
//! storage helpers come from, and [`state_label`]/[`plug_label`] for what the
//! numeric codes mean.
//!
//! # Eco mode
//!
//! [`eco_loop`] is the rest of the file: every ten seconds it looks at the
//! surplus the battery module is reporting and commands the current that
//! consumes it, so the car charges on solar rather than on the grid. The pure
//! decision is `eco_decision`, and the constants above it are the module's
//! real content — each one is there because of something observed in the field:
//!
//! - **The averaging window equals the tick.** A longer one (60 s was tried)
//!   mixes samples from before and after Eco's own last command, so the estimate
//!   describes a blend of two regimes and Eco oscillates between off and maximum
//!   instead of converging.
//! - **Hysteresis on the enable decision.** The protocol floor is 6 A; without a
//!   lower floor for *stopping*, a surplus hovering near it flapped the charger
//!   on and off every tick — seen in the field as repeated "5 kW available but
//!   charging is off".
//! - **Only 90% of the measured surplus is claimed**, because the estimate is a
//!   ten-second average and a cloud or a kettle should cost some unclaimed
//!   export rather than tip the car into drawing from the grid.
//! - **Volts and phases are [`Supply`], read from the database**, because they
//!   describe a building rather than a protocol. Getting them wrong does not
//!   fail; it scales every target by the ratio it is wrong by.
//!
//! `eco_decision` is a pure function for the same reason `cluster::decide_role`
//! is: it is the part that must not be wrong, and it can be exercised directly.
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::{Row, SqlitePool};
use tokio::net::UdpSocket;
use tokio::time::{MissedTickBehavior, interval, timeout};

use crate::app::{ConnStatus, KebaReading, SharedState};
use crate::devices::ts;
use crate::fingerprint::Fingerprint;

pub const NAME: &str = "KEBA Wallbox";
pub const API_PORT: u16 = 7090;
const DEFAULT_POLL_SECS: i64 = 5;
const UDP_TIMEOUT: Duration = Duration::from_secs(3);

/// KEBA wallboxes serve a small embedded status page (lwIP stack) on port 80,
/// but are actually monitored/controlled over the UDP protocol on port 7090
/// (see `fetch_reports`/`set_mode`) — the HTTP page only serves fingerprinting.
pub fn detect(fp: &Fingerprint) -> bool {
    if !fp.open_ports.contains(&80) {
        return false;
    }
    fp.http
        .iter()
        .any(|p| p.raw.contains("lwIP") && p.raw.contains("Wallbox"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargingMode {
    Disabled,
    FullPower,
    /// Charge only on solar surplus left over after the house battery has
    /// taken its share — the battery has priority; see `eco_loop` for the
    /// control algorithm.
    Eco,
    /// Like `Eco`, but the car claims surplus before the house battery does.
    EcoCarFirst,
}

impl ChargingMode {
    pub fn as_db_str(self) -> &'static str {
        match self {
            ChargingMode::Disabled => "disabled",
            ChargingMode::FullPower => "full_power",
            ChargingMode::Eco => "eco",
            ChargingMode::EcoCarFirst => "eco_car_first",
        }
    }

    pub fn from_db_str(s: &str) -> Self {
        match s {
            "full_power" => ChargingMode::FullPower,
            "eco" => ChargingMode::Eco,
            "eco_car_first" => ChargingMode::EcoCarFirst,
            _ => ChargingMode::Disabled,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ChargingMode::Disabled => "Disabled",
            ChargingMode::FullPower => "Full power",
            ChargingMode::Eco => "Eco",
            ChargingMode::EcoCarFirst => "Eco (car first)",
        }
    }
}

// ── UDP protocol ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Report2 {
    #[serde(rename = "State")]
    pub state: u8,
    #[serde(rename = "Plug")]
    pub plug: u8,
    #[serde(rename = "Curr HW")]
    pub curr_hw_ma: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Report3 {
    #[serde(rename = "P")]
    pub power_mw: f64,
    #[serde(rename = "E pres")]
    pub energy_session_deciwh: f64,
    #[serde(rename = "E total")]
    pub energy_total_deciwh: f64,
}

pub fn state_label(state: u8) -> &'static str {
    match state {
        0 => "Starting",
        1 => "Not ready",
        2 => "Ready",
        3 => "Charging",
        4 => "Error",
        5 => "Interrupted",
        _ => "Unknown",
    }
}

pub fn plug_label(plug: u8) -> &'static str {
    match plug {
        0 => "No cable",
        1 => "Plugged (station)",
        3 => "Plugged + locked (station)",
        5 => "Plugged (station + EV)",
        7 => "Plugged + locked (station + EV)",
        _ => "Unknown",
    }
}

/// KEBA always replies to UDP port 7090 on the sender's IP, regardless of which
/// port the request was sent from — so the local socket must itself be bound to
/// 7090 for the reply to reach it. Since that bind is process-wide and fixed,
/// there can only ever be one such socket: it's opened once, lazily, and every
/// caller (the poll loop's `fetch_reports` and a UI-triggered `set_mode`) takes
/// this mutex before using it. Without that serialization, two callers racing
/// to bind/use port 7090 concurrently would either fail to bind (`EADDRINUSE`)
/// or, worse, read the wrong reply for their own request.
static SOCKET: tokio::sync::OnceCell<tokio::sync::Mutex<UdpSocket>> =
    tokio::sync::OnceCell::const_new();

async fn socket() -> anyhow::Result<&'static tokio::sync::Mutex<UdpSocket>> {
    SOCKET
        .get_or_try_init(|| async {
            let sock = UdpSocket::bind(("0.0.0.0", API_PORT))
                .await
                .context("bind local UDP port 7090 (required so KEBA's reply reaches us)")?;
            Ok::<_, anyhow::Error>(tokio::sync::Mutex::new(sock))
        })
        .await
}

/// True if `msg` is a `report N` reply, i.e. a JSON object with `"ID": "<id>"`.
fn is_report_reply(msg: &str, id: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(msg)
        .ok()
        .and_then(|v| Some(v.get("ID")?.as_str()? == id))
        .unwrap_or(false)
}

/// KEBA proactively pushes unsolicited state-change broadcasts to the last UDP
/// sender (e.g. `{"E pres": 29181}` whenever energy ticks over) independently of
/// any request — these can arrive interleaved with the reply to our own command,
/// especially right around a plug/unplug event. They're JSON but never carry the
/// `"ID"` that a genuine `report N` reply has, and they never start with the
/// `"TCH-"` prefix a control-command ack has. So keep receiving (within the
/// overall timeout) until something that actually looks like our reply shows up,
/// rather than trusting whatever datagram happens to arrive first.
async fn query(socket: &UdpSocket, ip: IpAddr, port: u16, cmd: &str) -> anyhow::Result<String> {
    socket
        .send_to(cmd.as_bytes(), SocketAddr::new(ip, port))
        .await
        .context("send")?;

    let expected_report_id = cmd.strip_prefix("report ").map(str::trim);
    let deadline = tokio::time::Instant::now() + UDP_TIMEOUT;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(!remaining.is_zero(), "recv timeout");

        let mut buf = [0u8; 4096];
        let n = timeout(remaining, socket.recv(&mut buf))
            .await
            .context("recv timeout")?
            .context("recv failed")?;
        let msg = String::from_utf8_lossy(&buf[..n]).into_owned();

        let is_reply = match expected_report_id {
            Some(id) => is_report_reply(&msg, id),
            None => msg.trim_start().starts_with("TCH-"),
        };
        if is_reply {
            return Ok(msg);
        }
        // Otherwise: an unsolicited broadcast (or a stray reply to someone else's
        // command) — discard it and keep waiting for our own reply.
    }
}

/// Sends a control command (as opposed to a `report N` query) and checks the
/// acknowledgement. KEBA replies "TCH-OK :done" on success or "TCH-ERR ..." on
/// rejection; treat anything containing "ERR" as a failure rather than silently
/// reporting success for a command the wallbox actually refused.
async fn command(socket: &UdpSocket, ip: IpAddr, port: u16, cmd: &str) -> anyhow::Result<()> {
    let ack = query(socket, ip, port, cmd).await?;
    if ack.to_uppercase().contains("ERR") {
        anyhow::bail!("KEBA rejected {cmd:?}: {}", ack.trim());
    }
    Ok(())
}

pub async fn fetch_reports(ip: IpAddr, port: u16) -> anyhow::Result<(Report2, Report3)> {
    let socket = socket().await?.lock().await;
    let r2_raw = query(&socket, ip, port, "report 2").await?;
    let r2: Report2 = serde_json::from_str(&r2_raw).context("parse report 2")?;
    let r3_raw = query(&socket, ip, port, "report 3").await?;
    let r3: Report3 = serde_json::from_str(&r3_raw).context("parse report 3")?;
    Ok((r2, r3))
}

/// Applies a charging mode: Disabled turns charging off outright; FullPower
/// raises the current limit to the wallbox's own hardware maximum (read live
/// via `report 2`, since it varies by model) before enabling.
pub async fn set_mode(ip: IpAddr, port: u16, mode: ChargingMode) -> anyhow::Result<()> {
    let socket = socket().await?.lock().await;
    match mode {
        // Entering/holding an Eco mode starts from a safe "off" baseline;
        // `eco_loop` takes over from there and sets the actual current every
        // tick.
        ChargingMode::Disabled | ChargingMode::Eco | ChargingMode::EcoCarFirst => {
            command(&socket, ip, port, "ena 0").await?;
        }
        ChargingMode::FullPower => {
            let r2_raw = query(&socket, ip, port, "report 2").await?;
            let r2: Report2 = serde_json::from_str(&r2_raw).context("parse report 2")?;
            command(&socket, ip, port, &format!("curr {}", r2.curr_hw_ma)).await?;
            command(&socket, ip, port, "ena 1").await?;
        }
    }
    Ok(())
}

/// Sets the target charging current and enables the wallbox — used by `eco_loop`
/// once it has computed how much surplus power is available for the car.
async fn set_current_and_enable(ip: IpAddr, port: u16, ma: u32) -> anyhow::Result<()> {
    let socket = socket().await?.lock().await;
    command(&socket, ip, port, &format!("curr {ma}")).await?;
    command(&socket, ip, port, "ena 1").await?;
    Ok(())
}

// ── Database ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct DeviceRecord {
    pub id: i64,
    pub ip: IpAddr,
    pub port: u16,
    pub poll_interval_secs: i64,
    pub label: Option<String>,
}

/// Registers a discovered device; no credentials to preserve (KEBA's UDP
/// protocol has no authentication), so unlike Sonnen/MikroTik this can freely
/// upsert on every rescan. `fingerprint` (e.g. a MAC address) lets a device
/// that changed IP be recognized and migrated in place rather than
/// registered as a second device — see `db::upsert_device`.
pub async fn save_device(
    pool: &SqlitePool,
    ip: IpAddr,
    name: &str,
    fingerprint: Option<&str>,
) -> anyhow::Result<()> {
    crate::db::upsert_device(pool, "keba", name, ip, DEFAULT_POLL_SECS, fingerprint).await
}

pub async fn load_all(pool: &SqlitePool) -> anyhow::Result<Vec<DeviceRecord>> {
    let rows =
        sqlx::query("SELECT id, ip, poll_interval_secs, label FROM Devices WHERE type = 'keba'")
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

async fn save_raw(pool: &SqlitePool, device_id: i64, t: &str, power_w: f64) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
         VALUES (?, ?, 'power', ?)",
    )
    .bind(device_id)
    .bind(t)
    .bind(power_w)
    .execute(pool)
    .await?;
    Ok(())
}

async fn save_energy(
    pool: &SqlitePool,
    device_id: i64,
    prev_power_w: f64,
    prev_t: DateTime<Utc>,
    curr_power_w: f64,
    curr_t: DateTime<Utc>,
) -> anyhow::Result<()> {
    let dt = (curr_t - prev_t).num_milliseconds() as f64 / 1000.0;
    if dt <= 0.0 {
        return Ok(());
    }
    let energy_ws = (prev_power_w + curr_power_w) / 2.0 * dt;
    sqlx::query(
        "INSERT INTO Energy (device_id, timestamp, resolution, metric, energy_ws)
         VALUES (?, ?, '2s', 'power', ?)",
    )
    .bind(device_id)
    .bind(ts(curr_t))
    .bind(energy_ws)
    .execute(pool)
    .await?;
    Ok(())
}

// ── Poll loop ─────────────────────────────────────────────────────────────────

pub async fn poll_loop(
    pool: SqlitePool,
    device: DeviceRecord,
    mode: ChargingMode,
    state: SharedState,
) {
    // Re-apply the persisted mode on (re)start so the wallbox's actual state
    // always matches what the user last configured, even after an app restart.
    // If this fails, leave a trail in last_error rather than pretending it worked;
    // the very first successful poll below will clear it once connectivity is up.
    if let Err(e) = set_mode(device.ip, device.port, mode).await {
        state
            .write()
            .unwrap()
            .last_error
            .insert(device.ip, format!("mode reapply: {e:#}"));
    }

    let mut ticker = crate::devices::PollTicker::new(device.id, device.poll_interval_secs);

    let mut prev: Option<(f64, DateTime<Utc>)> = None;
    let mut prev_plug: Option<u8> = None;
    let mut failures: u32 = 0;

    loop {
        ticker.tick(&pool).await;
        let poll_time = Utc::now();

        match fetch_reports(device.ip, device.port).await {
            Ok((r2, r3)) => {
                let power_w = r3.power_mw / 1000.0;
                let t = ts(poll_time);
                // Logged rather than discarded, as the battery's and the switch's
                // loops already do. This is the largest load in the house, so a
                // run of failed writes is the one worth being able to find
                // afterwards — silently, the series simply has a hole in it.
                let what = format!("keba {}", device.ip);
                let mut wrote = true;
                if let Err(e) = save_raw(&pool, device.id, &t, power_w).await {
                    crate::devices::note_write_failure(&state, &what, &e);
                    wrote = false;
                }
                if let Some((prev_power, prev_t)) = prev
                    && let Err(e) =
                        save_energy(&pool, device.id, prev_power, prev_t, power_w, poll_time).await
                {
                    crate::devices::note_write_failure(&state, &what, &e);
                    wrote = false;
                }
                if wrote {
                    crate::devices::note_write_ok(&state);
                }

                // KEBA can silently ignore an `ena`/`curr` sent while the EV isn't
                // plugged in yet, leaving the station stuck "ready" once it is
                // plugged in — so reissue the currently configured mode right when
                // the plug transitions to "EV connected" (Plug >= 5), rather than
                // relying solely on the one-shot reapply at poll_loop startup.
                let ev_just_connected = matches!(prev_plug, Some(p) if p < 5) && r2.plug >= 5;
                prev_plug = Some(r2.plug);

                failures = 0;
                let current_mode = {
                    let mut app = state.write().unwrap();
                    app.keba_readings.insert(
                        device.ip,
                        KebaReading {
                            power_w,
                            energy_session_kwh: r3.energy_session_deciwh / 10_000.0,
                            energy_total_kwh: r3.energy_total_deciwh / 10_000.0,
                            state: r2.state,
                            plug: r2.plug,
                            curr_hw_ma: r2.curr_hw_ma,
                            updated_at: poll_time,
                        },
                    );
                    app.conn_status.insert(device.ip, ConnStatus::Online);
                    app.last_error.remove(&device.ip);
                    app.keba_modes.get(&device.ip).copied().unwrap_or(mode)
                };

                if ev_just_connected
                    && current_mode == ChargingMode::FullPower
                    && let Err(e) = set_mode(device.ip, device.port, current_mode).await
                {
                    state
                        .write()
                        .unwrap()
                        .last_error
                        .insert(device.ip, format!("mode reapply on plug-in: {e:#}"));
                }

                prev = Some((power_w, poll_time));
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

const ECO_TICK_SECS: u64 = 10;
/// Sonnen samples are only averaged back to the start of the *current* tick —
/// deliberately equal to `ECO_TICK_SECS` rather than some longer smoothing
/// window. `eco_loop` changes the car's current at most once per tick, so a
/// window this short never straddles two different commanded currents; a
/// longer window (a 60s rolling average was tried) mixes samples from before
/// and after our own last actuation, so the "available power" estimate below
/// reflects a blend of two different regimes rather than the current one —
/// wildly over- or under-estimating surplus and driving Eco into a permanent
/// off/max-current oscillation instead of converging.
const ECO_WINDOW_SECS: i64 = ECO_TICK_SECS as i64;
/// KEBA's UDP protocol floor for the `curr` command; values below this are
/// clamped to it by the wallbox itself rather than treated as "off". This is
/// also the bar Eco must clear to *start* charging.
const MIN_CURR_MA: u32 = 6000;
/// Once charging, Eco doesn't stop the instant the target dips fractionally
/// below the 6A floor — it rides it out down to this much lower floor first.
/// This is Schmitt-trigger hysteresis on the enable/disable *decision*, not
/// on the commanded current itself (`eco_decision` always clamps whatever it
/// sends back up to at least `MIN_CURR_MA`, since the wallbox would reject —
/// or silently re-clamp — anything lower). Without it, a target hovering
/// within a few tens of watts of the 6A boundary flaps the charger on and off
/// every tick, observed in the field as repeated "5kW+ available but
/// charging is off" reports. See `eco_decision`.
const ECO_DISABLE_HYSTERESIS_MA: u32 = 2000;
/// Only commit to charging at 90% of the measured surplus, never the full
/// amount. Leaves headroom for the fluctuations inherent in a 10s-old,
/// single-tick average (a passing cloud, a kettle switching on) so a normal
/// wobble doesn't tip the car into drawing from the grid — it costs a little
/// unclaimed export instead.
const ECO_SAFETY_MARGIN: f64 = 0.9;
/// Defaults for the site's electrical supply, used until the database says
/// otherwise. 230 V and three phases describe the installation this was written
/// against — confirmed from a live capture where P = 11.04 kW exactly matches
/// 16 A × 230 V × 3.
///
/// They are defaults rather than constants because they are facts about a
/// building, not about the protocol, and a wrong value does not fail: it scales
/// every Eco target by the ratio it is wrong by. A single-phase site left on
/// these would command three times the current it should. See `Supply`.
const DEFAULT_MAINS_VOLTAGE: f64 = 230.0;
const DEFAULT_PHASES: f64 = 3.0;

/// The site's electrical supply, as Eco mode needs it to convert power to current.
///
/// Read from the database rather than compiled in, since it describes the
/// building Dom is installed in. Stored in `Config` rather than on the wallbox's
/// own row because it is a property of the supply, which every device on the
/// site shares.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Supply {
    pub volts: f64,
    pub phases: f64,
}

impl Default for Supply {
    fn default() -> Self {
        Self {
            volts: DEFAULT_MAINS_VOLTAGE,
            phases: DEFAULT_PHASES,
        }
    }
}

impl Supply {
    /// Volt-amps per amp of charging current: what a target current costs, and
    /// therefore what divides available power to get one.
    fn watts_per_amp(self) -> f64 {
        self.volts * self.phases
    }

    /// Whether these values could describe a real supply.
    ///
    /// A stored value that cannot is ignored in favour of the default, because
    /// the failure mode is silent: zero or a negative would send the target to
    /// infinity or negative, and a plausible-looking but wrong figure mis-scales
    /// every decision. Anything outside these bounds is a typo, not a building.
    pub fn is_plausible(self) -> bool {
        (100.0..=500.0).contains(&self.volts) && (1.0..=3.0).contains(&self.phases)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum EcoDecision {
    Disable,
    SetCurrent(u32),
}

/// The pure decision at the heart of Eco mode, split out from `eco_loop` so it
/// can be unit-tested without a socket or database.
///
/// Grid export can't be used to estimate spare power directly: the Sonnen
/// battery soaks up solar surplus before any of it reaches the grid, so
/// `GridFeedIn_W` mostly reads near zero regardless of how much surplus
/// exists — it only reflects what's left after the battery has taken its
/// share, which isn't the number Eco needs. Instead reconstruct what's
/// actually available from the three raw quantities:
///   - `avg_production_w` — solar production, unaffected by anything Eco does.
///   - `avg_consumption_w` — whole-house consumption, which already includes
///     the car's own draw (it's downstream of the meter like everything
///     else). Subtracting `avg_car_w` back out gives the house's *baseline*
///     load without the car.
///   - `avg_pac_w` — the battery's own net power, positive when discharging
///     and negative when charging (Sonnen's convention).
///
/// `avg_car_w` must be averaged over this *same* window rather than read as a
/// live snapshot — that was a real bug, not a hypothetical one: while the car
/// ramps its current over several seconds, a windowed `avg_consumption_w`
/// lags the ramp but an instantaneous snapshot leads it, so their difference
/// swings hard in both directions and the "available" estimate chases it —
/// overshooting on the way up, cratering on the way down, forever. Averaging
/// both over the same interval makes the ramp appear identically in both
/// terms and cancel: `production − (baseline + car/2) + car/2 = production −
/// baseline`, a fixed point the estimate actually converges to instead of
/// oscillating around.
///
/// `car_first` selects which of the two Eco modes is active:
///   - `ChargingMode::Eco` (car_first = false): the battery has priority.
///     available = production − consumption + car + min(pac, 0). A charging
///     battery (negative pac) reduces what's available, since it's claiming
///     part of the surplus first. A *discharging* battery (positive pac) is
///     clamped to zero rather than added — that discharge is covering some
///     existing shortfall, not producing extra surplus, and crediting it to
///     the car would let the car draw down the battery's stored energy
///     instead of solar, going over what production alone justifies. That
///     was the actual bug behind "we consistently go over the production
///     target with charging": the earlier version added the raw (unclamped)
///     `pac`, so any battery discharge — for whatever reason — read as free
///     surplus and pushed the car's target above production.
///   - `ChargingMode::EcoCarFirst` (car_first = true): the car has priority
///     over the battery, so the battery's own draw is left out entirely:
///     available = production − consumption + car
///     The car claims surplus even while the battery would otherwise be
///     absorbing it; the battery gets whatever's left.
///
/// A 10% `ECO_SAFETY_MARGIN` is then held back so ordinary tick-to-tick
/// fluctuation (a cloud, a kettle) doesn't tip the car into drawing from the
/// grid — it costs a little unclaimed surplus instead.
///
/// An earlier version of this forecast grid export 10s ahead via linear
/// regression over the last 60s of samples, to react preemptively to clouds.
/// That window spans several past ticks, each of which may have commanded a
/// different car current — so its slope was as much a fingerprint of Eco's
/// own past actuations as of the weather, and extrapolating it forward
/// amplified that self-inflicted noise into wild over/undershoots, driving
/// the charger to oscillate between off and near-max current forever. Basing
/// the decision purely on this tick's own recent average avoids that: the
/// window (`ECO_WINDOW_SECS`) never straddles two different commanded
/// currents, so it converges instead of oscillating. A real cloud is simply
/// caught one tick later.
///
/// `currently_charging` is Eco's own memory of whether it was charging as of
/// the last tick (not a live wallbox read), and selects which side of the
/// `MIN_CURR_MA` / `ECO_DISABLE_HYSTERESIS_MA` band applies — see
/// `ECO_DISABLE_HYSTERESIS_MA`'s docs for why that band exists at all. The
/// commanded current itself is still always clamped to at least
/// `MIN_CURR_MA`; only the decision of *whether* to keep charging gets the
/// lower bar.
fn eco_decision(
    house: crate::db::SonnenAvgs,
    avg_car_w: f64,
    car_first: bool,
    curr_hw_ma: u32,
    currently_charging: bool,
    supply: Supply,
) -> EcoDecision {
    let (avg_production_w, avg_consumption_w, avg_pac_w) =
        (house.production_w, house.consumption_w, house.pac_w);
    let battery_term = if car_first { 0.0 } else { avg_pac_w.min(0.0) };
    let available_w =
        (avg_production_w - avg_consumption_w + avg_car_w + battery_term) * ECO_SAFETY_MARGIN;
    let target_ma = available_w / supply.watts_per_amp() * 1000.0;

    let min_ma = if currently_charging {
        MIN_CURR_MA.saturating_sub(ECO_DISABLE_HYSTERESIS_MA)
    } else {
        MIN_CURR_MA
    };

    // A hardware ceiling below the protocol floor leaves no current that can
    // legally be commanded, so there is nothing to charge at. Checked before the
    // clamp because `.min(curr_hw_ma)` would otherwise undo the `.max` below and
    // send a current under the floor — `curr 0` followed by `ena 1` in the worst
    // case, with `currently_charging` then set as though the car were drawing.
    if curr_hw_ma < MIN_CURR_MA || target_ma < min_ma as f64 {
        EcoDecision::Disable
    } else {
        EcoDecision::SetCurrent((target_ma as u32).max(MIN_CURR_MA).min(curr_hw_ma))
    }
}

/// Every `ECO_TICK_SECS`, averages this tick's Sonnen production/consumption/
/// battery samples and sets the KEBA's charging current so the car draws only
/// what's left over after the house — and, depending on the mode, the battery
/// — see `eco_decision` for why grid export alone can't tell us that. Runs
/// for the lifetime of the device regardless of mode, checking the currently
/// configured mode itself each tick (so switching away from Eco takes effect
/// on the very next tick without needing to be told).
pub async fn eco_loop(pool: SqlitePool, device: DeviceRecord, state: SharedState) {
    let mut ticker = interval(Duration::from_secs(ECO_TICK_SECS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Eco's own memory of whether it was charging last tick, for the
    // hysteresis band in `eco_decision` — reset to false (the stricter side)
    // whenever the loop (re)starts, which is the safe default.
    let mut currently_charging = false;

    loop {
        ticker.tick().await;

        let car_first = match state.read().unwrap().keba_modes.get(&device.ip).copied() {
            Some(ChargingMode::Eco) => false,
            Some(ChargingMode::EcoCarFirst) => true,
            _ => continue,
        };

        let avgs = match crate::db::query_recent_sonnen_avgs(&pool, ECO_WINDOW_SECS).await {
            Ok(Some(a)) => a,
            _ => continue,
        };

        // Windowed over the same interval as `avgs` above — not a live
        // snapshot, see `eco_decision`'s docs for why that matters.
        let avg_car_w =
            crate::db::query_recent_device_metric_avg(&pool, device.id, "power", ECO_WINDOW_SECS)
                .await
                .ok()
                .flatten()
                .unwrap_or(0.0);

        let curr_hw_ma = state
            .read()
            .unwrap()
            .keba_readings
            .get(&device.ip)
            .map(|r| r.curr_hw_ma)
            .unwrap_or(MIN_CURR_MA);

        // Re-read each tick rather than captured at spawn, so correcting the
        // supply in the database takes effect without a restart.
        let supply = crate::db::get_supply(&pool).await;

        let decision = eco_decision(
            avgs,
            avg_car_w,
            car_first,
            curr_hw_ma,
            currently_charging,
            supply,
        );
        currently_charging = matches!(decision, EcoDecision::SetCurrent(_));
        let result = match decision {
            EcoDecision::Disable => set_mode(device.ip, device.port, ChargingMode::Disabled).await,
            EcoDecision::SetCurrent(ma) => set_current_and_enable(device.ip, device.port, ma).await,
        };

        let mut app = state.write().unwrap();
        match result {
            Ok(()) => app.last_error.remove(&device.ip),
            Err(e) => app.last_error.insert(device.ip, format!("eco: {e:#}")),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ECO_SAFETY_MARGIN, EcoDecision, MIN_CURR_MA, Supply, eco_decision, is_report_reply,
    };
    use crate::db::SonnenAvgs;

    /// The house's side of an Eco decision, as the Sonnen reports it.
    fn avgs(production_w: f64, consumption_w: f64, pac_w: f64) -> SonnenAvgs {
        SonnenAvgs {
            production_w,
            consumption_w,
            pac_w,
        }
    }

    // Live-observed shape: KEBA pushes these to the last UDP sender on its own,
    // independent of any request, whenever a single tracked value changes.
    const UNSOLICITED_BROADCAST: &str = r#"{"E pres": 29181}"#;

    #[test]
    fn report_reply_matches_its_own_id() {
        let reply = r#"{"ID": "2", "State": 3, "Plug": 7}"#;
        assert!(is_report_reply(reply, "2"));
    }

    #[test]
    fn report_reply_rejects_wrong_id() {
        let reply = r#"{"ID": "3", "P": 11040000}"#;
        assert!(!is_report_reply(reply, "2"));
    }

    #[test]
    fn report_reply_rejects_unsolicited_broadcast() {
        // The exact bug this guards against: a broadcast arriving instead of the
        // reply to our own "report 2" query must never be mistaken for one, since
        // it lacks the fields (e.g. "Plug") a real report 2 always has.
        assert!(!is_report_reply(UNSOLICITED_BROADCAST, "2"));
    }

    #[test]
    fn report_reply_rejects_non_json() {
        assert!(!is_report_reply("TCH-OK :done", "2"));
    }

    #[test]
    fn eco_decision_charges_at_available_surplus_with_no_car_or_battery_load() {
        // 6kW production, no other consumption, battery idle — should enable
        // at ~90% of 6kW / (230*3), holding back the safety margin.
        let d = eco_decision(
            avgs(6000.0, 0.0, 0.0),
            0.0,
            false,
            20_000,
            false,
            Supply::default(),
        );
        assert_eq!(
            d,
            EcoDecision::SetCurrent((6000.0 * ECO_SAFETY_MARGIN / (230.0 * 3.0) * 1000.0) as u32)
        );
    }

    #[test]
    fn eco_decision_subtracts_baseline_but_adds_car_back_in() {
        // Whole-house consumption (6000W) is almost entirely the car's own
        // draw, so the house's baseline load net of the car is ~0. A gate
        // that used raw consumption without adding the car back in would see
        // "no surplus" here and shut off, then reoscillate forever once
        // consumption dropped back down. 6100W production covers the 6000W
        // baseline-plus-car and leaves ~100W over, plus the 6000W the car
        // itself is already using — a stable ~6100W is available in total.
        let d = eco_decision(
            avgs(6100.0, 6000.0, 0.0),
            6000.0,
            false,
            20_000,
            false,
            Supply::default(),
        );
        assert_eq!(
            d,
            EcoDecision::SetCurrent((6100.0 * ECO_SAFETY_MARGIN / (230.0 * 3.0) * 1000.0) as u32)
        );
    }

    #[test]
    fn eco_decision_battery_priority_subtracts_battery_charging() {
        // ChargingMode::Eco (car_first = false): 10kW production, house
        // otherwise idle, but the battery is pulling in 3kW (pac = -3000,
        // Sonnen's charging convention) — only 7kW is left over for the car,
        // not the full 10kW production, because the battery has priority.
        let d = eco_decision(
            avgs(10_000.0, 0.0, -3000.0),
            0.0,
            false,
            20_000,
            false,
            Supply::default(),
        );
        assert_eq!(
            d,
            EcoDecision::SetCurrent((7000.0 * ECO_SAFETY_MARGIN / (230.0 * 3.0) * 1000.0) as u32)
        );
    }

    #[test]
    fn eco_decision_battery_priority_ignores_battery_discharge() {
        // ChargingMode::Eco: the bug this guards against, in the exact
        // configuration that locked up in the field. 8kW production, 2kW
        // house baseline, car already drawing 9kW — more than the true 6kW
        // surplus (production 8000 - baseline 2000), so the battery is
        // discharging 3kW (pac = +3000) to cover the shortfall the car itself
        // is causing. The old, unclamped formula credited that discharge
        // straight back to the car: 8000 - 11000 + 9000 + 3000 = 9000, i.e.
        // exactly `avg_car_w` — the car's own present draw echoed back as its
        // "target", a fixed point that never commands a reduction no matter
        // how far over production it runs. Clamping discharge to zero breaks
        // the lock: available must fall to the real 6kW surplus.
        let d = eco_decision(
            avgs(8000.0, 11_000.0, 3000.0),
            9000.0,
            false,
            20_000,
            false,
            Supply::default(),
        );
        assert_eq!(
            d,
            EcoDecision::SetCurrent((6000.0 * ECO_SAFETY_MARGIN / (230.0 * 3.0) * 1000.0) as u32)
        );
    }

    #[test]
    fn eco_decision_car_first_ignores_battery_charging() {
        // ChargingMode::EcoCarFirst: same 10kW production / battery pulling
        // in 3kW as the battery-priority test above, but here the car claims
        // the full 10kW production regardless of what the battery would
        // otherwise take — the battery term is left out entirely.
        let d = eco_decision(
            avgs(10_000.0, 0.0, -3000.0),
            0.0,
            true,
            20_000,
            false,
            Supply::default(),
        );
        assert_eq!(
            d,
            EcoDecision::SetCurrent((10_000.0 * ECO_SAFETY_MARGIN / (230.0 * 3.0) * 1000.0) as u32)
        );
    }

    #[test]
    fn eco_decision_car_first_ignores_battery_discharge_too() {
        // ChargingMode::EcoCarFirst: the battery discharging (pac = +2000)
        // must not inflate the car's allowance either — car_first ignores
        // pac in both directions, not just when it would help the car.
        let d = eco_decision(
            avgs(5000.0, 0.0, 2000.0),
            0.0,
            true,
            20_000,
            false,
            Supply::default(),
        );
        assert_eq!(
            d,
            EcoDecision::SetCurrent((5000.0 * ECO_SAFETY_MARGIN / (230.0 * 3.0) * 1000.0) as u32)
        );
    }

    #[test]
    fn eco_decision_disables_below_threshold() {
        // Only 2kW production, nothing else going on — nowhere near enough
        // for even the 6A protocol floor, so disable rather than limp along
        // under the minimum.
        let d = eco_decision(
            avgs(2000.0, 0.0, 0.0),
            0.0,
            false,
            20_000,
            false,
            Supply::default(),
        );
        assert_eq!(d, EcoDecision::Disable);
    }

    #[test]
    fn eco_decision_wont_start_below_minimum_current() {
        // 4550W clears the old 4kW gate on its own, but the 10% safety
        // margin brings it to 4095W — not enough for the 6A protocol floor
        // (4140W), and this charger isn't running yet, so the full floor
        // applies (no hysteresis discount for a cold start).
        let d = eco_decision(
            avgs(4550.0, 0.0, 0.0),
            0.0,
            false,
            20_000,
            false,
            Supply::default(),
        );
        assert_eq!(d, EcoDecision::Disable);
    }

    #[test]
    fn eco_decision_hysteresis_keeps_charging_through_a_dip_below_the_floor() {
        // The bug this guards against, straight from the field replay: with
        // production=4000 (others 0), target ≈ 5217mA — below the 6000mA
        // floor by less than the 2000mA hysteresis margin. A charger that's
        // already running rides this out rather than shutting off and
        // immediately re-qualifying next tick; the commanded current is still
        // clamped up to the real 6A floor, never actually sent below it.
        let d = eco_decision(
            avgs(4000.0, 0.0, 0.0),
            0.0,
            false,
            20_000,
            true,
            Supply::default(),
        );
        assert_eq!(d, EcoDecision::SetCurrent(6000));
    }

    #[test]
    fn eco_decision_hysteresis_does_not_let_an_idle_charger_start_in_the_gap() {
        // Same numbers as the test above, but the charger is currently off.
        // Hysteresis only rides out a dip in an *already-running* charger —
        // it must not lower the bar for starting one up in the first place.
        let d = eco_decision(
            avgs(4000.0, 0.0, 0.0),
            0.0,
            false,
            20_000,
            false,
            Supply::default(),
        );
        assert_eq!(d, EcoDecision::Disable);
    }

    #[test]
    fn eco_decision_hysteresis_still_disables_below_the_lower_floor() {
        // production=2000 (others 0) puts the target far below even the
        // hysteresis-relaxed floor (2609mA vs. the 4000mA lower bound) — a
        // charger that's already running must still shut off here.
        let d = eco_decision(
            avgs(2000.0, 0.0, 0.0),
            0.0,
            false,
            20_000,
            true,
            Supply::default(),
        );
        assert_eq!(d, EcoDecision::Disable);
    }

    #[test]
    fn eco_decision_clamps_to_hardware_current_ceiling() {
        // Huge surplus, but this cable/charger caps out at 16A.
        let d = eco_decision(
            avgs(50_000.0, 0.0, 0.0),
            0.0,
            false,
            16_000,
            false,
            Supply::default(),
        );
        assert_eq!(d, EcoDecision::SetCurrent(16_000));
    }

    #[test]
    fn a_hardware_ceiling_below_the_floor_disables_rather_than_undercutting_it() {
        // `eco_decision` documents that the commanded current is always at least
        // MIN_CURR_MA, but `.min(curr_hw_ma)` used to undo that: a wallbox
        // reporting a bad "Curr HW" got a current below the protocol floor, and
        // `SetCurrent(0)` was sent as `curr 0` + `ena 1`.
        let plenty = 20_000.0;
        for ceiling in [0, 1, MIN_CURR_MA - 1] {
            assert_eq!(
                eco_decision(
                    avgs(plenty, 0.0, 0.0),
                    0.0,
                    false,
                    ceiling,
                    false,
                    Supply::default()
                ),
                EcoDecision::Disable,
                "ceiling {ceiling}"
            );
        }
        // At the floor exactly, charging is still possible.
        assert_eq!(
            eco_decision(
                avgs(plenty, 0.0, 0.0),
                0.0,
                false,
                MIN_CURR_MA,
                false,
                Supply::default()
            ),
            EcoDecision::SetCurrent(MIN_CURR_MA)
        );
    }

    #[test]
    fn a_commanded_current_is_never_below_the_protocol_floor() {
        // Swept across the whole decision space rather than spot-checked.
        for available in [-5000.0, 0.0, 1000.0, 4000.0, 12_000.0, 50_000.0] {
            for ceiling in [0, 3000, MIN_CURR_MA, 16_000, 32_000] {
                for charging in [false, true] {
                    if let EcoDecision::SetCurrent(ma) = eco_decision(
                        avgs(available, 0.0, 0.0),
                        0.0,
                        false,
                        ceiling,
                        charging,
                        Supply::default(),
                    ) {
                        assert!(ma >= MIN_CURR_MA, "{available} W / {ceiling} mA gave {ma}");
                        assert!(ma <= ceiling, "{ma} exceeds the hardware ceiling {ceiling}");
                    }
                }
            }
        }
    }

    // ── Site electrical supply ────────────────────────────────────────────────

    #[test]
    fn the_default_supply_is_the_installation_this_was_written_against() {
        let s = Supply::default();
        assert_eq!((s.volts, s.phases), (230.0, 3.0));
        assert!(s.is_plausible());
        // 16 A on this supply is the 11.04 kW seen in a live capture.
        assert!((16.0 * s.watts_per_amp() - 11_040.0).abs() < 1.0);
    }

    #[test]
    fn a_single_phase_site_is_given_a_third_of_the_current() {
        // The reason this is configuration and not a constant: the same surplus
        // means a different current, and getting it wrong does not fail — it
        // commands three times what the supply can carry.
        let surplus = 6_900.0 / ECO_SAFETY_MARGIN;
        let three = Supply::default();
        let one = Supply {
            volts: 230.0,
            phases: 1.0,
        };
        let ma =
            |s: Supply| match eco_decision(avgs(surplus, 0.0, 0.0), 0.0, false, 32_000, false, s) {
                EcoDecision::SetCurrent(ma) => ma,
                EcoDecision::Disable => panic!("expected charging at {surplus} W"),
            };
        let (three_phase, single_phase) = (ma(three), ma(one));
        assert!(
            (single_phase as f64 / 3.0 - three_phase as f64).abs() < 2.0,
            "three-phase {three_phase} mA, single-phase {single_phase} mA"
        );
    }

    #[test]
    fn a_supply_that_cannot_describe_a_building_is_rejected() {
        for (volts, phases) in [
            (0.0, 3.0),    // would send the target to infinity
            (-230.0, 3.0), // ...or negative
            (230.0, 0.0),
            (230.0, 4.0),    // no such supply
            (12.0, 1.0),     // not mains
            (10_000.0, 3.0), // not a house
        ] {
            assert!(
                !Supply { volts, phases }.is_plausible(),
                "{volts} V / {phases}"
            );
        }
        for (volts, phases) in [(230.0, 3.0), (230.0, 1.0), (120.0, 1.0), (400.0, 3.0)] {
            assert!(
                Supply { volts, phases }.is_plausible(),
                "{volts} V / {phases}"
            );
        }
    }
}
