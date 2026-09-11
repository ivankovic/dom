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

//! Everything the interface currently believes, in one place.
//!
//! [`App`] is the whole of the displayed state — some fifty fields covering the
//! device list, the last reading and connection status per address, the charts,
//! the dialogs, the scan in progress, the cluster's view of its peer. It lives
//! behind one `RwLock` as [`SharedState`]: the background tasks in `main` write
//! to it, and the render pass reads it. That is the only channel between them.
//!
//! One lock rather than many, because the alternative is a screen drawn from
//! several inconsistent snapshots, and because nothing here is held long enough
//! to matter — no lock in this project is held across an `await`.
//!
//! **This module performs no I/O and awaits nothing.** Whatever is derived from
//! the state is derived by a plain function of it, which is what makes those
//! functions testable without a database, a network or a terminal:
//! [`classify_network_devices`] sorts a scan into a topology,
//! [`status_transition`] decides whether a change is worth recording as an
//! event. Where a task needs to *do* something, it does it in `main` or `tui`
//! and writes the result here.
//!
//! Two consequences of holding state keyed by address are worth knowing about:
//!
//! - [`App::forget_device`] has to clear *every* address-keyed field, and there
//!   are fourteen of them. Forgetting a device while leaving its certificate alert or its
//!   cleartext warning behind left a row that could not be dismissed; a test now
//!   fills all of them and asserts nothing survives.
//! - A device's address is not stable. Anything that captures one before an
//!   `await` and uses it after must capture the address, not an index — see
//!   `tui::mod`'s action functions.
//!
//! [`App::write_error`] is the one field that is about Dom rather than about the
//! house: it says that recording a measurement failed, so that a working poll
//! loop writing into a full disk does not look identical to one that is fine.
//! What it cannot yet say is how much has failed — see REVIEW.md.
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};

pub type SharedState = Arc<RwLock<App>>;

pub fn new_shared() -> SharedState {
    Arc::new(RwLock::new(App::default()))
}

#[derive(Clone, Default, PartialEq)]
pub enum View {
    #[default]
    Current,
    Energy,
    Network,
    Devices,
    /// Long-term energy statistics, read from the daily rollup rather than the
    /// 2s series — see `crate::stats`.
    Statistics,
    /// Device temperatures, live and as a daily range history.
    Environment,
}

