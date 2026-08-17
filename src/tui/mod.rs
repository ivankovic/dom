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

fn detail_slot(app: &App, ip: IpAddr) -> DetailSlot {
    let is_time = matches!(app.switch_auto_modes.get(&ip), Some(SwitchAutoMode::Time));
    let n = app.switch_timers.get(&ip).map(|t| t.len()).unwrap_or(0);
    match app.detail_row {
        0 => DetailSlot::Relay,
        1 => DetailSlot::Auto,
        r if is_time && r >= 2 && r < 2 + n => DetailSlot::Timer(r - 2),
        _ if is_time => DetailSlot::AddTimer,
        _ => DetailSlot::Auto,
    }
}

fn max_detail_row(app: &App, ip: IpAddr) -> usize {
    let is_time = matches!(app.switch_auto_modes.get(&ip), Some(SwitchAutoMode::Time));
    if is_time {
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

pub async fn run(state: SharedState, pool: SqlitePool, rescan: Arc<Notify>) -> anyhow::Result<()> {
    let mut terminal = ratatui::try_init()
        .map_err(|e| anyhow::anyhow!("terminal init failed (is stdout a TTY?): {e}"))?;
    let result = event_loop(&mut terminal, &state, &pool, &rescan).await;
    ratatui::restore();
    result
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
                                                let new = if matches!(
                                                    app.switch_auto_modes.get(&dev.ip),
                                                    Some(SwitchAutoMode::Time)
                                                ) {
                                                    SwitchAutoMode::Disabled
                                                } else {
                                                    SwitchAutoMode::Time
                                                };
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

                if let Some((ip, time, relay_on)) = timer_dialog_save {
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
                    } else if app.rename_input.is_some() {
                        match event {
                            Event::Key(KeyEvent {
                                code: KeyCode::Char('c'),
                                modifiers: KeyModifiers::CONTROL,
                                ..
                            }) => break,
                            Event::Key(KeyEvent { code: KeyCode::Char(c), .. }) => {
                                app.rename_input.as_mut().unwrap().push(c);
                            }
                            Event::Key(KeyEvent { code: KeyCode::Backspace, .. }) => {
                                app.rename_input.as_mut().unwrap().pop();
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
                            Event::Key(KeyEvent { code: KeyCode::Char('q'), .. })
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
