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

//! Dom, as a library: every module the binary is assembled from.
//!
//! `main.rs` is the application — the tokio runtime, the background tasks, the
//! terminal — and everything it does is built out of the modules declared here.
//! The split exists so the parts can be tested without one: an integration test,
//! or `cargo test`, links this crate and never starts a runtime or a TUI.
//!
//! Read [`db`] first if you are looking for where data lives, [`app`] for what
//! the interface currently believes, and SPECS.md for why any of it is shaped
//! the way it is. The modules divide up roughly as:
//!
//! - [`app`] — the whole of the displayed state, behind one lock, plus the pure
//!   functions that derive from it. No I/O.
//! - [`db`] — the SQLite schema, every query, and the tiered retention that
//!   keeps a two-second sample rate affordable on an SD card.
//! - [`devices`] — one module per kind of hardware on the LAN, each with its own
//!   detection, polling and (where it applies) actuation.
//! - [`fingerprint`] — what a host on the network looks like from the outside,
//!   which is what `devices` classifies.
//! - [`tui`] — drawing and key handling, and nothing else.
//! - [`energy`], [`solar`], [`stats`] — the arithmetic: provenance, the array's
//!   own geometry, and the aggregates the statistics view draws.
//! - [`online`] — the three public-internet services, over hand-written HTTP
//!   on TLS. The only places Dom reaches anything off the LAN.
//! - [`cluster`], [`alarm`], [`logging`] — running as one of two nodes, saying
//!   so when something is wrong, and writing diagnostics somewhere other than
//!   the terminal the TUI owns.
pub mod alarm;
pub mod app;
pub mod cluster;
pub mod db;
pub mod devices;
pub mod energy;
pub mod fingerprint;
pub mod logging;
pub mod online;
pub mod solar;
pub mod stats;
pub mod tui;
