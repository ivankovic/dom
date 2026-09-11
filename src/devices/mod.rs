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

pub mod dom_local;
pub mod keba;
pub mod mikrotik;
pub mod mystrom_switch;
pub mod sonnen_batterie;
pub mod tls;

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use if_addrs::{IfAddr, get_if_addrs};
use rand::random;
use sqlx::SqlitePool;
use surge_ping::{Client, Config, IcmpPacket, PingIdentifier, PingSequence};
use tokio::task::JoinSet;

use crate::app::{ConnStatus, SharedState};

// surge-ping's Client::clone shares an `alive` flag; when any clone is dropped
// it marks the client destroyed, killing all in-flight pings. Use Arc<Client>
// so the Client lives until the last Arc is released (after all tasks finish).

// When scanning a large scope-link network (e.g. /12), the kernel must
// ARP-resolve every destination directly. The neighbor table (gc_thresh3=1024)
// fills up quickly, causing send_to to block until ARP times out (~3s). We
// work around this with a two-pass strategy:
//
//   1. Active scan  — ping the /24 subnet containing each interface's own IP.
//                     At most 254 IPs per interface; never overflows the ARP table.
//   2. Passive pass — read /proc/net/arp for complete (0x2) entries on non-loopback
//                     interfaces. These are devices that are or have recently been
//                     reachable; we ping-confirm them to get latency and verify liveness.
//
// Together this finds: all new devices on the local segment *and* all known
// devices spread across the wider network that appear in the kernel's ARP cache.

// Phase-1 (ARP cache) and Phase-2 (/24 probe) each use a fresh Client, so
// Phase-2's INCOMPLETE ARP entries cannot pollute Phase-1's dispatch map.
// 256 concurrent keeps max INCOMPLETE entries ≈ 256 per /24 sweep (well within
// gc_thresh3=1024) while completing two /24 subnets in ~1 second.
const CONCURRENT_PINGS: usize = 256;
// Outer timeout wraps the full ping_once future (send + reply wait) so that
// a blocked send_to due to ARP table pressure cannot hang a task indefinitely.
const TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct Device {
    pub ip: IpAddr,
    pub latency_ms: f64,
}

/// A device type discovery can recognize from a fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    SonnenBatterie,
    MystromSwitch,
    Mikrotik,
    Keba,
}

impl DeviceType {
    /// Human-readable model name shown in the UI. The matching `Devices.type`
    /// column value is not mirrored here — each device module already owns its
    /// own type string, and a second copy would be one more thing to keep in
    /// step.
    pub fn display_name(self) -> &'static str {
        match self {
            DeviceType::SonnenBatterie => sonnen_batterie::NAME,
            DeviceType::MystromSwitch => mystrom_switch::NAME,
            DeviceType::Mikrotik => mikrotik::NAME,
            DeviceType::Keba => keba::NAME,
        }
    }
}

/// Identifies a fingerprinted device, or `None` if no detector claims it.
///
/// Detectors are tried in a fixed order and the **first match wins**. That
/// matters because three of the four key off port 80 plus a substring of the
/// HTTP response body, so an unusual device could in principle satisfy two of
/// them. Discovery previously answered this question twice in two different
/// shapes — independent `if`s when persisting (which saved such a device as two
/// types and started two poll loops) and an `else if` chain when naming it for
/// the UI — so the stored type and the displayed type could disagree. There is
/// now one answer.
///
/// The local machine is deliberately absent: it is identified by comparing
/// against this host's own interface addresses (`dom_local::is_local_ip`), which
/// needs no fingerprint and cannot be mistaken.
pub fn detect_type(fp: &crate::fingerprint::Fingerprint) -> Option<DeviceType> {
    if sonnen_batterie::detect(fp) {
        Some(DeviceType::SonnenBatterie)
    } else if mystrom_switch::detect(fp) {
        Some(DeviceType::MystromSwitch)
    } else if mikrotik::detect(fp) {
        Some(DeviceType::Mikrotik)
    } else if keba::detect(fp) {
        Some(DeviceType::Keba)
    } else {
        None
    }
}

/// Format for every value written to a DB `timestamp` column.
///
/// Shared rather than repeated per device type because `db::load_*` parses
/// these strings back with this same format: a device type formatting its
/// timestamps even slightly differently would not fail at write time, it would
/// silently fail to parse on read.
pub const DB_TIMESTAMP_FMT: &str = "%Y-%m-%d %H:%M:%S";