#[derive(Clone, Default, PartialEq)]
pub enum Focus {
    #[default]
    DeviceList,
    Detail,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SwitchAutoMode {
    Disabled,
    Time,
    /// Each day, run the on/off window predicted to draw the least from the
    /// grid — see `devices::mystrom_switch::choose_window`.
    Eco,
}

/// A day's computed Eco-mode plan for one switch: when to turn on, when to
/// turn off, and what it was predicted to cost — recomputed once per local
/// day by `main::eco_job`, never persisted (like `SolarOutlook`, it is a
/// derived value rather than a fact worth storing).
#[derive(Clone, Debug, PartialEq)]
pub struct EcoPlan {
    /// The local day this plan is for — a plan is recomputed once `Utc::now()`
    /// crosses into a new local day.
    pub date: chrono::NaiveDate,
    pub on_at: DateTime<Utc>,
    pub off_at: DateTime<Utc>,
    /// Predicted grid import for the window, Wh — shown so the plan reads as
    /// more than a guess.
    ///
    /// `None` marks the *timer fallback*: a day Eco could not score, placed at
    /// the device's own configured timer clock times instead. A fallback plan
    /// predicts nothing, so there is no figure to show — see
    /// `main::compute_eco_plan` for what makes a day unscoreable.
    pub predicted_grid_wh: Option<f64>,
    /// True once `on_at`/`off_at` have actually been sent to the switch.
    pub on_fired: bool,
    pub off_fired: bool,
}

#[derive(Clone)]
pub struct SwitchTimer {
    pub id: i64,
    pub time_hhmm: String,
    pub relay_on: bool,
}

#[derive(Clone, PartialEq)]
pub enum TimerDialogField {
    Time,
    Action,
}

#[derive(Clone)]
pub struct TimerDialog {
    pub ip: IpAddr,
    pub time_buf: String,
    pub relay_on: bool,
    pub field: TimerDialogField,
}

#[derive(Clone, Default)]
pub struct EnergyChartData {
    pub consumption: Vec<(f64, f64)>,
    pub production: Vec<(f64, f64)>,
    pub grid: Vec<(f64, f64)>,
    pub battery: Vec<(f64, f64)>,
    pub grid_imported_kwh: f64,
    pub grid_exported_kwh: f64,
}

#[derive(Clone, Debug, Default)]
pub enum ConnStatus {
    /// Waiting for first successful poll or recovering after failures.
    #[default]
    Connecting,
    Online,
    /// More than 3 consecutive poll failures.
    Lost,
}

/// The most recent outdoor temperature, and where it came from.
///
/// Carries the station's distance and altitude because they qualify the reading:
/// a measurement from 20 km away and 900 m higher is not the temperature outside
/// the house, and the view says so rather than implying otherwise.
#[derive(Clone, Debug, PartialEq)]
pub struct OutdoorReading {
    pub station_name: String,
    pub temperature_c: f64,
    /// `None` when the published station data carries no altitude — see
    /// `online::weather::Station::altitude_m`.
    pub altitude_m: Option<f64>,
    pub distance_km: f64,
    pub measured_at: DateTime<Utc>,
}

/// Everything the Environment view says about solar production: what is expected,
/// what actually happened, and how well the two have matched.
///
/// Assembled by `online::forecast::refresh` rather than during render, like the
/// statistics and temperature history — the view stays a pure function of state.
///
/// Empty until a location is set. Predictions additionally need a calibration,
/// so between the first location and the first fit the weather is known and the
/// power is not; the two are separate `Option`s for exactly that reason.
#[derive(Clone, Debug, Default)]
pub struct SolarOutlook {
    /// The fitted array response, or `None` before the first calibration.
    pub calibration: Option<crate::solar::Calibration>,
    /// Expected production for the whole of today, kWh: what has already been
    /// measured plus what is still forecast. Not a pure forecast, and
    /// deliberately so — half of today is no longer a prediction.
    pub today_kwh: Option<f64>,
    /// Predicted production for tomorrow, kWh — the question this all answers.
    pub tomorrow_kwh: Option<f64>,
    /// What has actually been produced so far today, kWh.
    pub today_actual_kwh: f64,
    /// Tomorrow's predicted power in kW against hour of local day, for the chart.
    pub tomorrow_curve: Vec<(f64, f64)>,
    /// Mean cloud cover over tomorrow's daylight hours, percent. Shown because it
    /// is what a person reads off a forecast; deliberately not part of the model,
    /// which would otherwise count cloud twice — see `crate::solar`.
    pub tomorrow_cloud_pct: Option<f64>,
    /// Days that are over: (day, predicted kWh, actual kWh), oldest first.
    pub recent: Vec<(chrono::NaiveDate, f64, f64)>,
    /// How those days turned out, in aggregate.
    pub accuracy: crate::solar::Accuracy,
    /// Why the last forecast fetch failed, if it did.
    pub last_error: Option<String>,
}

/// A device presenting a TLS certificate other than the one pinned for it.
///
/// Dom cannot tell a router that was reset or reinstalled apart from something
/// impersonating one, so it does not guess: the connection is refused, no
/// credential is sent, and this is put in front of a person to decide.
#[derive(Clone, Debug, PartialEq)]
pub struct CertAlert {
    /// The fingerprint Dom trusts for this device.
    pub expected: String,
    /// The one it presented instead.
    pub observed: String,
}

/// A Dom peer found on the LAN (or an already-paired peer's identity no longer matching its pin),
/// awaiting one explicit keypress before its key is trusted — see `App::cluster_pairing_prompt`.
#[derive(Clone, Debug, PartialEq)]
pub struct ClusterPairingPrompt {
    /// `host:port` to write as `cluster_peer_addr` if this is confirmed.
    pub addr: String,
    pub node_id: String,
    /// Hex-encoded Ed25519 public key to pin as `cluster_peer_pubkey` if this is confirmed.
    pub public_key: String,
    /// `true` when confirming this would overwrite an existing pin (a re-pair, not a first
    /// pairing) — purely for the prompt's wording, the confirm action is identical either way.
    pub replaces_pin: bool,
}

/// How long a device may take to answer a poll before it is called slow, in
/// milliseconds.
///
/// A REST round-trip is a heavier thing than an ICMP echo, so this is looser than
/// the 50 ms the ping-based check used: it is measuring "the device is labouring",
/// not network latency.
pub const SLOW_POLL_MS: f64 = 750.0;

/// Status for network infrastructure devices.
#[derive(Clone, Debug, PartialEq)]
pub enum NetworkDeviceStatus {
    Ok,
    Slow,
    Degraded,
    Lost,
    /// No ping data available because ping-scanning itself is currently
    /// broken (see `App::last_scan_error`) — distinct from a healthy scan
    /// simply not having reached this specific device yet. Never silently
    /// reported as OK just because the device answers HTTP/TCP polls.
    Unknown,
}

impl NetworkDeviceStatus {
    pub fn display(&self) -> &'static str {
        match self {
            NetworkDeviceStatus::Ok => "OK",
            NetworkDeviceStatus::Slow => "SLOW",
            NetworkDeviceStatus::Degraded => "DEGRADED",
            NetworkDeviceStatus::Lost => "LOST",
            NetworkDeviceStatus::Unknown => "UNKNOWN",
        }
    }
}

