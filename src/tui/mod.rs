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

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use sqlx::SqlitePool;
use tokio::sync::Notify;

use crate::app::{App, Focus, ScannedDevice, SharedState, SwitchAutoMode, TimerDialogField, View};
use crate::devices;

mod render;
pub mod theme;
use render::render;

// ── Detail-panel slot model ───────────────────────────────────────────────────

#[derive(PartialEq)]
enum DetailSlot {
    Relay,
    Auto,
    Timer(usize),
    AddTimer,
}

/// Whether a switch's timer list (and "+ Add timer" row) is shown for the
/// given mode — Time mode runs them directly; Eco mode reads only their
/// *length* (see `devices::mystrom_switch`'s "Eco mode" docs) but still shows
/// and edits them the same way, since that is how its window's duration is
/// set.
fn shows_timers(mode: Option<&SwitchAutoMode>) -> bool {
    matches!(mode, Some(SwitchAutoMode::Time) | Some(SwitchAutoMode::Eco))
}

fn detail_slot(app: &App, ip: IpAddr) -> DetailSlot {
    let shows_timers = shows_timers(app.switch_auto_modes.get(&ip));
    let n = app.switch_timers.get(&ip).map(|t| t.len()).unwrap_or(0);
    match app.detail_row {
        0 => DetailSlot::Relay,
        1 => DetailSlot::Auto,
        r if shows_timers && r >= 2 && r < 2 + n => DetailSlot::Timer(r - 2),
        _ if shows_timers => DetailSlot::AddTimer,
        _ => DetailSlot::Auto,
    }
}

fn max_detail_row(app: &App, ip: IpAddr) -> usize {
    if shows_timers(app.switch_auto_modes.get(&ip)) {
        2 + app.switch_timers.get(&ip).map(|t| t.len()).unwrap_or(0)
    } else {
        1
    }
}

/// Only the Devices view uses the list+detail layout and its interactions
/// (Tab focus, rename, relay/auto/timer/KEBA-mode toggles). Current shows the
/// energy overview and network health panel instead — no device selection.
fn is_device_list_view(view: &View) -> bool {
    matches!(view, View::Devices)
}

/// The mode Enter would switch a KEBA wallbox to next: Disabled → Full power →
/// Eco → Eco (car first) → Disabled.
fn toggled_keba_mode(app: &App, ip: IpAddr) -> devices::keba::ChargingMode {
    use devices::keba::ChargingMode;
    match app.keba_modes.get(&ip).copied() {
        Some(ChargingMode::FullPower) => ChargingMode::Eco,
        Some(ChargingMode::Eco) => ChargingMode::EcoCarFirst,
        Some(ChargingMode::EcoCarFirst) => ChargingMode::Disabled,
        _ => ChargingMode::FullPower,
    }
}

/// The mode Enter would switch a myStrom switch's auto-mode to next:
/// Disabled → Time → Eco → Disabled.
fn toggled_switch_auto_mode(current: Option<&SwitchAutoMode>) -> SwitchAutoMode {
    match current {
        Some(SwitchAutoMode::Time) => SwitchAutoMode::Eco,
        Some(SwitchAutoMode::Eco) => SwitchAutoMode::Disabled,
        _ => SwitchAutoMode::Time,
    }
}

fn is_valid_hhmm(s: &str) -> bool {
    if s.len() != 5 {
        return false;
    }
    let b = s.as_bytes();
    if b[2] != b':' {
        return false;
    }
    let h: u8 = s[..2].parse().unwrap_or(99);
    let m: u8 = s[3..].parse().unwrap_or(99);
    h < 24 && m < 60
}

/// Devices shown in the Energy view's Device Power list: anything with a live
/// reading (battery or switch), in the same order as app.devices.
fn energy_active_devices(app: &App) -> Vec<&ScannedDevice> {
    app.devices
        .iter()
        .filter(|d| {
            app.readings.contains_key(&d.ip)
                || app.switch_readings.contains_key(&d.ip)
                || app.keba_readings.contains_key(&d.ip)
        })
        .collect()
}

/// Longest text either free-text input will accept.
///
/// A stuck or held key must not be able to grow a buffer without limit, and no
/// real address or device name comes close. Shared by the address and rename
/// inputs because there is no reason for them to differ — the rename buffer
/// previously had no bound at all, and neither did the label column it is
/// written to.
const MAX_INPUT_CHARS: usize = 120;

/// Appends a typed character to a text input, ignoring it once the input is full.
///
/// One function for both inputs so they cannot drift apart again. Counts
/// characters rather than bytes: the limit is about how much someone can type,
/// and a bound in bytes would cut a non-ASCII name short at an arbitrary point.
fn push_bounded(buf: &mut String, c: char) {
    if buf.chars().count() < MAX_INPUT_CHARS {
        buf.push(c);
    }
}

