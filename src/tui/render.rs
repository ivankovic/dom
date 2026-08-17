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

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Chart, Clear, Dataset, GraphType, List, ListItem, ListState, Paragraph,
};

use crate::app::{
    App, ConnStatus, Focus, KebaReading, LiveReading, NetworkDeviceStatus, ScanPhase,
    ScannedDevice, SwitchAutoMode, SwitchReading, TimerDialogField, View,
};
use crate::devices;
use crate::tui::theme::Theme;

use super::{DetailSlot, detail_slot, energy_active_devices, is_valid_hhmm};

pub(super) fn render(f: &mut Frame, app: &App) {
    let area = f.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);

    render_statusbar(f, rows[0], app);
    match app.view {
        View::Current => render_current_view(f, rows[1], app),
        View::Energy => render_energy_view(f, rows[1], app),
        View::Network => render_network_view(f, rows[1], app),
        View::Devices => render_devices_view(f, rows[1], app),
        View::Statistics => render_statistics_view(f, rows[1], app),
    }
    if app.timer_dialog.is_some() {
        render_timer_dialog(f, app);
    }
}

// ── Statistics view ───────────────────────────────────────────────────────────

/// Formats a kWh figure at a fixed width so columns line up.
fn kwh(v: f64) -> String {
    format!("{v:>7.1}")
}

/// A percentage, or an em dash when the ratio has no denominator — see
/// `stats::Totals::self_sufficiency_pct`. Never prints 0% for "unknown".
fn pct(v: Option<f64>) -> String {
    match v {
        Some(p) => format!("{p:>5.1} %"),
        None => "    — ".to_string(),
    }
}

fn render_statistics_view(f: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let st = &app.stats;

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(8),
            Constraint::Min(0),
        ])
        .split(area);

    // Header: which window, which period, and how to move.
    let browsing = if st.offset == 0 {
        Span::from("  (current)").style(Style::default().fg(theme.inactive))
    } else {
        Span::from(format!("  ({} back)", st.offset)).style(Style::default().fg(theme.input_active))
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::from(format!(" {} · ", st.window.label()))
                .style(Style::default().add_modifier(Modifier::BOLD)),
            Span::from(st.title.clone()).style(Style::default().fg(theme.focus_border)),
            browsing,
            Span::from("   w/m/y window · ←/→ period").style(Style::default().fg(theme.inactive)),
        ])),
        rows[0],
    );

    render_stats_totals(f, rows[1], app, &theme);
    render_stats_buckets(f, rows[2], app, &theme);
}

