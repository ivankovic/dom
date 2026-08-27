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

mod common;

use std::net::IpAddr;

use dom::devices::keba;
use dom::devices::keba::Supply;
use dom::fingerprint::{Fingerprint, HttpProbe};

// Captured live from a KEBA P30 (report 2/3) — see keba::Report2/Report3 for field meanings.
const REPORT2_JSON: &str = r#"{
    "ID": "2", "State": 0, "Error1": 0, "Error2": 0, "Plug": 3, "AuthON": 0,
    "Authreq": 0, "Enable sys": 1, "Enable user": 1, "Max curr": 0,
    "Max curr %": 1000, "Curr HW": 16000, "Curr user": 63000, "Curr FS": 0,
    "Tmo FS": 0, "Curr timer": 0, "Tmo CT": 0, "Setenergy": 0, "Output": 0,
    "Input": 0, "Serial": "21035749", "Sec": 237
}"#;

const REPORT3_JSON: &str = r#"{
    "ID": "3", "U1": 230, "U2": 231, "U3": 229, "I1": 16000, "I2": 16000,
    "I3": 16000, "P": 11040000, "PF": 987, "E pres": 45000, "E total": 170062150,
    "Serial": "21035749", "Sec": 237
}"#;

const WEBFIG_ROOT: &str = r#"<!DOCTYPE html><html><head><title>Wallbox</title></head>
<body>Server: lwIP/1.3.2 (http://www.sics.se/~adam/lwip/)</body></html>"#;

fn fp_with_root_body(ip: &str, open_ports: Vec<u16>, body: &str) -> Fingerprint {
    let ip: std::net::IpAddr = ip.parse().unwrap();
    Fingerprint {
        ip,
        open_ports,
        http: vec![HttpProbe {
            ip,
            port: 80,
            url: "/".to_string(),
            raw: format!(
                "HTTP/1.0 200 OK\r\nServer: lwIP/1.3.2 (http://www.sics.se/~adam/lwip/)\r\n\r\n{body}"
            ),
        }],
    }
}

#[test]
fn detect_recognizes_keba_wallbox_page() {
    let fp = fp_with_root_body("172.16.255.144", vec![80, 502], WEBFIG_ROOT);
    assert!(keba::detect(&fp));
}

#[test]
fn detect_rejects_device_without_port_80() {
    let fp = fp_with_root_body("172.16.255.144", vec![502], WEBFIG_ROOT);
    assert!(!keba::detect(&fp));
}

#[test]
fn detect_rejects_unrelated_http_server() {
    let fp = fp_with_root_body(
        "192.168.1.5",
        vec![80],
        "<html><title>Some Other Page</title></html>",
    );
    assert!(!keba::detect(&fp));
}

#[test]
fn report2_parses_live_capture() {
    let r2: keba::Report2 = serde_json::from_str(REPORT2_JSON).unwrap();
    assert_eq!(r2.state, 0);
    assert_eq!(r2.plug, 3);
    assert_eq!(r2.curr_hw_ma, 16000);
}

#[test]
fn report3_parses_live_capture() {
    let r3: keba::Report3 = serde_json::from_str(REPORT3_JSON).unwrap();
    assert_eq!(r3.power_mw, 11_040_000.0);
    assert_eq!(r3.energy_session_deciwh, 45_000.0);
    assert_eq!(r3.energy_total_deciwh, 170_062_150.0);
}

#[test]
fn state_label_covers_documented_codes() {
    assert_eq!(keba::state_label(0), "Starting");
    assert_eq!(keba::state_label(3), "Charging");
    assert_eq!(keba::state_label(4), "Error");
    assert_eq!(keba::state_label(99), "Unknown");
}

#[test]
fn plug_label_covers_documented_codes() {
    assert_eq!(keba::plug_label(0), "No cable");
    assert_eq!(keba::plug_label(7), "Plugged + locked (station + EV)");
    assert_eq!(keba::plug_label(2), "Unknown");
}

#[test]
fn charging_mode_db_string_roundtrip() {
    assert_eq!(keba::ChargingMode::Disabled.as_db_str(), "disabled");
    assert_eq!(keba::ChargingMode::FullPower.as_db_str(), "full_power");
    assert_eq!(keba::ChargingMode::Eco.as_db_str(), "eco");
    assert_eq!(keba::ChargingMode::EcoCarFirst.as_db_str(), "eco_car_first");
    assert_eq!(
        keba::ChargingMode::from_db_str("full_power"),
        keba::ChargingMode::FullPower
    );
    assert_eq!(
        keba::ChargingMode::from_db_str("disabled"),
        keba::ChargingMode::Disabled
    );
    assert_eq!(
        keba::ChargingMode::from_db_str("eco"),
        keba::ChargingMode::Eco
    );
    assert_eq!(
        keba::ChargingMode::from_db_str("eco_car_first"),
        keba::ChargingMode::EcoCarFirst
    );
    // Unknown/garbage values fail safe to Disabled rather than accidentally enabling charging.
    assert_eq!(
        keba::ChargingMode::from_db_str("time"),
        keba::ChargingMode::Disabled
    );
}

#[tokio::test]
async fn save_and_load_device_roundtrip() {
    let pool = common::db().await;
    let ip: IpAddr = "172.16.255.144".parse().unwrap();

    keba::save_device(&pool, ip, "Wallbox", None).await.unwrap();

    let devices = keba::load_all(&pool).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].ip, ip);
    assert_eq!(devices[0].port, keba::API_PORT);
}

