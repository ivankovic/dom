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

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};
use sqlx::{Row, SqlitePool};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::{MissedTickBehavior, interval, timeout};

use crate::app::{ConnStatus, MikrotikReading, SharedState};
use crate::fingerprint::Fingerprint;

pub const NAME: &str = "MikroTik RouterOS";
pub const API_PORT: u16 = 80;
const DHCP_LEASE_PATH: &str = "/rest/ip/dhcp-server/lease";
const FIREWALL_FILTER_PATH: &str = "/rest/ip/firewall/filter";
/// How often a MikroTik device is polled, in seconds.
///
/// Everything this fetches is slow-moving or coarse-grained: DHCP leases, which
/// discovery reads out of `App` whenever it happens to run, firewall rules, which
/// change when a person changes them, and cumulative byte counters, where a
/// longer interval costs resolution rather than accuracy.
///
/// Nothing here feeds an energy measurement. The battery, wallbox and switches
/// keep their own much faster intervals, because what they report is a rate that
/// has to be integrated rather than a state that can be sampled.
///
/// It used to be ten seconds, which across six devices and three requests each
/// meant nearly two REST round-trips a second, all day, carrying the router
/// administrator password every time. Alongside a ping task doing three ICMP
/// packets every ten seconds per router and every thirty per access point, that
/// was a steady stream of traffic answering questions nothing was asking.
///
/// What is lost at this interval is resolution, not correctness: the traffic
/// counters are cumulative, so a delta over half an hour is exactly the bytes
/// that passed, just without the shape of anything shorter.
const DEFAULT_POLL_SECS: i64 = 1800;
/// The modem's Internet-facing interface, shown in the Network view as
/// "Internet traffic". Only the modem reports this name; fetches against
/// other MikroTik devices (router, APs) simply come back empty and are
/// skipped.
const INTERNET_INTERFACE: &str = "lte1";

/// RouterOS's webfig login page always embeds this copyright string, unique
/// enough among LAN devices that we don't need to also gate on the API ports.
pub fn detect(fp: &Fingerprint) -> bool {
    if !fp.open_ports.contains(&80) {
        return false;
    }
    fp.http.iter().any(|p| p.raw.contains("MikroTik"))
}

// ── API response ──────────────────────────────────────────────────────────────

