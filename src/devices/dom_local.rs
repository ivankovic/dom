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

//! The machine Dom is running on, as an entry in its own device list.
//!
//! Every other module in `devices` talks to something across the network. This
//! one describes the host itself, which is a display concern rather than a
//! measurement one: nothing polls it, nothing actuates it, and a `dom_local` row
//! exists so that the computer serving the interface appears in the Devices view
//! alongside everything it is watching.
//!
//! What makes it more than one `INSERT` is the question of which address. A host
//! has many, and most of them are not "where Dom is on the house network":
//! loopback says nothing, IPv6 link-local is not routable and there is one per
//! interface, and a container or virtualisation bridge is genuinely this host but
//! is not on the LAN. Registering all of them made one computer appear four times
//! over. [`detect_local_ips`] applies those exclusions — see
//! `VIRTUAL_IFACE_PREFIXES` for the filter and the direction it is deliberately
//! wrong in.
//!
//! Because that definition has been narrowed since rows were first written,
//! [`forget_stale_addresses`] exists to remove the ones the old definition left
//! behind. It only removes rows with no history attached, so narrowing it again
//! cannot take measurements with it.
use std::net::IpAddr;

use sqlx::{Row, SqlitePool};

pub const NAME: &str = "Dom";
/// Matches the interval the previous hand-written INSERT used.
const DEFAULT_POLL_SECS: i64 = 10;

#[derive(Debug, Clone)]
pub struct DomLocalDevice {
    pub ip: IpAddr,
    pub name: String,
}

/// Interface name prefixes that belong to something running *on* this machine
/// rather than to a network it is on.
///
/// A container or virtualisation bridge has an address, and it is genuinely
/// this host's — but it is not a device on the house network, and registering it
/// makes Dom appear several times over in its own device list, each copy with a
/// poll loop and its own ping history. On this machine that was four rows for one
/// computer: the LAN address plus a Docker bridge and two others.
///
/// Matched on the prefix because that is what these names are: `docker0`,
/// `br-1a2b3c`, `virbr0`, `veth9f21`. Anything not listed is assumed to be a real
/// network interface, which is the safe direction to be wrong — an unrecognised
/// virtual interface shows up as an extra row, where an over-eager filter would
/// hide the machine itself.
const VIRTUAL_IFACE_PREFIXES: [&str; 8] = [
    "docker", "br-", "virbr", "veth", "tun", "tap", "vboxnet", "cni",
];

/// Whether an interface is one of this host's own virtual bridges.
fn is_virtual_iface(name: &str) -> bool {
    VIRTUAL_IFACE_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// Whether an address is one worth registering the local machine under.
///
/// Loopback is excluded because it says nothing about the network. IPv6
/// link-local (`fe80::/10`) is excluded because it is not routable and is not an
/// address anything reaches this machine by — every interface has one, so
/// keeping them would add a row per interface for no reachable address.
fn is_registerable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local(),
        IpAddr::V6(v6) => !v6.is_loopback() && !is_ipv6_link_local(v6),
    }
}