/// Formats an instant for a DB `timestamp` column. See `DB_TIMESTAMP_FMT`.
pub fn ts(dt: DateTime<Utc>) -> String {
    dt.format(DB_TIMESTAMP_FMT).to_string()
}

// ── Reading a device's answer ─────────────────────────────────────────────────

/// Largest reply Dom will read from a device on the LAN.
///
/// Every other network read in the project is already bounded in bytes as well
/// as in time — `fingerprint::http_get` stops at `HTTP_MAX_BYTES`, and
/// `online::https::get_inner` at `MAX_BODY` — and this is the path that was
/// missing it. `read_to_end` under a timeout bounds how *long* a device may
/// answer for, not how much it may say: a device that streams for its whole
/// ten-second window grows a `Vec` at line speed, which on the Raspberry Pi
/// this is meant to run on is the whole of memory. Nor does the device have to
/// be faulty for that to happen; nothing checks that the thing answering at a
/// remembered address is still the device that used to be there.
///
/// Two megabytes is not a measured figure. The largest answers here are the
/// router's DHCP lease and firewall tables, which are JSON arrays whose length
/// is the number of leases or rules — order kilobytes on a home network, and
/// bounded by the router's own configuration either way. The cap is set well
/// clear of that rather than close to it: what it has to rule out is a reply
/// that has no end, not one that is merely large.
pub const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

/// Reads a device's reply to end of stream, within `within` and
/// `MAX_RESPONSE_BYTES`.
///
/// Exceeding the cap is an error rather than a truncation: every caller parses
/// what comes back as JSON or as an HTTP status line, and half a reply is not a
/// smaller answer, it is a wrong one. `fingerprint::http_get` truncates instead
/// because it only ever looks for substrings.
pub(crate) async fn read_capped<S>(stream: &mut S, within: Duration) -> anyhow::Result<Vec<u8>>
where
    S: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let read = async {
        let mut out = Vec::new();
        let mut buf = [0u8; 8 * 1024];
        loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                return Ok(out);
            }
            out.extend_from_slice(&buf[..n]);
            if out.len() > MAX_RESPONSE_BYTES {
                anyhow::bail!("the device sent more than {MAX_RESPONSE_BYTES} bytes");
            }
        }
    };
    tokio::time::timeout(within, read)
        .await
        .map_err(|_| anyhow::anyhow!("read timeout"))?
}

/// Consecutive poll failures tolerated before a device is reported `Lost`.
/// Up to and including this many failures the device shows as `Connecting`,
/// which keeps a single dropped packet or a device rebooting from flapping the
/// UI. Mirrored in `ConnStatus::Lost`'s documentation.
const LOST_AFTER_FAILURES: u32 = 3;

/// How often, in further failed polls, a device that is already Lost is
/// re-checked for having moved to another address.
///
/// The check used to fire on exactly one tick — the transition into Lost — to
/// keep it to one query per device rather than one per poll interval. That is
/// the right instinct and the wrong mechanism: `device_moved` returning an error
/// was folded into "has not moved" with `unwrap_or(false)`, and since the
/// trigger was an equality against a counter that only ever rises, a single
/// database hiccup on that one tick meant the device was never checked again.
/// The old loop then went on failing against an address the device had left,
/// until the process was restarted.
///
/// Re-checking periodically costs one query per Lost device per minute or so at
/// the fastest poll interval — only for devices that are already failing — and
/// it also catches a device that moves long after it went quiet.
const MOVED_RECHECK_EVERY_FAILURES: u32 = 30;

/// Shared failure handling for every device poll loop.
///
/// Returns `true` when the caller should stop polling and return: its device
/// has moved to a different address and a loop for the new address is already
/// running. Returns `false` when the caller should keep polling, having
/// recorded the failure for display.
///
/// Every device type's poll loop had its own byte-identical copy of this,
/// differing only in which readings map it cleared — now handled uniformly by
/// `App::forget_device`.
/// How many poll intervals may elapse before an interval is treated as a gap.
///
/// Loose enough that ordinary scheduling jitter, a retry, or a slow response
/// never trips it.
const MAX_INTEGRATION_GAP_POLLS: i64 = 10;

