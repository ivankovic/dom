mod common;

use std::net::IpAddr;

use dom::devices::mikrotik;
use dom::fingerprint::{Fingerprint, HttpProbe};

const LEASES_JSON: &str = r#"[
    {".id":"*1","address":"192.168.1.50","mac-address":"AA:BB:CC:00:11:22","host-name":"laptop","status":"bound"},
    {".id":"*2","address":"192.168.1.51","mac-address":"AA:BB:CC:00:11:23","status":"waiting"}
]"#;

const FIREWALL_JSON: &str = r#"[
    {"chain":"input","action":"accept","comment":"established","bytes":"1048576","packets":"2048"},
    {"chain":"forward","action":"drop","bytes":2048,"packets":10}
]"#;

const WEBFIG_ROOT: &str = r#"<!doctype html><title>RouterOS</title><div class="credits"><a href="https://mikrotik.com/">© MikroTik</a></div>"#;

fn fp_with_root_body(ip: &str, open_ports: Vec<u16>, body: &str) -> Fingerprint {
    Fingerprint {
        ip: ip.parse().unwrap(),
        open_ports,
        http: vec![HttpProbe {
            port: 80,
            url: "/".to_string(),
            raw: format!("HTTP/1.1 200 OK\r\n\r\n{body}"),
        }],
    }
}

#[test]
fn detect_recognizes_mikrotik_webfig_page() {
    let fp = fp_with_root_body("172.16.0.1", vec![22, 80, 8291, 8728], WEBFIG_ROOT);
    assert!(mikrotik::detect(&fp));
}

#[test]
fn detect_rejects_device_without_port_80() {
    let fp = fp_with_root_body("172.16.0.1", vec![22, 8728], WEBFIG_ROOT);
    assert!(!mikrotik::detect(&fp));
}

#[test]
fn detect_rejects_unrelated_http_server() {
    let fp = fp_with_root_body(
        "192.168.1.5",
        vec![80],
        "<html><title>Some Other Router</title></html>",
    );
    assert!(!mikrotik::detect(&fp));
}

#[tokio::test]
async fn fetch_dhcp_leases_parses_valid_response() {
    let server = common::MockHttp::start(200, LEASES_JSON).await;
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    let leases = mikrotik::fetch_dhcp_leases(ip, server.port, "admin", "pass")
        .await
        .unwrap();

    assert_eq!(leases.len(), 2);
    assert_eq!(leases[0].address, "192.168.1.50");
    assert_eq!(leases[0].mac_address, "AA:BB:CC:00:11:22");
    assert_eq!(leases[0].host_name.as_deref(), Some("laptop"));
    assert_eq!(leases[0].status, "bound");
    assert_eq!(leases[1].host_name, None);
}

#[tokio::test]
async fn fetch_dhcp_leases_errors_on_invalid_json() {
    let server = common::MockHttp::start(200, "not json at all").await;
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    let result = mikrotik::fetch_dhcp_leases(ip, server.port, "admin", "pass").await;

    assert!(result.is_err());
}

#[tokio::test]
async fn fetch_dhcp_leases_reports_http_status_on_unauthorized() {
    let server = common::MockHttp::start(401, r#"{"error":401,"message":"Unauthorized"}"#).await;
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    let result = mikrotik::fetch_dhcp_leases(ip, server.port, "admin", "wrong").await;

    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("401"),
        "expected status code in error, got: {err}"
    );
    assert!(
        err.contains("Unauthorized"),
        "expected body in error, got: {err}"
    );
}

#[tokio::test]
async fn fetch_firewall_rules_accepts_string_and_numeric_counters() {
    let server = common::MockHttp::start(200, FIREWALL_JSON).await;
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    let rules = mikrotik::fetch_firewall_rules(ip, server.port, "admin", "pass")
        .await
        .unwrap();

    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].chain, "input");
    assert_eq!(rules[0].bytes, 1_048_576);
    assert_eq!(rules[0].packets, 2048);
    assert_eq!(rules[0].comment.as_deref(), Some("established"));
    assert_eq!(rules[1].bytes, 2048);
    assert_eq!(rules[1].packets, 10);
    assert_eq!(rules[1].comment, None);
}

#[tokio::test]
async fn fetch_sends_basic_auth_header() {
    let server = common::MockHttp::start(200, "[]").await;
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    mikrotik::fetch_dhcp_leases(ip, server.port, "admin", "pass")
        .await
        .unwrap();

    let req = server.nth_request(1).await;
    // base64("admin:pass") == "YWRtaW46cGFzcw=="
    assert!(
        req.contains("Authorization: Basic YWRtaW46cGFzcw=="),
        "Authorization header not found in request:\n{req}"
    );
    assert!(
        req.contains("GET /rest/ip/dhcp-server/lease"),
        "unexpected request: {req}"
    );
}

#[tokio::test]
async fn save_and_load_device_roundtrip() {
    let pool = common::db().await;
    let ip: IpAddr = "172.16.0.1".parse().unwrap();

    mikrotik::save_device(&pool, ip, "Router", None)
        .await
        .unwrap();
    dom::db::set_device_login(&pool, ip, "admin", "secret")
        .await
        .unwrap();

    let devices = mikrotik::load_all(&pool).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].ip, ip);
    assert_eq!(devices[0].port, mikrotik::API_PORT);
    assert_eq!(devices[0].username, "admin");
    assert_eq!(devices[0].password, "secret");
}

#[tokio::test]
async fn save_device_is_idempotent() {
    let pool = common::db().await;
    let ip: IpAddr = "172.16.0.2".parse().unwrap();

    mikrotik::save_device(&pool, ip, "Router v1", None)
        .await
        .unwrap();
    mikrotik::save_device(&pool, ip, "Router v2", None)
        .await
        .unwrap();

    let devices = mikrotik::load_all(&pool).await.unwrap();
    assert_eq!(devices.len(), 1);
}

#[tokio::test]
async fn save_device_does_not_clobber_existing_login_on_rescan() {
    let pool = common::db().await;
    let ip: IpAddr = "172.16.0.4".parse().unwrap();

    mikrotik::save_device(&pool, ip, "Router", None)
        .await
        .unwrap();
    dom::db::set_device_login(&pool, ip, "admin", "secret")
        .await
        .unwrap();

    // Simulate a rescan re-detecting the same device.
    mikrotik::save_device(&pool, ip, "Router", None)
        .await
        .unwrap();

    let devices = mikrotik::load_all(&pool).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].username, "admin");
    assert_eq!(devices[0].password, "secret");
}

#[tokio::test]
async fn fetch_then_db_end_to_end() {
    let pool = common::db().await;
    let server = common::MockHttp::start(200, LEASES_JSON).await;
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    mikrotik::save_device(&pool, ip, "Router", None)
        .await
        .unwrap();
    dom::db::set_device_login(&pool, ip, "admin", "pass")
        .await
        .unwrap();
    let devices = mikrotik::load_all(&pool).await.unwrap();
    assert_eq!(devices.len(), 1);

    let leases = mikrotik::fetch_dhcp_leases(ip, server.port, "admin", "pass")
        .await
        .unwrap();
    assert_eq!(leases.len(), 2);
}