fn render_stats_totals(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let t = &app.stats.totals;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let totals = vec![
        Line::from(vec![
            Span::from("  Consumption   "),
            Span::from(kwh(t.consumption_kwh)).style(Style::default().fg(theme.consumption)),
            Span::from(" kWh"),
        ]),
        Line::from(vec![
            Span::from("  Production    "),
            Span::from(kwh(t.production_kwh)).style(Style::default().fg(theme.production)),
            Span::from(" kWh"),
        ]),
        Line::from(vec![
            Span::from("  Grid import   "),
            Span::from(kwh(t.grid_import_kwh)).style(Style::default().fg(theme.grid_import)),
            Span::from(" kWh"),
        ]),
        Line::from(vec![
            Span::from("  Grid export   "),
            Span::from(kwh(t.grid_export_kwh)).style(Style::default().fg(theme.grid_export)),
            Span::from(" kWh"),
        ]),
    ];
    f.render_widget(
        Paragraph::new(totals).block(Block::default().borders(Borders::ALL).title(" Totals ")),
        cols[0],
    );

    // Self-sufficiency: how much of what was used did not have to be bought.
    // Self-consumption: how much of what was made was used rather than sold.
    let ss = t.self_sufficiency_pct();
    let sc = t.self_consumption_pct();
    let own = t.self_consumed_kwh();
    let ratios = vec![
        Line::from(vec![
            Span::from("  Self-sufficiency  "),
            Span::from(pct(ss)).style(
                Style::default()
                    .fg(theme.production)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(
            Span::from(format!(
                "    {:.1} of {:.1} kWh used was own",
                own, t.consumption_kwh
            ))
            .style(Style::default().fg(theme.inactive)),
        ),
        Line::from(vec![
            Span::from("  Self-consumption  "),
            Span::from(pct(sc)).style(
                Style::default()
                    .fg(theme.battery_charge)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(
            Span::from(format!(
                "    {:.1} of {:.1} kWh made was kept",
                own, t.production_kwh
            ))
            .style(Style::default().fg(theme.inactive)),
        ),
    ];
    f.render_widget(
        Paragraph::new(ratios).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Self-sufficiency "),
        ),
        cols[1],
    );
}

/// One row per bucket: a bar scaled against the period's largest consumption,
/// split into the part covered by own production and the part bought from the
/// grid, so the self-sufficiency of each day is visible at a glance.
fn render_stats_buckets(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let st = &app.stats;
    let title = match st.window {
        crate::stats::StatsWindow::Year => " By month ",
        _ => " By day ",
    };
    let block = Block::default().borders(Borders::ALL).title(title);

    if st.buckets.is_empty() || !st.buckets.iter().any(|b| b.has_data) {
        f.render_widget(
            Paragraph::new(Line::from(
                Span::from("  No energy data recorded for this period.")
                    .style(Style::default().fg(theme.inactive)),
            ))
            .block(block),
            area,
        );
        return;
    }

    let max = st
        .buckets
        .iter()
        .map(|b| b.totals.consumption_kwh.max(b.totals.production_kwh))
        .fold(0.0f64, f64::max);
    // Bar width: the inner area minus the label and numeric columns.
    let bar_w = (area.width as usize).saturating_sub(46).max(8);

    let lines: Vec<Line> = st
        .buckets
        .iter()
        .map(|b| {
            if !b.has_data {
                return Line::from(vec![
                    Span::from(format!("  {:<4}", b.label)),
                    Span::from("no data").style(Style::default().fg(theme.inactive)),
                ]);
            }
            let cells = if max > 0.0 {
                ((b.totals.consumption_kwh / max) * bar_w as f64).round() as usize
            } else {
                0
            };
            let cells = cells.min(bar_w);
            // Split the consumption bar: what came from own generation, and
            // what was imported.
            let own_cells = if b.totals.consumption_kwh > 0.0 {
                ((b.totals.self_consumed_kwh() / b.totals.consumption_kwh) * cells as f64).round()
                    as usize
            } else {
                0
            };
            let own_cells = own_cells.min(cells);

            Line::from(vec![
                Span::from(format!("  {:<4}", b.label)),
                Span::styled(
                    "\u{2588}".repeat(own_cells),
                    Style::default().fg(theme.production),
                ),
                Span::styled(
                    "\u{2588}".repeat(cells - own_cells),
                    Style::default().fg(theme.grid_import),
                ),
                Span::styled(
                    "\u{2591}".repeat(bar_w - cells),
                    Style::default().fg(theme.gauge_track),
                ),
                Span::from(format!("{} kWh", kwh(b.totals.consumption_kwh))),
                Span::from(format!("  prod{}", kwh(b.totals.production_kwh)))
                    .style(Style::default().fg(theme.production)),
                Span::from(format!("  {}", pct(b.totals.self_sufficiency_pct())))
                    .style(Style::default().fg(theme.inactive)),
            ])
        })
        .collect();

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_current_view(f: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(0)])
        .split(area);

    render_energy_overview(f, rows[0], app);

    let (routers, modems, access_points) = network_role_groups(app);
    render_network_infrastructure_health(f, rows[1], app, &routers, &modems, &access_points);
}

/// Groups scanned devices into Router / Modem / Access Point, in the same
/// order shown in the Network view's health panel.
fn network_role_groups(
    app: &App,
) -> (
    Vec<&ScannedDevice>,
    Vec<&ScannedDevice>,
    Vec<&ScannedDevice>,
) {
    let topology = crate::app::classify_network_devices(&app.devices);
    let find = |ip: IpAddr| app.devices.iter().find(|d| d.ip == ip);

    let routers: Vec<&ScannedDevice> = topology.routers.iter().copied().filter_map(find).collect();
    let modems: Vec<&ScannedDevice> = topology.modems.iter().copied().filter_map(find).collect();
    let mut access_points: Vec<&ScannedDevice> = topology
        .access_points
        .iter()
        .copied()
        .filter_map(find)
        .collect();
    access_points.sort_by_key(|a| get_device_display_name(a));

    (routers, modems, access_points)
}

/// Every discovered device, configured or not — same list+detail interactions
/// as Current, just without the energy overview and over the full device set.
fn render_devices_view(f: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    render_scan_status(f, rows[0], app);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(rows[1]);

    render_device_list(f, cols[0], app);
    render_detail(f, cols[1], app);
}

fn render_scan_status(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title(" Scan ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut text = match &app.scan {
        ScanPhase::Idle => "  Waiting for first scan".to_string(),
        ScanPhase::Scanning => "  Scanning...".to_string(),
        ScanPhase::Done { at, next_at } => {
            let secs = next_at
                .signed_duration_since(chrono::Utc::now())
                .num_seconds()
                .max(0);
            format!(
                "  Last scan {}  ·  Next scan in {:02}:{:02}",
                at.format("%H:%M:%S"),
                secs / 60,
                secs % 60,
            )
        }
    };
    if let Some(err) = &app.last_scan_error {
        text.push_str(&format!(
            "  ·  ping scan failed ({err}) — DHCP-lease devices still discovered"
        ));
    }
    // Surfaces whether the router's lease table is actually reaching this
    // app at all, separate from whether ping-scanning works — the two are
    // independent sources of newly discovered devices (see scanner_task).
    let lease_count: usize = app.mikrotik_readings.values().map(|r| r.leases.len()).sum();
    text.push_str(&format!("  ·  {lease_count} DHCP leases known"));
    f.render_widget(Paragraph::new(text), inner);
}

// ── Energy overview ───────────────────────────────────────────────────────────

struct Totals {
    consumption_kw: f64,
    production_kw: f64,
    grid_kw: f64,
    pac_kw: f64,
    remaining_kwh: f64,
    capacity_kwh: f64,
}

fn compute_totals(app: &App) -> Totals {
    let mut t = Totals {
        consumption_kw: 0.0,
        production_kw: 0.0,
        grid_kw: 0.0,
        pac_kw: 0.0,
        remaining_kwh: 0.0,
        capacity_kwh: 0.0,
    };
    for r in app.readings.values() {
        t.consumption_kw += r.consumption_w / 1000.0;
        t.production_kw += r.production_w / 1000.0;
        t.grid_kw += r.grid_w / 1000.0;
        t.pac_kw += r.pac_w / 1000.0;
        t.remaining_kwh += r.remaining_kwh;
        t.capacity_kwh += r.capacity_kwh;
    }
    t
}

fn render_energy_overview(f: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Energy Overview ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let t = compute_totals(app);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1); 6])
        .split(inner);

    // Each row: [22-char label] [gauge bar] [11-char value + direction]
    let max_kw = 10.0_f64;

    gauge_row(
        f,
        rows[0],
        &theme,
        Gauge {
            label: "Consumption",
            color: theme.consumption,
            value: &format!("{:>6.2} kW", t.consumption_kw),
            dir: "",
        },
        (t.consumption_kw.abs(), max_kw),
    );
    gauge_row(
        f,
        rows[1],
        &theme,
        Gauge {
            label: "Production",
            color: theme.production,
            value: &format!("{:>6.2} kW", t.production_kw),
            dir: "",
        },
        (t.production_kw.abs(), max_kw),
    );

    let (grid_color, grid_dir) = if t.grid_kw >= 0.0 {
        (theme.grid_export, "→ Exporting")
    } else {
        (theme.grid_import, "← Importing")
    };
    gauge_row(
        f,
        rows[2],
        &theme,
        Gauge {
            label: "Grid",
            color: grid_color,
            value: &format!("{:>6.2} kW", t.grid_kw.abs()),
            dir: grid_dir,
        },
        (t.grid_kw.abs(), max_kw),
    );

    let (bat_color, bat_dir) = if t.pac_kw >= 0.0 {
        (theme.battery_discharge, "↓ Discharging")
    } else {
        (theme.battery_charge, "↑ Charging")
    };
    gauge_row(
        f,
        rows[3],
        &theme,
        Gauge {
            label: "Battery Power",
            color: bat_color,
            value: &format!("{:>6.2} kW", t.pac_kw.abs()),
            dir: bat_dir,
        },
        (t.pac_kw.abs(), max_kw),
    );

    // Battery remaining uses kWh scale; color shifts by fill level.
    let (rem_ratio, rem_color) = if t.capacity_kwh > 0.0 {
        let ratio = (t.remaining_kwh / t.capacity_kwh).clamp(0.0, 1.0);
        let color = if ratio > 0.5 {
            theme.level_good
        } else if ratio > 0.2 {
            theme.level_warn
        } else {
            theme.level_critical
        };
        (ratio, color)
    } else {
        (0.0, theme.inactive)
    };
    let rem_value = if t.capacity_kwh > 0.0 {
        format!("{:.2}/{:.2} kWh", t.remaining_kwh, t.capacity_kwh)
    } else {
        format!("{:.2} kWh  (cap unknown)", t.remaining_kwh)
    };
    gauge_row_ratio(
        f,
        rows[4],
        &theme,
        Gauge {
            label: "Battery Energy",
            color: rem_color,
            value: &rem_value,
            dir: "",
        },
        rem_ratio,
    );

    let ec = &app.energy_chart;
    f.render_widget(
        Paragraph::new(format!(
            "{:<20} In {:.2} kWh    Out {:.2} kWh",
            "Grid Energy", ec.grid_imported_kwh, ec.grid_exported_kwh,
        )),
        rows[5],
    );
}

/// Render one gauge row: label | █ bar ░ | value  direction
/// The parts of a gauge row that describe *what* is being shown, as opposed to
/// where it is drawn. Grouped into a struct because passing five of these
/// positionally — three of them `&str` — made call sites hard to read and
/// trivial to transpose.
struct Gauge<'a> {
    label: &'a str,
    /// Colour of the filled part of the bar; comes from the active `Theme`.
    color: Color,
    /// Right-aligned reading, e.g. `"  1.23 kW"`.
    value: &'a str,
    /// Trailing annotation, e.g. `"→ Exporting"`. Empty for none.
    dir: &'a str,
}