/// Grouping of scanned devices into the network-infrastructure roles shown in
/// the Network view: Router first, then 5G Modem, then Access Points. Devices
/// are matched by label keyword first; any leftover MikroTik devices fill the
/// still-empty roles (router, then modem), with the rest treated as APs.
#[derive(Clone, Default)]
pub struct NetworkTopology {
    pub routers: Vec<IpAddr>,
    pub modems: Vec<IpAddr>,
    pub access_points: Vec<IpAddr>,
}

impl NetworkTopology {
    /// All infrastructure IPs, in Router, Modem, then Access Point order.
    pub fn iter_all(&self) -> impl Iterator<Item = IpAddr> + '_ {
        self.routers
            .iter()
            .chain(self.modems.iter())
            .chain(self.access_points.iter())
            .copied()
    }
}

pub fn classify_network_devices(devices: &[ScannedDevice]) -> NetworkTopology {
    let mut routers: Vec<IpAddr> = Vec::new();
    let mut modems: Vec<IpAddr> = Vec::new();
    let mut access_points: Vec<IpAddr> = Vec::new();

    // First pass: devices with an explicit, keyword-matching label.
    for device in devices {
        if let Some(label) = &device.label {
            let label_lower = label.to_lowercase();
            if label_lower.contains("router") {
                routers.push(device.ip);
            } else if label_lower.contains("modem") || label_lower.contains("5g") {
                modems.push(device.ip);
            } else if label_lower.contains("ap") || label_lower.contains("access point") {
                access_points.push(device.ip);
            }
        }
    }

    // Second pass: any MikroTik device not yet categorized. The first fills
    // Router if empty, the next fills Modem if empty, the rest become APs.
    let mut remaining_mikrotik: Vec<IpAddr> = devices
        .iter()
        .filter(|d| d.name == Some(crate::devices::mikrotik::NAME))
        .map(|d| d.ip)
        .filter(|ip| !routers.contains(ip) && !modems.contains(ip) && !access_points.contains(ip))
        .collect();

    if routers.is_empty() && !remaining_mikrotik.is_empty() {
        routers.push(remaining_mikrotik.remove(0));
    }
    if modems.is_empty() && !remaining_mikrotik.is_empty() {
        modems.push(remaining_mikrotik.remove(0));
    }
    access_points.extend(remaining_mikrotik);

    NetworkTopology {
        routers,
        modems,
        access_points,
    }
}

/// A logged change in a network-infrastructure device's status (e.g. Router
/// going OK → LOST). Startup and a device's first-ever observed status are
/// never logged — see `status_transition`.
#[derive(Clone)]
pub struct NetworkStatusEvent {
    pub ip: IpAddr,
    pub label: Option<String>,
    pub previous: NetworkDeviceStatus,
    pub current: NetworkDeviceStatus,
    pub at: DateTime<Utc>,
}

/// Decides whether a status observation is a loggable transition: `old` must
/// be `Some` (i.e. this device has already been observed at least once this
/// run) and differ from `new`. This naturally excludes both binary startup
/// (in-memory `last_network_status` is empty on launch) and a device's first
/// connection (its first observation has no prior entry to compare against).
pub fn status_transition(
    old: Option<&NetworkDeviceStatus>,
    new: &NetworkDeviceStatus,
) -> Option<NetworkDeviceStatus> {
    match old {
        Some(o) if o != new => Some(o.clone()),
        _ => None,
    }
}

/// Chart data for the modem's Internet-facing traffic, bucketed at 2-minute
/// resolution. x = hours since local midnight, y = average throughput in kbps.
#[derive(Clone, Default)]
pub struct InternetTrafficChartData {
    pub rx_kbps: Vec<(f64, f64)>,
    pub tx_kbps: Vec<(f64, f64)>,
}

