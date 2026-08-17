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

use dom::devices::mystrom_switch;

const REPORT_JSON: &str = r#"{"power":123.5,"relay":true,"temperature":22.3}"#;

#[tokio::test]
async fn fetch_report_parses_valid_response() {
    let server = common::MockHttp::start(200, REPORT_JSON).await;
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    let report = mystrom_switch::fetch_report(ip, server.port).await.unwrap();

    assert_eq!(report.power, 123.5);
    assert!(report.relay);
    assert_eq!(report.temperature, 22.3);
}

#[tokio::test]
async fn fetch_report_errors_on_invalid_json() {
    let server = common::MockHttp::start(200, "not json at all").await;
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    let result = mystrom_switch::fetch_report(ip, server.port).await;

    assert!(result.is_err());
}

#[tokio::test]
async fn set_relay_on_sends_correct_url() {
    let server = common::MockHttp::start(200, "").await;
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    mystrom_switch::set_relay(ip, server.port, true)
        .await
        .unwrap();

    let req = server.nth_request(1).await;
    assert!(
        req.contains("GET /relay?state=1"),
        "unexpected request: {req}"
    );
}

#[tokio::test]
async fn set_relay_off_sends_correct_url() {
    let server = common::MockHttp::start(200, "").await;
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    mystrom_switch::set_relay(ip, server.port, false)
        .await
        .unwrap();

    let req = server.nth_request(1).await;
    assert!(
        req.contains("GET /relay?state=0"),
        "unexpected request: {req}"
    );
}

#[tokio::test]
async fn save_and_load_device_roundtrip() {
    let pool = common::db().await;
    let ip: IpAddr = "192.168.1.100".parse().unwrap();

    mystrom_switch::save_device(&pool, ip, "My Switch", None)
        .await
        .unwrap();

    let devices = mystrom_switch::load_all(&pool).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].ip, ip);
    assert_eq!(devices[0].port, mystrom_switch::API_PORT);
}

#[tokio::test]
async fn save_device_is_idempotent() {
    let pool = common::db().await;
    let ip: IpAddr = "192.168.1.101".parse().unwrap();

    mystrom_switch::save_device(&pool, ip, "Switch v1", None)
        .await
        .unwrap();
    mystrom_switch::save_device(&pool, ip, "Switch v2", None)
        .await
        .unwrap();

    let devices = mystrom_switch::load_all(&pool).await.unwrap();
    assert_eq!(devices.len(), 1);
}

#[tokio::test]
async fn fetch_then_db_end_to_end() {
    let pool = common::db().await;
    let server = common::MockHttp::start(200, REPORT_JSON).await;
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    mystrom_switch::save_device(&pool, ip, "Switch", None)
        .await
        .unwrap();
    let devices = mystrom_switch::load_all(&pool).await.unwrap();
    assert_eq!(devices.len(), 1);

    let report = mystrom_switch::fetch_report(ip, server.port).await.unwrap();
    assert!(report.relay);
    assert_eq!(report.power, 123.5);
}
