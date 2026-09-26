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

//! The hardware on the network, and what every kind of it has in common.
//!
//! Each submodule owns one kind of device — [`mystrom_switch`],
//! [`sonnen_batterie`], [`keba`], [`mikrotik`], plus [`dom_local`] for the
//! machine Dom itself runs on. Reading one gives you roughly the shape of all of
//! them:
//!
//! - `NAME` and `API_PORT`, and a `detect` that says whether a
//!   [`crate::fingerprint::Fingerprint`] looks like this device. [`detect_type`]
//!   is what asks each of them.
//! - A struct for the device's own API response, and something that fetches one.
//! - `DeviceRecord`, `save_device`, `load_all` — the device's row.
//! - A `poll_loop`, spawned once per configured device by `main`, which reads,
//!   integrates, writes, and updates [`crate::app::App`].
//!
//! Two diverge on the fetch, for reasons in their own docs: [`keba`] speaks
//! line-oriented UDP and reads two reports at once, and [`mikrotik`] fetches
//! through an authenticated `Session` rather than a free function, because it
//! makes several requests per poll over one connection. [`dom_local`] has no
//! fetch at all — nothing polls the local machine.
//!
//! What lives *here* is everything that would otherwise be written four times,
//! and each piece of it exists because a divergence between those copies was a
//! bug:
//!
//! - **Bounded reads.** [`MAX_RESPONSE_BYTES`] and `read_capped`. A device's
//!   answer is read into memory, so a device that never stops talking must not
//!   be able to exhaust it. It errors rather than truncating, because a
//!   truncated JSON body is a parse failure reported as if the device were
//!   broken.
//! - **Integration across a gap.** [`max_integration_gap_ms`] — a poll loop
//!   integrates power between consecutive readings, which is only meaningful
//!   while the two bracket continuous polling. The worst observed case
//!   attributed 10.8 kWh to one row and inflated a day by 23%.
//! - **Failure handling.** [`handle_poll_failure`] moves a device through
//!   Connecting and Lost, and decides when a run of failures is worth asking
//!   whether the device changed address rather than went away.
//! - **The tick itself.** [`PollTicker`], which re-reads `poll_interval_secs`
//!   from the database periodically so changing a device's interval takes effect
//!   without a restart.
//! - **Saying when a write failed.** [`note_write_failure`] and
//!   [`note_write_ok`]. A fetch that keeps working while every write fails looks
//!   on screen exactly like one that is fine, which it is not.
//!
//! [`scan_all_networks`] and [`arp_cache`] are the discovery side: who answers a
//! ping on every local subnet, and what MAC the kernel has for them — which is
//! how a device that took a new DHCP lease is recognised as the same hardware.
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
// ARP-resolve every destination directly. The neighbor table (gc_thresh3=1024
// on this host) fills up quickly, causing send_to to block until ARP times out
// (~3s). That bound is what shapes the sweep into two passes:
//
//   1. Passive pass — read /proc/net/arp for complete (0x2) entries on non-loopback
//                     interfaces. These are devices that are or have recently been
//                     reachable; we ping-confirm them to get latency and verify liveness.
//   2. Active sweep — ping every host address of every subnet Dom has reason to
//                     believe is occupied. See `scan_subnets` for which those are.
//
// Together this finds: every device in a subnet Dom cares about, whether or not
// it has been seen before, *and* every device elsewhere on the wider network
// that still appears in the kernel's ARP cache.

// Each ping batch uses a fresh Client, so one batch's INCOMPLETE ARP entries
// cannot pollute the next one's dispatch map.
//
// 256 concurrent bounds how many resolutions are in flight, but *not* how many
// INCOMPLETE entries a batch leaves behind: a failed resolution stays in the
// table after its task has gone. What bounds those is the size of the batch,
// which is why the active sweep runs one subnet at a time rather than handing
// every address of every subnet to a single call — at most 254 per batch,
// against a gc_thresh3 of 1024, however many subnets a sweep covers.
const CONCURRENT_PINGS: usize = 256;
// How many subnets one active sweep covers, at most.
//
// Only a bound on the pathological case — a flat network whose ARP cache names
// dozens of subnets — not a budget anything normally reaches: a house has a
// handful. At 254 addresses and roughly half a second each, this caps a sweep
// that finds nothing at all at about four thousand pings and fifteen seconds,
// four hours apart. Interface subnets are never dropped to respect it; see
// `scan_subnets` for what is.
const MAX_SCAN_SUBNETS: usize = 16;
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

