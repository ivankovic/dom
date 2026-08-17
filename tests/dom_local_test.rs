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

use dom::devices::dom_local;

#[tokio::test]
async fn test_save_and_load_dom_local_device() {
    let pool = dom::db::init("sqlite::memory:").await.unwrap();

    // Test saving a local device
    let ip: std::net::IpAddr = "192.168.1.100".parse().unwrap();
    dom_local::save_device(&pool, ip).await.unwrap();

    // Test loading it back
    let devices = dom_local::load_all(&pool).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].ip, ip);
    assert_eq!(devices[0].name, dom_local::NAME);
}

#[tokio::test]
async fn test_local_ip_detection() {
    // This test just verifies that the detection function works
    let local_ips = dom_local::detect_local_ips();

    // Should return at least one non-loopback IP (unless in CI environment)
    if std::env::var("CI").is_err() {
        assert!(!local_ips.is_empty());
    }

    // All IPs should be non-loopback
    for ip in local_ips {
        assert!(!ip.is_loopback());
    }
}

#[tokio::test]
async fn test_is_local_ip_function() {
    let local_ips = dom_local::detect_local_ips();

    // Test that our detected IPs are considered local
    for ip in local_ips {
        assert!(dom_local::is_local_ip(ip));
    }

    // Test that loopback is not considered local
    let loopback: std::net::IpAddr = "127.0.0.1".parse().unwrap();
    assert!(!dom_local::is_local_ip(loopback));

    // Test that a known non-local IP is not considered local
    let non_local: std::net::IpAddr = "8.8.8.8".parse().unwrap();
    assert!(!dom_local::is_local_ip(non_local));
}