#[derive(Default)]
pub struct App {
    pub scan: ScanPhase,
    /// Cause of the most recent ping-scan failure (e.g. missing CAP_NET_RAW),
    /// if any. Cleared on the next successful scan. A ping-scan failure no
    /// longer aborts the whole scan cycle — DHCP-lease-based discovery keeps
    /// running regardless — but it's surfaced here so it's visible rather
    /// than silently limiting discovery to already-known devices.
    pub last_scan_error: Option<String>,
    /// Most recent successful ping-scan results, written by `ping_scan_task`
    /// and read by `discovery_task` — the two run on independent schedules,
    /// so this is how a fresh ping-scan result reaches discovery without the
    /// two tasks blocking on each other.
    pub last_ping_devices: Vec<crate::devices::Device>,
    pub devices: Vec<ScannedDevice>,
    pub readings: HashMap<IpAddr, LiveReading>,
    pub switch_readings: HashMap<IpAddr, SwitchReading>,
    pub mikrotik_readings: HashMap<IpAddr, MikrotikReading>,
    pub keba_readings: HashMap<IpAddr, KebaReading>,
    /// Persisted charging mode per KEBA wallbox (Disabled or FullPower).
    pub keba_modes: HashMap<IpAddr, crate::devices::keba::ChargingMode>,
    pub conn_status: HashMap<IpAddr, ConnStatus>,
    /// Human-readable cause of the most recent poll failure per device, shown
    /// in the UI while a device is Connecting or Lost. Cleared on success.
    pub last_error: HashMap<IpAddr, String>,
    /// How long each device's last successful poll took, in milliseconds. Fills
    /// in the Network view's slow/ok distinction without any traffic of its own —
    /// see `network_status`.
    pub poll_latency_ms: HashMap<IpAddr, f64>,
    pub view: View,
    pub focus: Focus,
    pub selected: usize,
    /// Currently highlighted row in the Energy view's Device Power list.
    pub energy_selected: usize,
    pub energy_chart: EnergyChartData,
    /// IPs that already have an active poll loop, to avoid spawning duplicates on rescan.
    pub polled_ips: HashSet<IpAddr>,
    /// When Some, the user is editing a device name; holds the in-progress text.
    pub rename_input: Option<String>,
    /// Per-device energy breakdown for today.
    pub device_energy_today: HashMap<IpAddr, DeviceEnergyToday>,
    /// Auto-mode setting per switch device (Disabled or Time).
    pub switch_auto_modes: HashMap<IpAddr, SwitchAutoMode>,
    /// Scheduled timers per switch device.
    pub switch_timers: HashMap<IpAddr, Vec<SwitchTimer>>,
    /// Today's Eco-mode plan per switch device, once one has been computed —
    /// see `EcoPlan`.
    pub switch_eco_plans: HashMap<IpAddr, EcoPlan>,
    /// This node's current cluster role — see `crate::cluster`. Defaults to `Active` (via
    /// `Role::default()`), matching `cluster::decide_role`'s own answer for a node with no peer
    /// configured — but that default is only trustworthy for the single-node case. `main()`
    /// seeds this field synchronously with the *correct* starting value (`Standby` when a peer
    /// is configured) before spawning `cluster_heartbeat_listener_task`, which answers a peer's
    /// heartbeat from this field the moment it starts accepting connections — if it were left at
    /// the raw default, a paired node could briefly claim `Active` to a peer that asks before
    /// `main::cluster_task`'s own first tick corrects it.
    pub cluster_role: crate::cluster::Role,
    /// A discovered peer awaiting one explicit human confirmation before its identity is pinned
    /// — see `main::cluster_discovery_task` (which populates this from an unrecognized reply) and
    /// `main::cluster_task` (which populates it when a *paired* peer's identity no longer matches
    /// the pin). Deliberately `Option`, not a map of many: this is a two-node design, there is
    /// realistically at most one relevant prompt at a time.
    pub cluster_pairing_prompt: Option<ClusterPairingPrompt>,
    /// When Some, the "Add Timer" popup is open.
    pub timer_dialog: Option<TimerDialog>,
    /// Currently focused row in the Detail panel (for switch sub-navigation).
    pub detail_row: usize,
    /// Last status observed this run for each network-infrastructure device,
    /// used only to detect transitions worth logging (see `status_transition`).
    /// Deliberately not restored from the DB on startup so the first
    /// observation of each run is never itself logged as a transition.
    pub last_network_status: HashMap<IpAddr, NetworkDeviceStatus>,
    /// Recent network-infrastructure status-change events, newest first,
    /// capped for display; durable copies live in the NetworkStatusEvents table.
    pub network_status_events: Vec<NetworkStatusEvent>,
    /// 2-minute-resolution chart of the modem's Internet-facing (lte1) traffic.
    pub internet_traffic_chart: InternetTrafficChartData,
    /// Long-term statistics: the selected window and period, and the buckets
    /// loaded for it. Populated by `stats::refresh`, not computed during render.
    pub stats: crate::stats::Stats,
    /// Latest outdoor reading from the nearest MeteoSwiss station, once a
    /// location has been configured and a fetch has succeeded.
    pub outdoor: Option<OutdoorReading>,
    /// Today's lowest and highest recorded outdoor temperature.
    pub outdoor_today: Option<(f64, f64)>,
    /// The configured location, or `None` until the user sets an address.
    pub location: Option<crate::db::Location>,
    /// Why the last outdoor fetch failed, if it did. Shown rather than swallowed:
    /// this is the one part of Dom that depends on the internet, so a failure
    /// should look different from "no data yet".
    pub last_weather_error: Option<String>,
    /// When `Some`, the user is typing an address; holds the in-progress text.
    pub address_input: Option<String>,
    /// Daily temperature summaries for the Environment view's history chart,
    /// newest last. Loaded in the background; the live readings it sits under
    /// come from `switch_readings`.
    pub temperature_history: Vec<crate::db::DailyTemperature>,
    /// Highlighted row in the Environment view's sensor list.
    pub env_selected: usize,
    /// Predicted solar production, and how past predictions turned out. Empty
    /// until a location is set; see `SolarOutlook`.
    pub solar: SolarOutlook,
    /// Devices whose TLS certificate no longer matches the pinned one. While a
    /// device is in here its credentials are not being sent at all — see
    /// `devices::tls`. Cleared when the connection succeeds again, which
    /// accepting the new certificate causes.
    pub cert_alerts: HashMap<IpAddr, CertAlert>,
    /// Devices Dom is talking to over plain HTTP because they are not serving
    /// HTTPS. Their credentials cross the network readable by anything that can
    /// see the traffic, which is worth saying out loud rather than leaving as
    /// the silent default it used to be.
    pub cleartext_devices: HashSet<IpAddr>,
    /// Why the last energy rollup pass failed, if it did.
    ///
    /// Surfaced rather than only logged because of what it costs: a failing
    /// rollup also stops pruning, and the measurement series resumes growing by
    /// about a gigabyte a month. That is not something to discover from a file
    /// weeks later — see `crate::logging`.
    pub rollup_error: Option<String>,
    /// Why the most recent attempt to *record* a measurement failed, if one did.
    ///
    /// The counterpart to `conn_status`, and the gap it fills: a poll that
    /// cannot reach its device is visible on the device's own row, but a poll
    /// that reads the device perfectly and then fails to write shows nothing at
    /// all. The views go on displaying live readings, every device stays
    /// `Online`, and the series quietly fills with holes — the one failure mode
    /// here that looks exactly like success.
    ///
    /// One field rather than one per device, because the thing that failed is
    /// the database and not the device whose poll happened to hit it: every
    /// loop would otherwise report the same fault at once, against whichever
    /// addresses they belong to. Cleared by the next write that succeeds, from
    /// any loop, since that is what says the database is answering again.
    ///
    /// Set through `devices::note_write_failure` so every loop words it the
    /// same way. See `crate::logging` for why a log line alone is not enough.
    pub write_error: Option<String>,
    /// Which colour palette to draw with. Auto-detected from the terminal at
    /// startup unless the user has stored an explicit choice, and toggled with
    /// 't' — see `tui::theme`. Stored as the mode rather than a built `Theme`
    /// so there is a single source of truth; `theme()` builds the palette.
    pub theme_mode: crate::tui::theme::ThemeMode,
}