/// How many polls pass between re-readings of a device's configured interval.
///
/// `poll_interval_secs` is a column, and a poll loop used to read it once when
/// it started — so changing a device's interval had no effect until the process
/// was restarted. Re-reading it on *every* poll is the obvious fix and the
/// wrong one: it puts a query in front of each poll, which is what SPECS.md's
/// "one transaction per poll" decision exists to avoid, to learn a number that
/// changes perhaps once in the life of an installation.
///
/// Thirty polls is a minute at the default two-second interval — soon enough
/// that a change feels like it took, rare enough to cost nothing.
const INTERVAL_RECHECK_EVERY_POLLS: u32 = 30;

/// A poll loop's ticker, and the configured interval behind it.
///
/// All four device types had the same three lines of ticker setup and the same
/// `poll_interval_secs.max(1) as u64`; this is that, once, plus the re-reading
/// none of them did.
pub struct PollTicker {
    device_id: i64,
    secs: u64,
    ticker: tokio::time::Interval,
    polls_since_recheck: u32,
}

impl PollTicker {
    /// Starts a loop ticking at the device's currently configured interval. The
    /// first tick completes immediately, so a loop polls as soon as it starts.
    #[must_use]
    pub fn new(device_id: i64, poll_interval_secs: i64) -> Self {
        let secs = poll_interval_secs.max(1) as u64;
        Self {
            device_id,
            secs,
            ticker: Self::build(secs, tokio::time::Instant::now()),
            polls_since_recheck: 0,
        }
    }

    fn build(secs: u64, first: tokio::time::Instant) -> tokio::time::Interval {
        let mut ticker = tokio::time::interval_at(first, Duration::from_secs(secs));
        // A poll that overran drops the tick it missed rather than running twice
        // back to back against a device that is evidently already struggling.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker
    }

    /// The interval currently in force, in seconds.
    ///
    /// Read back rather than remembered by the caller because
    /// `max_integration_gap_ms` is derived from it: an interval that changed
    /// while the gap limit did not would either refuse to integrate any
    /// interval at all, or integrate straight across a real gap.
    #[must_use]
    pub fn secs(&self) -> u64 {
        self.secs
    }

    /// Waits for the next poll, picking up a changed interval on the way.
    pub async fn tick(&mut self, pool: &SqlitePool) {
        self.ticker.tick().await;

        self.polls_since_recheck += 1;
        if self.polls_since_recheck >= INTERVAL_RECHECK_EVERY_POLLS {
            self.polls_since_recheck = 0;
            self.recheck(pool).await;
        }
    }

    /// Re-reads the configured interval, and rebuilds the ticker if it changed.
    ///
    /// Split from `tick` so the part worth testing does not need a clock: what
    /// this decides is a function of the database and the interval in force,
    /// and waiting out thirty real poll intervals to reach it would put a
    /// minute into the suite for each test. (`tokio`'s pausable clock is the
    /// other way to do that, and it cannot be used here — it expires sqlx's own
    /// pool and busy timeouts the moment the runtime goes idle.) The same split
    /// `elapsed_window` and `cluster::decide_role` already make.
    ///
    /// A failed read is not a changed interval: the one in force stays, and the
    /// next check asks again. Rebuilding starts the new interval from now rather
    /// than immediately, so a device whose interval was just lengthened is not
    /// polled once more straight away.
    async fn recheck(&mut self, pool: &SqlitePool) {
        let Ok(Some(configured)) = crate::db::poll_interval_secs(pool, self.device_id).await else {
            return;
        };
        let secs = configured.max(1) as u64;
        if secs != self.secs {
            log::info!(
                "device {}: poll interval changed from {}s to {secs}s",
                self.device_id,
                self.secs
            );
            self.secs = secs;
            self.ticker = Self::build(
                secs,
                tokio::time::Instant::now() + Duration::from_secs(secs),
            );
        }
    }
}

/// Records that a poll read its device but could not write what it read.
///
/// Both logged and put in front of the user, which is the rule `logging`'s
/// module doc sets: a message in a file is one nobody sees, so anything a
/// person has to act on belongs in `App` too. This qualifies — see
/// `App::write_error` for why a failed write is invisible without it.
///
/// `what` names the device, e.g. `"sonnen 172.16.0.5"`, so the log line says
/// which loop hit the problem even though the displayed field does not
/// distinguish them.
pub fn note_write_failure(state: &SharedState, what: &str, error: &anyhow::Error) {
    log::warn!("{what}: recording this poll failed: {error:#}");
    state.write().unwrap().write_error = Some(format!("{what}: {error:#}"));
}