/// RouterOS's REST API renders most numeric properties as JSON strings for
/// backwards compatibility with the classic API. Accept either form.
fn str_or_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = u64;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a string or integer")
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<u64, E> {
            Ok(v)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<u64, E> {
            Ok(v.max(0) as u64)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
            v.parse().map_err(de::Error::custom)
        }
    }
    deserializer.deserialize_any(V)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DhcpLease {
    pub address: String,
    #[serde(rename = "mac-address", default)]
    pub mac_address: String,
    #[serde(rename = "host-name", default)]
    pub host_name: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FirewallRule {
    #[serde(default)]
    pub chain: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(rename = "bytes", deserialize_with = "str_or_u64", default)]
    pub bytes: u64,
    #[serde(rename = "packets", deserialize_with = "str_or_u64", default)]
    pub packets: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InterfaceStats {
    #[serde(rename = "rx-byte", deserialize_with = "str_or_u64", default)]
    pub rx_byte: u64,
    #[serde(rename = "tx-byte", deserialize_with = "str_or_u64", default)]
    pub tx_byte: u64,
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = (b0 as u32) << 16 | (b1 as u32) << 8 | b2 as u32;
        out.push(TABLE[(n >> 18 & 0x3F) as usize] as char);
        out.push(TABLE[(n >> 12 & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6 & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Port RouterOS serves the REST API over TLS on, when `www-ssl` is enabled.
pub const API_PORT_TLS: u16 = 443;

/// How a request reached the device, and what it saw on the way.
#[derive(Debug, Clone, PartialEq)]
pub enum Transport {
    /// TLS, with the certificate the device presented.
    Tls { fingerprint: String },
    /// Plain HTTP, because the device is not serving `www-ssl`. The credentials
    /// went across the network readable by anything that could see them.
    Cleartext,
}

/// The outcome of one REST call: the body, and how it got there.
pub struct Response {
    pub body: String,
    pub transport: Transport,
}

/// Fetches `path` from RouterOS, over TLS where the device offers it.
///
/// TLS is tried first, on `API_PORT_TLS`, authenticated by pinning — see
/// `devices::tls` for why pinning rather than a certificate authority. If the
/// device is not listening there at all, the request falls back to plain HTTP
/// and says so in the `Transport` it returns, so the caller can tell the user
/// their router password is crossing the network in the clear.
///
/// A **pin mismatch never falls back.** The two failures look similar and are
/// not: "not serving TLS" is a device that has not been set up for it, while
/// "serving a different certificate" is either a device that changed or
/// something pretending to be it. Retrying that in cleartext would hand the
/// credentials to exactly the party the pin just refused.
async fn rest_get(
    ip: IpAddr,
    port: u16,
    path: &str,
    username: &str,
    password: &str,
    pin: Option<&str>,
) -> anyhow::Result<Response> {
    match rest_get_tls(ip, path, username, password, pin).await {
        Ok(response) => return Ok(response),
        Err(e) => {
            // A failure to *reach* TLS is expected on a device that has not had
            // `www-ssl` enabled, and is the case that falls back.
            if !e.is_unreachable {
                return Err(e.error);
            }
            log::debug!("{ip} is not serving HTTPS ({}); using HTTP", e.error);
        }
    }

    let raw = http_request(
        TcpStream::connect(SocketAddr::new(ip, port)),
        ip,
        path,
        username,
        password,
    )
    .await?;
    Ok(Response {
        body: parse_rest_response(&raw)?,
        transport: Transport::Cleartext,
    })
}

/// A TLS attempt that failed, and whether it failed because there was nothing
/// listening — which is the only reason to try cleartext instead.
struct TlsAttemptError {
    error: anyhow::Error,
    is_unreachable: bool,
}

async fn rest_get_tls(
    ip: IpAddr,
    path: &str,
    username: &str,
    password: &str,
    pin: Option<&str>,
) -> Result<Response, TlsAttemptError> {
    let addr = SocketAddr::new(ip, API_PORT_TLS);
    let tcp = match timeout(Duration::from_secs(5), TcpStream::connect(addr)).await {
        Ok(Ok(tcp)) => tcp,
        Ok(Err(e)) => {
            return Err(TlsAttemptError {
                error: anyhow::anyhow!("connect failed: {e}"),
                is_unreachable: true,
            });
        }
        Err(_) => {
            return Err(TlsAttemptError {
                error: anyhow::anyhow!("connect timeout"),
                is_unreachable: true,
            });
        }
    };

    let (config, observed) = crate::devices::tls::pinned_config(pin.map(str::to_string));
    // The certificate is identified by its key, not by a name, so any valid
    // server name serves — see `devices::tls`.
    let server_name = tokio_rustls::rustls::pki_types::ServerName::IpAddress(ip.into());
    let stream = match tokio_rustls::TlsConnector::from(config)
        .connect(server_name, tcp)
        .await
    {
        Ok(stream) => stream,
        Err(e) => {
            // A rejected pin is a refusal, not an unreachable device.
            return Err(TlsAttemptError {
                error: anyhow::anyhow!("{e}"),
                is_unreachable: false,
            });
        }
    };

    let fingerprint = observed
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or_default();

    match http_exchange(stream, ip, path, username, password).await {
        Ok(raw) => match parse_rest_response(&raw) {
            Ok(body) => Ok(Response {
                body,
                transport: Transport::Tls { fingerprint },
            }),
            Err(e) => Err(TlsAttemptError {
                error: e,
                is_unreachable: false,
            }),
        },
        Err(e) => Err(TlsAttemptError {
            error: e,
            is_unreachable: false,
        }),
    }
}

/// Connects, then runs the HTTP exchange over whatever the future yields.
async fn http_request(
    connect: impl std::future::Future<Output = std::io::Result<TcpStream>>,
    ip: IpAddr,
    path: &str,
    username: &str,
    password: &str,
) -> anyhow::Result<String> {
    let stream = timeout(Duration::from_secs(5), connect)
        .await
        .context("connect timeout")?
        .context("connect failed")?;
    http_exchange(stream, ip, path, username, password).await
}

/// One HTTP/1.1 request and its response, over any stream.
///
/// Written once and used for both transports so the request cannot differ
/// between them — the plain and encrypted paths send byte-identical bytes.
async fn http_exchange<S>(
    mut stream: S,
    ip: IpAddr,
    path: &str,
    username: &str,
    password: &str,
) -> anyhow::Result<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let auth = base64_encode(format!("{username}:{password}").as_bytes());
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {ip}\r\nAuthorization: Basic {auth}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .await
        .context("send request")?;

    let buf = crate::devices::read_capped(&mut stream, Duration::from_secs(10))
        .await
        .context("read failed")?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Splits a RouterOS response into a body, or an error naming the status.
fn parse_rest_response(raw: &str) -> anyhow::Result<String> {
    let status = raw
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok());
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or(raw).trim();

    if status != Some(200) {
        let code = status
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".to_string());
        anyhow::bail!("HTTP {code} from RouterOS: {body}");
    }
    Ok(body.to_string())
}

/// One device's REST endpoint, with everything a request needs.
///
/// Introduced so the pin travels with the credentials rather than as a fourth
/// argument on every call, and so the transport a poll actually used can be
/// reported back once instead of per request.
pub struct Session<'a> {
    pub ip: IpAddr,
    pub port: u16,
    pub username: &'a str,
    pub password: &'a str,
    /// The certificate fingerprint pinned for this device, if any.
    pub pin: Option<&'a str>,
}

impl Session<'_> {
    async fn get(&self, path: &str) -> anyhow::Result<Response> {
        rest_get(
            self.ip,
            self.port,
            path,
            self.username,
            self.password,
            self.pin,
        )
        .await
    }

    pub async fn dhcp_leases(&self) -> anyhow::Result<(Vec<DhcpLease>, Transport)> {
        let r = self.get(DHCP_LEASE_PATH).await?;
        Ok((
            serde_json::from_str(&r.body).context("parse JSON")?,
            r.transport,
        ))
    }

    pub async fn firewall_rules(&self) -> anyhow::Result<Vec<FirewallRule>> {
        let r = self.get(FIREWALL_FILTER_PATH).await?;
        serde_json::from_str(&r.body).context("parse JSON")
    }

    /// Fetches rx/tx byte counters for `INTERNET_INTERFACE`. Returns `None` if
    /// the device has no such interface (e.g. the router or an AP, rather than
    /// the modem) rather than treating that as a poll failure.
    pub async fn internet_traffic(&self) -> anyhow::Result<Option<InterfaceStats>> {
        let path = format!("/rest/interface?name={INTERNET_INTERFACE}");
        let r = self.get(&path).await?;
        let mut stats: Vec<InterfaceStats> = serde_json::from_str(&r.body).context("parse JSON")?;
        Ok(if stats.is_empty() {
            None
        } else {
            Some(stats.remove(0))
        })
    }
}

// ── Database ──────────────────────────────────────────────────────────────────

pub struct DeviceRecord {
    pub id: i64,
    pub ip: IpAddr,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub poll_interval_secs: i64,
    pub label: Option<String>,
}

/// Registers a discovered device. Credentials are never known at scan time —
/// they're configured separately (see `db::set_device_login`) and preserved
/// across rescans; only name/poll_interval are touched here. This also means
/// different MikroTik devices (router, APs, ...) can each hold their own
/// username/password without any of it passing through source code.
/// `fingerprint` (e.g. a MAC address) lets a device that changed IP be
/// recognized and migrated in place, credentials and all, rather than
/// registered as a second device with no login — see `db::upsert_device`.
pub async fn save_device(
    pool: &SqlitePool,
    ip: IpAddr,
    name: &str,
    fingerprint: Option<&str>,
) -> anyhow::Result<()> {
    crate::db::upsert_device(pool, "mikrotik", name, ip, DEFAULT_POLL_SECS, fingerprint).await
}

pub async fn load_all(pool: &SqlitePool) -> anyhow::Result<Vec<DeviceRecord>> {
    let rows = sqlx::query(
        "SELECT id, ip, username, password, poll_interval_secs, label
         FROM Devices WHERE type = 'mikrotik'",
    )
    .fetch_all(pool)
    .await?;

    let mut out = Vec::new();
    for row in rows {
        let ip_str: String = row.get("ip");
        let ip = ip_str
            .parse::<IpAddr>()
            .with_context(|| format!("bad IP in DB: {ip_str}"))?;
        let label: Option<String> = row.get("label");
        out.push(DeviceRecord {
            id: row.get("id"),
            ip,
            port: API_PORT,
            username: row.get::<Option<String>, _>("username").unwrap_or_default(),
            password: row.get::<Option<String>, _>("password").unwrap_or_default(),
            poll_interval_secs: row.get("poll_interval_secs"),
            label,
        });
    }
    Ok(out)
}

// ── Poll loop ─────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum PollOutcome {
    Online {
        leases: Vec<DhcpLease>,
        firewall_rules: Vec<FirewallRule>,
        /// Set when the (best-effort) firewall fetch failed even though
        /// leases succeeded — surfaced without downgrading connectivity.
        firewall_warning: Option<String>,
    },
    Disconnected {
        error: String,
    },
}

/// Combines the independently-fetched lease/firewall results into a single
/// outcome. Leases are load-bearing — the scanner fingerprints devices from
/// the DHCP lease table — so a lease-fetch success always reports Online,
/// carrying forward the previous poll's firewall data (rather than
/// discarding the working lease data) when the firewall fetch fails.
/// Requiring *both* to succeed, as this used to, meant a firewall-only
/// failure (e.g. a router without firewall filter rules configured) silently
/// discarded working lease data on every single poll.
fn decide_poll_outcome(
    leases: anyhow::Result<Vec<DhcpLease>>,
    rules: anyhow::Result<Vec<FirewallRule>>,
    prev_firewall_rules: Vec<FirewallRule>,
) -> PollOutcome {
    match leases {
        Ok(leases) => {
            let (firewall_rules, firewall_warning) = match rules {
                Ok(rules) => (rules, None),
                Err(e) => (
                    prev_firewall_rules,
                    Some(format!("firewall (non-critical): {e:#}")),
                ),
            };
            PollOutcome::Online {
                leases,
                firewall_rules,
                firewall_warning,
            }
        }
        Err(e) => PollOutcome::Disconnected {
            error: format!("leases: {e:#}"),
        },
    }
}

/// Persists the bytes transferred since the previous poll for a traffic
/// metric (`traffic_rx_bytes` / `traffic_tx_bytes`) in RawDeviceMeasurements,
/// alongside the rest of this device's raw measurements.
async fn save_traffic_delta(
    pool: &SqlitePool,
    device_id: i64,
    t: &str,
    metric: &str,
    delta_bytes: u64,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
         VALUES (?, ?, ?, ?)",
    )
    .bind(device_id)
    .bind(t)
    .bind(metric)
    .bind(delta_bytes as f64)
    .execute(pool)
    .await?;
    Ok(())
}

