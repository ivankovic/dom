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

//! Drawing, and only drawing.
//!
//! One entry point — `render` — which lays out a one-line status bar above
//! whichever of the six views [`View`] currently names, then draws the timer
//! dialog over the top if one is open. Everything below that is the six views
//! and the widgets they are made of, in the sections the `── ──` headers mark.
//!
//! # What this module is not allowed to do
//!
//! It takes `&App` and a `Frame`. It performs no I/O, awaits nothing, queries
//! nothing, and mutates nothing — every number it draws was put in
//! [`App`](crate::app::App) by a background task or by `apply_key`. A view that
//! wants data that is not there yet does not fetch it; something else fetches it
//! on a schedule and the view draws what has arrived.
//!
//! That is what keeps a 100 ms redraw affordable on a Raspberry Pi, and it is
//! also why this file is large: the work of turning state into a chart happens
//! here rather than being spread through the tasks that produced the state.
//!
//! # Drawing a value that is not there
//!
//! Most of the formatting helpers take an `Option` and return a string either
//! way, because "not measured yet", "the device is unreachable" and "the value
//! is zero" are three different things and the interface has to keep them apart.
//! A missing reading is a dash, never a zero — a zero is a measurement.
//! `altitude` is the smallest example of the pattern.
//!
//! The status bar carries the same idea for Dom's own health: normally it shows
//! the key hints, but when `App::write_error` is set it replaces them with
//! `not recording measurements — …`. A poll loop that fetches happily while
//! every write fails otherwise looks identical, on screen, to one that is fine.
//!
//! Colours come from [`Theme`](crate::tui::theme::Theme) rather than being named
//! here, since the palette depends on what the terminal turned out to be — see
//! `tui::theme`.
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
        View::Environment => render_environment_view(f, rows[1], app),
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

/// A station altitude, or an em dash when the published data carries none.
///
/// The same choice `pct` makes below, for the same reason: the figure is there
/// to qualify the temperature beside it, so a value that qualifies nothing has
/// to read as absent rather than as a number. It used to be `f64::NAN` and
/// printed as `NaN m`.
fn altitude(m: Option<f64>) -> String {
    match m {
        Some(m) => format!("{m:.0} m"),
        None => "— m".to_string(),
    }
}

/// A percentage, or an em dash when the ratio has no denominator — see
/// `stats::Totals::self_sufficiency_pct`. Never prints 0% for "unknown".
fn pct(v: Option<f64>) -> String {
    match v {
        Some(p) => format!("{p:>5.1} %"),
        // Padded to the same width as a real figure, so the column below stays
        // aligned when a period has nothing to divide by.
        None => "    —  ".to_string(),
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
    // A failing rollup is stated here rather than only logged: these totals stop
    // advancing when it fails, and so does pruning. See `crate::logging`.
    let mut header = vec![
        Span::from(format!(" {} · ", st.window.label()))
            .style(Style::default().add_modifier(Modifier::BOLD)),
        Span::from(st.title.clone()).style(Style::default().fg(theme.focus_border)),
        browsing,
    ];
    match &app.rollup_error {
        Some(e) => header.push(
            Span::from(format!("   rollup failing — totals are stale: {e}"))
                .style(Style::default().fg(theme.status_lost)),
        ),
        None => header.push(
            Span::from("   m/y window · ←/→ period").style(Style::default().fg(theme.inactive)),
        ),
    }
    f.render_widget(Paragraph::new(Line::from(header)), rows[0]);

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
    let mut totals = totals;
    // Shown only when it happened. On an installation whose battery charges from
    // the sun alone this is always zero, and a permanent zero row would be one
    // more thing to read past; when it is not zero it is the line that explains
    // an import figure much larger than the house used.
    if t.grid_to_battery_kwh > 0.0 {
        totals.push(Line::from(vec![
            Span::from("   of which to  "),
            Span::from(kwh(t.grid_to_battery_kwh)).style(Style::default().fg(theme.grid_import)),
            Span::from(" kWh  charged the battery").style(Style::default().fg(theme.inactive)),
        ]));
    }
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
                    bar_segment(own_cells),
                    Style::default().fg(theme.production),
                ),
                Span::styled(
                    bar_segment(cells - own_cells),
                    Style::default().fg(theme.grid_import),
                ),
                Span::from(" ".repeat(bar_w - cells)),
                Span::from(format!("{} kWh", kwh(b.totals.consumption_kwh))),
                Span::from(format!("  {} kWh", kwh(b.totals.production_kwh)))
                    .style(Style::default().fg(theme.production)),
                Span::from(format!("  {}", pct(b.totals.self_sufficiency_pct())))
                    .style(Style::default().fg(theme.inactive)),
            ])
        })
        .collect();

    // Named, because the two segments are the whole point of the row and colour
    // alone does not say which is which.
    let dim = Style::default().fg(theme.inactive);
    let legend = Line::from(vec![
        Span::from("      "),
        Span::styled(bar_segment(6), Style::default().fg(theme.production)),
        Span::from(" own generation   ").style(dim),
        Span::styled(bar_segment(6), Style::default().fg(theme.grid_import)),
        Span::from(" imported").style(dim),
    ]);
    // The three figures after the bar, named over the columns they belong to.
    // Right-aligned to each column's end, which is where the numbers end too.
    let headings = Line::from(
        Span::from(format!(
            "{:>label$}{:>consumed$}{:>produced$}{:>ratio$}",
            "",
            "consumed",
            "produced",
            "self",
            label = ROW_LABEL_WIDTH + bar_w,
            consumed = COL_CONSUMED,
            produced = COL_PRODUCED,
            ratio = COL_SELF_SUFFICIENCY,
        ))
        .style(dim),
    );
    let lines = [legend, headings]
        .into_iter()
        .chain(lines)
        .collect::<Vec<_>>();

    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// The day or month label at the start of each row: `"  {:<4}"`.
const ROW_LABEL_WIDTH: usize = 6;

/// Widths of the three figures that follow each bar.
///
/// Named because the heading that labels them has to line up with them, and the
/// only way to be sure of that is for both to be laid out from the same numbers.
/// Each is the full width of its column including the leading gap: `{:>7.1} kWh`,
/// `  {:>7.1} kWh`, and `  ` followed by the seven characters `pct` returns.
///
/// Both energy figures carry their unit, because both are energy and the ratio
/// column already repeats its `%` on every row. Carrying it also means a row
/// says what it means on its own, without reading back up to the heading.
///
/// The production column used to carry a literal `prod` before it, from before
/// the columns were named in a heading. Two labels for one number is one too
/// many, and the heading is the one that stays put when the row is scrolled past.
const COL_CONSUMED: usize = 11;
const COL_PRODUCED: usize = 13;
const COL_SELF_SUFFICIENCY: usize = 9;

/// One run of a stacked bar, drawn with an end mark at each end.
///
/// The reason the bars are not solid blocks. Two abutting runs of `█` in
/// different colours are ambiguous about what the second one is measured from:
/// it reads equally as "imported, stacked on top of own generation" and as
/// "imported, drawn from zero and overlapping". Ending each run explicitly —
/// `┃━━━━┃┃━━┃` — leaves only the first reading.
///
/// The returned string is exactly `width` characters, so the columns after the
/// bar stay aligned. A run of one has no room for two ends and gets a single
/// mark; a run of none draws nothing at all, which is what a fully self-
/// sufficient day or a fully imported one should look like.
fn bar_segment(width: usize) -> String {
    /// Heavy vertical and horizontal box-drawing, as the Environment view's
    /// daily-range chart already uses.
    const END: &str = "\u{2503}";
    const LINE: &str = "\u{2501}";

    match width {
        0 => String::new(),
        1 => END.to_string(),
        n => {
            let mut out = String::from(END);
            out.push_str(&LINE.repeat(n - 2));
            out.push_str(END);
            out
        }
    }
}