/// Longest interval, in milliseconds, that may be integrated as one step.
///
/// Poll loops integrate power between consecutive readings. That is only
/// meaningful while the two bracket a continuous stretch of polling: when a loop
/// stalls — a restart, a network drop, the device unreachable — the previous
/// reading is whatever was seen before the gap, and integrating across it invents
/// energy that was never measured. The worst observed case attributed 10.8 kWh to
/// a single row labelled `2s`, which flowed into `EnergyDaily` and inflated that
/// day by 23% while looking entirely plausible beside its neighbours.
///
/// A gap is missing data. Recording nothing for it is correct and leaves a hole
/// that `EnergyMinute.span_secs` makes visible; interpolating across it would not.
///
/// Derived from the device's own configured interval rather than a fixed figure:
/// `poll_interval_secs` is a column, and a limit tuned to one device's default
/// would silently stop recording *all* energy for a device polled any slower.
pub fn max_integration_gap_ms(poll_interval_secs: u64) -> i64 {
    MAX_INTEGRATION_GAP_POLLS * (poll_interval_secs.max(1) as i64) * 1_000
}

/// Whether this failed poll is one that should ask whether the device moved.
///
/// True on the tick the device is first declared Lost, and every
/// `MOVED_RECHECK_EVERY_FAILURES` failures after that. Never while the device is
/// merely Connecting: a device that has missed one or two polls is far more
/// likely to be briefly busy than to have changed address.
fn is_moved_recheck_tick(failures: u32) -> bool {
    let Some(since_lost) = failures.checked_sub(LOST_AFTER_FAILURES + 1) else {
        return false;
    };
    since_lost % MOVED_RECHECK_EVERY_FAILURES == 0
}

pub async fn handle_poll_failure(
    pool: &SqlitePool,
    state: &SharedState,
    device_id: i64,
    ip: IpAddr,
    failures: u32,
    error: String,
) -> bool {
    // Whether a fingerprint match moved this device to a new address (see
    // db::upsert_device). Checked on the tick it goes Lost and periodically
    // after — see `MOVED_RECHECK_EVERY_FAILURES` for why "once, exactly" was
    // not enough. An error still reads as "has not moved" for this tick, but a
    // later tick asks again.
    if is_moved_recheck_tick(failures)
        && crate::db::device_moved(pool, device_id, ip)
            .await
            .unwrap_or(false)
    {
        state.write().unwrap().forget_device(&ip);
        return true;
    }

    let status = if failures <= LOST_AFTER_FAILURES {
        ConnStatus::Connecting
    } else {
        ConnStatus::Lost
    };
    let mut app = state.write().unwrap();
    app.conn_status.insert(ip, status);
    app.last_error.insert(ip, error);
    false
}

/// Result of a ping scan - includes both successful and failed attempts.
#[derive(Debug, Clone)]
pub struct PingScanResult {
    pub successful: Vec<Device>,
    pub failed: Vec<IpAddr>,
}

/// Ping-scans local networks and ARP-cache entries.
///
/// Requires either:
///   - `CAP_NET_RAW` capability:  `sudo setcap cap_net_raw+ep ./target/debug/dom`
///   - Or unprivileged ICMP enabled: `sudo sysctl -w net.ipv4.ping_group_range="0 2147483647"`
///   - Or simply run as root.
///
/// surge-ping tries a DGRAM socket first (works without root when
/// ping_group_range covers the current GID) and falls back to RAW automatically.
pub async fn scan_all_networks() -> Result<PingScanResult, std::io::Error> {
    // Phase 1: confirm ARP-cache entries with a dedicated client.
    // Running this first (before the /24 probe) avoids dispatch-map pollution
    // from the hundreds of timed-out tasks in the /24 sweep.
    let known_ips = arp_cache_ips();
    let mut result = ping_batch_with_failures(known_ips).await?;

    // Phase 2: active /24 sweep to discover new devices not yet in the ARP cache.
    // Uses a fresh Client so the Phase-1 dispatch map is gone.
    let scan_ips: Vec<Ipv4Addr> = collect_scan_targets()
        .into_iter()
        .filter(|ip| !result.successful.iter().any(|d| d.ip == IpAddr::V4(*ip)))
        .collect();
    let phase2_result = ping_batch_with_failures(scan_ips).await?;

    result.successful.extend(phase2_result.successful);
    result.failed.extend(phase2_result.failed);

    Ok(result)
}