/// Records what a poll's transport implies: pins a certificate seen for the first
/// time, clears any standing alert once a connection succeeds, and raises one
/// when the pinned certificate no longer matches.
///
/// Reads the outcome of the first request of the tick rather than making its own,
/// so this costs nothing.
async fn record_transport(
    pool: &SqlitePool,
    state: &SharedState,
    device: &DeviceRecord,
    pin: Option<&str>,
    outcome: &anyhow::Result<(Vec<DhcpLease>, Transport)>,
) {
    match outcome {
        Ok((_, Transport::Tls { fingerprint })) => {
            if pin.is_none() && !fingerprint.is_empty() {
                // Trust on first use: whatever it presented becomes the pin.
                match crate::db::set_tls_pin(pool, device.ip, fingerprint).await {
                    Ok(()) => log::info!(
                        "pinned the TLS certificate for {} ({})",
                        device.ip,
                        crate::devices::tls::short_fingerprint(fingerprint)
                    ),
                    Err(e) => log::warn!("could not pin {}'s certificate: {e:#}", device.ip),
                }
            }
            state.write().unwrap().cert_alerts.remove(&device.ip);
            state.write().unwrap().cleartext_devices.remove(&device.ip);
        }
        Ok((_, Transport::Cleartext)) => {
            state.write().unwrap().cert_alerts.remove(&device.ip);
            state.write().unwrap().cleartext_devices.insert(device.ip);
        }
        Err(e) => {
            // Only a pin mismatch becomes an alert; an ordinary failure is just a
            // failed poll, already reported as one.
            if let Some(crate::devices::tls::PinFailure::Changed { expected, observed }) =
                crate::devices::tls::pin_failure(&format!("{e:#}"), pin)
            {
                log::warn!(
                    "{} presented a different TLS certificate ({}, pinned {}); refusing to send \
                     credentials until it is accepted",
                    device.ip,
                    crate::devices::tls::short_fingerprint(&observed),
                    crate::devices::tls::short_fingerprint(&expected)
                );
                state
                    .write()
                    .unwrap()
                    .cert_alerts
                    .insert(device.ip, crate::app::CertAlert { expected, observed });
            }
        }
    }
}

