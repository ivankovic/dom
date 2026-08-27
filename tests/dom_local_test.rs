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

#[tokio::test]
async fn addresses_this_machine_no_longer_has_are_dropped() {
    // A `dom_local` row left behind after an address moves — or after the
    // definition of "this machine" is narrowed, which is what excluded the Docker
    // and libvirt bridges — shows in the Devices view as another copy of the
    // machine Dom runs on.
    let pool = common::db().await;
    let lan: std::net::IpAddr = "172.16.0.3".parse().unwrap();
    let docker: std::net::IpAddr = "172.17.0.1".parse().unwrap();
    let bridge: std::net::IpAddr = "192.168.0.1".parse().unwrap();

    for ip in [lan, docker, bridge] {
        dom::devices::dom_local::save_device(&pool, ip)
            .await
            .unwrap();
    }
    assert_eq!(
        dom::devices::dom_local::load_all(&pool)
            .await
            .unwrap()
            .len(),
        3
    );

    let removed = dom::devices::dom_local::forget_stale_addresses(&pool, &[lan])
        .await
        .unwrap();

    assert_eq!(removed, 2);
    let left: Vec<std::net::IpAddr> = dom::devices::dom_local::load_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.ip)
        .collect();
    assert_eq!(left, vec![lan]);
}

#[tokio::test]
async fn a_local_row_that_holds_measurements_is_kept() {
    // Nothing polls the local machine today, so this cannot happen yet — which is
    // exactly why it is worth pinning before something starts to.
    let pool = common::db().await;
    let ip: std::net::IpAddr = "172.16.0.3".parse().unwrap();
    dom::devices::dom_local::save_device(&pool, ip)
        .await
        .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM Devices WHERE ip = ?")
        .bind(ip.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
         VALUES (?, '2026-08-01 12:00:00', 'temperature', 40.0)",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();

    let removed = dom::devices::dom_local::forget_stale_addresses(&pool, &[])
        .await
        .unwrap();

    assert_eq!(removed, 0, "history must not be deleted along with the row");
    assert_eq!(
        dom::devices::dom_local::load_all(&pool)
            .await
            .unwrap()
            .len(),
        1
    );
}