async fn ping_batch_with_failures(ips: Vec<Ipv4Addr>) -> Result<PingScanResult, std::io::Error> {
    if ips.is_empty() {
        return Ok(PingScanResult {
            successful: Vec::new(),
            failed: Vec::new(),
        });
    }
    let client = Arc::new(Client::new(&Config::default())?);
    let mut successful = Vec::new();
    let mut failed = Vec::new();
    let mut set: JoinSet<(IpAddr, Option<Device>)> = JoinSet::new();
    let mut iter = ips.into_iter();
    for ip in iter.by_ref().take(CONCURRENT_PINGS) {
        let c = Arc::clone(&client);
        let ip_v4 = IpAddr::V4(ip);
        set.spawn(async move {
            let device = ping_once(c, ip_v4).await;
            (ip_v4, device)
        });
    }
    while let Some(result) = set.join_next().await {
        match result {
            Ok((_ip, Some(device))) => {
                successful.push(device);
            }
            Ok((ip, None)) => {
                failed.push(ip);
            }
            Err(_) => {
                // Task failed
            }
        }
        if let Some(ip) = iter.next() {
            let c = Arc::clone(&client);
            let ip_v4 = IpAddr::V4(ip);
            set.spawn(async move {
                let device = ping_once(c, ip_v4).await;
                (ip_v4, device)
            });
        }
    }
    Ok(PingScanResult { successful, failed })
}

fn collect_scan_targets() -> Vec<Ipv4Addr> {
    let mut ips: HashSet<Ipv4Addr> = HashSet::new();
    if let Ok(ifaces) = get_if_addrs() {
        for iface in ifaces {
            if iface.is_loopback() || !is_iface_up(&iface.name) {
                continue;
            }
            let IfAddr::V4(v4) = iface.addr else {
                continue;
            };
            // Clamp to /24 to keep INCOMPLETE ARP entries per batch ≤ 254,
            // preserving gc_thresh3 headroom across consecutive scans.
            let prefix = prefix_len(v4.netmask).max(24);
            let mask = !0u32 << (32 - prefix);
            let network = u32::from(v4.ip) & mask;
            let broadcast = network | !mask;
            for host in (network + 1)..broadcast {
                ips.insert(Ipv4Addr::from(host));
            }
        }
    }
    let mut v: Vec<Ipv4Addr> = ips.into_iter().collect();
    v.sort_unstable();
    v
}

fn is_iface_up(name: &str) -> bool {
    std::fs::read_to_string(format!("/sys/class/net/{name}/operstate"))
        .map(|s| s.trim() == "up")
        .unwrap_or(true)
}

// Returns complete (flag 0x2) non-loopback IPv4 entries from the kernel ARP cache.
fn arp_cache_ips() -> Vec<Ipv4Addr> {
    arp_cache().into_keys().collect()
}

/// Complete (flag 0x2) non-loopback IPv4 -> MAC address entries from the
/// kernel ARP cache, e.g. `"AA:BB:CC:DD:EE:FF"`. The MAC survives a device's
/// IP changing (new DHCP lease, static IP reassigned, etc.), so it's used as
/// a stable per-device fingerprint independent of address — see
/// `db::upsert_device`. Read fresh on every call since the kernel cache is
/// the source of truth and can change between scans.
pub fn arp_cache() -> HashMap<Ipv4Addr, String> {
    let Ok(content) = std::fs::read_to_string("/proc/net/arp") else {
        return HashMap::new();
    };
    parse_arp_cache(&content)
}

/// Parsing half of `arp_cache`, split out so it's testable without touching
/// `/proc/net/arp`.
fn parse_arp_cache(content: &str) -> HashMap<Ipv4Addr, String> {
    let mut macs = HashMap::new();
    for line in content.lines().skip(1) {
        let mut cols = line.split_whitespace();
        let ip_str = cols.next().unwrap_or("");
        let _hw_type = cols.next();
        let flags = cols.next().unwrap_or("0x0");
        let mac = cols.next().unwrap_or("");
        // 0x0 = incomplete/failed; 0x2 = complete; skip incomplete. A
        // complete entry always has a real MAC, but guard against the
        // all-zero placeholder anyway rather than trusting the flag alone.
        if flags == "0x0" || mac == "00:00:00:00:00:00" {
            continue;
        }
        if let Ok(ip) = ip_str.parse::<Ipv4Addr>()
            && !ip.is_loopback()
        {
            macs.insert(ip, mac.to_uppercase());
        }
    }
    macs
}

fn prefix_len(mask: Ipv4Addr) -> u8 {
    u32::from(mask).count_ones() as u8
}