/// `Ipv6Addr::is_unicast_link_local` is unstable, so the `fe80::/10` test is
/// written out.
fn is_ipv6_link_local(ip: std::net::Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

/// This host's own addresses on real networks, as the local machine is registered
/// under.
pub fn detect_local_ips() -> Vec<IpAddr> {
    use if_addrs::get_if_addrs;

    let Ok(ifaces) = get_if_addrs() else {
        return Vec::new();
    };
    ifaces
        .into_iter()
        .filter(|iface| !iface.is_loopback() && !is_virtual_iface(&iface.name))
        .map(|iface| iface.addr.ip())
        .filter(|ip| is_registerable(*ip))
        .collect()
}

/// Removes `dom_local` rows for addresses this host no longer has.
///
/// The list of what counts as "this machine" changes: an address moves, an
/// interface goes away, or — as happened here — the definition itself is
/// narrowed, and rows registered under the old one are left behind. They are not
/// harmless: each one appears in the Devices view as another copy of the machine
/// Dom is running on.
///
/// Only rows with no history attached are removed, which today is all of them —
/// nothing polls the local machine, so a `dom_local` row is a display entry and
/// nothing else. Should that change, the rows carrying measurements stay and the
/// foreign key is never at risk.
pub async fn forget_stale_addresses(pool: &SqlitePool, current: &[IpAddr]) -> anyhow::Result<u64> {
    let known = load_all(pool).await?;
    let mut removed = 0;
    for device in known {
        if current.contains(&device.ip) {
            continue;
        }
        let result = sqlx::query(
            "DELETE FROM Devices
             WHERE type = 'dom_local' AND ip = ?
               AND id NOT IN (SELECT device_id FROM RawDeviceMeasurements)
               AND id NOT IN (SELECT device_id FROM Energy)
               AND id NOT IN (SELECT device_id FROM EnergyStorage)",
        )
        .bind(device.ip.to_string())
        .execute(pool)
        .await?;
        if result.rows_affected() > 0 {
            log::info!(
                "removed {} from the device list: no longer an address of this machine",
                device.ip
            );
            removed += result.rows_affected();
        }
    }
    Ok(removed)
}

/// Save the local machine as a device named "Dom".
///
/// Goes through `db::upsert_device` like every other device type rather than
/// issuing its own SQL. The previous implementation followed an
/// `INSERT OR IGNORE` with an unconditional
/// `UPDATE Devices SET name = ?, type = ? WHERE ip = ?`, which rewrote whatever
/// row happened to sit at that address — so if one of this host's interface
/// addresses ever coincided with a row discovered as another device type, that
/// row's type and name were silently overwritten to `dom_local`.
///
/// Passes no fingerprint: the local machine is found by reading this host's own
/// interface addresses every discovery cycle, so it has no identity to preserve
/// across an IP change the way a remote device does. (Our own address is also
/// absent from our ARP cache, which is where fingerprints come from.)
pub async fn save_device(pool: &SqlitePool, ip: IpAddr) -> anyhow::Result<()> {
    crate::db::upsert_device(pool, "dom_local", NAME, ip, DEFAULT_POLL_SECS, None).await
}

/// Load all configured local devices
pub async fn load_all(pool: &SqlitePool) -> Result<Vec<DomLocalDevice>, sqlx::Error> {
    let rows = sqlx::query("SELECT ip, name FROM Devices WHERE type = 'dom_local'")
        .fetch_all(pool)
        .await?;

    let mut devices = Vec::new();
    for row in rows {
        let ip_str: String = row.get("ip");
        let name: String = row.get("name");
        if let Ok(ip) = ip_str.parse::<IpAddr>() {
            devices.push(DomLocalDevice { ip, name });
        }
    }
    Ok(devices)
}

/// Check if the given IP is one of our local IPs
pub fn is_local_ip(ip: IpAddr) -> bool {
    let local_ips = detect_local_ips();
    local_ips.contains(&ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detected_addresses_are_real_reachable_ones() {
        let local_ips = detect_local_ips();
        // May legitimately be empty in a network-less container.
        for ip in &local_ips {
            assert!(!ip.is_loopback(), "{ip}");
            assert!(is_registerable(*ip), "{ip}");
        }
    }

    #[test]
    fn a_container_or_virtualisation_bridge_is_not_a_device_on_the_network() {
        // These have addresses, and they are genuinely this host's — but each one
        // registered made Dom appear again in its own device list, with a poll
        // loop and a ping history. Four rows for one computer, in practice.
        for name in [
            "docker0",
            "br-1a2b3c4d5e6f",
            "virbr0",
            "virbr0-nic",
            "veth9f21ab",
            "tun0",
            "tap0",
            "vboxnet0",
        ] {
            assert!(is_virtual_iface(name), "{name}");
        }
    }

    #[test]
    fn a_real_interface_is_not_filtered_out() {
        // The safe direction to be wrong is to show an extra row, not to hide the
        // machine — so anything unrecognised has to pass.
        for name in [
            "eth0",
            "enp3s0",
            "wlan0",
            "wlp2s0",
            "eno1",
            "bond0",
            "br0",
            "enx001122",
        ] {
            assert!(!is_virtual_iface(name), "{name}");
        }
    }

    #[test]
    fn link_local_and_loopback_addresses_are_not_registered() {
        // Every interface has an IPv6 link-local address and nothing reaches this
        // machine by one, so keeping them would add a row per interface.
        for ip in [
            "fe80::1",
            "fe80::a00:27ff:fe4e:66a1",
            "::1",
            "127.0.0.1",
            "169.254.1.1",
        ] {
            assert!(!is_registerable(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["172.16.0.3", "192.168.1.20", "2001:db8::1"] {
            assert!(is_registerable(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn this_hosts_own_addresses_are_recognised_as_local() {
        for ip in detect_local_ips() {
            assert!(is_local_ip(ip));
        }
        assert!(!is_local_ip(IpAddr::from([127, 0, 0, 1])));
    }
}