/// Clears a recorded write failure, once any loop's write succeeds again.
///
/// Checks under a read lock first because the overwhelmingly common case is
/// that there is nothing to clear, and this runs once per poll per device —
/// every two seconds, for every device in the house.
pub fn note_write_ok(state: &SharedState) {
    if state.read().unwrap().write_error.is_some() {
        state.write().unwrap().write_error = None;
    }
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
///
/// `known_ips` is every address Dom holds a device row for. They are not
/// scanned as addresses — the ARP pass already covers any that answer — but as
/// evidence of which subnets are worth sweeping; see `scan_subnets`.
pub async fn scan_all_networks(known_ips: &[IpAddr]) -> Result<PingScanResult, std::io::Error> {
    // Read once and used by both passes, so what is confirmed and what decides
    // the sweep are the same reading of the cache.
    let arp_ips = arp_cache_ips();

    // Phase 1: confirm ARP-cache entries with a dedicated client.
    // Running this first (before the active sweep) avoids dispatch-map pollution
    // from the hundreds of timed-out tasks the sweep leaves behind.
    let mut result = ping_batch_with_failures(arp_ips.clone()).await?;

    let answered: HashSet<Ipv4Addr> = result
        .successful
        .iter()
        .filter_map(|d| match d.ip {
            IpAddr::V4(v4) => Some(v4),
            IpAddr::V6(_) => None,
        })
        .collect();

    // Phase 2: active sweep of each occupied subnet, to find devices not in the
    // ARP cache — including one that has moved to an address nothing has spoken
    // to yet. One batch per subnet, each with a fresh Client: see
    // `CONCURRENT_PINGS` for why the batch, not the concurrency, is what keeps
    // the neighbour table from overflowing.
    for subnet in scan_subnets(&interface_v4s(), &arp_ips, known_ips) {
        let ips: Vec<Ipv4Addr> = subnet.hosts().filter(|ip| !answered.contains(ip)).collect();
        let swept = ping_batch_with_failures(ips).await?;
        result.successful.extend(swept.successful);
        result.failed.extend(swept.failed);
    }

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

/// An IPv4 subnet the active sweep covers, as a network address and a prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Subnet {
    network: u32,
    prefix: u8,
}

impl Subnet {
    /// The subnet of `prefix` bits that `ip` falls in.
    fn containing(ip: Ipv4Addr, prefix: u8) -> Self {
        Self {
            network: u32::from(ip) & mask_of(prefix),
            prefix,
        }
    }

    fn contains(self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & mask_of(self.prefix) == self.network
    }

    /// Every address in it a host can have.
    ///
    /// The network and broadcast addresses are skipped, as they are not host
    /// addresses of the subnet they delimit. On a flat segment carved into
    /// `/24`-shaped ranges by convention rather than by netmask — the
    /// `172.16.<group>.<host>` scheme this was written against — `x.y.z.0` and
    /// `x.y.z.255` *are* usable, and a device there is missed. Left alone: the
    /// address a person hands a heat pump is not that one, and treating them as
    /// hosts would mean ARP-resolving two more dead addresses per subnet on
    /// every sweep.
    fn hosts(self) -> impl Iterator<Item = Ipv4Addr> {
        let broadcast = self.network | !mask_of(self.prefix);
        (self.network.saturating_add(1)..broadcast).map(Ipv4Addr::from)
    }
}

/// The netmask of a prefix length, as bits.
fn mask_of(prefix: u8) -> u32 {
    match prefix {
        0 => 0,
        p => !0u32 << (32 - p.min(32)),
    }
}

/// Whether `ip` is on the same link as an interface addressed `iface_ip/netmask`
/// — i.e. whether this host reaches it by ARP rather than through a router.
fn on_link(ip: Ipv4Addr, iface_ip: Ipv4Addr, netmask: Ipv4Addr) -> bool {
    let mask = u32::from(netmask);
    u32::from(ip) & mask == u32::from(iface_ip) & mask
}

/// Address and netmask of every up, non-loopback IPv4 interface.
fn interface_v4s() -> Vec<(Ipv4Addr, Ipv4Addr)> {
    let Ok(ifaces) = get_if_addrs() else {
        return Vec::new();
    };
    ifaces
        .into_iter()
        .filter(|iface| !iface.is_loopback() && is_iface_up(&iface.name))
        .filter_map(|iface| match iface.addr {
            IfAddr::V4(v4) => Some((v4.ip, v4.netmask)),
            IfAddr::V6(_) => None,
        })
        .collect()
}

/// The subnets one active sweep covers.
///
/// The sweep used to be each interface's own subnet and nothing else, clamped
/// to a `/24` so a large scope-link prefix could not turn into a million pings.
/// On a flat `/12` addressed as `172.16.<group>.<host>` that clamp meant only
/// `172.16.0.0/24` was ever swept, and every device in every other group — the
/// energy hardware among them — was reachable by discovery *only* while its own
/// poll loop kept its ARP entry warm. That is a circular condition, and it
/// breaks exactly when it is needed: a device that changes address leaves an
/// INCOMPLETE entry at the old one, has none at the new one, and so falls out
/// of the only pass that would have found it. It stays lost until someone
/// notices, which is what happened to a heat pump that moved from
/// `172.16.20.1` to `172.16.20.7`.
///
/// So a subnet is swept when there is reason to think something lives in it:
///
/// - **Each interface's own subnet**, at its own prefix, still clamped to a
///   `/24`. Unchanged, and the only source that needs no prior knowledge: it is
///   what finds the first device on a network Dom has never seen.
/// - **The `/24` around every address Dom holds a device row for.** This is the
///   durable half. A row outlives the ARP cache, a restart, and the device
///   being unplugged for a week, so the subnet a configured device lives in
///   goes on being swept while the device itself is silent — which is the
///   condition under which a move has to be noticed.
/// - **The `/24` around every complete ARP-cache entry.** The live half: a
///   neighbour Dom has never registered is still evidence that its subnet is
///   worth a look.
///
/// A candidate is taken only if it is on-link (reaching it must be ARP, not a
/// route through the gateway — sweeping someone else's network is both rude
/// and pointless) and not already covered. "Already covered" rather than "not
/// equal" so that a small interface prefix keeps its size: an ARP entry inside
/// a `/28` interface's own subnet must not widen that sweep to a `/24`.
///
/// Known-device subnets are considered before ARP-derived ones because
/// `MAX_SCAN_SUBNETS` truncates the tail, and a cache full of transient
/// neighbours must never crowd out the subnet a configured device lives in.
fn scan_subnets(
    ifaces: &[(Ipv4Addr, Ipv4Addr)],
    arp_ips: &[Ipv4Addr],
    known_ips: &[IpAddr],
) -> Vec<Subnet> {
    let mut subnets: Vec<Subnet> = Vec::new();

    for (ip, netmask) in ifaces {
        let subnet = Subnet::containing(*ip, prefix_len(*netmask).max(24));
        if !subnets.contains(&subnet) {
            subnets.push(subnet);
        }
    }

    // Sorted, because `arp_cache` is a `HashMap` and iterating one is
    // deliberately not stable. Unsorted, which subnets `MAX_SCAN_SUBNETS` drops
    // would differ from run to run on a network large enough to reach it — the
    // same unreproducibility `ip_sort_key` exists to keep out of the device
    // list. `known_ips` arrives ordered from `db::all_device_ips`.
    let mut cached: Vec<Ipv4Addr> = arp_ips.to_vec();
    cached.sort_unstable();

    let candidates = known_ips
        .iter()
        .filter_map(|ip| match ip {
            IpAddr::V4(v4) => Some(*v4),
            IpAddr::V6(_) => None,
        })
        .chain(cached);

    for ip in candidates {
        if subnets.len() >= MAX_SCAN_SUBNETS {
            break;
        }
        if subnets.iter().any(|subnet| subnet.contains(ip)) {
            continue;
        }
        if !ifaces
            .iter()
            .any(|(iface_ip, netmask)| on_link(ip, *iface_ip, *netmask))
        {
            continue;
        }
        subnets.push(Subnet::containing(ip, 24));
    }

    subnets
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

    // ── scan_subnets ────────────────────────────────────────────────────────

    fn v4(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// The flat `/12` this was written against: one interface, whole prefix
    /// on-link, devices grouped into `/24`-shaped ranges by convention.
    fn flat_slash_12() -> Vec<(Ipv4Addr, Ipv4Addr)> {
        vec![(v4("172.16.0.3"), v4("255.240.0.0"))]
    }

    fn subnet(s: &str, prefix: u8) -> Subnet {
        Subnet::containing(v4(s), prefix)
    }

    #[test]
    fn an_interfaces_own_subnet_is_swept_clamped_to_a_slash_24() {
        let subnets = scan_subnets(&flat_slash_12(), &[], &[]);

        // Not the whole /12, which would be a million addresses.
        assert_eq!(subnets, vec![subnet("172.16.0.0", 24)]);
    }

    #[test]
    fn a_neighbour_outside_the_interfaces_own_slash_24_adds_its_subnet() {
        // The old behaviour swept 172.16.0.0/24 and stopped, so a device in
        // any other group was invisible unless it was already in the ARP cache.
        let subnets = scan_subnets(&flat_slash_12(), &[v4("172.16.20.2")], &[]);

        assert_eq!(
            subnets,
            vec![subnet("172.16.0.0", 24), subnet("172.16.20.0", 24)]
        );
    }

    #[test]
    fn a_configured_devices_subnet_is_swept_though_nothing_in_it_is_in_the_arp_cache() {
        // The regression this exists for: the heat pump left 172.16.20.1, so
        // there is no complete ARP entry anywhere in that group — but the row
        // for the address it left is enough to keep sweeping the group, which
        // is what finds it at its new address.
        let subnets = scan_subnets(&flat_slash_12(), &[], &[ip("172.16.20.1")]);

        assert!(
            subnets.contains(&subnet("172.16.20.0", 24)),
            "a device row must keep its own subnet in the sweep: {subnets:?}"
        );
        assert!(subnets.iter().any(|s| s.contains(v4("172.16.20.7"))));
    }

    #[test]
    fn an_address_reached_through_a_router_is_not_swept() {
        // Off-link: scanning it would mean scanning somebody else's network.
        let subnets = scan_subnets(&flat_slash_12(), &[v4("10.9.0.4")], &[ip("192.0.2.7")]);

        assert_eq!(subnets, vec![subnet("172.16.0.0", 24)]);
    }

    #[test]
    fn a_neighbour_inside_a_small_interface_prefix_does_not_widen_it_to_a_slash_24() {
        // The interface owns a /28, so its sweep stays a /28: the rest of the
        // surrounding /24 is not on-link and must not be pulled in.
        let ifaces = vec![(v4("192.168.5.20"), v4("255.255.255.240"))];

        let subnets = scan_subnets(&ifaces, &[v4("192.168.5.22")], &[]);

        assert_eq!(subnets, vec![subnet("192.168.5.16", 28)]);
    }

    #[test]
    fn the_subnet_cap_drops_cached_neighbours_before_configured_devices() {
        // More groups in the ARP cache than the cap allows. The device row must
        // survive the truncation; a transient neighbour is what gets dropped.
        let noise: Vec<Ipv4Addr> = (30..60).map(|g| v4(&format!("172.16.{g}.9"))).collect();

        let subnets = scan_subnets(&flat_slash_12(), &noise, &[ip("172.16.20.1")]);

        assert_eq!(subnets.len(), MAX_SCAN_SUBNETS);
        assert!(
            subnets.contains(&subnet("172.16.20.0", 24)),
            "the configured device's subnet was crowded out: {subnets:?}"
        );
    }

    #[test]
    fn a_subnet_is_swept_once_however_many_neighbours_name_it() {
        let arp = vec![v4("172.16.20.2"), v4("172.16.20.3"), v4("172.16.20.4")];

        let subnets = scan_subnets(&flat_slash_12(), &arp, &[ip("172.16.20.1")]);

        assert_eq!(
            subnets,
            vec![subnet("172.16.0.0", 24), subnet("172.16.20.0", 24)]
        );
    }

    #[test]
    fn hosts_covers_the_subnet_without_its_network_and_broadcast_addresses() {
        let hosts: Vec<Ipv4Addr> = subnet("172.16.20.0", 24).hosts().collect();

        assert_eq!(hosts.len(), 254);
        assert_eq!(hosts.first(), Some(&v4("172.16.20.1")));
        assert_eq!(hosts.last(), Some(&v4("172.16.20.254")));
        assert!(hosts.contains(&v4("172.16.20.7")));
    }

    #[test]
    fn hosts_of_a_single_address_subnet_is_empty_rather_than_overflowing() {
        // /31 and /32 have no host range. The top of the address space is the
        // case that would wrap a `network + 1`.
        assert_eq!(subnet("255.255.255.255", 32).hosts().count(), 0);
        assert_eq!(subnet("172.16.20.6", 31).hosts().count(), 0);
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

    // ── PollTicker ────────────────────────────────────────────────────────────
    //
    // Against a real in-memory database rather than a fake, as the
    // `handle_poll_failure` tests below are, and with the recheck counter driven
    // directly so a test does not have to wait out thirty real poll intervals.

    /// An in-memory DB holding one switch polled every `secs`; returns (pool, id).
    async fn db_with_interval(secs: i64) -> (SqlitePool, i64) {
        let pool = crate::db::init("sqlite://:memory:").await.unwrap();
        crate::db::upsert_device(&pool, "mystrom_switch", "test", IP_A, secs, None)
            .await
            .unwrap();
        let id = sqlx::query_scalar::<_, i64>("SELECT id FROM Devices WHERE ip = ?")
            .bind(IP_A.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        (pool, id)
    }

    #[tokio::test]
    async fn a_ticker_starts_at_the_configured_interval_and_polls_at_once() {
        let (pool, id) = db_with_interval(7).await;
        let mut ticker = PollTicker::new(id, 7);
        assert_eq!(ticker.secs(), 7);
        // The first tick completing immediately is what makes a loop poll as
        // soon as it starts rather than an interval into it.
        tokio::time::timeout(Duration::from_millis(200), ticker.tick(&pool))
            .await
            .expect("the first tick is immediate");
    }

    #[tokio::test]
    async fn an_interval_of_zero_or_less_is_read_as_one_second() {
        // `Duration::from_secs(0)` makes `interval` panic, and the column is a
        // plain INTEGER with nothing stopping a 0 or a negative going in.
        let (_pool, id) = db_with_interval(2).await;
        assert_eq!(PollTicker::new(id, 0).secs(), 1);
        assert_eq!(PollTicker::new(id, -5).secs(), 1);
    }

    #[tokio::test]
    async fn a_changed_interval_is_picked_up_without_a_restart() {
        // The finding: the interval was read once when the loop started, so a
        // change to the column did nothing until the process was restarted.
        let (pool, id) = db_with_interval(2).await;
        let mut ticker = PollTicker::new(id, 2);

        sqlx::query("UPDATE Devices SET poll_interval_secs = 60 WHERE id = ?")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();

        ticker.recheck(&pool).await;
        assert_eq!(ticker.secs(), 60);
        assert_eq!(
            crate::devices::max_integration_gap_ms(ticker.secs()),
            600_000,
            "the gap limit follows the interval, or it would reject every \
             interval the device now reports"
        );
    }

    #[tokio::test]
    async fn an_unchanged_interval_is_left_exactly_as_it_was() {
        let (pool, id) = db_with_interval(2).await;
        let mut ticker = PollTicker::new(id, 2);
        ticker.recheck(&pool).await;
        assert_eq!(ticker.secs(), 2);
    }

    #[tokio::test]
    async fn a_deleted_device_leaves_the_interval_in_force() {
        // A read that comes back with nothing is not a changed interval. The
        // loop is about to exit anyway; it must not reach `Duration::from_secs(0)`
        // on the way, which would panic inside `interval`.
        let (pool, id) = db_with_interval(2).await;
        let mut ticker = PollTicker::new(id, 2);
        sqlx::query("DELETE FROM Devices WHERE id = ?")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        ticker.recheck(&pool).await;
        assert_eq!(ticker.secs(), 2);
    }

    #[tokio::test]
    async fn the_interval_is_re_read_periodically_rather_than_before_every_poll() {
        // Putting a query in front of every poll is what SPECS.md's "one
        // transaction per poll" decision exists to avoid; at two seconds that
        // would be a query every two seconds per device, to learn a number that
        // changes perhaps once in the life of an installation.
        let (pool, id) = db_with_interval(2).await;
        let mut ticker = PollTicker::new(id, 2);

        // The first tick completes immediately, so this costs no real time.
        ticker.tick(&pool).await;
        assert_eq!(
            ticker.polls_since_recheck, 1,
            "one poll in, and no re-read owed yet — otherwise there is a query \
             in front of every poll"
        );
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