impl App {
    /// The colour palette for the active mode. Cheap to call per widget — a
    /// `Theme` is `Copy` and building one is a struct literal.
    pub fn theme(&self) -> crate::tui::theme::Theme {
        crate::tui::theme::Theme::for_mode(self.theme_mode)
    }

    /// Drops every trace of the device at `ip` from the live in-memory state.
    ///
    /// Called by a poll loop that has discovered its device migrated to a new
    /// address (see `db::upsert_device`): a fresh loop for the new address is
    /// already running, so the old address must disappear from the UI rather
    /// than linger as a permanently-`Lost` row.
    ///
    /// Clears *every* map keyed by IP, not just the readings map belonging to
    /// the calling device's type. An IP hosts exactly one device, so removals
    /// for other types are no-ops — and doing it uniformly means a map added
    /// later can't be forgotten in one of the callers and leave a stale row on
    /// screen. Every `HashMap<IpAddr, _>` and `HashSet<IpAddr>` field below
    /// should be listed here; the address is gone, so nothing keyed by it is
    /// still meaningful.
    pub fn forget_device(&mut self, ip: &IpAddr) {
        self.conn_status.remove(ip);
        self.last_error.remove(ip);
        self.readings.remove(ip);
        self.switch_readings.remove(ip);
        self.mikrotik_readings.remove(ip);
        self.keba_readings.remove(ip);
        self.keba_modes.remove(ip);
        self.polled_ips.remove(ip);
        self.poll_latency_ms.remove(ip);
        self.device_energy_today.remove(ip);
        self.switch_auto_modes.remove(ip);
        self.switch_timers.remove(ip);
        self.switch_eco_plans.remove(ip);
        self.last_network_status.remove(ip);
        // Added with the TLS work, and missed here until the code health pass
        // of 2026-09-11 — which is the omission the paragraph above warns
        // about, happening. Left behind, the two of them kept the Network
        // view's security panel telling a person about an address nothing was
        // talking to: a certificate warning offering 'k' to accept a pin for a
        // device that had moved, and a cleartext-credentials warning for
        // traffic that was no longer being sent.
        self.cert_alerts.remove(ip);
        self.cleartext_devices.remove(ip);
    }