/// Draws a gauge whose fill is `value / max`. A non-positive `max` renders empty
/// rather than dividing by zero.
fn gauge_row(f: &mut Frame, area: Rect, theme: &Theme, g: Gauge, value_max: (f64, f64)) {
    let (value, max) = value_max;
    let ratio = if max > 0.0 {
        (value / max).clamp(0.0, 1.0)
    } else {
        0.0
    };
    gauge_row_ratio(f, area, theme, g, ratio);
}

/// Draws a gauge from an already-computed fill ratio, clamped 0.0..=1.0.
fn gauge_row_ratio(f: &mut Frame, area: Rect, theme: &Theme, g: Gauge, ratio: f64) {
    let Gauge {
        label,
        color,
        value: value_str,
        dir: dir_str,
    } = g;
    const LABEL_W: u16 = 20;
    const DIR_W: u16 = 12;
    const VALUE_W: u16 = 20;

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(LABEL_W),
            Constraint::Min(0),
            Constraint::Length(VALUE_W),
            Constraint::Length(DIR_W),
        ])
        .split(area);

    f.render_widget(
        Paragraph::new(format!("{label:<width$}", width = LABEL_W as usize)),
        cols[0],
    );

    // Filled bar with block characters.
    let bar_w = cols[1].width as usize;
    let filled = ((ratio * bar_w as f64) as usize).min(bar_w);
    let unfilled = bar_w - filled;
    let bar_line = Line::from(vec![
        Span::styled("█".repeat(filled), Style::default().fg(color)),
        Span::styled("░".repeat(unfilled), Style::default().fg(theme.gauge_track)),
    ]);
    f.render_widget(Paragraph::new(bar_line), cols[1]);

    f.render_widget(Paragraph::new(format!("  {value_str}")), cols[2]);
    f.render_widget(
        Paragraph::new(format!("{dir_str:<width$}", width = DIR_W as usize)),
        cols[3],
    );
}

// ── Status bar ────────────────────────────────────────────────────────────────

fn render_statusbar(f: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let nav = "[c] current  [e] energy  [n] network  [d] devices  [w/m/y] stats  [s] rescan";
    let focus_hints = if app.timer_dialog.is_some() {
        "[Tab] switch field  [Enter] save  [Esc] cancel  [Ctrl+C] quit".to_string()
    } else if app.rename_input.is_some() {
        "[Enter] save name  [Esc] cancel  [Ctrl+C] quit".to_string()
    } else if app.view == View::Energy {
        "[↑↓] select device  [Enter] toggle switch  [q] quit".to_string()
    } else if app.view == View::Statistics {
        "[←→] period  [w] weekly  [m] monthly  [y] yearly  [t] theme  [q] quit".to_string()
    } else if app.view == View::Network || app.view == View::Current {
        "[q] quit".to_string()
    } else if app.focus == Focus::Detail {
        let is_switch = app
            .selected_device()
            .map(|d| d.name == Some(crate::devices::mystrom_switch::NAME))
            .unwrap_or(false);
        if is_switch {
            if let Some(dev) = app.selected_device() {
                let action = match detail_slot(app, dev.ip) {
                    DetailSlot::Relay => "[Enter] toggle relay",
                    DetailSlot::Auto => "[Enter] toggle mode",
                    DetailSlot::Timer(_) => "[d] delete timer",
                    DetailSlot::AddTimer => "[Enter] add timer",
                };
                format!("[↑↓] row  {action}  [Tab] back  [r] rename  [q] quit")
            } else {
                "[Tab] back  [r] rename  [q] quit".to_string()
            }
        } else {
            "[Tab] back  [r] rename  [q] quit".to_string()
        }
    } else {
        "[↑↓] select  [Tab] details  [r] rename  [q] quit".to_string()
    };
    let hints = format!("{nav}  ·  {focus_hints}");

    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let msg = format!("  {now}  ·  {hints}");
    f.render_widget(
        Paragraph::new(msg).style(
            Style::default()
                .bg(theme.status_bar_bg)
                .fg(theme.status_bar_fg),
        ),
        area,
    );
}

fn render_device_list(f: &mut Frame, area: Rect, app: &App) {
    let devices = app.visible_devices();
    let items: Vec<ListItem> = devices.iter().map(|d| device_list_item(d, app)).collect();

    let selected = if devices.is_empty() {
        None
    } else {
        Some(app.selected)
    };
    let mut list_state = ListState::default().with_selected(selected);

    let list_border = if app.focus == Focus::DeviceList {
        Style::default().fg(app.theme().focus_border)
    } else {
        Style::default()
    };
    let title = format!(" All Devices ({}) ", devices.len());
    f.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(list_border)
                    .title(title),
            )
            .highlight_style(
                Style::default()
                    .bg(app.theme().selection_bg)
                    .fg(app.theme().selection_fg)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶"),
        area,
        &mut list_state,
    );
}

fn device_list_item<'a>(d: &ScannedDevice, app: &App) -> ListItem<'a> {
    let name = d.display_name().to_owned();
    match app.conn_status.get(&d.ip) {
        Some(ConnStatus::Lost) => ListItem::new(format!(" ✕ {:>15}  Lost           {name}", d.ip)),
        Some(ConnStatus::Connecting) => {
            ListItem::new(format!(" ◌ {:>15}  Connecting…    {name}", d.ip))
        }
        Some(ConnStatus::Online) | None => {
            let has_reading =
                app.readings.contains_key(&d.ip) || app.switch_readings.contains_key(&d.ip);
            let indicator = if has_reading { "●" } else { "○" };
            ListItem::new(format!(
                " {indicator} {:>15}  {:.1}ms  {name}",
                d.ip, d.latency_ms
            ))
        }
    }
}

