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

use std::sync::{Arc, Mutex};

use sqlx::SqlitePool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Starts an in-memory SQLite pool with schema applied.
pub async fn db() -> SqlitePool {
    dom::db::init("sqlite://:memory:").await.unwrap()
}

/// Minimal raw-TCP HTTP server. Reads each request, stores it, then writes the
/// pre-canned response and drops the connection (clean EOF for read_to_end).
/// Not every integration test binary uses this — each `tests/*.rs` file is
/// compiled as its own crate, so unused-in-this-binary triggers dead_code.
#[allow(dead_code)]
pub struct MockHttp {
    pub port: u16,
    requests: Arc<Mutex<Vec<String>>>,
}

#[allow(dead_code)]
impl MockHttp {
    pub async fn start(status: u16, body: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let reqs = Arc::clone(&requests);

        let response = format!(
            "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let reqs = Arc::clone(&reqs);
                let resp = response.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    reqs.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&buf[..n]).into_owned());
                    let _ = stream.write_all(resp.as_bytes()).await;
                    // drop stream → EOF → client's read_to_end returns
                });
            }
        });

        Self { port, requests }
    }

    /// Wait until the server has recorded at least `n` requests and return the nth (1-based).
    pub async fn nth_request(&self, n: usize) -> String {
        loop {
            let req = self.requests.lock().unwrap().get(n - 1).cloned();
            if let Some(r) = req {
                return r;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}