    /// Live status for a network-infrastructure device: prefers ping-history
    /// (covers latency/packet-loss). If this device has no ping-history entry
    /// because ping-scanning itself is currently broken (`last_scan_error` is
    /// set), that's reported as `Unknown` rather than silently falling back
    /// to app-level connectivity — a device answering our HTTP/TCP polls
    /// fine is not the same claim as "ping health is OK". The conn_status
    /// fallback is only used when scanning is healthy but just hasn't
    /// reached this particular device yet (e.g. right after startup, or a
    /// device outside the ping-scanned subnets).
    /// How an infrastructure device is doing, as its own poll loop sees it.
    ///
    /// Derived from whether Dom can actually talk to the device, not from ICMP.
    /// There used to be a task pinging routers and modems three times every ten
    /// seconds and access points three times every thirty, purely to fill this
    /// in — a packet a second, all day, to answer a question the poll loop was
    /// already answering for free every time it fetched anything.
    ///
    /// It is also a better answer. A device can return a ping while its API is
    /// unreachable or refusing credentials, and it is the API that Dom needs.
    ///
    /// `Slow` reflects how long the device took to answer its last poll, which
    /// is measured on a request that was going to happen regardless.
    pub fn network_status(&self, ip: IpAddr) -> NetworkDeviceStatus {
        match self.conn_status.get(&ip) {
            Some(ConnStatus::Lost) => NetworkDeviceStatus::Lost,
            Some(ConnStatus::Connecting) => NetworkDeviceStatus::Degraded,
            Some(ConnStatus::Online) => match self.poll_latency_ms.get(&ip) {
                Some(ms) if *ms >= SLOW_POLL_MS => NetworkDeviceStatus::Slow,
                _ => NetworkDeviceStatus::Ok,
            },
            // Nothing has polled it: a device Dom recognises but does not know
            // how to talk to, or one whose loop has not run yet.
            None => NetworkDeviceStatus::Unknown,
        }
    }

    /// Devices shown in the Devices view's list+detail layout: every device
    /// the scanner has found, configured or not.
    pub fn visible_devices(&self) -> Vec<&ScannedDevice> {
        self.devices.iter().collect()
    }

    pub fn selected_device(&self) -> Option<&ScannedDevice> {
        self.visible_devices().get(self.selected).copied()
    }
}

#[derive(Default, Clone)]
pub enum ScanPhase {
    #[default]
    Idle,
    Scanning,
    Done {
        at: DateTime<Utc>,
        next_at: DateTime<Utc>,
    },
}

#[derive(Clone)]
pub struct ScannedDevice {
    pub ip: IpAddr,
    pub latency_ms: f64,
    pub open_ports: Vec<u16>,
    pub name: Option<&'static str>,
    /// User-set display name; when Some, replaces `name` in the UI.
    pub label: Option<String>,
}

#[derive(Clone, Default)]
pub struct DeviceEnergyToday {
    /// kWh consumed today (switches: power metric).
    pub kwh: f64,
    /// kWh absorbed by battery today (pac < 0 periods); 0 for switches.
    pub charged_kwh: f64,
    /// kWh delivered by battery today (pac > 0 periods); 0 for switches.
    pub discharged_kwh: f64,
}

impl ScannedDevice {
    pub fn display_name(&self) -> &str {
        self.label.as_deref().or(self.name).unwrap_or("Unknown")
    }
}

