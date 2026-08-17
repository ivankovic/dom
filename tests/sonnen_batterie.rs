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

mod common;

use std::net::IpAddr;

use dom::devices::sonnen_batterie;

const STATUS_JSON: &str = r#"{
    "Consumption_W": 1500.0,
    "Production_W": 3200.0,
    "Pac_total_W": -800.0,
    "RSOC": 75.0,
    "GridFeedIn_W": 900.0,
    "RemainingCapacity_Wh": 7500.0
}"#;

#[tokio::test]
async fn fetch_status_parses_valid_response() {
    let server = common::MockHttp::start(200, STATUS_JSON).await;
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    let reading = sonnen_batterie::fetch_status(ip, server.port, "test-key")
        .await
        .unwrap();

    assert_eq!(reading.Consumption_W, 1500.0);
    assert_eq!(reading.Production_W, 3200.0);
    assert_eq!(reading.Pac_total_W, -800.0);
    assert_eq!(reading.RSOC, 75.0);
    assert_eq!(reading.GridFeedIn_W, 900.0);
    assert_eq!(reading.RemainingCapacity_Wh, 7500.0);
}

#[tokio::test]
async fn fetch_status_errors_on_invalid_json() {
    let server = common::MockHttp::start(200, "not json").await;
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    let result = sonnen_batterie::fetch_status(ip, server.port, "key").await;

    assert!(result.is_err());
}

#[tokio::test]
async fn fetch_status_sends_auth_token_header() {
    let server = common::MockHttp::start(200, STATUS_JSON).await;
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    sonnen_batterie::fetch_status(ip, server.port, "secret-api-key")
        .await
        .unwrap();

    let req = server.nth_request(1).await;
    assert!(
        req.contains("Auth-Token: secret-api-key"),
        "Auth-Token header not found in request:\n{req}"
    );
}

#[tokio::test]
async fn save_and_load_device_roundtrip() {
    let pool = common::db().await;
    let ip: IpAddr = "192.168.10.1".parse().unwrap();

    sonnen_batterie::save_device(&pool, ip, "Battery", None)
        .await
        .unwrap();
    dom::db::set_device_api_key(&pool, ip, "api-key-123")
        .await
        .unwrap();

    let devices = sonnen_batterie::load_all(&pool).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].ip, ip);
    assert_eq!(devices[0].port, sonnen_batterie::API_PORT);
    assert_eq!(devices[0].api_key.as_deref(), Some("api-key-123"));
}

#[tokio::test]
async fn save_device_does_not_clobber_existing_api_key_on_rescan() {
    let pool = common::db().await;
    let ip: IpAddr = "192.168.10.3".parse().unwrap();

    sonnen_batterie::save_device(&pool, ip, "Battery", None)
        .await
        .unwrap();
    dom::db::set_device_api_key(&pool, ip, "api-key-123")
        .await
        .unwrap();

    // Simulate a rescan re-detecting the same device.
    sonnen_batterie::save_device(&pool, ip, "Battery", None)
        .await
        .unwrap();

    let devices = sonnen_batterie::load_all(&pool).await.unwrap();
    assert_eq!(devices[0].api_key.as_deref(), Some("api-key-123"));
}

#[tokio::test]
async fn fetch_then_db_end_to_end() {
    let pool = common::db().await;
    let server = common::MockHttp::start(200, STATUS_JSON).await;
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    sonnen_batterie::save_device(&pool, ip, "Battery", None)
        .await
        .unwrap();
    dom::db::set_device_api_key(&pool, ip, "key").await.unwrap();
    let devices = sonnen_batterie::load_all(&pool).await.unwrap();
    assert_eq!(devices.len(), 1);

    let reading = sonnen_batterie::fetch_status(ip, server.port, "key")
        .await
        .unwrap();
    assert_eq!(reading.RSOC, 75.0);
}