async fn ping_once(client: Arc<Client>, ip: IpAddr) -> Option<Device> {
    // Outer timeout covers send + reply-wait so a blocked send_to (ARP table
    // pressure) cannot stall a task past TIMEOUT.
    tokio::time::timeout(TIMEOUT, async move {
        let mut pinger = client.pinger(ip, PingIdentifier(random())).await;
        match pinger.ping(PingSequence(0), &[0u8; 56]).await {
            Ok((IcmpPacket::V4(_), dur)) => Some(Device {
                ip,
                latency_ms: dur.as_secs_f64() * 1000.0,
            }),
            _ => None,
        }
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARP_TABLE: &str = "IP address       HW type     Flags       HW address            Mask     Device\n\
172.16.20.1      0x1         0x2         aa:bb:cc:dd:ee:01     *        eth0\n\
172.16.20.2      0x1         0x0         00:00:00:00:00:00     *        eth0\n\
172.16.20.3      0x1         0x2         00:00:00:00:00:00     *        eth0\n\
127.0.0.1        0x1         0x2         aa:bb:cc:dd:ee:02     *        lo\n";

    #[test]
    fn parse_arp_cache_keeps_only_complete_non_loopback_entries_uppercased() {
        let macs = parse_arp_cache(ARP_TABLE);
        assert_eq!(macs.len(), 1);
        assert_eq!(
            macs.get(&"172.16.20.1".parse::<Ipv4Addr>().unwrap()),
            Some(&"AA:BB:CC:DD:EE:01".to_string())
        );
        // Incomplete (0x0 flag), all-zero MAC despite 0x2, and loopback: all excluded.
        assert!(!macs.contains_key(&"172.16.20.2".parse::<Ipv4Addr>().unwrap()));
        assert!(!macs.contains_key(&"172.16.20.3".parse::<Ipv4Addr>().unwrap()));
        assert!(!macs.contains_key(&"127.0.0.1".parse::<Ipv4Addr>().unwrap()));
    }

    #[test]
    fn parse_arp_cache_handles_empty_input() {
        assert!(parse_arp_cache("").is_empty());
        assert!(parse_arp_cache("IP address HW type Flags HW address Mask Device\n").is_empty());
    }

    // ── detect_type ───────────────────────────────────────────────────────────

    use crate::fingerprint::{Fingerprint, HttpProbe};

    fn fp(ports: Vec<u16>, probes: Vec<(&str, &str)>) -> Fingerprint {
        let ip = IpAddr::V4(Ipv4Addr::new(172, 16, 20, 30));
        Fingerprint {
            ip,
            open_ports: ports,
            http: probes
                .into_iter()
                .map(|(url, raw)| HttpProbe {
                    ip,
                    port: 80,
                    url: url.to_string(),
                    raw: raw.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn detect_type_identifies_each_device_type() {
        assert_eq!(
            detect_type(&fp(vec![8080, 8883], vec![("/", "sonnenbatterie.de")])),
            Some(DeviceType::SonnenBatterie)
        );
        assert_eq!(
            detect_type(&fp(vec![80], vec![("/report", r#"{"relay":true}"#)])),
            Some(DeviceType::MystromSwitch)
        );
        assert_eq!(
            detect_type(&fp(vec![80], vec![("/", "<h1>MikroTik</h1>")])),
            Some(DeviceType::Mikrotik)
        );
        assert_eq!(
            detect_type(&fp(vec![80], vec![("/", "lwIP Wallbox")])),
            Some(DeviceType::Keba)
        );
    }

    #[test]
    fn detect_type_is_none_for_an_unrecognized_device() {
        assert_eq!(detect_type(&fp(vec![22, 443], vec![])), None);
        assert_eq!(
            detect_type(&fp(vec![80], vec![("/", "<h1>hello</h1>")])),
            None
        );
    }

    #[test]
    fn detect_type_returns_a_single_answer_when_detectors_overlap() {
        // Three detectors key off port 80 plus a substring, so a contrived
        // response can satisfy more than one. The point of detect_type is that
        // exactly one type comes back — the earlier in precedence order —
        // rather than the device being persisted as two types with two poll
        // loops while the UI displayed only the first.
        let both = fp(
            vec![80],
            vec![
                ("/report", r#"{"relay":true}"#),
                ("/", "MikroTik lwIP Wallbox"),
            ],
        );
        assert!(mystrom_switch::detect(&both));
        assert!(mikrotik::detect(&both));
        assert!(keba::detect(&both));
        assert_eq!(detect_type(&both), Some(DeviceType::MystromSwitch));
    }

    #[test]
    fn device_type_display_names_are_distinct() {
        let all = [
            DeviceType::SonnenBatterie,
            DeviceType::MystromSwitch,
            DeviceType::Mikrotik,
            DeviceType::Keba,
        ];
        // Two types sharing a display name would be indistinguishable in the
        // device list.
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.display_name(), b.display_name());
            }
        }
    }

    // ── read_capped ───────────────────────────────────────────────────────────
    //
    // Against real `AsyncRead`s rather than a fake one: an endless reader and a
    // duplex pipe whose other end simply never speaks are exactly the two
    // devices this guards against, and neither needs a socket to stand in for.

    #[tokio::test]
    async fn a_reply_that_ends_is_read_whole() {
        let mut stream = &b"HTTP/1.1 200 OK\r\n\r\n{\"power\":42.0}"[..];
        let got = read_capped(&mut stream, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(got).unwrap(),
            "HTTP/1.1 200 OK\r\n\r\n{\"power\":42.0}"
        );
    }

    #[tokio::test]
    async fn a_device_that_will_not_stop_talking_is_cut_off() {
        // The failure this exists for: `read_to_end` under a timeout bounds how
        // long a device may answer for, not how much it may say.
        let mut endless = tokio::io::repeat(b'x');
        let err = read_capped(&mut endless, Duration::from_secs(30))
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("more than"),
            "cut off by the byte cap, not by the clock: {err:#}"
        );
    }

    #[tokio::test]
    async fn a_device_that_says_nothing_at_all_still_gives_up() {
        // The write half stays alive and silent, so there is no EOF to end the
        // read — only the timeout.
        let (mut ours, _theirs) = tokio::io::duplex(64);
        let err = read_capped(&mut ours, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("timeout"), "{err:#}");
    }

    // ── handle_poll_failure ───────────────────────────────────────────────────
    //
    // All four device poll loops route their error path through this function,
    // so its exit decision is what keeps a migrated device from lingering in
    // the UI forever — and what keeps a merely-offline device from being
    // abandoned. Exercised against a real in-memory SQLite schema rather than a
    // fake, per the no-mocks rule.

    use crate::app::{SwitchReading, new_shared};

    const IP_A: IpAddr = IpAddr::V4(Ipv4Addr::new(172, 16, 20, 21));
    const IP_B: IpAddr = IpAddr::V4(Ipv4Addr::new(172, 16, 20, 22));

    /// In-memory DB holding one myStrom device at `ip`; returns (pool, id).
    async fn db_with_device(ip: IpAddr) -> (SqlitePool, i64) {
        let pool = crate::db::init("sqlite://:memory:").await.unwrap();
        crate::db::upsert_device(&pool, "mystrom_switch", "test", ip, 2, None)
            .await
            .unwrap();
        let id: i64 = sqlx::query_scalar("SELECT id FROM Devices WHERE ip = ?")
            .bind(ip.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        (pool, id)
    }

    /// Live state as a poll loop would have left it after a successful poll.
    fn state_polling(ip: IpAddr) -> SharedState {
        let state = new_shared();
        {
            let mut app = state.write().unwrap();
            app.polled_ips.insert(ip);
            app.conn_status.insert(ip, ConnStatus::Online);
            app.switch_readings.insert(
                ip,
                SwitchReading {
                    power_w: 1.0,
                    relay_on: true,
                    temperature_c: 20.0,
                    updated_at: Utc::now(),
                },
            );
        }
        state
    }

    #[tokio::test]
    async fn poll_failure_below_the_threshold_reports_connecting_and_keeps_polling() {
        let (pool, id) = db_with_device(IP_A).await;
        let state = state_polling(IP_A);

        let exit = handle_poll_failure(
            &pool,
            &state,
            id,
            IP_A,
            LOST_AFTER_FAILURES,
            "boom".to_string(),
        )
        .await;

        assert!(!exit, "must keep polling below the Lost threshold");
        let app = state.read().unwrap();
        assert!(matches!(
            app.conn_status.get(&IP_A),
            Some(ConnStatus::Connecting)
        ));
        assert_eq!(app.last_error.get(&IP_A).map(String::as_str), Some("boom"));
        // Still ours to poll, and the last good reading stays on screen.
        assert!(app.polled_ips.contains(&IP_A));
        assert!(app.switch_readings.contains_key(&IP_A));
    }

    #[tokio::test]
    async fn poll_failure_past_the_threshold_reports_lost_when_the_device_has_not_moved() {
        let (pool, id) = db_with_device(IP_A).await;
        let state = state_polling(IP_A);

        // The DB still has this device at the address we are polling: it is
        // simply down, so the loop must stay alive waiting for it to return.
        let exit = handle_poll_failure(
            &pool,
            &state,
            id,
            IP_A,
            LOST_AFTER_FAILURES + 1,
            "boom".to_string(),
        )
        .await;

        assert!(!exit, "an offline device must not be abandoned");
        let app = state.read().unwrap();
        assert!(matches!(app.conn_status.get(&IP_A), Some(ConnStatus::Lost)));
        assert!(app.polled_ips.contains(&IP_A));
    }

    #[tokio::test]
    async fn poll_failure_exits_and_forgets_the_address_once_the_device_has_moved() {
        let (pool, id) = db_with_device(IP_A).await;
        let state = state_polling(IP_A);

        // Discovery matched this device's fingerprint at a new address and
        // migrated the row in place; a fresh loop is already running for IP_B.
        sqlx::query("UPDATE Devices SET ip = ? WHERE id = ?")
            .bind(IP_B.to_string())
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();

        let exit = handle_poll_failure(
            &pool,
            &state,
            id,
            IP_A,
            LOST_AFTER_FAILURES + 1,
            "boom".to_string(),
        )
        .await;

        assert!(exit, "the loop for the stale address must exit");
        let app = state.read().unwrap();
        // The dead address disappears from the UI entirely rather than
        // remaining as a permanently-Lost row.
        assert!(!app.conn_status.contains_key(&IP_A));
        assert!(!app.last_error.contains_key(&IP_A));
        assert!(!app.switch_readings.contains_key(&IP_A));
        assert!(!app.polled_ips.contains(&IP_A));
    }

    #[tokio::test]
    async fn poll_failure_exits_when_the_device_row_is_gone() {
        let (pool, id) = db_with_device(IP_A).await;
        let state = state_polling(IP_A);

        // A deleted device is "moved" as far as device_moved is concerned;
        // either way there is nothing left to poll.
        sqlx::query("DELETE FROM Devices WHERE id = ?")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();

        let exit = handle_poll_failure(
            &pool,
            &state,
            id,
            IP_A,
            LOST_AFTER_FAILURES + 1,
            "boom".to_string(),
        )
        .await;

        assert!(exit);
        assert!(!state.read().unwrap().polled_ips.contains(&IP_A));
    }

    // ── When a failing device is asked whether it moved ───────────────────────

    #[test]
    fn a_device_that_is_only_connecting_is_not_asked() {
        // One or two missed polls is far more likely to be a busy device than a
        // changed address, and the query is not free.
        for failures in 0..=LOST_AFTER_FAILURES {
            assert!(!is_moved_recheck_tick(failures), "failures = {failures}");
        }
    }

    #[test]
    fn the_tick_it_goes_lost_is_asked() {
        assert!(is_moved_recheck_tick(LOST_AFTER_FAILURES + 1));
    }

    #[test]
    fn it_is_asked_again_later_rather_than_only_once() {
        // The regression: the check used to be an equality against a counter
        // that only rises, so one database error on that single tick meant the
        // device was never checked again for the life of the process.
        let first = LOST_AFTER_FAILURES + 1;
        assert!(is_moved_recheck_tick(first + MOVED_RECHECK_EVERY_FAILURES));
        assert!(is_moved_recheck_tick(
            first + MOVED_RECHECK_EVERY_FAILURES * 2
        ));
        // ...and not on every tick in between, which is what the equality was
        // avoiding in the first place.
        let asked = (first..first + MOVED_RECHECK_EVERY_FAILURES * 2)
            .filter(|f| is_moved_recheck_tick(*f))
            .count();
        assert_eq!(asked, 2);
    }

    #[test]
    fn a_device_that_has_been_down_for_days_is_still_asked() {
        // The counter used to be a `u8`, so it pinned at 255 and the modulo
        // below would have frozen on whatever residue it landed on.
        let first = LOST_AFTER_FAILURES + 1;
        let far = first + MOVED_RECHECK_EVERY_FAILURES * 100_000;
        assert!(
            is_moved_recheck_tick(far),
            "still asking after {far} failures"
        );
    }
}
