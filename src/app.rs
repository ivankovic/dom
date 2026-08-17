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
}

#[derive(Clone, Default, PartialEq)]
pub enum Focus {
    #[default]
    DeviceList,
    Detail,
}

#[derive(Clone, PartialEq)]
pub enum SwitchAutoMode {
    Disabled,
    Time,
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

#[derive(Clone, Default)]
pub enum ConnStatus {
    /// Waiting for first successful poll or recovering after failures.
    #[default]
    Connecting,
    Online,
    /// More than 3 consecutive poll failures.
    Lost,
}

/// Result of a single ping to a device.
#[derive(Clone, Debug)]
pub struct PingResult {
    pub latency_ms: f64,
    pub success: bool,
}

/// Ping history for a device - stores up to 5 most recent pings.
#[derive(Clone, Debug, Default)]
pub struct PingHistory {
    pub results: Vec<PingResult>,
}

impl PingHistory {
    pub fn new() -> Self {
        Self {
            results: Vec::new(),
        }
    }

    pub fn add(&mut self, latency_ms: f64, success: bool) {
        self.results.push(PingResult {
            latency_ms,
            success,
        });
        if self.results.len() > 5 {
            self.results.remove(0);
        }
    }

    /// Calculate device status based on the last 5 pings.
    pub fn status(&self) -> NetworkDeviceStatus {
        // If we have no successful pings, device is lost
        if self.results.is_empty() {
            return NetworkDeviceStatus::Lost;
        }

        // Check if any ping failed (packet loss)
        let has_packet_loss = self.results.iter().any(|r| !r.success);

        if has_packet_loss {
            return NetworkDeviceStatus::Degraded;
        }

        // All pings were successful, check latencies
        let all_fast = self.results.iter().all(|r| r.latency_ms < 50.0);

        if all_fast && self.results.len() >= 5 {
            return NetworkDeviceStatus::Ok;
        }

        // If we have successful pings but any are slow (> 50ms)
        let any_slow = self.results.iter().any(|r| r.latency_ms >= 50.0);
        if any_slow {
            return NetworkDeviceStatus::Slow;
        }

        // Not enough pings yet, or mixed but none slow - consider OK if online
        NetworkDeviceStatus::Ok
    }
}

/// Configuration for per-device ping health checks.
#[derive(Clone, Debug)]
pub struct PingConfig {
    /// How often to ping this device (seconds).
    pub interval_secs: u64,
    /// Number of ICMP packets to send per ping check.
    pub packet_count: u8,
}

impl Default for PingConfig {
    fn default() -> Self {
        // Default: ping every 5 minutes with 1 packet (legacy behavior)
        Self {
            interval_secs: 300,
            packet_count: 1,
        }
    }
}

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
    /// Ping history for each device (last 5 pings) for network infrastructure health status.
    pub ping_history: HashMap<IpAddr, PingHistory>,
    /// Per-device ping configuration (interval and packet count).
    pub ping_configs: HashMap<IpAddr, PingConfig>,
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
        self.ping_history.remove(ip);
        self.ping_configs.remove(ip);
        self.device_energy_today.remove(ip);
        self.switch_auto_modes.remove(ip);
        self.switch_timers.remove(ip);
        self.last_network_status.remove(ip);
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
    pub fn network_status(&self, ip: IpAddr) -> NetworkDeviceStatus {
        match self.ping_history.get(&ip) {
            Some(h) => h.status(),
            None if self.last_scan_error.is_some() => NetworkDeviceStatus::Unknown,
            None => match self.conn_status.get(&ip) {
                Some(ConnStatus::Online) => NetworkDeviceStatus::Ok,
                Some(ConnStatus::Lost) => NetworkDeviceStatus::Lost,
                _ => NetworkDeviceStatus::Ok,
            },
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

    /// Get the ping configuration for a device based on its label/role.
    /// Router and 5G modem: 3 packets every 10 seconds.
    /// Access points: 3 packets every 30 seconds.
    /// All other devices: default (300 seconds, 1 packet).
    pub fn get_ping_config(&self, device: &ScannedDevice) -> PingConfig {
        if let Some(label) = &device.label {
            let label_lower = label.to_lowercase();
            // Router and 5G modem get high-frequency pings
            if label_lower.contains("router")
                || label_lower.contains("modem")
                || label_lower.contains("5g")
            {
                return PingConfig {
                    interval_secs: 10,
                    packet_count: 3,
                };
            }
            // Access points get medium-frequency pings
            if label_lower.contains("ap") || label_lower.contains("access point") {
                return PingConfig {
                    interval_secs: 30,
                    packet_count: 3,
                };
            }
        }
        // Default for all other devices (including MikroTik devices without explicit labels)
        // Check if this is a MikroTik device that should be classified
        if device.name == Some(crate::devices::mikrotik::NAME) {
            // For unlabeled MikroTik devices, we'll use the classification logic
            // But we can't do that here without the full device list, so return default
            // and let the caller handle it
        }
        PingConfig::default()
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
    fn network_status_is_unknown_when_scan_broken_and_never_ping_scanned() {
        // No ping_history entry for this IP, and the ping scan is currently
        // failing (e.g. missing CAP_NET_RAW): must not silently report OK
        // just because app-level (HTTP/TCP) polling happens to be healthy.
        let ip: IpAddr = "192.168.1.1".parse::<Ipv4Addr>().unwrap().into();
        let mut app = App {
            last_scan_error: Some("permission denied".to_string()),
            ..Default::default()
        };
        app.conn_status.insert(ip, ConnStatus::Online);

        assert_eq!(app.network_status(ip), NetworkDeviceStatus::Unknown);
    }

    #[test]
    fn network_status_falls_back_to_conn_status_when_scan_is_healthy() {
        // No ping_history entry yet, but the ping scan itself isn't broken —
        // e.g. right after startup, or a device outside the scanned subnets.
        // Falling back to app-level connectivity is still reasonable here.
        let ip: IpAddr = "192.168.1.1".parse::<Ipv4Addr>().unwrap().into();
        let mut app = App::default();
        app.conn_status.insert(ip, ConnStatus::Online);

        assert_eq!(app.network_status(ip), NetworkDeviceStatus::Ok);
    }

    #[test]
    fn network_status_prefers_ping_history_even_when_scan_broken() {
        // A stale ping_history entry from before the scan broke must still
        // win over the Unknown fallback — it's real (if aging) ping data.
        let ip: IpAddr = "192.168.1.1".parse::<Ipv4Addr>().unwrap().into();
        let mut app = App {
            last_scan_error: Some("permission denied".to_string()),
            ..Default::default()
        };
        let mut history = PingHistory::new();
        history.add(5.0, true);
        history.add(5.0, true);
        history.add(5.0, true);
        history.add(5.0, true);
        history.add(5.0, true);
        app.ping_history.insert(ip, history);

        assert_eq!(app.network_status(ip), NetworkDeviceStatus::Ok);
    }
}