// ── Environment view ──────────────────────────────────────────────────────────

/// Sensors reporting a temperature, in a stable display order.
///
/// Today these are myStrom switches reporting their own case temperature rather
/// than a room reading — the view says so, because presenting an appliance's
/// internal temperature as ambient would be misleading.
pub(super) fn temperature_sensors(app: &App) -> Vec<(IpAddr, String, &SwitchReading)> {
    let mut out: Vec<(IpAddr, String, &SwitchReading)> = app
        .switch_readings
        .iter()
        .map(|(ip, r)| {
            let name = app
                .devices
                .iter()
                .find(|d| d.ip == *ip)
                .map(|d| d.display_name().to_string())
                .unwrap_or_else(|| ip.to_string());
            (*ip, name, r)
        })
        .collect();
    out.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    out
}

fn render_environment_view(f: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let sensors = temperature_sensors(app);

    // Outdoor and Solar sit side by side and share a height, so the row is as
    // tall as whichever has more to say.
    let outdoor = outdoor_lines(app, &theme);
    let solar = solar_lines(app, &theme);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(outdoor.len().max(solar.len()) as u16 + 2),
            Constraint::Length(sensors.len().max(1) as u16 + 2),
            Constraint::Min(0),
        ])
        .split(area);
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
        .split(rows[1]);

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::from(" Environment").style(Style::default().add_modifier(Modifier::BOLD)),
            Span::from(
                "  ·  outdoor from MeteoSwiss; sensor readings are device, not room, temperature",
            )
            .style(Style::default().fg(theme.inactive)),
        ])),
        rows[0],
    );

    render_outdoor(f, top[0], app, &theme);
    f.render_widget(
        Paragraph::new(solar).block(Block::default().borders(Borders::ALL).title(" Solar ")),
        top[1],
    );
    render_environment_sensors(f, rows[2], app, &theme, &sensors);
    render_environment_history(f, rows[3], app, &theme, &sensors);
}

/// Outdoor temperature, the station it came from, and the configured location.
///
/// While an address is being typed this becomes the input. Every state is
/// distinguished explicitly — no location set, typing, a failed fetch, and a real
/// reading all look different, because a stale number and a broken network must
/// not be indistinguishable.
fn render_outdoor(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let block = Block::default().borders(Borders::ALL).title(" Outdoor ");
    f.render_widget(Paragraph::new(outdoor_lines(app, theme)).block(block), area);
}

/// The Outdoor panel's contents. Built separately from rendering so the layout can
/// size the panel to what it actually has to say — the number of lines varies with
/// whether a location is set, a fetch failed, or the user is typing.
fn outdoor_lines<'a>(app: &'a App, theme: &Theme) -> Vec<Line<'a>> {
    if let Some(buf) = &app.address_input {
        return vec![
            Line::from(vec![
                Span::from("  Address  ["),
                Span::styled(
                    format!("{buf}│"),
                    Style::default()
                        .fg(theme.input_active)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::from("]"),
            ]),
            Line::from(
                Span::from("  Enter to look up · Esc to cancel")
                    .style(Style::default().fg(theme.inactive)),
            ),
        ];
    }

    let mut lines: Vec<Line> = Vec::new();
    match (&app.outdoor, &app.location) {
        (Some(o), _) => {
            lines.push(Line::from(vec![
                Span::from("  "),
                Span::from(format!("{:5.1} °C", o.temperature_c)).style(
                    Style::default()
                        .fg(theme.battery_charge)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::from(format!("   {}", o.station_name))
                    .style(Style::default().fg(theme.focus_border)),
                Span::from(format!(
                    "  ·  {}  ·  {:.1} km away",
                    altitude(o.altitude_m),
                    o.distance_km
                ))
                .style(Style::default().fg(theme.inactive)),
            ]));
            let range = match app.outdoor_today {
                Some((lo, hi)) => format!("today {lo:.1} – {hi:.1} °C"),
                None => "today —".to_string(),
            };
            lines.push(Line::from(
                Span::from(format!(
                    "  {range}   measured {}",
                    o.measured_at.with_timezone(&chrono::Local).format("%H:%M")
                ))
                .style(Style::default().fg(theme.inactive)),
            ));
        }
        (None, Some(loc)) => lines.push(Line::from(
            Span::from(format!("  Waiting for the first reading for {}", loc.label))
                .style(Style::default().fg(theme.inactive)),
        )),
        (None, None) => lines.push(Line::from(
            Span::from("  No location set — press 'a' to enter an address.")
                .style(Style::default().fg(theme.inactive)),
        )),
    }

    if let Some(loc) = &app.location {
        lines.push(Line::from(
            Span::from(format!("  {}   ['a' to change]", loc.label))
                .style(Style::default().fg(theme.inactive)),
        ));
    }
    if let Some(err) = &app.last_weather_error {
        lines.push(Line::from(
            Span::from(format!("  last fetch failed: {err}"))
                .style(Style::default().fg(theme.status_lost)),
        ));
    }
    lines
}

/// Eight levels of block, for the inline forecast curve. A sparkline rather than
/// a chart widget because it sits on one line beside the numbers it qualifies —
/// the shape of tomorrow (an even arc, or a hole punched in the middle of the
/// day) is the part a person reads at a glance.
const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Tomorrow's predicted power as one line of blocks, over daylight only.
fn sparkline(curve: &[(f64, f64)], width: usize) -> String {
    let peak = curve.iter().map(|(_, kw)| *kw).fold(0.0_f64, f64::max);
    if peak <= 0.0 || width == 0 {
        return String::new();
    }
    // Daylight only: a bar per hour of darkness says nothing and squeezes the
    // part that does.
    let lit: Vec<_> = curve.iter().filter(|(_, kw)| *kw > 0.0).collect();
    if lit.is_empty() {
        return String::new();
    }
    (0..width)
        .map(|i| {
            let from = i * lit.len() / width;
            let to = ((i + 1) * lit.len() / width).max(from + 1).min(lit.len());
            let mean = lit[from..to].iter().map(|(_, kw)| *kw).sum::<f64>() / (to - from) as f64;
            let level = ((mean / peak) * (SPARK.len() - 1) as f64).round() as usize;
            SPARK[level.min(SPARK.len() - 1)]
        })
        .collect()
}

/// The Solar panel's contents.
///
/// Every state is distinguished: no location, a location but no fit yet, a fit
/// too thin to lean on, and a working forecast all read differently. A predicted
/// number with nothing behind it must not look like one with forty days behind
/// it.
fn solar_lines<'a>(app: &'a App, theme: &Theme) -> Vec<Line<'a>> {
    let dim = Style::default().fg(theme.inactive);
    let s = &app.solar;

    if app.location.is_none() {
        return vec![Line::from(
            Span::from("  Set a location with 'a' to forecast production").style(dim),
        )];
    }

    let mut lines: Vec<Line> = Vec::new();

    match (s.tomorrow_kwh, &s.calibration) {
        (Some(kwh), Some(cal)) => {
            // The band comes from how past forecasts actually turned out, not
            // from a figure written into the code: before there are enough
            // compared days it is simply absent, which is the honest reading.
            let band = if s.accuracy.is_meaningful() {
                format!("± {:.0}%", s.accuracy.mape * 100.0)
            } else {
                String::new()
            };
            lines.push(Line::from(vec![
                Span::from("  Tomorrow  "),
                Span::from(format!("{kwh:5.1} kWh")).style(
                    Style::default()
                        .fg(theme.production)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::from(format!("  {band}")).style(dim),
                Span::from("   "),
                Span::from(sparkline(&s.tomorrow_curve, 16))
                    .style(Style::default().fg(theme.production)),
            ]));
            if !cal.is_well_founded() {
                lines.push(Line::from(
                    Span::from(format!(
                        "  Fitted on {} days — rough until there are {}",
                        cal.days,
                        crate::solar::MIN_CALIBRATION_DAYS
                    ))
                    .style(Style::default().fg(theme.level_warn)),
                ));
            }
        }
        (_, None) => lines.push(Line::from(
            Span::from("  Learning the array from recorded production…").style(dim),
        )),
        (None, Some(_)) => lines.push(Line::from(
            Span::from("  No forecast for tomorrow yet").style(dim),
        )),
    }

    if let Some(today) = s.today_kwh {
        lines.push(Line::from(vec![
            Span::from("  Today     "),
            Span::from(format!("{today:5.1} kWh")),
            Span::from(format!("  expected · {:.1} kWh so far", s.today_actual_kwh)).style(dim),
        ]));
    }

    if let Some(cloud) = s.tomorrow_cloud_pct {
        lines.push(Line::from(
            Span::from(format!("  Cloud cover tomorrow {cloud:.0}% of daylight")).style(dim),
        ));
    }

    // The most recent finished day, as forecast against as produced. One day
    // rather than a list: it answers "is this thing working" without becoming a
    // table nobody reads.
    if let Some((day, predicted, actual)) = s.recent.last() {
        let err = predicted - actual;
        let colour = if actual > &0.0 && (err / actual).abs() <= 0.10 {
            theme.status_ok
        } else {
            theme.level_warn
        };
        lines.push(Line::from(vec![
            Span::from(format!("  {}    ", day.format("%d %b"))),
            Span::from(format!("{predicted:5.1} kWh")).style(dim),
            Span::from(format!(" expected, {actual:.1} made  ")),
            Span::from(format!("{err:+.1}")).style(Style::default().fg(colour)),
        ]));
    }

    if let Some(cal) = &s.calibration {
        let facing = match cal.azimuth_deg {
            0 => "due south".to_string(),
            d if d > 0 => format!("{d}° west of south"),
            d => format!("{}° east of south", -d),
        };
        lines.push(Line::from(
            Span::from(format!(
                "  {:.1} kWp · {}° tilt · {facing}",
                cal.peak_w() / 1000.0,
                cal.tilt_deg
            ))
            .style(dim),
        ));
    }
    if s.accuracy.is_meaningful() {
        lines.push(Line::from(
            Span::from(format!(
                "  {} of the last {} days within 10%",
                s.accuracy.within_10pct, s.accuracy.days
            ))
            .style(dim),
        ));
    }

    if let Some(e) = &s.last_error {
        lines.push(Line::from(
            Span::from(format!("  Forecast unavailable: {e}"))
                .style(Style::default().fg(theme.status_lost)),
        ));
    }

    // Required by the data licence wherever the data is shown.
    lines.push(Line::from(
        Span::from(format!("  {}", crate::online::forecast::ATTRIBUTION)).style(dim),
    ));
    lines
}

