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

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio::time::{Instant, timeout};

// 8291 = MikroTik Winbox, 8728 = MikroTik API (unencrypted). Surfaced in the
// device's open_ports so the UI shows them even though we talk to the router
// over the REST API on port 80 rather than the binary API protocol.
const PROBE_PORTS: &[u16] = &[
    22, 23, 80, 443, 554, 1883, 7547, 8080, 8291, 8443, 8728, 8883,
];
const HTTP_PORTS: &[u16] = &[80, 8080];
const TCP_TIMEOUT: Duration = Duration::from_millis(500);
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct HttpProbe {
    pub port: u16,
    /// Path (and optional query string) that was requested, e.g. "/" or "/admin/login".
    pub url: String,
    /// Full raw HTTP response: status line + headers + body, exactly as received.
    pub raw: String,
}

#[derive(Debug, Clone)]
pub struct Fingerprint {
    pub ip: IpAddr,
    pub open_ports: Vec<u16>,
    /// HTTP probes, keyed logically by (port, url); ordered by fetch sequence.
    pub http: Vec<HttpProbe>,
}

pub async fn fingerprint(ip: IpAddr) -> Fingerprint {
    let open_ports = port_scan(ip).await;

    let mut http: Vec<HttpProbe> = Vec::new();
    for &port in &open_ports {
        if HTTP_PORTS.contains(&port) {
            probe_http(ip, port, "/", &mut http).await;
            // Probe /report for myStrom switch detection; harmless 404 on other devices.
            probe_http(ip, port, "/report", &mut http).await;
        }
    }

    Fingerprint {
        ip,
        open_ports,
        http,
    }
}

const MAX_REDIRECTS: usize = 5;

/// Fetches `path` on `ip:port`, stores the probe, then follows up to MAX_REDIRECTS 301s.
async fn probe_http(ip: IpAddr, port: u16, initial_path: &str, probes: &mut Vec<HttpProbe>) {
    let mut cur_ip = ip;
    let mut cur_port = port;
    let mut cur_path = initial_path.to_string();

    for _ in 0..=MAX_REDIRECTS {
        let Some(raw) = http_get(cur_ip, cur_port, &cur_path).await else {
            break;
        };

        let location = if status_code(&raw) == Some(301) {
            extract_location(&raw)
        } else {
            None
        };

        probes.push(HttpProbe {
            port: cur_port,
            url: cur_path.clone(),
            raw,
        });

        let Some(loc) = location else { break };
        let Some((next_ip, next_port, next_path)) = resolve_redirect(&loc, cur_ip, cur_port) else {
            break;
        };

        if probes
            .iter()
            .any(|p| p.port == next_port && p.url == next_path)
        {
            break; // cycle guard
        }

        cur_ip = next_ip;
        cur_port = next_port;
        cur_path = next_path;
    }
}

fn status_code(raw: &str) -> Option<u16> {
    // Status line: "HTTP/1.1 301 Moved Permanently"
    let status_str = raw.lines().next()?.split_whitespace().nth(1)?;
    status_str.parse().ok()
}

fn extract_location(raw: &str) -> Option<String> {
    for line in raw.lines() {
        if line.len() > 9 && line[..9].eq_ignore_ascii_case("location:") {
            return Some(line[9..].trim().to_string());
        }
    }
    None
}

/// Resolves a Location header value to (ip, port, path).
/// Returns None for HTTPS (no TLS support) or unparseable locations.
fn resolve_redirect(
    location: &str,
    original_ip: IpAddr,
    original_port: u16,
) -> Option<(IpAddr, u16, String)> {
    if location.starts_with("https://") {
        return None;
    }
    if let Some(rest) = location.strip_prefix("http://") {
        let slash = rest.find('/').unwrap_or(rest.len());
        let host_port = &rest[..slash];
        let path = if slash < rest.len() {
            rest[slash..].to_string()
        } else {
            "/".to_string()
        };

        let (host, port) = match host_port.rfind(':') {
            Some(i) => (
                &host_port[..i],
                host_port[i + 1..].parse::<u16>().unwrap_or(80),
            ),
            None => (host_port, 80u16),
        };
        // If the host doesn't parse as an IP address (e.g. "router.local"), fall
        // back to the original IP — we're fingerprinting a known local device.
        let ip = host.parse::<IpAddr>().unwrap_or(original_ip);
        return Some((ip, port, path));
    }
    if location.starts_with('/') {
        return Some((original_ip, original_port, location.to_string()));
    }
    None
}

async fn port_scan(ip: IpAddr) -> Vec<u16> {
    let mut set: JoinSet<Option<u16>> = JoinSet::new();
    for &port in PROBE_PORTS {
        set.spawn(async move {
            let addr = SocketAddr::new(ip, port);
            timeout(TCP_TIMEOUT, TcpStream::connect(addr))
                .await
                .ok()
                .and_then(|r| r.ok())
                .map(|_| port)
        });
    }
    let mut open = Vec::new();
    while let Some(result) = set.join_next().await {
        if let Ok(Some(port)) = result {
            open.push(port);
        }
    }
    open.sort_unstable();
    open
}

async fn http_get(ip: IpAddr, port: u16, path: &str) -> Option<String> {
    let addr = SocketAddr::new(ip, port);
    let mut stream = timeout(TCP_TIMEOUT, TcpStream::connect(addr))
        .await
        .ok()?
        .ok()?;

    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {ip}\r\nConnection: close\r\nUser-Agent: dom/0.1\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await.ok()?;

    // Use a fixed deadline so partial responses are kept even if the server
    // ignores Connection: close and leaves the socket open after the body.
    let deadline = Instant::now() + HTTP_TIMEOUT;
    let mut response: Vec<u8> = Vec::with_capacity(4096);
    let mut buf = [0u8; 4096];
    loop {
        match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => {
                response.extend_from_slice(&buf[..n]);
                if response.len() >= HTTP_MAX_BYTES {
                    break;
                }
            }
            Ok(Err(_)) => break,
        }
    }

    (!response.is_empty()).then(|| String::from_utf8_lossy(&response).into_owned())
}
