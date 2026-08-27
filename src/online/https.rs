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

//! A minimal HTTPS GET, hand-written on top of a TLS transport.
//!
//! Dom already hand-rolls HTTP/1.1 for LAN devices (`fingerprint::http_get`,
//! `devices::*::fetch_report`); this is the same approach with `tokio-rustls`
//! underneath, which is why the dependency added is a TLS transport rather than
//! an HTTP client.
//!
//! Deliberately requests **HTTP/1.0**. Asked over 1.1, swisstopo replies with
//! `Transfer-Encoding: chunked`, which would mean implementing de-chunking for no
//! benefit; over 1.0 it sends the body and closes, so read-to-EOF is correct.
//! Both endpoints were checked. Do not "modernise" this to 1.1 without adding
//! chunked decoding first.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

/// Whole-request budget. These are background refreshes, so waiting longer
/// helps nobody: a slow answer is as good as none until the next tick.
const TIMEOUT: Duration = Duration::from_secs(15);

/// Cap on a response body. MeteoSwiss's station file is ~170 KB; this leaves
/// generous headroom while bounding what a misbehaving or hostile endpoint can
/// make the app allocate.
const MAX_BODY: usize = 8 * 1024 * 1024;

/// Identifies Dom to the services it calls, as their usage policies expect.
const USER_AGENT: &str = concat!(
    "dom/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/ivankovic/dom)"
);

/// Shared TLS configuration. Built once — assembling the root store per request
/// would dominate the cost of these calls.
fn tls_config() -> Arc<ClientConfig> {
    static CONFIG: std::sync::OnceLock<Arc<ClientConfig>> = std::sync::OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
            let config = ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("ring provider supports the default protocol versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

/// Fetches `https://{host}{path}` and returns the response body.
///
/// Redirects are reported as an error rather than followed: both endpoints answer
/// 200 directly, so a redirect means something changed and should be noticed, not
/// chased.
pub async fn get(host: &str, path: &str) -> anyhow::Result<String> {
    tokio::time::timeout(TIMEOUT, get_inner(host, path))
        .await
        .with_context(|| format!("timed out fetching https://{host}{path}"))?
}

async fn get_inner(host: &str, path: &str) -> anyhow::Result<String> {
    let tcp = TcpStream::connect((host, 443))
        .await
        .with_context(|| format!("connecting to {host}:443"))?;
    let server_name = ServerName::try_from(host.to_string())
        .with_context(|| format!("{host} is not a valid TLS server name"))?;
    let mut stream = TlsConnector::from(tls_config())
        .connect(server_name, tcp)
        .await
        .with_context(|| format!("TLS handshake with {host}"))?;

    let request = format!(
        "GET {path} HTTP/1.0\r\n\
         Host: {host}\r\n\
         User-Agent: {USER_AGENT}\r\n\
         Accept: application/json\r\n\
         Connection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .context("sending request")?;

    let mut raw = Vec::with_capacity(64 * 1024);
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = stream.read(&mut buf).await.context("reading response")?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.len() > MAX_BODY {
            bail!("response from {host} exceeded {MAX_BODY} bytes");
        }
    }

    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .context("response had no header/body separator")?;
    let status = status_code(head).context("response had no status line")?;
    if status != 200 {
        bail!("https://{host}{path} returned HTTP {status}");
    }
    Ok(body.to_string())
}

/// Status code from a response's header block.
fn status_code(head: &str) -> Option<u16> {
    head.lines().next()?.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_code_reads_the_status_line() {
        assert_eq!(
            status_code("HTTP/1.1 200 OK\r\nContent-Length: 2"),
            Some(200)
        );
        assert_eq!(status_code("HTTP/1.0 404 Not Found"), Some(404));
    }

    #[test]
    fn status_code_is_none_for_a_non_http_response() {
        assert_eq!(status_code(""), None);
        assert_eq!(status_code("garbage"), None);
        assert_eq!(status_code("HTTP/1.1 notanumber OK"), None);
    }

    #[test]
    fn user_agent_identifies_the_app_and_its_source() {
        // Both services' usage policies expect a contactable identifier.
        assert!(USER_AGENT.starts_with("dom/"));
        assert!(USER_AGENT.contains("github.com/ivankovic/dom"));
    }
}