/// Whether the interface could be started.
///
/// Not an error, because there is nothing wrong: a Dom running as a service on a
/// headless machine has no terminal to draw on and every reason to keep
/// collecting. See `run`.
pub enum Interface {
    /// The TUI ran and the user asked to quit.
    Closed,
    /// There was no terminal to attach to.
    Unavailable(String),
}

/// Runs the interface, if there is one to run.
///
/// Returning `Unavailable` rather than an error is the whole point. Dom's actual
/// work — polling devices, integrating energy, rolling up tiers — happens in
/// background tasks that neither need nor notice a terminal. Treating a missing
/// TTY as fatal meant a headless install collected nothing at all, which is the
/// normal way to run this on a Raspberry Pi.
pub async fn run(
    state: SharedState,
    pool: SqlitePool,
    rescan: Arc<Notify>,
) -> anyhow::Result<Interface> {
    let mut terminal = match ratatui::try_init() {
        Ok(t) => t,
        Err(e) => return Ok(Interface::Unavailable(e.to_string())),
    };
    let result = event_loop(&mut terminal, &state, &pool, &rescan).await;
    ratatui::restore();
    result.map(|()| Interface::Closed)
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    state: &SharedState,
    pool: &SqlitePool,
    rescan: &Arc<Notify>,
) -> anyhow::Result<()> {
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let app = state.read().unwrap();
                terminal.draw(|f| render(f, &app))?;
            }
            maybe = events.next() => {
                let Some(Ok(event)) = maybe else { break };

                // Extract async actions under a read lock, then drop before awaiting.
                // Priority: timer_dialog_save > rename_save > relay/auto/delete (mutually exclusive).
                // open_timer_dialog is a sync action handled in the write-lock branch below.
                let is_enter = matches!(&event, Event::Key(KeyEvent { code: KeyCode::Enter, .. }));
                let is_d = matches!(
                    &event,
                    Event::Key(KeyEvent { code: KeyCode::Char('d' | 'D'), .. })
                );

                // Accepting a device's new TLS certificate. Only from the Network
                // view, where the alert that prompts it is shown, and only while
                // no text input is open — 'k' is a character like any other while
                // someone is typing a name.
                let accept_cert: Option<(IpAddr, String)> = {
                    let app = state.read().unwrap();
                    let is_k = matches!(
                        &event,
                        Event::Key(KeyEvent { code: KeyCode::Char('k' | 'K'), .. })
                    );
                    if is_k
                        && app.view == View::Network
                        && app.address_input.is_none()
                        && app.rename_input.is_none()
                        && app.timer_dialog.is_none()
                    {
                        // Oldest address first, so repeated presses work through
                        // them in a stable order rather than at random.
                        let mut pending: Vec<_> = app.cert_alerts.iter().collect();
                        pending.sort_by_key(|(ip, _)| **ip);
                        pending
                            .first()
                            .map(|(ip, alert)| (**ip, alert.observed.clone()))
                    } else {
                        None
                    }
                };

                // Confirming a discovered (or re-identified) cluster peer — see
                // App::cluster_pairing_prompt. Same gating as accept_cert: only from
                // the Network view, where the prompt is shown, and only while no
                // text input is open.
                let confirm_pairing: Option<crate::app::ClusterPairingPrompt> = {
                    let app = state.read().unwrap();
                    let is_p = matches!(
                        &event,
                        Event::Key(KeyEvent { code: KeyCode::Char('p' | 'P'), .. })
                    );
                    if is_p
                        && app.view == View::Network
                        && app.address_input.is_none()
                        && app.rename_input.is_none()
                        && app.timer_dialog.is_none()
                    {
                        app.cluster_pairing_prompt.clone()
                    } else {
                        None
                    }
                };

                // Address entry is modal: while it is open, Enter looks the address
                // up and every other key edits the buffer.
                let address_save: Option<String> = if is_enter {
                    let app = state.read().unwrap();
                    app.address_input
                        .as_ref()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                } else {
                    None
                };

                let (
                    timer_dialog_save,
                    rename_save,
                    relay_toggle,
                    auto_toggle,
                    timer_delete,
                    open_dialog_ip,
                    keba_mode_toggle,
                ) = {
                    let app = state.read().unwrap();

                    // 1. Timer dialog save — Enter with a valid time when dialog is open.
                    let timer_dialog_save: Option<(IpAddr, String, bool)> =
                        if is_enter {
                            app.timer_dialog.as_ref().and_then(|d| {
                                if is_valid_hhmm(&d.time_buf) {
                                    Some((d.ip, d.time_buf.clone(), d.relay_on))
                                } else {
                                    None
                                }
                            })
                        } else {
                            None
                        };

                    // 2. Rename save — Enter while editing a device name (no dialog open).
                    let rename_save: Option<(IpAddr, String)> =
                        if is_enter && app.timer_dialog.is_none() {
                            if app.rename_input.is_some() && app.focus == Focus::Detail {
                                app.selected_device()
                                    .and_then(|d| app.rename_input.as_ref().map(|s| (d.ip, s.clone())))
                            } else {
                                None
                            }
                        } else {
                            None
                        };

                    // 3–5. Switch-slot / KEBA-mode async actions (no dialog, no rename,
                    // Current/Detail/switch or KEBA, or the Energy view's Device Power list).
                    let (
                        relay_toggle,
                        auto_toggle,
                        timer_delete,
                        open_dialog_ip,
                        keba_mode_toggle,
                    ) = if timer_dialog_save.is_none()
                            && rename_save.is_none()
                            && app.timer_dialog.is_none()
                            && app.rename_input.is_none()
                        {
                            if is_device_list_view(&app.view) && app.focus == Focus::Detail {
                                if let Some(dev) = app.selected_device() {
                                    if dev.name == Some(crate::devices::mystrom_switch::NAME) {
                                        let slot = detail_slot(&app, dev.ip);

                                        let relay: Option<(IpAddr, bool)> =
                                            if is_enter && slot == DetailSlot::Relay {
                                                app.switch_readings
                                                    .get(&dev.ip)
                                                    .map(|r| (dev.ip, r.relay_on))
                                            } else {
                                                None
                                            };

                                        let auto: Option<(IpAddr, SwitchAutoMode)> =
                                            if is_enter && slot == DetailSlot::Auto {
                                                let new = toggled_switch_auto_mode(
                                                    app.switch_auto_modes.get(&dev.ip),
                                                );
                                                Some((dev.ip, new))
                                            } else {
                                                None
                                            };

                                        let del: Option<i64> = if is_d {
                                            if let DetailSlot::Timer(idx) = slot {
                                                app.switch_timers
                                                    .get(&dev.ip)
                                                    .and_then(|t| t.get(idx))
                                                    .map(|t| t.id)
                                            } else {
                                                None
                                            }
                                        } else {
                                            None
                                        };

                                        let open_ip: Option<IpAddr> =
                                            if is_enter && slot == DetailSlot::AddTimer {
                                                Some(dev.ip)
                                            } else {
                                                None
                                            };

                                        (relay, auto, del, open_ip, None)
                                    } else if dev.name == Some(crate::devices::keba::NAME) {
                                        let keba = if is_enter {
                                            Some((dev.ip, toggled_keba_mode(&app, dev.ip)))
                                        } else {
                                            None
                                        };
                                        (None, None, None, None, keba)
                                    } else {
                                        (None, None, None, None, None)
                                    }
                                } else {
                                    (None, None, None, None, None)
                                }
                            } else if app.view == View::Energy && is_enter {
                                let active = energy_active_devices(&app);
                                let idx = app.energy_selected.min(active.len().saturating_sub(1));
                                let selected_ip = active.get(idx).map(|d| d.ip);
                                let relay = selected_ip.and_then(|ip| {
                                    app.switch_readings.get(&ip).map(|r| (ip, r.relay_on))
                                });
                                let keba = selected_ip
                                    .filter(|ip| app.keba_readings.contains_key(ip))
                                    .map(|ip| (ip, toggled_keba_mode(&app, ip)));
                                (relay, None, None, None, keba)
                            } else {
                                (None, None, None, None, None)
                            }
                        } else {
                            (None, None, None, None, None)
                        };

                    (
                        timer_dialog_save,
                        rename_save,
                        relay_toggle,
                        auto_toggle,
                        timer_delete,
                        open_dialog_ip,
                        keba_mode_toggle,
                    )
                }; // read lock dropped here

                if let Some((ip, fingerprint)) = accept_cert {
                    // Pin the new certificate and clear the alert. The poll loop
                    // re-reads the pin every tick, so the device is reachable
                    // again on its next poll without a restart.
                    match crate::db::set_tls_pin(pool, ip, &fingerprint).await {
                        Ok(()) => {
                            log::info!("accepted a new TLS certificate for {ip}");
                            state.write().unwrap().cert_alerts.remove(&ip);
                        }
                        Err(e) => log::warn!("could not accept {ip}'s new certificate: {e:#}"),
                    }
                }

                if let Some(prompt) = confirm_pairing {
                    // Pin the peer's address and identity — the one explicit human action this
                    // feature deliberately still requires (see SPECS.md). cluster_task/
                    // cluster_discovery_task re-read both from Config on their own schedule, no
                    // restart needed.
                    let addr_result = crate::db::set_cluster_peer_addr(pool, &prompt.addr).await;
                    let key_result =
                        crate::db::set_cluster_peer_pubkey(pool, &prompt.public_key).await;
                    match (addr_result, key_result) {
                        (Ok(()), Ok(())) => {
                            log::info!(
                                "cluster: paired with {} at {}",
                                prompt.node_id,
                                prompt.addr
                            );
                            state.write().unwrap().cluster_pairing_prompt = None;
                        }
                        (addr_result, key_result) => {
                            for e in [addr_result.err(), key_result.err()].into_iter().flatten() {
                                log::warn!("cluster: could not save pairing: {e:#}");
                            }
                        }
                    }
                }

                if let Some(address) = address_save {
                    match crate::online::geocode::lookup(&address).await {
                        Ok(place) => {
                            let loc = crate::db::Location {
                                label: place.label,
                                east: place.east,
                                north: place.north,
                                latitude: place.latitude,
                                longitude: place.longitude,
                            };
                            if let Err(e) = crate::db::set_location(pool, &loc).await {
                                state.write().unwrap().last_weather_error =
                                    Some(format!("saving the location failed: {e:#}"));
                            } else {
                                {
                                    let mut app = state.write().unwrap();
                                    app.location = Some(loc);
                                    app.address_input = None;
                                    app.last_weather_error = None;
                                    // The previous location's reading is not this
                                    // location's; clear it rather than leave a
                                    // number that now means somewhere else.
                                    app.outdoor = None;
                                    app.outdoor_today = None;
                                }
                                // Fetch straight away, so a new address shows a
                                // reading instead of waiting for the next tick.
                                crate::online::weather::refresh(pool, state).await;
                            }
                        }
                        Err(e) => {
                            // Keep the typed text so it can be corrected rather
                            // than retyped.
                            state.write().unwrap().last_weather_error =
                                Some(format!("could not find \"{address}\": {e:#}"));
                        }
                    }
                } else if let Some((ip, time, relay_on)) = timer_dialog_save {
                    if let Ok(id) = crate::db::add_switch_timer(pool, ip, &time, relay_on).await {
                        let mut app = state.write().unwrap();
                        let entry = app.switch_timers.entry(ip).or_default();
                        entry.push(crate::app::SwitchTimer { id, time_hhmm: time, relay_on });
                        entry.sort_by(|a, b| a.time_hhmm.cmp(&b.time_hhmm));
                        app.timer_dialog = None;
                    }
                } else if let Some((ip, label)) = rename_save {
                    // Spawn DB updates in the background to avoid blocking the UI
                    let pool_clone = pool.clone();
                    let label_clone = label.clone();
                    tokio::spawn(async move {
                        let _ = crate::db::update_device_label(&pool_clone, ip, &label_clone).await;
                        let _ = crate::db::update_network_status_events_label(&pool_clone, ip, if label_clone.is_empty() { None } else { Some(&label_clone) }).await;
                    });
                    let mut app = state.write().unwrap();
                    // Update device label in memory
                    let new_label: Option<String> = if label.is_empty() { None } else { Some(label) };
                    if let Some(dev) = app.devices.iter_mut().find(|d| d.ip == ip) {
                        dev.label = new_label.clone();
                    }
                    // Update label in existing network status events in memory
                    for event in &mut app.network_status_events {
                        if event.ip == ip {
                            event.label = new_label.clone();
                        }
                    }
                    app.rename_input = None;
                } else if let Some((ip, currently_on)) = relay_toggle {
                    let _ = crate::devices::mystrom_switch::set_relay(ip, crate::devices::mystrom_switch::API_PORT, !currently_on).await;
                } else if let Some((ip, new_mode)) = auto_toggle {
                    let _ = crate::db::set_switch_auto_mode(pool, ip, &new_mode).await;
                    let mut app = state.write().unwrap();
                    app.switch_auto_modes.insert(ip, new_mode);
                    let max = max_detail_row(&app, ip);
                    app.detail_row = app.detail_row.min(max);
                } else if let Some(timer_id) = timer_delete {
                    let _ = crate::db::delete_switch_timer(pool, timer_id).await;
                    let mut app = state.write().unwrap();
                    if let Some(dev) = app.selected_device().cloned() {
                        if let Some(t) = app.switch_timers.get_mut(&dev.ip) {
                            t.retain(|t| t.id != timer_id);
                        }
                        let max = max_detail_row(&app, dev.ip);
                        app.detail_row = app.detail_row.min(max);
                    }
                } else if let Some((ip, new_mode)) = keba_mode_toggle {
                    // Only record the new mode as fact once the wallbox actually
                    // confirms it — otherwise the UI would show "Full power" while
                    // the charger silently stayed disabled (or vice versa).
                    let result =
                        crate::devices::keba::set_mode(ip, crate::devices::keba::API_PORT, new_mode)
                            .await;
                    match result {
                        Ok(()) => {
                            let _ = crate::db::set_keba_mode(pool, ip, new_mode).await;
                            let mut app = state.write().unwrap();
                            app.keba_modes.insert(ip, new_mode);
                            app.last_error.remove(&ip);
                        }
                        Err(e) => {
                            state.write().unwrap().last_error.insert(ip, format!("{e:#}"));
                        }
                    }
                } else {
                    // Set when the user toggles the theme, so the new choice can
                    // be written to the DB after the write guard is dropped —
                    // never hold the lock across an await.
                    let mut theme_chosen: Option<theme::ThemeMode> = None;
                    // Set when the selected statistics window or period changes,
                    // so the new range is queried after the guard is released.
                    let mut stats_reload = false;

                    // Synchronous write-lock branch. Scoped so the guard is
                    // provably released before the await below.
                    {
                    let mut app = state.write().unwrap();

                    if app.timer_dialog.is_some() {
                        match event {
                            Event::Key(KeyEvent {
                                code: KeyCode::Char('c'),
                                modifiers: KeyModifiers::CONTROL,
                                ..
                            }) => break,
                            Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => {
                                app.timer_dialog = None;
                            }
                            Event::Key(KeyEvent { code: KeyCode::Tab, .. }) => {
                                let dlg = app.timer_dialog.as_mut().unwrap();
                                dlg.field = match dlg.field {
                                    TimerDialogField::Time => TimerDialogField::Action,
                                    TimerDialogField::Action => TimerDialogField::Time,
                                };
                            }
                            Event::Key(KeyEvent { code: KeyCode::Backspace, .. }) => {
                                let dlg = app.timer_dialog.as_mut().unwrap();
                                if dlg.field == TimerDialogField::Time {
                                    dlg.time_buf.pop();
                                }
                            }
                            Event::Key(KeyEvent { code: KeyCode::Char(c), .. }) => {
                                let dlg = app.timer_dialog.as_mut().unwrap();
                                if dlg.field == TimerDialogField::Time {
                                    if dlg.time_buf.len() < 5 && (c.is_ascii_digit() || c == ':') {
                                        dlg.time_buf.push(c);
                                        // Auto-insert colon after 2 digits.
                                        if dlg.time_buf.len() == 2 && !dlg.time_buf.contains(':') {
                                            dlg.time_buf.push(':');
                                        }
                                    }
                                } else {
                                    dlg.relay_on = !dlg.relay_on;
                                }
                            }
                            _ => {}
                        }
                    } else if app.address_input.is_some() {
                        match event {
                            Event::Key(KeyEvent {
                                code: KeyCode::Char('c'),
                                modifiers: KeyModifiers::CONTROL,
                                ..
                            }) => break,
                            Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => {
                                app.address_input = None;
                            }
                            Event::Key(KeyEvent { code: KeyCode::Backspace, .. }) => {
                                if let Some(buf) = app.address_input.as_mut() {
                                    buf.pop();
                                }
                            }
                            Event::Key(KeyEvent { code: KeyCode::Char(c), .. }) => {
                                if let Some(buf) = app.address_input.as_mut() {
                                    push_bounded(buf, c);
                                }
                            }
                            _ => {}
                        }
                    } else if app.rename_input.is_some() {
                        match event {
                            Event::Key(KeyEvent {
                                code: KeyCode::Char('c'),
                                modifiers: KeyModifiers::CONTROL,
                                ..
                            }) => break,
                            Event::Key(KeyEvent { code: KeyCode::Char(c), .. }) => {
                                if let Some(buf) = app.rename_input.as_mut() {
                                    push_bounded(buf, c);
                                }
                            }
                            Event::Key(KeyEvent { code: KeyCode::Backspace, .. }) => {
                                if let Some(buf) = app.rename_input.as_mut() {
                                    buf.pop();
                                }
                            }
                            Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => {
                                app.rename_input = None;
                            }
                            _ => {}
                        }
                    } else if let Some(ip) = open_dialog_ip {
                        app.timer_dialog = Some(crate::app::TimerDialog {
                            ip,
                            time_buf: String::new(),
                            relay_on: true,
                            field: TimerDialogField::Time,
                        });
                    } else {
                        match event {
                            Event::Key(KeyEvent { code: KeyCode::Char('q' | 'Q'), .. })
                            | Event::Key(KeyEvent {
                                code: KeyCode::Char('c'),
                                modifiers: KeyModifiers::CONTROL,
                                ..
                            }) => break,
                            Event::Key(KeyEvent { code: KeyCode::Char('c' | 'C'), .. }) => {
                                app.view = View::Current;
                            }
                            Event::Key(KeyEvent { code: KeyCode::Char('e' | 'E'), .. }) => {
                                app.view = View::Energy;
                            }
                            Event::Key(KeyEvent { code: KeyCode::Char('n' | 'N'), .. }) => {
                                app.view = View::Network;
                            }
                            // Reachable only when timer_delete (computed above from the same
                            // 'd'/'D' press) was None — i.e. not currently deleting a timer.
                            Event::Key(KeyEvent { code: KeyCode::Char('d' | 'D'), .. }) => {
                                app.view = View::Devices;
                                let max = app.visible_devices().len().saturating_sub(1);
                                app.selected = app.selected.min(max);
                            }
                            // Wakes the ping-scan and discovery tasks immediately, out of
                            // their normal schedule. A no-op for whichever of the two (if
                            // any) is already mid-run, since Notify only wakes waiters that
                            // are actually parked — it doesn't queue.
                            Event::Key(KeyEvent { code: KeyCode::Char('s' | 'S'), .. }) => {
                                rescan.notify_waiters();
                            }
                            // Switches between the dark and light palettes. Recorded
                            // as an explicit choice, which from now on overrides
                            // terminal auto-detection on every startup.
                            // Each of m/y opens the statistics view directly on
                            // that window; pressing the same one again returns to
                            // the current period after browsing.
                            Event::Key(KeyEvent { code: KeyCode::Char('m' | 'M'), .. }) => {
                                app.view = View::Statistics;
                                app.stats.set_window(crate::stats::StatsWindow::Month);
                                stats_reload = true;
                            }
                            Event::Key(KeyEvent { code: KeyCode::Char('y' | 'Y'), .. }) => {
                                app.view = View::Statistics;
                                app.stats.set_window(crate::stats::StatsWindow::Year);
                                stats_reload = true;
                            }
                            Event::Key(KeyEvent { code: KeyCode::Left, .. })
                                if app.view == View::Statistics =>
                            {
                                app.stats.back(chrono::Local::now().date_naive());
                                stats_reload = true;
                            }
                            Event::Key(KeyEvent { code: KeyCode::Right, .. })
                                if app.view == View::Statistics =>
                            {
                                app.stats.forward();
                                stats_reload = true;
                            }
                            Event::Key(KeyEvent { code: KeyCode::Char('a' | 'A'), .. })
                                if app.view == View::Environment =>
                            {
                                app.address_input =
                                    Some(app.location.as_ref().map(|l| l.label.clone()).unwrap_or_default());
                            }
                            Event::Key(KeyEvent { code: KeyCode::Char('v' | 'V'), .. }) => {
                                app.view = View::Environment;
                            }
                            Event::Key(KeyEvent { code: KeyCode::Up, .. })
                                if app.view == View::Environment =>
                            {
                                app.env_selected = app.env_selected.saturating_sub(1);
                            }
                            Event::Key(KeyEvent { code: KeyCode::Down, .. })
                                if app.view == View::Environment =>
                            {
                                let max = render::temperature_sensors(&app).len().saturating_sub(1);
                                app.env_selected = (app.env_selected + 1).min(max);
                            }
                            Event::Key(KeyEvent { code: KeyCode::Char('t' | 'T'), .. }) => {
                                app.theme_mode = app.theme_mode.toggled();
                                theme_chosen = Some(app.theme_mode);
                            }
                            Event::Key(KeyEvent { code: KeyCode::Up, .. })
                                if app.view == View::Energy =>
                            {
                                app.energy_selected = app.energy_selected.saturating_sub(1);
                            }
                            Event::Key(KeyEvent { code: KeyCode::Down, .. })
                                if app.view == View::Energy =>
                            {
                                let max = energy_active_devices(&app).len().saturating_sub(1);
                                app.energy_selected = (app.energy_selected + 1).min(max);
                            }
                            Event::Key(KeyEvent { code: KeyCode::Tab, .. })
                                if is_device_list_view(&app.view) =>
                            {
                                app.focus = match app.focus {
                                    Focus::DeviceList => Focus::Detail,
                                    Focus::Detail => {
                                        app.detail_row = 0;
                                        Focus::DeviceList
                                    }
                                };
                            }
                            Event::Key(KeyEvent { code: KeyCode::Up, .. })
                                if is_device_list_view(&app.view)
                                    && app.focus == Focus::DeviceList =>
                            {
                                app.selected = app.selected.saturating_sub(1);
                                app.detail_row = 0;
                            }
                            Event::Key(KeyEvent { code: KeyCode::Down, .. })
                                if is_device_list_view(&app.view)
                                    && app.focus == Focus::DeviceList =>
                            {
                                let max = app.visible_devices().len().saturating_sub(1);
                                app.selected = (app.selected + 1).min(max);
                                app.detail_row = 0;
                            }
                            Event::Key(KeyEvent { code: KeyCode::Up, .. })
                                if is_device_list_view(&app.view)
                                    && app.focus == Focus::Detail =>
                            {
                                app.detail_row = app.detail_row.saturating_sub(1);
                            }
                            Event::Key(KeyEvent { code: KeyCode::Down, .. })
                                if is_device_list_view(&app.view)
                                    && app.focus == Focus::Detail =>
                            {
                                if let Some(ip) = app.selected_device().map(|d| d.ip)
                                    && app
                                        .selected_device()
                                        .map(|d| d.name == Some(crate::devices::mystrom_switch::NAME))
                                        .unwrap_or(false)
                                {
                                    let max = max_detail_row(&app, ip);
                                    app.detail_row = (app.detail_row + 1).min(max);
                                }
                            }
                            Event::Key(KeyEvent { code: KeyCode::Char('r' | 'R'), .. })
                                if is_device_list_view(&app.view) =>
                            {
                                // Cloned out of the immutable borrow before the
                                // mutable writes below; also looks the selected
                                // device up once rather than twice.
                                if let Some(current) = app
                                    .selected_device()
                                    .map(|d| d.label.clone().unwrap_or_default())
                                {
                                    app.rename_input = Some(current);
                                    app.focus = Focus::Detail;
                                    app.detail_row = 0;
                                }
                            }
                            _ => {}
                        }
                    }
                    }

                    if stats_reload {
                        crate::stats::refresh(pool, state).await;
                    }

                    if let Some(mode) = theme_chosen {
                        let _ =
                            crate::db::set_config(pool, theme::CONFIG_KEY, mode.as_str()).await;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, SwitchTimer};
    use devices::keba::ChargingMode;

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(172, 16, 75, 4));

    fn app_with_switch(auto: Option<SwitchAutoMode>, timers: usize) -> App {
        let mut app = App::default();
        if let Some(mode) = auto {
            app.switch_auto_modes.insert(IP, mode);
        }
        app.switch_timers.insert(
            IP,
            (0..timers)
                .map(|i| SwitchTimer {
                    id: i as i64,
                    time_hhmm: "07:00".into(),
                    relay_on: true,
                })
                .collect(),
        );
        app
    }

    // ── Detail-panel slots ────────────────────────────────────────────────────

    #[test]
    fn a_switch_with_auto_mode_off_has_only_two_rows() {
        // Timers exist in the model but are not reachable while auto mode is
        // disabled, so Down must not walk onto them.
        let mut app = app_with_switch(Some(SwitchAutoMode::Disabled), 3);
        assert_eq!(max_detail_row(&app, IP), 1);
        app.detail_row = 0;
        assert!(detail_slot(&app, IP) == DetailSlot::Relay);
        app.detail_row = 1;
        assert!(detail_slot(&app, IP) == DetailSlot::Auto);
    }

    #[test]
    fn time_mode_exposes_each_timer_and_an_add_row() {
        let mut app = app_with_switch(Some(SwitchAutoMode::Time), 2);
        assert_eq!(max_detail_row(&app, IP), 4, "relay, auto, two timers, add");
        for (row, expected) in [
            (0, DetailSlot::Relay),
            (1, DetailSlot::Auto),
            (2, DetailSlot::Timer(0)),
            (3, DetailSlot::Timer(1)),
            (4, DetailSlot::AddTimer),
        ] {
            app.detail_row = row;
            assert!(detail_slot(&app, IP) == expected, "row {row}");
        }
    }

    #[test]
    fn a_row_past_the_end_does_not_select_a_timer_that_is_not_there() {
        // Deleting the last timer leaves `detail_row` beyond the new end; the
        // slot has to degrade to something harmless rather than index off it.
        let mut app = app_with_switch(Some(SwitchAutoMode::Time), 0);
        app.detail_row = 9;
        assert!(detail_slot(&app, IP) == DetailSlot::AddTimer);

        let mut app = app_with_switch(Some(SwitchAutoMode::Disabled), 0);
        app.detail_row = 9;
        assert!(detail_slot(&app, IP) == DetailSlot::Auto);
    }

    #[test]
    fn a_device_with_no_switch_configuration_still_has_a_relay_row() {
        let app = App::default();
        assert_eq!(max_detail_row(&app, IP), 1);
        assert!(detail_slot(&app, IP) == DetailSlot::Relay);
    }

    #[test]
    fn eco_mode_exposes_the_timer_list_too() {
        // Eco reads the timer pair's *length*, not its clock time, but still
        // shows and edits it exactly like Time mode does.
        let mut app = app_with_switch(Some(SwitchAutoMode::Eco), 2);
        assert_eq!(max_detail_row(&app, IP), 4, "relay, auto, two timers, add");
        for (row, expected) in [
            (0, DetailSlot::Relay),
            (1, DetailSlot::Auto),
            (2, DetailSlot::Timer(0)),
            (3, DetailSlot::Timer(1)),
            (4, DetailSlot::AddTimer),
        ] {
            app.detail_row = row;
            assert!(detail_slot(&app, IP) == expected, "row {row}");
        }
    }

    // ── Switch auto-mode cycling ──────────────────────────────────────────────

    #[test]
    fn enter_cycles_a_switch_through_every_auto_mode_and_back() {
        let mut app = App::default();
        let mut seen = Vec::new();
        for _ in 0..3 {
            let next = toggled_switch_auto_mode(app.switch_auto_modes.get(&IP));
            seen.push(next.clone());
            app.switch_auto_modes.insert(IP, next);
        }
        assert!(
            matches!(seen[0], SwitchAutoMode::Time)
                && matches!(seen[1], SwitchAutoMode::Eco)
                && matches!(seen[2], SwitchAutoMode::Disabled),
            "all three modes must be reachable, and the cycle must close"
        );
        // A fourth press returns to the start.
        assert!(matches!(
            toggled_switch_auto_mode(app.switch_auto_modes.get(&IP)),
            SwitchAutoMode::Time
        ));
    }

    // ── KEBA mode cycling ─────────────────────────────────────────────────────

    #[test]
    fn enter_cycles_a_wallbox_through_every_mode_and_back() {
        let mut app = App::default();
        let mut seen = Vec::new();
        for _ in 0..4 {
            let next = toggled_keba_mode(&app, IP);
            seen.push(next);
            app.keba_modes.insert(IP, next);
        }
        assert_eq!(
            seen,
            vec![
                ChargingMode::FullPower,
                ChargingMode::Eco,
                ChargingMode::EcoCarFirst,
                ChargingMode::Disabled,
            ],
            "all four modes must be reachable, and the cycle must close"
        );
        // A fifth press returns to the start.
        assert_eq!(toggled_keba_mode(&app, IP), ChargingMode::FullPower);
    }

    // ── Timer entry validation ────────────────────────────────────────────────

    #[test]
    fn a_well_formed_time_is_accepted() {
        for s in ["00:00", "07:30", "23:59", "09:05"] {
            assert!(is_valid_hhmm(s), "{s}");
        }
    }

    #[test]
    fn an_out_of_range_or_malformed_time_is_rejected() {
        for s in [
            "24:00", "23:60", "99:99", // out of range
            "7:30", "07:3", "073:0", "", "07:300", // wrong shape
            "07-30", "0730", // wrong separator
            "ab:cd", "07:cd", // not numbers
        ] {
            assert!(!is_valid_hhmm(s), "{s} should be rejected");
        }
    }

    #[test]
    fn a_multibyte_time_string_is_rejected_without_panicking() {
        // `is_valid_hhmm` checks `len()` in bytes and then slices at 2 and 3, so
        // it is worth pinning that a multi-byte character cannot make it slice
        // through a character boundary. "é:00" is five *bytes* and passes the
        // length check, and byte 2 really is ':' — which is also what makes it
        // safe, since a ':' at index 2 can only follow whole characters.
        assert_eq!("é:00".len(), 5);
        assert!(!is_valid_hhmm("é:00"));
        assert!(!is_valid_hhmm("0é:00"));
        assert!(!is_valid_hhmm("»»"));
    }

    // ── View predicates and the energy list ───────────────────────────────────

    #[test]
    fn only_the_devices_view_uses_the_list_and_detail_layout() {
        assert!(is_device_list_view(&View::Devices));
        for v in [
            View::Current,
            View::Energy,
            View::Network,
            View::Statistics,
            View::Environment,
        ] {
            assert!(!is_device_list_view(&v), "{:?}", std::mem::discriminant(&v));
        }
    }

    #[test]
    fn the_energy_list_holds_only_devices_that_are_reporting() {
        let mut app = App::default();
        let ips: Vec<IpAddr> = (1..=4)
            .map(|i| IpAddr::V4(std::net::Ipv4Addr::new(172, 16, 0, i)))
            .collect();
        for ip in &ips {
            app.devices.push(crate::app::ScannedDevice {
                ip: *ip,
                latency_ms: 1.0,
                open_ports: vec![],
                name: None,
                label: None,
            });
        }
        // Built out rather than defaulted: none of the reading types implements
        // `Default`, deliberately — a zeroed reading is indistinguishable from a
        // device genuinely reporting zero.
        let now = chrono::Utc::now();
        app.readings.insert(
            ips[0],
            crate::app::LiveReading {
                consumption_w: 400.0,
                production_w: 0.0,
                pac_w: 0.0,
                rsoc: 50.0,
                grid_w: 400.0,
                remaining_kwh: 4.0,
                capacity_kwh: 8.0,
                updated_at: now,
            },
        );
        app.switch_readings.insert(
            ips[2],
            crate::app::SwitchReading {
                power_w: 12.0,
                relay_on: true,
                temperature_c: 24.0,
                updated_at: now,
            },
        );
        app.keba_readings.insert(
            ips[3],
            crate::app::KebaReading {
                power_w: 0.0,
                energy_session_kwh: 0.0,
                energy_total_kwh: 100.0,
                state: 2,
                plug: 0,
                curr_hw_ma: 16_000,
                updated_at: now,
            },
        );

        let listed: Vec<IpAddr> = energy_active_devices(&app).iter().map(|d| d.ip).collect();
        // ips[1] has no reading of any kind, and the order follows app.devices.
        assert_eq!(listed, vec![ips[0], ips[2], ips[3]]);
    }

    // ── Input bounds ──────────────────────────────────────────────────────────

    #[test]
    fn a_text_input_stops_growing_once_it_is_full() {
        // The rename buffer previously had no bound at all, so a held key grew
        // it without limit and it was then written to the database.
        let mut buf = String::new();
        for _ in 0..MAX_INPUT_CHARS * 3 {
            push_bounded(&mut buf, 'x');
        }
        assert_eq!(buf.chars().count(), MAX_INPUT_CHARS);
    }

    #[test]
    fn the_input_bound_counts_characters_not_bytes() {
        // A bound in bytes would cut a non-ASCII device name short at a third of
        // the length an ASCII one gets.
        let mut buf = String::new();
        for _ in 0..MAX_INPUT_CHARS * 2 {
            push_bounded(&mut buf, 'ä');
        }
        assert_eq!(buf.chars().count(), MAX_INPUT_CHARS);
        assert_eq!(buf.len(), MAX_INPUT_CHARS * 2, "two bytes each");
    }
}