#[tokio::test]
async fn save_device_migrates_ip_when_fingerprint_reappears_elsewhere() {
    let pool = common::db().await;
    let old_ip: IpAddr = "172.16.255.144".parse().unwrap();
    let new_ip: IpAddr = "172.16.20.4".parse().unwrap();

    keba::save_device(&pool, old_ip, "Wallbox", Some("AA:BB:CC:DD:EE:FF"))
        .await
        .unwrap();
    let id_before = keba::load_all(&pool).await.unwrap()[0].id;

    // The wallbox gets a new DHCP lease; the next discovery cycle sees the
    // same MAC at the new address.
    keba::save_device(&pool, new_ip, "Wallbox", Some("AA:BB:CC:DD:EE:FF"))
        .await
        .unwrap();

    let devices = keba::load_all(&pool).await.unwrap();
    assert_eq!(
        devices.len(),
        1,
        "old address must not linger as a second device"
    );
    assert_eq!(devices[0].id, id_before, "same device, id preserved");
    assert_eq!(devices[0].ip, new_ip);
}

#[tokio::test]
async fn save_device_is_idempotent() {
    let pool = common::db().await;
    let ip: IpAddr = "172.16.255.145".parse().unwrap();

    keba::save_device(&pool, ip, "Wallbox v1", None)
        .await
        .unwrap();
    keba::save_device(&pool, ip, "Wallbox v2", None)
        .await
        .unwrap();

    let devices = keba::load_all(&pool).await.unwrap();
    assert_eq!(devices.len(), 1);
}

#[tokio::test]
async fn keba_mode_defaults_to_disabled_and_persists() {
    let pool = common::db().await;
    let ip: IpAddr = "172.16.255.146".parse().unwrap();
    keba::save_device(&pool, ip, "Wallbox", None).await.unwrap();

    // Schema default ('disabled') must match ChargingMode::Disabled without any extra migration.
    let mode = dom::db::load_keba_mode(&pool, ip).await.unwrap();
    assert_eq!(mode, keba::ChargingMode::Disabled);

    dom::db::set_keba_mode(&pool, ip, keba::ChargingMode::FullPower)
        .await
        .unwrap();
    let mode = dom::db::load_keba_mode(&pool, ip).await.unwrap();
    assert_eq!(mode, keba::ChargingMode::FullPower);
}

#[tokio::test]
async fn the_site_supply_round_trips_and_falls_back_safely() {
    let pool = common::db().await;

    // Nothing stored: the default, which is what an existing installation gets.
    assert_eq!(dom::db::get_supply(&pool).await, Supply::default());

    let single = Supply {
        volts: 230.0,
        phases: 1.0,
    };
    dom::db::set_supply(&pool, single).await.unwrap();
    assert_eq!(dom::db::get_supply(&pool).await, single);

    // A value that cannot describe a building is refused rather than stored.
    assert!(
        dom::db::set_supply(
            &pool,
            Supply {
                volts: 0.0,
                phases: 3.0
            }
        )
        .await
        .is_err()
    );
    assert_eq!(
        dom::db::get_supply(&pool).await,
        single,
        "the good value stands"
    );

    // ...and one edited into the database by hand is ignored, not divided by.
    // Eco divides by this on every tick, so it must never return a zero.
    dom::db::set_config(&pool, "supply_phases", "0")
        .await
        .unwrap();
    assert_eq!(dom::db::get_supply(&pool).await, Supply::default());

    dom::db::set_config(&pool, "supply_phases", "not a number")
        .await
        .unwrap();
    let recovered = dom::db::get_supply(&pool).await;
    assert_eq!(recovered.phases, Supply::default().phases);
    assert!(recovered.is_plausible());
}
