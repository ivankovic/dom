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

//! Where diagnostics go.
//!
//! Dom is a full-screen TUI for its whole life, so the usual destination for a
//! log — stderr — is not available: writes would either scroll behind the
//! alternate screen or draw straight through the interface. Everything therefore
//! goes to a file next to the database, and the file is the only place it goes.
//!
//! This is not free of consequence, so it is worth being explicit: a message
//! written here is one the user will not see unless they go looking. Anything a
//! person needs to *act* on belongs in `App` as well, where a view can show it —
//! `last_scan_error`, `last_weather_error` and `solar.last_error` are the
//! existing examples, and `rollup_error` was added because the alternative was a
//! `log::warn!` that stopped pruning without telling anyone.
//!
//! Level defaults to `info` and honours `RUST_LOG`, so `RUST_LOG=debug dom`
//! turns on the per-device detail without a rebuild.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Log file, beside `db.sqlite` in the working directory — the same place the
/// rest of Dom's state lives, so there is one directory to look in.
pub const LOG_FILE: &str = "dom.log";

/// Size at which the log is rotated, at startup only.
///
/// Rotating on startup rather than continuously keeps the writer a plain
/// append-only file handle with no locking or size check per record. The cost is
/// that a single long-lived run can exceed this; the benefit is that the common
/// case — a run that grows the log over weeks and is then restarted — stays
/// bounded at twice this, and the previous run's tail survives as `dom.log.1`
/// for exactly the case where the restart is what you are investigating.
const ROTATE_AT_BYTES: u64 = 5 * 1024 * 1024;

/// Installs the file logger. Returns the path written to.
///
/// Failing to open the log is deliberately not fatal: a read-only working
/// directory should cost visibility, not the ability to run. The caller reports
/// what happened before the TUI takes the terminal, which is the last moment
/// anything printed is visible.
pub fn init() -> Result<PathBuf, String> {
    init_at(Path::new(LOG_FILE))
}

fn init_at(path: &Path) -> Result<PathBuf, String> {
    rotate_if_large(path, ROTATE_AT_BYTES);

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;

    env_logger::Builder::new()
        // `RUST_LOG` if set, `info` otherwise. Parsed after the default so an
        // explicit setting wins.
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .target(env_logger::Target::Pipe(Box::new(file)))
        .format(|buf, record| {
            writeln!(
                buf,
                "{} {:<5} {}: {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                record.level(),
                record.target(),
                record.args()
            )
        })
        .try_init()
        .map_err(|e| format!("a logger was already installed: {e}"))?;

    log::info!(
        "dom {} starting; logging to {}",
        env!("CARGO_PKG_VERSION"),
        path.display()
    );
    Ok(path.to_path_buf())
}

/// Moves an oversized log aside so the new run starts fresh.
///
/// Every failure here is ignored on purpose. Rotation is housekeeping; if it
/// cannot happen the log simply keeps growing, which is a much smaller problem
/// than refusing to start.
fn rotate_if_large(path: &Path, limit: u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() < limit {
        return;
    }
    let _ = std::fs::rename(path, path.with_extension("log.1"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real files in a real directory: rotation is filesystem behaviour, and a
    /// fake would be testing the fake. `tempfile` rather than a fixed path under
    /// `/tmp`, so concurrent runs of the suite cannot collide.
    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_small_log_is_left_alone() {
        let dir = tmpdir();
        let path = dir.path().join("dom.log");
        std::fs::write(&path, b"one line\n").unwrap();

        rotate_if_large(&path, 1024);

        assert_eq!(std::fs::read(&path).unwrap(), b"one line\n");
        assert!(
            !dir.path().join("dom.log.1").exists(),
            "nothing to rotate yet"
        );
    }

    #[test]
    fn an_oversized_log_is_moved_aside_rather_than_deleted() {
        // The previous run's tail is the thing you want when the restart itself
        // is what you are investigating, so rotation must not truncate.
        let dir = tmpdir();
        let path = dir.path().join("dom.log");
        std::fs::write(&path, vec![b'x'; 2048]).unwrap();

        rotate_if_large(&path, 1024);

        assert!(!path.exists(), "the live log is moved out of the way");
        assert_eq!(
            std::fs::read(dir.path().join("dom.log.1")).unwrap().len(),
            2048
        );
    }

    #[test]
    fn rotating_twice_keeps_only_the_previous_run() {
        let dir = tmpdir();
        let path = dir.path().join("dom.log");
        std::fs::write(&path, b"older").unwrap();
        rotate_if_large(&path, 1);
        std::fs::write(&path, b"newer").unwrap();
        rotate_if_large(&path, 1);

        assert_eq!(
            std::fs::read(dir.path().join("dom.log.1")).unwrap(),
            b"newer"
        );
    }

    #[test]
    fn a_missing_log_is_not_an_error() {
        let dir = tmpdir();
        rotate_if_large(&dir.path().join("nothing-here.log"), 1);
    }

    #[test]
    fn the_log_lives_beside_the_database() {
        // Both are relative to the working directory; a log that landed somewhere
        // else would be one more place to look.
        assert!(!Path::new(LOG_FILE).is_absolute());
        assert_eq!(Path::new(LOG_FILE).parent(), Some(Path::new("")));
    }
}