fn render_environment_sensors(
    f: &mut Frame,
    area: Rect,
    app: &App,
    theme: &Theme,
    sensors: &[(IpAddr, String, &SwitchReading)],
) {
    let block = Block::default().borders(Borders::ALL).title(" Sensors ");
    if sensors.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(
                Span::from("  No device is reporting a temperature.")
                    .style(Style::default().fg(theme.inactive)),
            ))
            .block(block),
            area,
        );
        return;
    }

    let today = chrono::Local::now().date_naive();
    let lines: Vec<Line> = sensors
        .iter()
        .enumerate()
        .map(|(i, (ip, name, r))| {
            let selected = i == app.env_selected.min(sensors.len() - 1);
            // Today's range comes from the rollup, which is only as current as
            // the last pass — so it can lag the live reading by a few minutes.
            let today_row = app
                .temperature_history
                .iter()
                .find(|t| t.day == today && t.ip == *ip);
            let range = match today_row {
                Some(t) => format!("today {:5.1} – {:5.1}", t.min_c, t.max_c),
                None => "today       –      ".to_string(),
            };
            let age = format_age(chrono::Utc::now() - r.updated_at);
            let marker = if selected { "▸ " } else { "  " };
            let style = if selected {
                Style::default()
                    .fg(theme.selection_fg)
                    .bg(theme.selection_bg)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::from(format!("{marker}{name:<24}")).style(style),
                Span::from(format!("{:6.1} °C  ", r.temperature_c))
                    .style(Style::default().fg(theme.battery_discharge)),
                Span::from(range).style(Style::default().fg(theme.inactive)),
                Span::from(format!("  {age:>9}")).style(Style::default().fg(theme.inactive)),
            ])
        })
        .collect();

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn format_age(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

/// A row per day for the selected sensor: the min–max span drawn as a bar across
/// a shared temperature scale, so a hot day is visibly wider and higher than a
/// cold one, with the daily mean marked inside it.
fn render_environment_history(
    f: &mut Frame,
    area: Rect,
    app: &App,
    theme: &Theme,
    sensors: &[(IpAddr, String, &SwitchReading)],
) {
    let selected = sensors.get(app.env_selected.min(sensors.len().saturating_sub(1)));
    let title = match selected {
        Some((_, name, _)) => format!(" Daily range — {name} "),
        None => " Daily range ".to_string(),
    };
    let block = Block::default().borders(Borders::ALL).title(title);

    let Some((ip, _, _)) = selected else {
        f.render_widget(block, area);
        return;
    };
    let rows: Vec<&crate::db::DailyTemperature> = app
        .temperature_history
        .iter()
        .filter(|t| t.ip == *ip)
        .collect();

    if rows.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(
                Span::from("  No daily history recorded yet — it accumulates as days pass.")
                    .style(Style::default().fg(theme.inactive)),
            ))
            .block(block),
            area,
        );
        return;
    }

    let lo = rows.iter().map(|t| t.min_c).fold(f64::INFINITY, f64::min);
    let hi = rows
        .iter()
        .map(|t| t.max_c)
        .fold(f64::NEG_INFINITY, f64::max);
    let span = (hi - lo).max(0.1);
    let bar_w = (area.width as usize).saturating_sub(38).max(10);

    // Newest last reads naturally downward, like the rest of the app's history.
    let lines: Vec<Line> = rows
        .iter()
        .map(|t| {
            let cell =
                |v: f64| -> usize { (((v - lo) / span) * (bar_w - 1) as f64).round() as usize };
            let (from, to, mean) = (cell(t.min_c), cell(t.max_c), cell(t.avg_c));
            let mut bar = String::new();
            for i in 0..bar_w {
                bar.push(if i == mean {
                    '┃'
                } else if i >= from && i <= to {
                    '━'
                } else {
                    ' '
                });
            }
            Line::from(vec![
                Span::from(format!("  {}  ", t.day.format("%d %b"))),
                Span::from(format!("{:5.1}", t.min_c))
                    .style(Style::default().fg(theme.battery_charge)),
                Span::from(" "),
                Span::from(bar).style(Style::default().fg(theme.battery_discharge)),
                Span::from(" "),
                Span::from(format!("{:5.1}", t.max_c))
                    .style(Style::default().fg(theme.consumption)),
                Span::from(format!("   avg {:5.1}", t.avg_c))
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
    let nav = "[c] current  [e] energy  [n] network  [d] devices  [v] environment  [m/y] stats  [s] rescan";
    let focus_hints = if app.timer_dialog.is_some() {
        "[Tab] switch field  [Enter] save  [Esc] cancel  [Ctrl+C] quit".to_string()
    } else if app.rename_input.is_some() {
        "[Enter] save name  [Esc] cancel  [Ctrl+C] quit".to_string()
    } else if app.view == View::Energy {
        "[↑↓] select device  [Enter] toggle switch  [q] quit".to_string()
    } else if app.view == View::Environment {
        if app.address_input.is_some() {
            "[Enter] look up address  [Esc] cancel  [Ctrl+C] quit".to_string()
        } else {
            "[↑↓] select sensor  [a] set address  [t] theme  [q] quit".to_string()
        }
    } else if app.view == View::Statistics {
        "[←→] period  [m] monthly  [y] yearly  [t] theme  [q] quit".to_string()
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

    // A failed *write* replaces the hints outright rather than sitting beside
    // them. It is the one fault with no home in any single view — every device
    // still reads fine and every panel still shows live numbers, which is
    // exactly what makes it worth interrupting for — and the status bar is the
    // only line on screen in all six views. See `App::write_error`.
    let line = match &app.write_error {
        Some(e) => Line::from(vec![
            Span::from(format!("  {now}  ·  ")),
            Span::from(format!("not recording measurements — {e}")).style(
                Style::default()
                    .fg(theme.status_lost)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        None => Line::from(format!("  {now}  ·  {hints}")),
    };
    f.render_widget(
        Paragraph::new(line).style(
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
    let mode = app.switch_auto_modes.get(&ip);
    let is_time = matches!(mode, Some(SwitchAutoMode::Time));
    let is_eco = matches!(mode, Some(SwitchAutoMode::Eco));
    let timers = app.switch_timers.get(&ip);

    let relay = if r.relay_on { "● ON" } else { "○ OFF" };
    lines.push(highlighted_line(
        format!("  Relay    {relay}"),
        focused && row == 0,
        &theme,
    ));

    let mode_str = match mode {
        Some(SwitchAutoMode::Time) => "Time",
        Some(SwitchAutoMode::Eco) => "Eco",
        _ => "Disabled",
    };
    lines.push(highlighted_line(
        format!("  Auto     {mode_str}"),
        focused && row == 1,
        &theme,
    ));

    if is_eco {
        let text = match app.switch_eco_plans.get(&ip) {
            Some(plan) => {
                let on = plan.on_at.with_timezone(&chrono::Local).format("%H:%M");
                let off = plan.off_at.with_timezone(&chrono::Local).format("%H:%M");
                match plan.predicted_grid_wh {
                    Some(wh) => format!(
                        "  Eco      today {on}\u{2013}{off}, ~{:.1} kWh from grid",
                        wh / 1000.0
                    ),
                    // The timer fallback — say so, rather than dressing the
                    // user's own clock times up as something Eco chose.
                    None => format!(
                        "  Eco      today {on}\u{2013}{off} — timer fallback, not enough data to plan"
                    ),
                }
            }
            // Only reachable with no on/off pair at all: `eco_job` skips a
            // switch without one before it ever plans, and every other
            // shortfall now lands on the fallback above.
            None => "  Eco      no plan yet — needs an on/off timer pair".to_string(),
        };
        lines.push(Line::from(text));
    }

    if is_time || is_eco {
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
    let security = security_lines(app, &app.theme());
    let cluster = cluster_lines(app, &app.theme());
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(list_h),
            Constraint::Length(if security.is_empty() {
                0
            } else {
                security.len() as u16 + 2
            }),
            Constraint::Length(if cluster.is_empty() {
                0
            } else {
                cluster.len() as u16 + 2
            }),
            Constraint::Min(0),
        ])
        .split(area);

    render_network_infrastructure_health(
        f,
        rows[0],
        app,
        &router_devices,
        &modem_devices,
        &ap_devices,
    );

    if !security.is_empty() {
        f.render_widget(
            Paragraph::new(security)
                .block(Block::default().borders(Borders::ALL).title(" Security ")),
            rows[1],
        );
    }

    if !cluster.is_empty() {
        f.render_widget(
            Paragraph::new(cluster)
                .block(Block::default().borders(Borders::ALL).title(" Cluster ")),
            rows[2],
        );
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[3]);

    render_network_status_history(f, cols[0], app);
    render_internet_traffic_chart(f, cols[1], app);
}

/// How Dom is talking to the infrastructure, when that is worth saying.
///
/// Empty — and so not drawn at all — when every device is reached over TLS with
/// the certificate Dom pinned for it. Two things break that, and they are very
/// different: a device that is not serving HTTPS at all, whose credentials are
/// therefore crossing the network readable; and a device presenting a
/// certificate that is not the pinned one, which Dom refuses to talk to until a
/// person accepts it.
fn security_lines<'a>(app: &'a App, theme: &Theme) -> Vec<Line<'a>> {
    let mut lines: Vec<Line> = Vec::new();

    // Changed certificates first: nothing is being polled on those devices.
    let mut changed: Vec<_> = app.cert_alerts.iter().collect();
    changed.sort_by_key(|(ip, _)| **ip);
    for (ip, alert) in changed {
        lines.push(Line::from(vec![
            Span::from("  "),
            Span::from(format!("{ip} presented a different TLS certificate")).style(
                Style::default()
                    .fg(theme.status_lost)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(
            Span::from(format!(
                "    pinned {}   now {}",
                crate::devices::tls::short_fingerprint(&alert.expected),
                crate::devices::tls::short_fingerprint(&alert.observed),
            ))
            .style(Style::default().fg(theme.inactive)),
        ));
        lines.push(Line::from(
            Span::from(
                "    Not polling it, and not sending its password. If you reset or reinstalled \
                 this device, press 'k' to accept the new certificate.",
            )
            .style(Style::default().fg(theme.level_warn)),
        ));
    }

    let mut cleartext: Vec<_> = app.cleartext_devices.iter().collect();
    cleartext.sort();
    if !cleartext.is_empty() {
        let list = cleartext
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(Line::from(
            Span::from(format!("  Sending credentials in the clear to {list}"))
                .style(Style::default().fg(theme.level_warn)),
        ));
        lines.push(Line::from(
            Span::from(
                "    These are not serving HTTPS. Enable it on each: \
                 /certificate add name=dom common-name=dom; /certificate sign dom; \
                 /ip service set www-ssl certificate=dom disabled=no",
            )
            .style(Style::default().fg(theme.inactive)),
        ));
    }
    lines
}

/// A discovered (or re-identified) cluster peer awaiting one explicit pairing confirmation — see
/// `App::cluster_pairing_prompt`. Empty, and so not drawn at all, once nothing is pending — same
/// "absent when nominal" shape as `security_lines`.
fn cluster_lines<'a>(app: &'a App, theme: &Theme) -> Vec<Line<'a>> {
    let Some(prompt) = &app.cluster_pairing_prompt else {
        return Vec::new();
    };

    let heading = if prompt.replaces_pin {
        format!(
            "A peer at {} answers with a different identity than the one pinned",
            prompt.addr
        )
    } else {
        format!("Found a Dom instance at {}", prompt.addr)
    };
    let instruction = if prompt.replaces_pin {
        "    If this is expected (the peer was reinstalled or reset), press 'p' to re-pair. If \
         not, leave it — an unrecognized identity is never trusted automatically."
    } else {
        "    If this is your other Dom instance, press 'p' to pair with it."
    };

    vec![
        Line::from(vec![
            Span::from("  "),
            Span::from(heading).style(
                Style::default()
                    .fg(theme.status_lost)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(
            Span::from(format!(
                "    node {}   key {}",
                prompt.node_id,
                crate::devices::tls::short_fingerprint(&prompt.public_key),
            ))
            .style(Style::default().fg(theme.inactive)),
        ),
        Line::from(Span::from(instruction).style(Style::default().fg(theme.level_warn))),
    ]
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

    // ── The status bar's write-failure line ───────────────────────────────────

    #[test]
    fn the_status_bar_shows_the_key_hints_when_everything_is_recording() {
        let out = draw(&App::default(), 160, 6);
        let bar = out.lines().next().unwrap();
        assert!(bar.contains("[q] quit"), "{bar}");
        assert!(!bar.contains("not recording"), "{bar}");
    }

    #[test]
    fn a_failed_write_is_stated_in_every_view() {
        // The failure this exists for is invisible everywhere else: the device
        // reads fine, so its row says Online and the panels show live numbers,
        // while nothing is reaching the database. The status bar is the only
        // line present in all six views, so that is where it has to go.
        let mut app = App {
            write_error: Some("sonnen 172.16.0.5: database is locked".to_string()),
            ..Default::default()
        };

        for view in [
            View::Current,
            View::Energy,
            View::Network,
            View::Devices,
            View::Statistics,
            View::Environment,
        ] {
            app.view = view;
            let out = draw(&app, 160, 8);
            let bar = out.lines().next().unwrap();
            assert!(
                bar.contains("not recording measurements"),
                "view did not carry the warning: {bar}"
            );
            assert!(bar.contains("database is locked"), "{bar}");
        }
    }

    #[test]
    fn the_write_failure_replaces_the_hints_rather_than_crowding_them() {
        // At a realistic width the hints alone already fill the line, so
        // appending would push the warning off the end of a terminal that is
        // not especially narrow — which is the same as not showing it.
        let app = App {
            write_error: Some("keba 172.16.0.9: disk I/O error".to_string()),
            ..Default::default()
        };
        let bar = draw(&app, 100, 6).lines().next().unwrap().to_string();
        assert!(bar.contains("not recording measurements"), "{bar}");
        assert!(!bar.contains("[q] quit"), "{bar}");
    }

    /// A bucket whose imported energy all reached the house, which is every day
    /// on a battery that only ever charges from the sun.
    fn bucket(label: &str, consumption: f64, production: f64, import: f64) -> Bucket {
        Bucket {
            label: label.to_string(),
            totals: Totals {
                consumption_kwh: consumption,
                production_kwh: production,
                grid_import_kwh: import,
                grid_export_kwh: 0.0,
                grid_to_house_kwh: import,
                grid_to_battery_kwh: 0.0,
            },
            has_data: true,
        }
    }

    /// A bucket that imported energy into the battery rather than the house.
    fn bucket_charging_from_grid(
        label: &str,
        consumption: f64,
        to_house: f64,
        to_battery: f64,
    ) -> Bucket {
        Bucket {
            label: label.to_string(),
            totals: Totals {
                consumption_kwh: consumption,
                production_kwh: 0.0,
                grid_import_kwh: to_house + to_battery,
                grid_export_kwh: 0.0,
                grid_to_house_kwh: to_house,
                grid_to_battery_kwh: to_battery,
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

    fn env_app(temps: Vec<(&str, f64)>, history: Vec<(&str, &str, f64, f64, f64)>) -> App {
        let mut app = App {
            view: View::Environment,
            ..Default::default()
        };
        for (ip, t) in temps {
            let addr: IpAddr = ip.parse().unwrap();
            app.devices.push(ScannedDevice {
                ip: addr,
                latency_ms: 1.0,
                open_ports: vec![80],
                name: Some(devices::mystrom_switch::NAME),
                label: Some(format!("Sensor {}", ip.split('.').next_back().unwrap())),
            });
            app.switch_readings.insert(
                addr,
                SwitchReading {
                    power_w: 10.0,
                    relay_on: true,
                    temperature_c: t,
                    updated_at: chrono::Utc::now(),
                },
            );
        }
        app.temperature_history = history
            .into_iter()
            .map(|(ip, d, lo, hi, avg)| crate::db::DailyTemperature {
                ip: ip.parse().unwrap(),
                day: chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").unwrap(),
                min_c: lo,
                max_c: hi,
                avg_c: avg,
            })
            .collect();
        app
    }

    fn with_outdoor(mut app: App) -> App {
        app.location = Some(crate::db::Location {
            label: "Bundesplatz 3 3011 Bern".to_string(),
            east: 2_600_423.0,
            north: 1_199_521.0,
            latitude: 46.9468,
            longitude: 7.4442,
        });
        app.outdoor = Some(crate::app::OutdoorReading {
            station_name: "Bern / Zollikofen".to_string(),
            temperature_c: 28.2,
            altitude_m: Some(555.0),
            distance_km: 5.1,
            measured_at: chrono::Utc::now(),
        });
        app.outdoor_today = Some((14.6, 29.3));
        app
    }

    #[test]
    fn an_unknown_altitude_reads_as_absent_rather_than_as_nan() {
        // `f64::NAN` formatted straight through as `NaN m`, which is worse than
        // silence: the altitude is there to qualify the temperature, so one
        // that qualifies nothing must look like the absence it is.
        assert_eq!(altitude(Some(555.0)), "555 m");
        assert_eq!(altitude(Some(1880.4)), "1880 m");
        assert_eq!(altitude(None), "— m");

        let mut app = with_outdoor(env_app(vec![("192.168.1.10", 21.5)], vec![]));
        if let Some(o) = app.outdoor.as_mut() {
            o.altitude_m = None;
        }
        let out = draw(&app, 130, 24);
        assert!(!out.contains("NaN"), "{out}");
        assert!(
            out.contains("28.2"),
            "the reading itself is still shown: {out}"
        );
        assert!(out.contains("5.1 km"), "and so is the distance: {out}");
    }

    #[test]
    fn outdoor_panel_qualifies_the_reading_with_station_distance_and_altitude() {
        let app = with_outdoor(env_app(vec![("192.168.1.10", 21.5)], vec![]));
        let out = draw(&app, 130, 24);
        assert!(out.contains("28.2"), "{out}");
        assert!(out.contains("Bern / Zollikofen"), "{out}");
        // Distance and altitude are what stop the figure reading as "the
        // temperature outside the house".
        assert!(out.contains("5.1 km"), "{out}");
        assert!(out.contains("555 m"), "{out}");
        assert!(out.contains("14.6"), "today's range missing:\n{out}");
    }

    #[test]
    fn outdoor_panel_asks_for_an_address_when_no_location_is_set() {
        let app = env_app(vec![("192.168.1.10", 21.5)], vec![]);
        let out = draw(&app, 130, 24);
        assert!(out.contains("No location set"), "{out}");
        assert!(
            out.contains("'a'"),
            "the way to fix it should be stated:\n{out}"
        );
    }

    #[test]
    fn outdoor_panel_distinguishes_waiting_from_having_no_location() {
        let mut app = with_outdoor(env_app(vec![("192.168.1.10", 21.5)], vec![]));
        app.outdoor = None;
        app.outdoor_today = None;
        let out = draw(&app, 130, 24);
        assert!(out.contains("Waiting for the first reading"), "{out}");
        assert!(!out.contains("No location set"), "{out}");
    }

    #[test]
    fn outdoor_panel_surfaces_a_failed_fetch_rather_than_showing_a_stale_number_silently() {
        let mut app = with_outdoor(env_app(vec![("192.168.1.10", 21.5)], vec![]));
        app.last_weather_error = Some("connecting to data.geo.admin.ch:443".to_string());
        let out = draw(&app, 130, 24);
        assert!(out.contains("last fetch failed"), "{out}");
        assert!(out.contains("data.geo.admin.ch"), "{out}");
    }

    #[test]
    fn outdoor_panel_becomes_the_address_input_while_typing() {
        let mut app = with_outdoor(env_app(vec![("192.168.1.10", 21.5)], vec![]));
        app.address_input = Some("Bahnhofstrasse 1 Zür".to_string());
        let out = draw(&app, 130, 24);
        assert!(out.contains("Bahnhofstrasse 1 Zür"), "{out}");
        assert!(out.contains("Esc to cancel"), "{out}");
        // While typing, the reading is out of the way.
        assert!(!out.contains("28.2"), "{out}");
    }

    #[test]
    fn statusbar_offers_the_address_key_in_the_environment_view() {
        let app = env_app(vec![("192.168.1.10", 21.5)], vec![]);
        let out = draw(&app, 170, 10);
        assert!(out.contains("set address"), "{out}");
    }

    #[test]
    fn environment_view_lists_sensors_with_live_temperature() {
        let app = env_app(vec![("192.168.1.10", 21.5), ("192.168.1.11", 30.2)], vec![]);
        let out = draw(&app, 120, 20);
        assert!(out.contains("Environment"), "{out}");
        assert!(out.contains("21.5"), "{out}");
        assert!(out.contains("30.2"), "{out}");
        // The sensor readings are devices' own temperatures; presenting them as
        // ambient would misrepresent them.
        assert!(out.contains("not room"), "{out}");
    }

    #[test]
    fn environment_view_explains_an_empty_history_rather_than_drawing_nothing() {
        let app = env_app(vec![("192.168.1.10", 21.5)], vec![]);
        let out = draw(&app, 120, 20);
        assert!(out.contains("No daily history"), "{out}");
    }

    #[test]
    fn environment_view_says_so_when_nothing_reports_a_temperature() {
        let app = env_app(vec![], vec![]);
        let out = draw(&app, 120, 20);
        assert!(out.contains("No device is reporting"), "{out}");
    }

    #[test]
    fn environment_history_shows_only_the_selected_sensor() {
        let mut app = env_app(
            vec![("192.168.1.10", 21.5), ("192.168.1.11", 30.2)],
            vec![
                ("192.168.1.10", "2026-08-15", 18.0, 24.0, 21.0),
                ("192.168.1.11", "2026-08-15", 40.0, 48.0, 44.0),
            ],
        );
        let first = draw(&app, 120, 20);
        assert!(
            first.contains(" 18.0"),
            "selected sensor's range missing:\n{first}"
        );
        assert!(!first.contains(" 48.0"), "other sensor leaked in:\n{first}");

        app.env_selected = 1;
        let second = draw(&app, 120, 20);
        assert!(second.contains(" 48.0"), "{second}");
        assert!(!second.contains(" 24.0"), "{second}");
    }

    #[test]
    fn environment_view_shows_todays_range_next_to_the_live_reading() {
        let today = chrono::Local::now().date_naive();
        let app = env_app(
            vec![("192.168.1.10", 21.5)],
            vec![(
                "192.168.1.10",
                &today.format("%Y-%m-%d").to_string(),
                19.25,
                26.75,
                22.0,
            )],
        );
        let out = draw(&app, 120, 20);
        assert!(out.contains("19.2") || out.contains("19.3"), "{out}");
        assert!(out.contains("26.8") || out.contains("26.7"), "{out}");
    }

    #[test]
    fn statusbar_advertises_the_environment_key() {
        let app = env_app(vec![("192.168.1.10", 21.5)], vec![]);
        let out = draw(&app, 170, 8);
        assert!(out.contains("environment"), "{out}");
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
    fn a_stacked_bar_marks_where_each_part_ends() {
        // The ambiguity this replaces: two abutting runs of solid blocks read
        // equally as "stacked" and as "both drawn from zero, overlapping".
        assert_eq!(bar_segment(6), "┃━━━━┃");
        // The join between two segments is two end marks, which is what makes
        // the boundary unmistakable.
        let joined = format!("{}{}", bar_segment(6), bar_segment(4));
        assert!(joined.contains("┃┃"), "{joined}");
    }

    #[test]
    fn a_segment_is_exactly_as_wide_as_it_is_asked_for() {
        // The columns after the bar are aligned by padding to a fixed width, so
        // a segment that miscounts by one shifts every number on the row.
        for width in 0..40 {
            assert_eq!(bar_segment(width).chars().count(), width, "width {width}");
        }
    }

    #[test]
    fn a_segment_too_narrow_for_two_ends_still_draws_something() {
        assert_eq!(bar_segment(1), "┃");
        assert_eq!(bar_segment(2), "┃┃");
        // And nothing at all is nothing at all — a day that imported none, or
        // generated none, has no second segment rather than a stray mark.
        assert_eq!(bar_segment(0), "");
    }

    #[test]
    fn a_fully_self_sufficient_day_draws_one_segment_and_a_fully_imported_one_the_other() {
        // consumption 10, production 10, import 0 -> all own generation.
        let app = stats_app(vec![bucket("1", 10.0, 10.0, 0.0)], StatsWindow::Month);
        let out = draw(&app, 120, 24);
        let row = out.lines().find(|l| l.contains("  1 ")).unwrap_or_default();
        assert_eq!(row.matches("┃┃").count(), 0, "no boundary to draw: {row}");

        // consumption 10, production 0, import 10 -> all imported.
        let app = stats_app(vec![bucket("1", 10.0, 0.0, 10.0)], StatsWindow::Month);
        let out = draw(&app, 120, 24);
        let row = out.lines().find(|l| l.contains("  1 ")).unwrap_or_default();
        assert_eq!(row.matches("┃┃").count(), 0, "no boundary to draw: {row}");
    }

    #[test]
    fn a_partly_imported_day_shows_the_boundary_between_the_two() {
        let app = stats_app(vec![bucket("1", 10.0, 6.0, 4.0)], StatsWindow::Month);
        let out = draw(&app, 120, 24);
        let row = out.lines().find(|l| l.contains("  1 ")).unwrap_or_default();
        assert_eq!(row.matches("┃┃").count(), 1, "one join expected: {row}");
    }

    #[test]
    fn the_two_parts_of_the_bar_are_named() {
        // Colour alone does not say which segment is which.
        let app = stats_app(vec![bucket("1", 10.0, 6.0, 4.0)], StatsWindow::Month);
        let out = draw(&app, 120, 24);
        assert!(out.contains("own generation"), "{out}");
        assert!(out.contains("imported"), "{out}");
    }

    #[test]
    fn the_column_headings_line_up_with_the_figures_they_name() {
        // The headings and the rows are laid out from the same widths, and this
        // is what proves it: each heading has to end in the same column as the
        // figure below it, or it is labelling the wrong number.
        let app = stats_app(vec![bucket("1", 30.0, 40.0, 10.0)], StatsWindow::Month);
        let out = draw(&app, 120, 24);
        let lines: Vec<&str> = out.lines().collect();
        let headings = lines
            .iter()
            .find(|l| l.contains("consumed"))
            .expect("a heading row");
        // A bucket row, not the totals panel above it — both contain " kWh".
        let row = lines
            .iter()
            .find(|l| l.contains(" kWh") && l.contains('\u{2503}'))
            .expect("a bucket row");

        // Character positions, not byte offsets: the bar is drawn from box-drawing
        // characters that are three bytes each, so byte indices are not columns.
        let ends_at = |line: &str, needle: &str| {
            let at = line
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} not in {line}"));
            line[..at].chars().count() + needle.chars().count()
        };

        assert_eq!(
            ends_at(headings, "consumed"),
            ends_at(row, "kWh"),
            "\n{headings}\n{row}"
        );
        // The production column ends with its own unit, so this is the second
        // "kWh" on the row, not the number before it.
        let second_kwh = {
            let first = row.find("kWh").expect("a consumed figure") + 3;
            let rest = &row[first..];
            let at = rest.find("kWh").expect("a produced figure");
            row[..first + at].chars().count() + 3
        };
        assert_eq!(
            ends_at(headings, "produced"),
            second_kwh,
            "\n{headings}\n{row}"
        );
        assert_eq!(
            ends_at(headings, "self"),
            ends_at(row, "%"),
            "\n{headings}\n{row}"
        );
    }

    #[test]
    fn the_headings_do_not_run_into_one_another() {
        let app = stats_app(vec![bucket("1", 30.0, 40.0, 10.0)], StatsWindow::Month);
        let out = draw(&app, 120, 24);
        let headings = out.lines().find(|l| l.contains("consumed")).unwrap();
        // At least two spaces: one is not a gap, it is a near miss, and the
        // headings are the only thing naming these columns.
        for (left, right) in [("consumed", "produced"), ("produced", "self")] {
            let from = headings.find(left).unwrap() + left.len();
            let to = headings.find(right).unwrap();
            assert!(
                to >= from + 2,
                "only {} space(s) between {left} and {right}:\n{headings}",
                to - from
            );
        }
    }

    #[test]
    fn a_period_with_no_ratio_keeps_the_column_aligned() {
        // `pct` returns an em dash when there is nothing to divide by, and it has
        // to be as wide as a real percentage or the column shifts under it.
        assert_eq!(pct(None).chars().count(), pct(Some(100.0)).chars().count());
        assert_eq!(pct(None).chars().count(), pct(Some(7.5)).chars().count());
    }

    #[test]
    fn energy_bought_to_charge_the_battery_is_shown_only_when_it_happened() {
        // A permanent zero row would be one more thing to read past on an
        // installation that never charges from the grid.
        let summer = stats_app(vec![bucket("1", 30.0, 40.0, 5.0)], StatsWindow::Month);
        assert!(!draw(&summer, 120, 24).contains("charged the battery"));

        let winter = stats_app(
            vec![bucket_charging_from_grid("1", 2.0, 2.0, 10.0)],
            StatsWindow::Month,
        );
        let out = draw(&winter, 120, 24);
        assert!(out.contains("charged the battery"), "{out}");
        // The line that explains why import is six times what the house used.
        assert!(out.contains("12.0"), "import: {out}");
        assert!(out.contains("10.0"), "of which to the battery: {out}");
    }

    #[test]
    fn a_night_spent_charging_the_battery_does_not_look_like_a_full_bar() {
        // Importing 10 kWh into the battery while the house uses 2 is a night
        // that ran entirely on the grid — the bar is all imported, and the
        // battery charging is not consumption and so is not in the bar at all.
        let app = stats_app(
            vec![bucket_charging_from_grid("1", 2.0, 2.0, 10.0)],
            StatsWindow::Month,
        );
        let out = draw(&app, 120, 24);
        let row = out
            .lines()
            .find(|l| l.contains(" kWh") && l.contains('\u{2503}'))
            .unwrap();
        assert!(
            row.contains("2.0 kWh"),
            "consumption is the house alone: {row}"
        );
        assert!(row.contains("0.0 %"), "none of it was self-supplied: {row}");
        assert_eq!(
            row.matches("┃┃").count(),
            0,
            "one segment, all imported: {row}"
        );
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
            StatsWindow::Month,
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
    fn a_failing_rollup_is_stated_on_the_statistics_view() {
        // The failure that silently stops pruning must not be visible only in a
        // log file — see `crate::logging`.
        let mut app = stats_app(vec![bucket("1", 10.0, 8.0, 4.0)], StatsWindow::Month);
        assert!(!draw(&app, 120, 24).contains("rollup failing"));

        app.rollup_error = Some("database is locked".into());
        let out = draw(&app, 120, 24);
        assert!(out.contains("rollup failing"), "{out}");
        assert!(out.contains("database is locked"), "{out}");
        assert!(out.contains("totals are stale"), "{out}");
    }

    #[test]
    fn statusbar_advertises_the_statistics_keys() {
        let app = stats_app(vec![bucket("1", 1.0, 1.0, 0.0)], StatsWindow::Month);
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

    // ── Solar panel ───────────────────────────────────────────────────────────

    fn solar_app(solar: crate::app::SolarOutlook, located: bool) -> App {
        let mut app = App {
            view: View::Environment,
            solar,
            ..Default::default()
        };
        if located {
            app.location = Some(crate::db::Location {
                label: "8912 - Obfelden".into(),
                east: 2_674_204.0,
                north: 1_235_037.0,
                latitude: 47.262,
                longitude: 8.419,
            });
        }
        app
    }

    fn calibration(days: usize) -> crate::solar::Calibration {
        crate::solar::Calibration {
            k: 2.2963,
            tilt_deg: 35,
            azimuth_deg: 60,
            days,
            samples: 4096,
            rmse_w: 1196.8,
            daily_rmse_kwh: 4.28,
            fitted_at: chrono::Utc::now(),
        }
    }

    fn rendered(app: &App) -> String {
        let theme = app.theme();
        solar_lines(app, &theme)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn with_no_location_the_panel_says_how_to_get_one() {
        let out = rendered(&solar_app(Default::default(), false));
        assert!(out.contains("Set a location"), "{out}");
        // Nothing is claimed about production.
        assert!(!out.contains("kWh"), "{out}");
    }

    #[test]
    fn before_the_first_fit_the_panel_says_it_is_still_learning() {
        let out = rendered(&solar_app(Default::default(), true));
        assert!(out.contains("Learning the array"), "{out}");
        assert!(!out.contains("kWh"), "{out}");
        // The data licence requires the credit wherever the data is shown.
        assert!(out.contains(crate::online::forecast::ATTRIBUTION), "{out}");
    }

    #[test]
    fn a_working_forecast_reports_tomorrow_and_the_array_behind_it() {
        let app = solar_app(
            crate::app::SolarOutlook {
                calibration: Some(calibration(44)),
                tomorrow_kwh: Some(47.3),
                today_kwh: Some(52.1),
                today_actual_kwh: 31.4,
                tomorrow_cloud_pct: Some(34.0),
                tomorrow_curve: (0..96)
                    .map(|i| {
                        (
                            i as f64 / 4.0,
                            (i as f64 - 48.0).abs().mul_add(-0.2, 9.0).max(0.0),
                        )
                    })
                    .collect(),
                ..Default::default()
            },
            true,
        );
        let out = rendered(&app);
        assert!(out.contains("47.3 kWh"), "{out}");
        assert!(out.contains("52.1 kWh"), "{out}");
        assert!(out.contains("31.4 kWh so far"), "{out}");
        assert!(out.contains("34% of daylight"), "{out}");
        assert!(out.contains("9.2 kWp"), "{out}");
        assert!(out.contains("35° tilt"), "{out}");
        assert!(out.contains("60° west of south"), "{out}");
        // No error band until enough forecasts have been checked.
        assert!(!out.contains("±"), "{out}");
    }

    #[test]
    fn the_error_band_appears_only_once_it_has_been_measured() {
        let mut solar = crate::app::SolarOutlook {
            calibration: Some(calibration(44)),
            tomorrow_kwh: Some(47.3),
            ..Default::default()
        };
        // Five days exact and five out by 20%: a 10% mean error, half of them
        // inside the band.
        let mut pairs = vec![(50.0, 50.0); 5];
        pairs.extend(vec![(60.0, 50.0); 5]);
        solar.accuracy = crate::solar::Accuracy::from_pairs(&pairs);
        let out = rendered(&solar_app(solar, true));
        assert!(out.contains("± 10%"), "{out}");
        assert!(out.contains("5 of the last 10 days within 10%"), "{out}");
    }

    #[test]
    fn a_thin_calibration_is_flagged_rather_than_quietly_used() {
        let app = solar_app(
            crate::app::SolarOutlook {
                calibration: Some(calibration(4)),
                tomorrow_kwh: Some(47.3),
                ..Default::default()
            },
            true,
        );
        let out = rendered(&app);
        assert!(out.contains("Fitted on 4 days"), "{out}");
        assert!(out.contains("rough until"), "{out}");
    }

    #[test]
    fn a_failed_fetch_is_shown_not_swallowed() {
        let app = solar_app(
            crate::app::SolarOutlook {
                calibration: Some(calibration(44)),
                tomorrow_kwh: Some(47.3),
                last_error: Some("timed out".into()),
                ..Default::default()
            },
            true,
        );
        assert!(rendered(&app).contains("Forecast unavailable: timed out"));
    }

    #[test]
    fn a_south_facing_array_is_described_as_south_facing() {
        let mut c = calibration(44);
        for (az, expected) in [
            (0, "due south"),
            (-40, "40° east of south"),
            (25, "25° west of south"),
        ] {
            c.azimuth_deg = az;
            let app = solar_app(
                crate::app::SolarOutlook {
                    calibration: Some(c.clone()),
                    tomorrow_kwh: Some(1.0),
                    ..Default::default()
                },
                true,
            );
            assert!(rendered(&app).contains(expected), "az {az}");
        }
    }

    #[test]
    fn the_sparkline_shows_the_shape_of_the_day() {
        // A flat night either side of an arc: the drawn line covers only the lit
        // part, so it is all shape and no padding.
        let curve: Vec<(f64, f64)> = (0..96)
            .map(|i| {
                let h = i as f64 / 4.0;
                (
                    h,
                    if (6.0..20.0).contains(&h) {
                        9.0 - (h - 13.0).abs()
                    } else {
                        0.0
                    },
                )
            })
            .collect();
        let line = sparkline(&curve, 16);
        assert_eq!(line.chars().count(), 16);
        assert!(line.contains('█'), "the peak should reach the top: {line}");
        assert!(!line.contains(' '), "no gaps: {line}");
    }

    #[test]
    fn a_day_with_no_sun_draws_no_sparkline() {
        assert_eq!(sparkline(&[(0.0, 0.0), (1.0, 0.0)], 16), "");
        assert_eq!(sparkline(&[], 16), "");
        // A zero-width area must not panic or divide by zero.
        assert_eq!(sparkline(&[(0.0, 5.0)], 0), "");
    }

    #[test]
    fn the_environment_view_draws_with_a_forecast_present() {
        let app = solar_app(
            crate::app::SolarOutlook {
                calibration: Some(calibration(44)),
                tomorrow_kwh: Some(47.3),
                today_kwh: Some(52.1),
                today_actual_kwh: 31.4,
                recent: vec![(
                    chrono::NaiveDate::from_ymd_opt(2026, 8, 21).unwrap(),
                    50.5,
                    48.2,
                )],
                ..Default::default()
            },
            true,
        );
        let screen = draw(&app, 120, 30);
        assert!(screen.contains("Solar"), "{screen}");
        assert!(screen.contains("47.3"), "{screen}");
        assert!(screen.contains("Outdoor"), "{screen}");
    }

    // ── Transport security notice ─────────────────────────────────────────────

    fn net_app() -> App {
        App {
            view: View::Network,
            ..Default::default()
        }
    }

    fn security_text(app: &App) -> String {
        let theme = app.theme();
        security_lines(app, &theme)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn nothing_is_said_when_every_device_is_reached_over_a_pinned_connection() {
        // The panel is not drawn at all in the good case; a permanent "all fine"
        // banner would train the eye to ignore the place the warning appears.
        assert!(security_lines(&net_app(), &net_app().theme()).is_empty());
    }

    #[test]
    fn a_changed_certificate_says_what_is_not_happening_and_what_to_do() {
        let mut app = net_app();
        app.cert_alerts.insert(
            "172.16.0.1".parse().unwrap(),
            crate::app::CertAlert {
                expected: "a".repeat(64),
                observed: "b".repeat(64),
            },
        );
        let out = security_text(&app);
        assert!(out.contains("172.16.0.1"), "{out}");
        assert!(out.contains("different TLS certificate"), "{out}");
        // The consequence, not just the event.
        assert!(out.contains("Not polling it"), "{out}");
        assert!(out.contains("not sending its password"), "{out}");
        assert!(out.contains("press 'k'"), "{out}");
        // Both fingerprints, so they can be compared against the device.
        assert!(out.contains("aaaa:aaaa"), "{out}");
        assert!(out.contains("bbbb:bbbb"), "{out}");
    }

    #[test]
    fn a_cleartext_device_is_named_with_the_commands_that_fix_it() {
        let mut app = net_app();
        app.cleartext_devices.insert("172.16.0.10".parse().unwrap());
        app.cleartext_devices.insert("172.16.0.1".parse().unwrap());
        let out = security_text(&app);
        assert!(out.contains("in the clear"), "{out}");
        // Sorted, so the list does not reshuffle between frames.
        assert!(
            out.find("172.16.0.1,").unwrap() < out.find("172.16.0.10").unwrap(),
            "{out}"
        );
        assert!(out.contains("www-ssl"), "{out}");
    }

    #[test]
    fn a_changed_certificate_is_listed_before_a_merely_unencrypted_one() {
        // One means Dom has stopped talking to the device; the other means it is
        // talking to it badly. They must not be able to be confused.
        let mut app = net_app();
        app.cleartext_devices.insert("172.16.0.10".parse().unwrap());
        app.cert_alerts.insert(
            "172.16.0.20".parse().unwrap(),
            crate::app::CertAlert {
                expected: "a".repeat(64),
                observed: "b".repeat(64),
            },
        );
        let out = security_text(&app);
        assert!(out.find("different TLS certificate").unwrap() < out.find("in the clear").unwrap());
    }

    #[test]
    fn the_network_view_draws_the_notice() {
        let mut app = net_app();
        app.cleartext_devices.insert("172.16.0.1".parse().unwrap());
        let screen = draw(&app, 140, 30);
        assert!(screen.contains("Security"), "{screen}");
        assert!(screen.contains("in the clear"), "{screen}");
    }

    // ── Cluster pairing prompt ──────────────────────────────────────────────

    fn cluster_text(app: &App) -> String {
        let theme = app.theme();
        cluster_lines(app, &theme)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn nothing_is_said_when_no_pairing_is_pending() {
        assert!(cluster_lines(&net_app(), &net_app().theme()).is_empty());
    }

    #[test]
    fn a_fresh_candidate_invites_pairing_without_implying_one_already_existed() {
        let mut app = net_app();
        app.cluster_pairing_prompt = Some(crate::app::ClusterPairingPrompt {
            addr: "192.168.1.50:7878".to_string(),
            node_id: "deadbeef".to_string(),
            public_key: "a".repeat(64),
            replaces_pin: false,
        });
        let out = cluster_text(&app);
        assert!(out.contains("192.168.1.50:7878"), "{out}");
        assert!(out.contains("Found a Dom instance"), "{out}");
        assert!(out.contains("press 'p' to pair"), "{out}");
        assert!(!out.contains("re-pair"), "{out}");
        assert!(out.contains("aaaa:aaaa"), "{out}");
    }

    #[test]
    fn a_mismatched_pin_is_worded_as_re_pairing_and_warns_against_blind_acceptance() {
        let mut app = net_app();
        app.cluster_pairing_prompt = Some(crate::app::ClusterPairingPrompt {
            addr: "192.168.1.50:7878".to_string(),
            node_id: "deadbeef".to_string(),
            public_key: "c".repeat(64),
            replaces_pin: true,
        });
        let out = cluster_text(&app);
        assert!(
            out.contains("different identity than the one pinned"),
            "{out}"
        );
        assert!(out.contains("press 'p' to re-pair"), "{out}");
        assert!(out.contains("never trusted automatically"), "{out}");
    }

    #[test]
    fn the_network_view_draws_the_cluster_panel_only_when_a_prompt_is_pending() {
        let mut app = net_app();
        app.cluster_pairing_prompt = Some(crate::app::ClusterPairingPrompt {
            addr: "192.168.1.50:7878".to_string(),
            node_id: "deadbeef".to_string(),
            public_key: "a".repeat(64),
            replaces_pin: false,
        });
        let screen = draw(&app, 140, 30);
        assert!(screen.contains("Cluster"), "{screen}");
        assert!(screen.contains("192.168.1.50:7878"), "{screen}");

        let screen = draw(&net_app(), 140, 30);
        assert!(!screen.contains("Cluster"), "{screen}");
    }
}