fn render_detail(f: &mut Frame, area: Rect, app: &App) {
    let detail_border = if app.focus == Focus::Detail {
        Style::default().fg(app.theme().focus_border)
    } else {
        Style::default()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(detail_border)
        .title(" Detail ");

    let Some(dev) = app.selected_device() else {
        f.render_widget(Paragraph::new("\n  No device selected.").block(block), area);
        return;
    };

    let latency = if dev.latency_ms > 0.0 {
        format!("{:.1} ms", dev.latency_ms)
    } else {
        "--".to_string()
    };
    let mut lines: Vec<Line> = vec![
        Line::from(format!("  {}", dev.display_name())),
        Line::from(format!("  {}  ·  {latency}", dev.ip)),
        Line::from(""),
    ];

    if let Some(buf) = &app.rename_input {
        lines.push(Line::from(vec![
            Span::raw("  Name  ["),
            Span::styled(
                format!("{buf}│"),
                Style::default()
                    .fg(app.theme().input_active)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("]"),
        ]));
        lines.push(Line::from(""));
    }

    match app.conn_status.get(&dev.ip) {
        Some(ConnStatus::Connecting) => {
            lines.push(Line::from("  Status   Connecting..."));
            if let Some(err) = app.last_error.get(&dev.ip) {
                lines.push(Line::from(format!("  Error    {err}")));
            }
            lines.push(Line::from(""));
        }
        Some(ConnStatus::Lost) => {
            lines.push(Line::from("  Status   Connection lost"));
            if let Some(err) = app.last_error.get(&dev.ip) {
                lines.push(Line::from(format!("  Error    {err}")));
            }
            lines.push(Line::from(""));
        }
        _ => {}
    }

    if !dev.open_ports.is_empty() {
        let ports = dev
            .open_ports
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(Line::from(format!("  Ports    {ports}")));
        lines.push(Line::from(""));
    }

    if dev.name == Some(devices::mystrom_switch::NAME) {
        let dev_ip = dev.ip;
        match app.switch_readings.get(&dev_ip) {
            Some(r) => switch_reading_lines(&mut lines, r, app, dev_ip),
            None => lines.push(Line::from("  No readings yet.")),
        }
    } else if dev.name == Some(devices::keba::NAME) {
        let dev_ip = dev.ip;
        match app.keba_readings.get(&dev_ip) {
            Some(r) => keba_reading_lines(&mut lines, r, app, dev_ip),
            None => lines.push(Line::from("  No readings yet.")),
        }
    } else if dev.name == Some(devices::mikrotik::NAME) {
        let dev_ip = dev.ip;
        match app.mikrotik_readings.get(&dev_ip) {
            Some(r) => mikrotik_reading_lines(&mut lines, app, dev_ip, r),
            None => lines.push(Line::from("  No readings yet.")),
        }
    } else {
        match app.readings.get(&dev.ip) {
            Some(r) => reading_lines(&mut lines, r),
            None => lines.push(Line::from("  No readings yet.")),
        }
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn highlighted_line(text: String, active: bool, theme: &Theme) -> Line<'static> {
    if active {
        Line::from(Span::styled(
            text,
            Style::default()
                .bg(theme.selection_bg)
                .fg(theme.selection_fg)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(text)
    }
}

fn switch_reading_lines(lines: &mut Vec<Line>, r: &SwitchReading, app: &App, ip: IpAddr) {
    let theme = app.theme();
    let focused = app.focus == Focus::Detail;
    let row = app.detail_row;
    let is_time = matches!(app.switch_auto_modes.get(&ip), Some(SwitchAutoMode::Time));
    let timers = app.switch_timers.get(&ip);

    let relay = if r.relay_on { "● ON" } else { "○ OFF" };
    lines.push(highlighted_line(
        format!("  Relay    {relay}"),
        focused && row == 0,
        &theme,
    ));

    let mode_str = if is_time { "Time" } else { "Disabled" };
    lines.push(highlighted_line(
        format!("  Auto     {mode_str}"),
        focused && row == 1,
        &theme,
    ));

    if is_time {
        if let Some(ts) = timers {
            for (i, t) in ts.iter().enumerate() {
                let action = if t.relay_on { "→ ON " } else { "→ OFF" };
                lines.push(highlighted_line(
                    format!("  {}  {}  [d] delete", t.time_hhmm, action),
                    focused && row == 2 + i,
                    &theme,
                ));
            }
            let add_row = 2 + ts.len();
            lines.push(highlighted_line(
                "  + Add timer".to_string(),
                focused && row == add_row,
                &theme,
            ));
        } else {
            lines.push(highlighted_line(
                "  + Add timer".to_string(),
                focused && row == 2,
                &theme,
            ));
        }
    }

    lines.extend([
        Line::from(""),
        Line::from(format!("  Power    {:>7.1} W", r.power_w)),
        Line::from(format!("  Temp     {:>7.1} °C", r.temperature_c)),
        Line::from(""),
        Line::from(format!("  Updated  {}", r.updated_at.format("%H:%M:%S"))),
    ]);
}

fn keba_reading_lines(lines: &mut Vec<Line>, r: &KebaReading, app: &App, ip: IpAddr) {
    let theme = app.theme();
    let focused = app.focus == Focus::Detail;
    let row = app.detail_row;

    let mode = app
        .keba_modes
        .get(&ip)
        .copied()
        .unwrap_or(devices::keba::ChargingMode::Disabled);
    lines.push(highlighted_line(
        format!("  Mode     {}", mode.label()),
        focused && row == 0,
        &theme,
    ));

    lines.extend([
        Line::from(""),
        Line::from(format!(
            "  State    {}",
            devices::keba::state_label(r.state)
        )),
        Line::from(format!("  Plug     {}", devices::keba::plug_label(r.plug))),
        Line::from(""),
        Line::from(format!("  Power    {:>7.1} W", r.power_w)),
        Line::from(format!("  Session  {:>7.3} kWh", r.energy_session_kwh)),
        Line::from(format!("  Total    {:>7.1} kWh", r.energy_total_kwh)),
        Line::from(""),
        Line::from(format!("  Updated  {}", r.updated_at.format("%H:%M:%S"))),
    ]);
}

fn render_timer_dialog(f: &mut Frame, app: &App) {
    let dlg = match &app.timer_dialog {
        Some(d) => d,
        None => return,
    };

    let area = f.area();
    let popup_w = 44u16.min(area.width);
    let popup_h = 7u16.min(area.height);
    let x = (area.width.saturating_sub(popup_w)) / 2;
    let y = (area.height.saturating_sub(popup_h)) / 2;
    let popup_area = Rect::new(area.x + x, area.y + y, popup_w, popup_h);

    f.render_widget(Clear, popup_area);

    let time_style = if dlg.field == TimerDialogField::Time {
        Style::default()
            .fg(app.theme().input_active)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let action_style = if dlg.field == TimerDialogField::Action {
        Style::default()
            .fg(app.theme().input_active)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let relay_str = if dlg.relay_on { "ON " } else { "OFF" };
    let enter_hint = if is_valid_hhmm(&dlg.time_buf) {
        "[Enter] save"
    } else {
        "need HH:MM  "
    };

    let content = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  Time:    "),
            Span::styled(format!("{:5}│", dlg.time_buf), time_style),
        ]),
        Line::from(vec![
            Span::raw("  Action:  "),
            Span::styled(format!("→ {relay_str}  (any key toggles)"), action_style),
        ]),
        Line::from(""),
        Line::from(format!("  [Tab] switch field  {enter_hint}  [Esc] cancel")),
    ];

    f.render_widget(
        Paragraph::new(content).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Add Timer ")
                .border_style(Style::default().fg(app.theme().input_active)),
        ),
        popup_area,
    );
}

fn reading_lines(lines: &mut Vec<Line>, r: &LiveReading) {
    let bat = if r.pac_w >= 0.0 {
        "↓ Discharging"
    } else {
        "↑ Charging"
    };
    let grid = if r.grid_w >= 0.0 {
        "→ Exporting"
    } else {
        "← Importing"
    };
    lines.extend([
        Line::from(format!("  Consume  {:>7.0} W", r.consumption_w)),
        Line::from(format!("  Produce  {:>7.0} W", r.production_w)),
        Line::from(format!("  Battery  {:>7.0} W  {bat}", r.pac_w.abs())),
        Line::from(format!("  Grid     {:>7.0} W  {grid}", r.grid_w.abs())),
        Line::from(format!("  SoC      {:>7.0} %", r.rsoc)),
        Line::from(""),
        Line::from(format!("  Updated  {}", r.updated_at.format("%H:%M:%S"))),
    ]);
}

fn mikrotik_reading_lines(
    lines: &mut Vec<Line>,
    app: &App,
    ip: IpAddr,
    r: &crate::app::MikrotikReading,
) {
    let bound = r.leases.iter().filter(|l| l.status == "bound").count();
    let total_packets: u64 = r.firewall_rules.iter().map(|rule| rule.packets).sum();
    let total_bytes: u64 = r.firewall_rules.iter().map(|rule| rule.bytes).sum();
    lines.extend([
        Line::from(format!(
            "  DHCP     {} leases ({bound} bound)",
            r.leases.len()
        )),
        Line::from(format!(
            "  Firewall {} rules  ·  {total_packets} pkts  ·  {}",
            r.firewall_rules.len(),
            format_bytes(total_bytes)
        )),
    ]);
    // Leases are load-bearing (device discovery); firewall is best-effort —
    // a firewall-fetch failure doesn't downgrade ConnStatus, so surface it
    // here rather than nowhere, without implying the connection is down.
    if matches!(app.conn_status.get(&ip), Some(ConnStatus::Online))
        && let Some(err) = app.last_error.get(&ip)
    {
        lines.push(Line::from(format!("  Warning  {err}")));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(format!(
        "  Updated  {}",
        r.updated_at.format("%H:%M:%S")
    )));
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// ── Energy view ───────────────────────────────────────────────────────────────

fn render_energy_view(f: &mut Frame, area: Rect, app: &App) {
    let n_batteries = app
        .devices
        .iter()
        .filter(|d| app.readings.contains_key(&d.ip))
        .count();
    let n_switches = app
        .devices
        .iter()
        .filter(|d| app.switch_readings.contains_key(&d.ip))
        .count();
    let n_keba = app
        .devices
        .iter()
        .filter(|d| app.keba_readings.contains_key(&d.ip))
        .count();
    // Power panel: 1 row per device. Energy panel: 2 rows per battery (chg+dis), 1 per switch/KEBA.
    let power_h = (n_batteries + n_switches + n_keba).max(1) as u16 + 2;
    let energy_h = (n_batteries * 2 + n_switches + n_keba).max(1) as u16 + 2;

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Length(power_h),
            Constraint::Length(energy_h),
            Constraint::Min(0),
        ])
        .split(area);

    render_energy_overview(f, rows[0], app);
    render_per_device_power(f, rows[1], app);
    render_device_energy_gauges(f, rows[2], app);
    render_energy_line_charts(f, rows[3], app);
}

fn render_per_device_power(f: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Device Power ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let active = energy_active_devices(app);
    if active.is_empty() {
        f.render_widget(Paragraph::new("  No readings yet."), inner);
        return;
    }

    let idx = app.energy_selected.min(active.len() - 1);
    let lines: Vec<Line> = active
        .iter()
        .enumerate()
        .map(|(i, dev)| {
            let text = if let Some(r) = app.readings.get(&dev.ip) {
                let dir = if r.pac_w >= 0.0 {
                    "↓ Discharging"
                } else {
                    "↑ Charging"
                };
                format!(
                    "  {}   {:>5.2} kW  {}",
                    dev.display_name(),
                    r.pac_w.abs() / 1000.0,
                    dir,
                )
            } else if let Some(r) = app.switch_readings.get(&dev.ip) {
                let relay = if r.relay_on { "● ON" } else { "○ OFF" };
                format!(
                    "  {}   power {:>5.2}kW  {}",
                    dev.display_name(),
                    r.power_w / 1000.0,
                    relay,
                )
            } else if let Some(r) = app.keba_readings.get(&dev.ip) {
                format!(
                    "  {}   power {:>5.2}kW  {}",
                    dev.display_name(),
                    r.power_w / 1000.0,
                    devices::keba::state_label(r.state),
                )
            } else {
                format!("  {}   no reading", dev.display_name())
            };
            highlighted_line(text, i == idx, &theme)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_device_energy_gauges(f: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Device Energy Today ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Same device order as Device Power: only those with live readings.
    let active = energy_active_devices(app);

    if active.is_empty() {
        f.render_widget(Paragraph::new("  No readings yet."), inner);
        return;
    }

    // Gauge max: highest single kWh value across all rows (charged, discharged, or switch kwh).
    let max_kwh = active
        .iter()
        .filter_map(|d| app.device_energy_today.get(&d.ip))
        .flat_map(|e| {
            if e.kwh > 0.0 {
                vec![e.kwh]
            } else {
                vec![e.charged_kwh, e.discharged_kwh]
            }
        })
        .fold(0.0f64, f64::max);

    // Batteries take 2 rows; switches take 1.
    let n_rows: usize = active
        .iter()
        .map(|d| {
            if app.readings.contains_key(&d.ip) {
                2
            } else {
                1
            }
        })
        .sum();
    let gauge_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Length(1); n_rows.max(1)])
        .split(inner);

    let mut ri = 0usize;
    for dev in &active {
        let label = format!("{:.19}", dev.display_name());
        if app.readings.contains_key(&dev.ip) {
            // Battery: charged row then discharged row.
            let entry = app.device_energy_today.get(&dev.ip);
            let charged = entry.map(|e| e.charged_kwh).unwrap_or(0.0);
            let discharged = entry.map(|e| e.discharged_kwh).unwrap_or(0.0);
            if ri < gauge_rows.len() {
                gauge_row(
                    f,
                    gauge_rows[ri],
                    &theme,
                    Gauge {
                        label: &label,
                        color: theme.battery_charge,
                        value: &format!("{:>6.3} kWh", charged),
                        dir: "↑ Charged",
                    },
                    (charged, max_kwh),
                );
                ri += 1;
            }
            if ri < gauge_rows.len() {
                gauge_row(
                    f,
                    gauge_rows[ri],
                    &theme,
                    Gauge {
                        label: &label,
                        color: theme.battery_discharge,
                        value: &format!("{:>6.3} kWh", discharged),
                        dir: "↓ Discharged",
                    },
                    (discharged, max_kwh),
                );
                ri += 1;
            }
        } else {
            // Switch: single row.
            let kwh = app
                .device_energy_today
                .get(&dev.ip)
                .map(|e| e.kwh)
                .unwrap_or(0.0);
            if ri < gauge_rows.len() {
                gauge_row(
                    f,
                    gauge_rows[ri],
                    &theme,
                    Gauge {
                        label: &label,
                        color: theme.battery_charge,
                        value: &format!("{:>6.3} kWh", kwh),
                        dir: "",
                    },
                    (kwh, max_kwh),
                );
                ri += 1;
            }
        }
    }
}

fn render_energy_line_charts(f: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let data = &app.energy_chart;

    // Split area into 4 equal vertical sections for the 4 line charts
    let chart_areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Ratio(1, 4),
            Constraint::Ratio(1, 4),
            Constraint::Ratio(1, 4),
            Constraint::Ratio(1, 4),
        ])
        .split(area);

    // Helper to create a line chart for a single metric
    fn render_line_chart(
        f: &mut Frame,
        area: Rect,
        title: &str,
        color: Color,
        data: &[(f64, f64)],
        y_max: f64,
    ) {
        if data.is_empty() {
            let block = Block::default()
                .borders(Borders::ALL)
                .title(format!(" {title} "));
            f.render_widget(block, area);
            return;
        }

        let dataset = Dataset::default()
            .name(title)
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(color))
            .data(data);

        let y_hi = (y_max * 1.1).max(1.0);
        let y_lo = 0.0;
        let y_step = y_hi / 4.0;
        let y_labels: Vec<Span> = (0..=4)
            .map(|i| Span::raw(format!("{:.1}", y_step * i as f64)))
            .collect();

        let chart = Chart::new(vec![dataset])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {title} ")),
            )
            .x_axis(
                Axis::default().bounds([0.0, 24.0]).labels(
                    ["0h", "6h", "12h", "18h", "24h"]
                        .iter()
                        .map(|s| Span::raw(*s))
                        .collect::<Vec<_>>(),
                ),
            )
            .y_axis(Axis::default().bounds([y_lo, y_hi]).labels(y_labels));

        f.render_widget(chart, area);
    }

    // Calculate global y_max across all metrics for consistent scaling
    let all_y: Vec<f64> = data
        .consumption
        .iter()
        .chain(data.production.iter())
        .chain(data.grid.iter())
        .chain(data.battery.iter())
        .map(|(_, y)| y.abs())
        .collect();

    let y_max = all_y.iter().cloned().fold(1.0f64, f64::max);

    // Render 4 line charts
    render_line_chart(
        f,
        chart_areas[0],
        "Consumption (kW)",
        theme.consumption,
        &data.consumption,
        y_max,
    );
    render_line_chart(
        f,
        chart_areas[1],
        "Production (kW)",
        theme.production,
        &data.production,
        y_max,
    );
    render_line_chart(
        f,
        chart_areas[2],
        "Grid (kW)",
        theme.grid_export,
        &data.grid,
        y_max,
    );
    render_line_chart(
        f,
        chart_areas[3],
        "Battery (kW)",
        theme.battery_discharge,
        &data.battery,
        y_max,
    );
}