#[derive(Clone)]
pub struct SwitchReading {
    pub power_w: f64,
    pub relay_on: bool,
    pub temperature_c: f64,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct MikrotikReading {
    pub leases: Vec<crate::devices::mikrotik::DhcpLease>,
    pub firewall_rules: Vec<crate::devices::mikrotik::FirewallRule>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct KebaReading {
    pub power_w: f64,
    /// Energy delivered in the current charging session, kWh.
    pub energy_session_kwh: f64,
    /// Lifetime energy counter reported by the wallbox itself, kWh.
    pub energy_total_kwh: f64,
    /// Raw "State" code from report 2; see `devices::keba::state_label`.
    pub state: u8,
    /// Raw "Plug" code from report 2; see `devices::keba::plug_label`.
    pub plug: u8,
    /// Wallbox's own reported hardware current ceiling ("Curr HW"), mA. Cached
    /// here so Eco mode's control loop can clamp its target without an extra
    /// device round-trip.
    pub curr_hw_ma: u32,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct LiveReading {
    pub consumption_w: f64,
    pub production_w: f64,
    pub pac_w: f64,
    pub rsoc: f64,
    pub grid_w: f64,
    /// Battery energy currently stored, kWh (0.0 if not a storage device).
    pub remaining_kwh: f64,
    /// Total usable battery capacity, kWh (0.0 while unknown).
    pub capacity_kwh: f64,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn device(ip: &str, name: Option<&'static str>, label: Option<&str>) -> ScannedDevice {
        ScannedDevice {
            ip: ip.parse::<Ipv4Addr>().unwrap().into(),
            latency_ms: 1.0,
            open_ports: vec![],
            name,
            label: label.map(String::from),
        }
    }

    // ── status_transition ──────────────────────────────────────────────────

    #[test]
    fn status_transition_none_on_first_observation() {
        // No prior entry at all: this is a device's first observation this
        // run (or the very first scan after binary startup) — never logged.
        assert!(status_transition(None, &NetworkDeviceStatus::Lost).is_none());
        assert!(status_transition(None, &NetworkDeviceStatus::Ok).is_none());
    }

    #[test]
    fn status_transition_none_when_unchanged() {
        assert!(
            status_transition(Some(&NetworkDeviceStatus::Ok), &NetworkDeviceStatus::Ok).is_none()
        );
    }

    #[test]
    fn status_transition_some_on_real_change() {
        let prev = status_transition(Some(&NetworkDeviceStatus::Ok), &NetworkDeviceStatus::Lost);
        assert_eq!(prev, Some(NetworkDeviceStatus::Ok));
    }

    // ── classify_network_devices ───────────────────────────────────────────

    #[test]
    fn classify_uses_explicit_labels_first() {
        let devices = vec![
            device("192.168.1.1", None, Some("Router")),
            device("192.168.1.2", None, Some("5G Modem")),
            device("192.168.1.3", None, Some("Living Room AP")),
        ];
        let topo = classify_network_devices(&devices);
        assert_eq!(topo.routers, vec![devices[0].ip]);
        assert_eq!(topo.modems, vec![devices[1].ip]);
        assert_eq!(topo.access_points, vec![devices[2].ip]);
    }

    #[test]
    fn classify_fills_router_then_modem_from_unlabeled_mikrotik() {
        let devices = vec![
            device("192.168.1.1", Some(crate::devices::mikrotik::NAME), None),
            device("192.168.1.2", Some(crate::devices::mikrotik::NAME), None),
            device("192.168.1.3", Some(crate::devices::mikrotik::NAME), None),
        ];
        let topo = classify_network_devices(&devices);
        assert_eq!(topo.routers, vec![devices[0].ip]);
        assert_eq!(topo.modems, vec![devices[1].ip]);
        assert_eq!(topo.access_points, vec![devices[2].ip]);
    }

    #[test]
    fn classify_ignores_unrelated_devices() {
        let devices = vec![device("192.168.1.42", Some("myStrom WiFi Switch"), None)];
        let topo = classify_network_devices(&devices);
        assert!(topo.routers.is_empty());
        assert!(topo.modems.is_empty());
        assert!(topo.access_points.is_empty());
    }

    // ── App::network_status ─────────────────────────────────────────────────

    #[test]
    fn a_device_nothing_has_polled_yet_is_unknown_rather_than_ok() {
        // Reporting OK for a device Dom has never spoken to would be a guess
        // dressed as a measurement.
        let ip: IpAddr = "192.168.1.1".parse::<Ipv4Addr>().unwrap().into();
        assert_eq!(
            App::default().network_status(ip),
            NetworkDeviceStatus::Unknown
        );
    }

    #[test]
    fn status_follows_whether_the_poll_loop_can_reach_the_device() {
        // Derived from the poll loop rather than from ICMP: a device can answer
        // a ping while its API is unreachable, and it is the API Dom needs.
        let ip: IpAddr = "192.168.1.1".parse::<Ipv4Addr>().unwrap().into();
        for (conn, expected) in [
            (ConnStatus::Online, NetworkDeviceStatus::Ok),
            (ConnStatus::Connecting, NetworkDeviceStatus::Degraded),
            (ConnStatus::Lost, NetworkDeviceStatus::Lost),
        ] {
            let mut app = App::default();
            app.conn_status.insert(ip, conn.clone());
            assert_eq!(app.network_status(ip), expected, "{conn:?}");
        }
    }

    #[test]
    fn a_device_that_answers_slowly_is_reported_slow() {
        let ip: IpAddr = "192.168.1.1".parse::<Ipv4Addr>().unwrap().into();
        let mut app = App::default();
        app.conn_status.insert(ip, ConnStatus::Online);

        app.poll_latency_ms.insert(ip, SLOW_POLL_MS - 1.0);
        assert_eq!(app.network_status(ip), NetworkDeviceStatus::Ok);

        app.poll_latency_ms.insert(ip, SLOW_POLL_MS);
        assert_eq!(app.network_status(ip), NetworkDeviceStatus::Slow);
    }

    #[test]
    fn a_slow_reading_never_outranks_being_unreachable() {
        // Latency is only meaningful for a device that answered; a stale figure
        // must not soften "Dom cannot talk to this at all".
        let ip: IpAddr = "192.168.1.1".parse::<Ipv4Addr>().unwrap().into();
        let mut app = App::default();
        app.poll_latency_ms.insert(ip, 10.0);
        app.conn_status.insert(ip, ConnStatus::Lost);
        assert_eq!(app.network_status(ip), NetworkDeviceStatus::Lost);
    }

    #[test]
    fn a_broken_ping_scan_no_longer_affects_infrastructure_status() {
        // It used to force Unknown, because status came from ICMP and a scan
        // that could not run meant no data. Status now comes from the poll loop,
        // which does not need CAP_NET_RAW.
        let ip: IpAddr = "192.168.1.1".parse::<Ipv4Addr>().unwrap().into();
        let mut app = App {
            last_scan_error: Some("permission denied".to_string()),
            ..Default::default()
        };
        app.conn_status.insert(ip, ConnStatus::Online);
        assert_eq!(app.network_status(ip), NetworkDeviceStatus::Ok);
    }

    /// `forget_device` promises to clear *every* map keyed by address, and had
    /// quietly stopped doing so: `cert_alerts` and `cleartext_devices` arrived
    /// with the TLS work and were never added to it. There was no test, which
    /// is why nothing noticed.
    ///
    /// Written against the whole struct rather than against a list of fields,
    /// so the next map added to `App` is caught by this too: it fills every
    /// address-keyed field, forgets the address, and asserts nothing anywhere
    /// still mentions it.
    #[test]
    fn forgetting_a_device_leaves_nothing_behind_under_its_address() {
        let ip: IpAddr = "172.16.0.7".parse().unwrap();
        let other: IpAddr = "172.16.0.8".parse().unwrap();
        let now = Utc::now();

        let mut app = App::default();
        for at in [ip, other] {
            app.conn_status.insert(at, ConnStatus::Lost);
            app.last_error.insert(at, "unreachable".to_string());
            app.readings.insert(
                at,
                LiveReading {
                    consumption_w: 1.0,
                    production_w: 1.0,
                    pac_w: 0.0,
                    rsoc: 50.0,
                    grid_w: 0.0,
                    remaining_kwh: 1.0,
                    capacity_kwh: 2.0,
                    updated_at: now,
                },
            );
            app.switch_readings.insert(
                at,
                SwitchReading {
                    power_w: 1.0,
                    relay_on: true,
                    temperature_c: 20.0,
                    updated_at: now,
                },
            );
            app.mikrotik_readings.insert(
                at,
                MikrotikReading {
                    leases: vec![],
                    firewall_rules: vec![],
                    updated_at: now,
                },
            );
            app.keba_readings.insert(
                at,
                KebaReading {
                    power_w: 0.0,
                    energy_session_kwh: 0.0,
                    energy_total_kwh: 0.0,
                    state: 0,
                    plug: 0,
                    curr_hw_ma: 0,
                    updated_at: now,
                },
            );
            app.keba_modes
                .insert(at, crate::devices::keba::ChargingMode::Disabled);
            app.polled_ips.insert(at);
            app.poll_latency_ms.insert(at, 10.0);
            app.device_energy_today
                .insert(at, DeviceEnergyToday::default());
            app.switch_auto_modes.insert(at, SwitchAutoMode::Time);
            app.switch_timers.insert(at, vec![]);
            app.switch_eco_plans.insert(
                at,
                EcoPlan {
                    date: now.date_naive(),
                    on_at: now,
                    off_at: now,
                    predicted_grid_wh: None,
                    on_fired: false,
                    off_fired: false,
                },
            );
            app.last_network_status
                .insert(at, NetworkDeviceStatus::Lost);
            app.cert_alerts.insert(
                at,
                CertAlert {
                    expected: "aa".to_string(),
                    observed: "bb".to_string(),
                },
            );
            app.cleartext_devices.insert(at);
        }

        app.forget_device(&ip);

        assert!(!app.conn_status.contains_key(&ip));
        assert!(!app.last_error.contains_key(&ip));
        assert!(!app.readings.contains_key(&ip));
        assert!(!app.switch_readings.contains_key(&ip));
        assert!(!app.mikrotik_readings.contains_key(&ip));
        assert!(!app.keba_readings.contains_key(&ip));
        assert!(!app.keba_modes.contains_key(&ip));
        assert!(!app.polled_ips.contains(&ip));
        assert!(!app.poll_latency_ms.contains_key(&ip));
        assert!(!app.device_energy_today.contains_key(&ip));
        assert!(!app.switch_auto_modes.contains_key(&ip));
        assert!(!app.switch_timers.contains_key(&ip));
        assert!(!app.switch_eco_plans.contains_key(&ip));
        assert!(!app.last_network_status.contains_key(&ip));
        assert!(
            !app.cert_alerts.contains_key(&ip),
            "a certificate warning for an address that has moved offers 'k' to \
             pin a device that is not there"
        );
        assert!(
            !app.cleartext_devices.contains(&ip),
            "nothing is being sent to this address at all, in the clear or otherwise"
        );

        // The device that did not move keeps everything.
        assert!(app.conn_status.contains_key(&other));
        assert!(app.cert_alerts.contains_key(&other));
        assert!(app.cleartext_devices.contains(&other));
    }
}