pub async fn poll_loop(pool: SqlitePool, device: DeviceRecord, state: SharedState) {
    let secs = device.poll_interval_secs.max(1) as u64;
    let mut ticker = interval(Duration::from_secs(secs));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut failures: u32 = 0;
    // Previous poll's raw counters, kept locally per poll loop (i.e. per
    // device) so a delta can be computed without a DB round-trip.
    let mut prev_traffic: Option<InterfaceStats> = None;

    loop {
        ticker.tick().await;

        // Re-read each tick, so accepting a changed certificate in the UI takes
        // effect on the next poll rather than at the next restart.
        let pin = crate::db::get_tls_pin(&pool, device.ip)
            .await
            .unwrap_or(None);
        let session = Session {
            ip: device.ip,
            port: device.port,
            username: &device.username,
            password: &device.password,
            pin: pin.as_deref(),
        };

        // Timed because the Network view's slow/ok distinction is derived from
        // it — see `app::network_status`. The request was going to happen
        // anyway, so this replaces a dedicated ping task at no cost.
        let started = std::time::Instant::now();
        let leases = session.dhcp_leases().await;
        let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
        let rules = session.firewall_rules().await;
        if leases.is_ok() {
            state
                .write()
                .unwrap()
                .poll_latency_ms
                .insert(device.ip, latency_ms);
        }
        record_transport(&pool, &state, &device, pin.as_deref(), &leases).await;
        let leases = leases.map(|(l, _)| l);

        if let Ok(Some(traffic)) = session.internet_traffic().await {
            if let Some(prev) = &prev_traffic {
                // Skip a sample straddling a counter reset/reboot (new < old)
                // rather than recording a bogus huge delta from wraparound.
                if traffic.rx_byte >= prev.rx_byte && traffic.tx_byte >= prev.tx_byte {
                    let t = crate::devices::ts(chrono::Utc::now());
                    // Logged rather than discarded, as the other three poll
                    // loops do. These deltas are the only source of the
                    // Internet-traffic chart, and a write that fails takes that
                    // interval's traffic with it — `prev_traffic` advances
                    // regardless, so it is not made up on the next pass.
                    for (metric, delta) in [
                        ("traffic_rx_bytes", traffic.rx_byte - prev.rx_byte),
                        ("traffic_tx_bytes", traffic.tx_byte - prev.tx_byte),
                    ] {
                        if let Err(e) =
                            save_traffic_delta(&pool, device.id, &t, metric, delta).await
                        {
                            log::warn!("mikrotik {}: recording {metric} failed: {e:#}", device.ip);
                        }
                    }
                }
            }
            prev_traffic = Some(traffic);
        }

        let prev_firewall_rules = state
            .read()
            .unwrap()
            .mikrotik_readings
            .get(&device.ip)
            .map(|r| r.firewall_rules.clone())
            .unwrap_or_default();

        match decide_poll_outcome(leases, rules, prev_firewall_rules) {
            PollOutcome::Online {
                leases,
                firewall_rules,
                firewall_warning,
            } => {
                failures = 0;
                let mut app = state.write().unwrap();
                app.mikrotik_readings.insert(
                    device.ip,
                    MikrotikReading {
                        leases,
                        firewall_rules,
                        updated_at: chrono::Utc::now(),
                    },
                );
                app.conn_status.insert(device.ip, ConnStatus::Online);
                match firewall_warning {
                    Some(w) => {
                        app.last_error.insert(device.ip, w);
                    }
                    None => {
                        app.last_error.remove(&device.ip);
                    }
                }
            }
            PollOutcome::Disconnected { error } => {
                failures = failures.saturating_add(1);
                if crate::devices::handle_poll_failure(
                    &pool, &state, device.id, device.ip, failures, error,
                )
                .await
                {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(addr: &str) -> DhcpLease {
        DhcpLease {
            address: addr.to_string(),
            mac_address: "AA:BB:CC:DD:EE:FF".to_string(),
            host_name: None,
            comment: None,
            status: "bound".to_string(),
        }
    }

    fn rule() -> FirewallRule {
        FirewallRule {
            chain: "forward".to_string(),
            action: "accept".to_string(),
            comment: None,
            bytes: 100,
            packets: 1,
        }
    }

    /// The regression this guards: a firewall-only fetch failure used to be
    /// combined with leases via `match (leases, rules) { (Ok, Ok) => ..., _
    /// => disconnected }`, discarding a successful lease fetch — the data
    /// the scanner needs to discover DHCP-known devices — every single poll.
    #[test]
    fn leases_ok_firewall_err_reports_online_and_keeps_previous_firewall_data() {
        let leases = vec![lease("192.168.1.100")];
        let prev_firewall = vec![rule()];

        let outcome = decide_poll_outcome(
            Ok(leases.clone()),
            Err(anyhow::anyhow!("no such item (firewall)")),
            prev_firewall.clone(),
        );

        match outcome {
            PollOutcome::Online {
                leases: got_leases,
                firewall_rules,
                firewall_warning,
            } => {
                assert_eq!(got_leases, leases);
                assert_eq!(firewall_rules, prev_firewall);
                assert!(firewall_warning.unwrap().contains("firewall"));
            }
            PollOutcome::Disconnected { error } => {
                panic!("expected Online despite firewall failure, got Disconnected({error})")
            }
        }
    }

    #[test]
    fn leases_and_firewall_both_ok_reports_online_with_no_warning() {
        let leases = vec![lease("192.168.1.100")];
        let rules = vec![rule()];

        let outcome = decide_poll_outcome(Ok(leases.clone()), Ok(rules.clone()), vec![]);

        match outcome {
            PollOutcome::Online {
                leases: got_leases,
                firewall_rules,
                firewall_warning,
            } => {
                assert_eq!(got_leases, leases);
                assert_eq!(firewall_rules, rules);
                assert!(firewall_warning.is_none());
            }
            PollOutcome::Disconnected { error } => panic!("expected Online, got {error}"),
        }
    }

    #[test]
    fn leases_err_reports_disconnected_regardless_of_firewall() {
        let outcome = decide_poll_outcome(
            Err(anyhow::anyhow!("connection refused")),
            Ok(vec![rule()]),
            vec![],
        );

        match outcome {
            PollOutcome::Disconnected { error } => assert!(error.contains("leases")),
            PollOutcome::Online { .. } => panic!("expected Disconnected when leases fail"),
        }
    }

    #[test]
    fn the_traffic_chart_bucket_matches_the_poll_interval() {
        // A bucket narrower than the poll interval leaves most bars empty and
        // divides one poll's bytes by a span it did not cover.
        assert_eq!(
            crate::db::traffic_bucket_minutes() * 60,
            DEFAULT_POLL_SECS,
            "the chart's bar width and the modem's poll interval must agree"
        );
    }
}