// ── Network view ──────────────────────────────────────────────────────────────

/// Get the display name for a network infrastructure device
fn get_device_display_name(device: &ScannedDevice) -> String {
    // Use label if set, otherwise use IP
    device
        .label
        .clone()
        .unwrap_or_else(|| device.ip.to_string())
}

fn render_network_view(f: &mut Frame, area: Rect, app: &App) {
    let (router_devices, modem_devices, ap_devices) = network_role_groups(app);

    // Calculate total height needed
    let total_devices = router_devices.len() + modem_devices.len() + ap_devices.len();
    let list_h = total_devices.max(1) as u16 + 2;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(list_h), Constraint::Min(0)])
        .split(area);

    render_network_infrastructure_health(
        f,
        rows[0],
        app,
        &router_devices,
        &modem_devices,
        &ap_devices,
    );

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);

    render_network_status_history(f, cols[0], app);
    render_internet_traffic_chart(f, cols[1], app);
}

fn render_network_infrastructure_health(
    f: &mut Frame,
    area: Rect,
    app: &App,
    routers: &[&ScannedDevice],
    modems: &[&ScannedDevice],
    access_points: &[&ScannedDevice],
) {
    let theme = app.theme();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Network Infrastructure Health ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Build a single list of all network infrastructure devices in order:
    // Router first, then 5G Modem, then Access Points
    let mut all_devices: Vec<(&ScannedDevice, NetworkDeviceStatus)> = Vec::new();

    // Add Router (first)
    if !routers.is_empty() {
        for device in routers {
            let status = app.network_status(device.ip);
            all_devices.push((*device, status));
        }
    }

    // Add 5G Modem (second)
    if !modems.is_empty() {
        for device in modems {
            let status = app.network_status(device.ip);
            all_devices.push((*device, status));
        }
    }

    // Add Access Points (rest)
    for device in access_points {
        let status = app.network_status(device.ip);
        all_devices.push((*device, status));
    }

    // Render all devices
    if !all_devices.is_empty() {
        for (device, status) in all_devices {
            let display_name = get_device_display_name(device);
            lines.push(render_network_device_line_static(
                display_name,
                status,
                &theme,
            ));
        }
    } else {
        lines.push(Line::from(
            "  No network infrastructure devices detected yet.",
        ));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

/// Render a line for a network infrastructure device with colored status (static lifetime)
fn render_network_device_line_static(
    name: String,
    status: NetworkDeviceStatus,
    theme: &Theme,
) -> Line<'static> {
    use ratatui::style::Style;
    let status_span = Span::from(format!(" [{}] ", status.display()))
        .style(Style::default().fg(theme.status_color(&status)));

    Line::from(vec![Span::from(format!("  {:<20} ", name)), status_span])
}

/// Recent status-change events for network infrastructure devices (e.g.
/// Router OK → LOST). Startup and each device's first observation are never
/// logged — see `app::status_transition`.
fn render_network_status_history(f: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Network Status History ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.network_status_events.is_empty() {
        f.render_widget(Paragraph::new("  No status changes recorded yet."), inner);
        return;
    }

    let lines: Vec<Line<'static>> = app
        .network_status_events
        .iter()
        .take(inner.height as usize)
        .map(|event| {
            let name = event.label.clone().unwrap_or_else(|| event.ip.to_string());
            Line::from(vec![
                Span::from(format!("  {} ", event.at.format("%H:%M:%S"))),
                Span::from(format!("{name:<20} ")),
                Span::from(event.previous.display().to_string())
                    .style(Style::default().fg(theme.status_color(&event.previous))),
                Span::from(" \u{2192} "),
                Span::from(event.current.display().to_string())
                    .style(Style::default().fg(theme.status_color(&event.current))),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

/// 2-minute-resolution chart of the modem's Internet-facing (lte1) traffic.
fn render_internet_traffic_chart(f: &mut Frame, area: Rect, app: &App) {
    let data = &app.internet_traffic_chart;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Internet Traffic ");

    if data.rx_kbps.is_empty() && data.tx_kbps.is_empty() {
        f.render_widget(block, area);
        return;
    }

    let y_max = data
        .rx_kbps
        .iter()
        .chain(data.tx_kbps.iter())
        .map(|(_, y)| *y)
        .fold(1.0f64, f64::max);
    let y_hi = (y_max * 1.1).max(1.0);
    let y_step = y_hi / 4.0;
    let y_labels: Vec<Span> = (0..=4)
        .map(|i| Span::raw(format!("{:.0}", y_step * i as f64)))
        .collect();

    let datasets = vec![
        Dataset::default()
            .name("Download (kbps)")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(app.theme().traffic_rx))
            .data(&data.rx_kbps),
        Dataset::default()
            .name("Upload (kbps)")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(app.theme().traffic_tx))
            .data(&data.tx_kbps),
    ];

    let chart = Chart::new(datasets)
        .block(block)
        .x_axis(
            Axis::default().bounds([0.0, 24.0]).labels(
                ["0h", "6h", "12h", "18h", "24h"]
                    .iter()
                    .map(|s| Span::raw(*s))
                    .collect::<Vec<_>>(),
            ),
        )
        .y_axis(Axis::default().bounds([0.0, y_hi]).labels(y_labels));

    f.render_widget(chart, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, KebaReading, ScannedDevice, SwitchReading};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::net::{IpAddr, Ipv4Addr};

    use crate::stats::{Bucket, Stats, StatsWindow, Totals};

    fn draw(app: &App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn bucket(label: &str, consumption: f64, production: f64, import: f64) -> Bucket {
        Bucket {
            label: label.to_string(),
            totals: Totals {
                consumption_kwh: consumption,
                production_kwh: production,
                grid_import_kwh: import,
                grid_export_kwh: 0.0,
            },
            has_data: true,
        }
    }

    fn stats_app(buckets: Vec<Bucket>, window: StatsWindow) -> App {
        let totals = crate::stats::total(&buckets);
        App {
            view: View::Statistics,
            stats: Stats {
                window,
                offset: 0,
                title: "August 2026".to_string(),
                buckets,
                totals,
                oldest_day: None,
            },
            ..Default::default()
        }
    }

    #[test]
    fn statistics_view_shows_totals_and_self_sufficiency() {
        let app = stats_app(
            vec![bucket("1", 10.0, 8.0, 4.0), bucket("2", 20.0, 10.0, 6.0)],
            StatsWindow::Month,
        );
        let out = draw(&app, 120, 24);

        assert!(out.contains("Monthly"), "{out}");
        assert!(out.contains("August 2026"), "{out}");
        // 30 kWh consumed, 18 produced, 10 imported -> 20/30 = 66.7% self-sufficient.
        assert!(out.contains("30.0"), "consumption total missing:\n{out}");
        assert!(out.contains("18.0"), "production total missing:\n{out}");
        assert!(out.contains("66.7"), "self-sufficiency missing:\n{out}");
        // 20 of 18 made was kept -> clamped to 100%.
        assert!(out.contains("Self-consumption"), "{out}");
    }

    #[test]
    fn statistics_view_marks_days_without_data_rather_than_drawing_zero_bars() {
        let mut buckets = vec![bucket("1", 10.0, 8.0, 4.0)];
        buckets.push(Bucket {
            label: "2".to_string(),
            totals: Totals::default(),
            has_data: false,
        });
        let app = stats_app(buckets, StatsWindow::Month);
        let out = draw(&app, 120, 24);
        assert!(out.contains("no data"), "gap should be labelled:\n{out}");
    }

    #[test]
    fn statistics_view_says_so_when_a_period_is_entirely_empty() {
        let app = stats_app(
            vec![Bucket {
                label: "1".to_string(),
                totals: Totals::default(),
                has_data: false,
            }],
            StatsWindow::Week,
        );
        let out = draw(&app, 120, 24);
        assert!(
            out.contains("No energy data recorded"),
            "empty period needs an explanation:\n{out}"
        );
        // With nothing to divide by, the ratios must read as unknown, never 0%.
        assert!(!out.contains("0.0 %"), "must not claim 0%:\n{out}");
    }

    #[test]
    fn statistics_view_shows_month_labels_for_the_yearly_window() {
        let app = stats_app(
            vec![
                bucket("Jan", 100.0, 20.0, 80.0),
                bucket("Feb", 90.0, 40.0, 50.0),
            ],
            StatsWindow::Year,
        );
        let out = draw(&app, 120, 24);
        assert!(out.contains("Yearly"), "{out}");
        assert!(out.contains("By month"), "{out}");
        assert!(out.contains("Jan"), "{out}");
        assert!(out.contains("Feb"), "{out}");
    }

    #[test]
    fn statistics_view_marks_that_it_is_showing_a_past_period() {
        let mut app = stats_app(vec![bucket("1", 10.0, 8.0, 4.0)], StatsWindow::Month);
        app.stats.offset = 3;
        let out = draw(&app, 120, 24);
        assert!(
            out.contains("3 back"),
            "browsing state should be visible:\n{out}"
        );
    }

    #[test]
    fn statusbar_advertises_the_statistics_keys() {
        let app = stats_app(vec![bucket("1", 1.0, 1.0, 0.0)], StatsWindow::Week);
        let out = draw(&app, 160, 10);
        assert!(out.contains("stats"), "{out}");
        assert!(out.contains("period"), "{out}");
    }

    /// Regression test: the Device Power / Device Energy panel heights must
    /// account for KEBA devices, not just batteries and switches, or a KEBA
    /// row gets clipped off the bottom of the panel.
    #[test]
    fn keba_row_visible_in_device_power_panel() {
        let switch_ip: IpAddr = Ipv4Addr::new(192, 168, 1, 40).into();
        let keba_ip: IpAddr = Ipv4Addr::new(192, 168, 1, 50).into();
        let mut app = App::default();
        app.devices.push(ScannedDevice {
            ip: switch_ip,
            latency_ms: 1.0,
            open_ports: vec![],
            name: Some(devices::mystrom_switch::NAME),
            label: None,
        });
        app.switch_readings.insert(
            switch_ip,
            SwitchReading {
                power_w: 100.0,
                relay_on: true,
                temperature_c: 20.0,
                updated_at: chrono::Utc::now(),
            },
        );
        app.devices.push(ScannedDevice {
            ip: keba_ip,
            latency_ms: 1.0,
            open_ports: vec![],
            name: Some(devices::keba::NAME),
            label: None,
        });
        app.keba_readings.insert(
            keba_ip,
            KebaReading {
                power_w: 1500.0,
                energy_session_kwh: 0.5,
                energy_total_kwh: 10.0,
                state: 3,
                plug: 7,
                curr_hw_ma: 16000,
                updated_at: chrono::Utc::now(),
            },
        );

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_energy_view(f, f.area(), &app))
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let content: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(
            content.contains(devices::keba::NAME),
            "KEBA row should be visible:\n{content}"
        );
        assert!(
            content.contains(devices::mystrom_switch::NAME),
            "switch row should be visible:\n{content}"
        );
    }

    /// Regression test: the Network view must no longer show DHCP Leases or
    /// Firewall Counters, and must render the new status-history and
    /// Internet-traffic panels (with real event/chart data) without panicking.
    #[test]
    fn network_view_shows_history_and_traffic_not_dhcp_or_firewall() {
        let router_ip: IpAddr = Ipv4Addr::new(192, 168, 1, 1).into();
        let mut app = App::default();
        app.devices.push(ScannedDevice {
            ip: router_ip,
            latency_ms: 1.0,
            open_ports: vec![],
            name: Some(devices::mikrotik::NAME),
            label: Some("Router".to_string()),
        });
        app.network_status_events
            .push(crate::app::NetworkStatusEvent {
                ip: router_ip,
                label: Some("Router".to_string()),
                previous: crate::app::NetworkDeviceStatus::Ok,
                current: crate::app::NetworkDeviceStatus::Lost,
                at: chrono::Utc::now(),
            });
        app.internet_traffic_chart.rx_kbps = vec![(1.0, 500.0), (1.5, 800.0)];
        app.internet_traffic_chart.tx_kbps = vec![(1.0, 50.0), (1.5, 80.0)];

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_network_view(f, f.area(), &app))
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let content: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(content.contains("Network Status History"), "{content}");
        assert!(content.contains("Internet Traffic"), "{content}");
        assert!(content.contains("Router"), "{content}");
        assert!(!content.contains("DHCP"), "{content}");
        assert!(!content.contains("Firewall"), "{content}");
    }

    /// Regression test: the Current view must show the network health panel
    /// where the device list/detail used to be, not "All Devices" or the
    /// per-device Detail panel.
    #[test]
    fn current_view_shows_network_health_not_device_list() {
        let router_ip: IpAddr = Ipv4Addr::new(192, 168, 1, 1).into();
        let mut app = App::default();
        app.devices.push(ScannedDevice {
            ip: router_ip,
            latency_ms: 1.0,
            open_ports: vec![],
            name: Some(devices::mikrotik::NAME),
            label: Some("Router".to_string()),
        });

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_current_view(f, f.area(), &app))
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let content: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(
            content.contains("Network Infrastructure Health"),
            "{content}"
        );
        assert!(content.contains("Router"), "{content}");
        assert!(!content.contains("All Devices"), "{content}");
        assert!(!content.contains(" Detail "), "{content}");
    }

    #[test]
    fn devices_view_lists_unconfigured_and_unknown_devices() {
        let unconfigured_ip: IpAddr = Ipv4Addr::new(192, 168, 1, 60).into();
        let unknown_ip: IpAddr = Ipv4Addr::new(192, 168, 1, 70).into();
        let mut app = App::default();
        // Recognized type, but never added to polled_ips (no credentials set).
        app.devices.push(ScannedDevice {
            ip: unconfigured_ip,
            latency_ms: 2.0,
            open_ports: vec![80],
            name: Some(devices::mikrotik::NAME),
            label: None,
        });
        // No recognized type at all.
        app.devices.push(ScannedDevice {
            ip: unknown_ip,
            latency_ms: 3.0,
            open_ports: vec![],
            name: None,
            label: None,
        });
        app.view = View::Devices;

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_devices_view(f, f.area(), &app))
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let content: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(content.contains("192.168.1.60"), "{content}");
        assert!(content.contains("192.168.1.70"), "{content}");
    }

    /// Regression test: a ping-scan failure must be visible in the Scan
    /// panel rather than silently reported as a normal completed scan.
    #[test]
    fn scan_status_shows_ping_scan_error() {
        let app = App {
            scan: ScanPhase::Done {
                at: chrono::Utc::now(),
                next_at: chrono::Utc::now(),
            },
            last_scan_error: Some("permission denied (os error 13)".to_string()),
            ..Default::default()
        };

        let backend = TestBackend::new(100, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_scan_status(f, f.area(), &app))
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let content: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(content.contains("ping scan failed"), "{content}");
        assert!(content.contains("permission denied"), "{content}");
    }

    /// Regression test: the Scan panel must show how many DHCP leases have
    /// actually reached the app — a separate signal from ping-scan health,
    /// since a router with an empty/never-fetched lease table is a different
    /// failure mode than ping-scanning being broken.
    #[test]
    fn scan_status_shows_dhcp_lease_count() {
        let router_ip: IpAddr = Ipv4Addr::new(192, 168, 1, 1).into();
        let mut app = App::default();
        app.mikrotik_readings.insert(
            router_ip,
            crate::app::MikrotikReading {
                leases: vec![
                    devices::mikrotik::DhcpLease {
                        address: "192.168.1.100".to_string(),
                        mac_address: "AA:BB:CC:DD:EE:FF".to_string(),
                        host_name: None,
                        comment: None,
                        status: "bound".to_string(),
                    },
                    devices::mikrotik::DhcpLease {
                        address: "192.168.1.101".to_string(),
                        mac_address: "AA:BB:CC:DD:EE:00".to_string(),
                        host_name: None,
                        comment: None,
                        status: "bound".to_string(),
                    },
                ],
                firewall_rules: vec![],
                updated_at: chrono::Utc::now(),
            },
        );

        let backend = TestBackend::new(100, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_scan_status(f, f.area(), &app))
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let content: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(content.contains("2 DHCP leases known"), "{content}");
    }

    /// Regression test: a MikroTik device's Detail panel must show its
    /// DHCP/firewall summary once it has a reading, not "No readings yet."
    /// forever — `render_detail`'s type dispatch previously had no branch
    /// for MikroTik, so it fell through to the LiveReading (Sonnen) branch,
    /// which is always empty for a router.
    #[test]
    fn detail_panel_shows_mikrotik_reading_not_no_readings_yet() {
        let router_ip: IpAddr = Ipv4Addr::new(192, 168, 1, 1).into();
        let mut app = App::default();
        app.devices.push(ScannedDevice {
            ip: router_ip,
            latency_ms: 1.0,
            open_ports: vec![80],
            name: Some(devices::mikrotik::NAME),
            label: Some("Router".to_string()),
        });
        app.mikrotik_readings.insert(
            router_ip,
            crate::app::MikrotikReading {
                leases: vec![devices::mikrotik::DhcpLease {
                    address: "192.168.1.100".to_string(),
                    mac_address: "AA:BB:CC:DD:EE:FF".to_string(),
                    host_name: None,
                    comment: None,
                    status: "bound".to_string(),
                }],
                firewall_rules: vec![],
                updated_at: chrono::Utc::now(),
            },
        );
        app.view = View::Devices;
        app.selected = 0;

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render_detail(f, f.area(), &app)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let content: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(content.contains("DHCP"), "{content}");
        assert!(content.contains("1 leases"), "{content}");
        assert!(!content.contains("No readings yet"), "{content}");
    }
}
