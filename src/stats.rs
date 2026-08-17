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

//! Long-term energy statistics: which span is being looked at, and how the daily
//! rollup rows are folded into the bars that span shows.
//!
//! Everything here is pure — dates in, buckets out — so the period arithmetic
//! (which is where the off-by-one-day mistakes live) is testable without a
//! database or a terminal. `db::query_daily_energy` supplies the input and
//! `tui::render` draws the output.

use chrono::{Datelike, Duration, Months, NaiveDate};

use crate::db::DailyEnergy;

/// Calendar spans the statistics view can aggregate over.
///
/// Calendar-aligned rather than rolling: the week is Monday to Sunday and the
/// month is the 1st to the last, so browsing back with Left lands on periods a
/// person recognizes ("July") instead of arbitrary 30-day slices.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum StatsWindow {
    #[default]
    Week,
    Month,
    Year,
}

impl StatsWindow {
    pub fn label(self) -> &'static str {
        match self {
            StatsWindow::Week => "Weekly",
            StatsWindow::Month => "Monthly",
            StatsWindow::Year => "Yearly",
        }
    }
}

/// Energy totals for one bar, or for a whole period.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Totals {
    pub consumption_kwh: f64,
    pub production_kwh: f64,
    pub grid_import_kwh: f64,
    pub grid_export_kwh: f64,
}

impl Totals {
    fn add(&mut self, d: &DailyEnergy) {
        self.consumption_kwh += d.consumption_kwh;
        self.production_kwh += d.production_kwh;
        self.grid_import_kwh += d.grid_import_kwh;
        self.grid_export_kwh += d.grid_export_kwh;
    }

    /// Share of what was consumed that came from own production or the battery
    /// rather than from the grid, as a percentage.
    ///
    /// `None` when nothing was consumed in the period — a period with no data,
    /// or one before the battery was installed, has no meaningful ratio, and
    /// showing 0% would read as "bought everything from the grid" when the truth
    /// is "don't know".
    pub fn self_sufficiency_pct(&self) -> Option<f64> {
        (self.consumption_kwh > 0.0).then(|| {
            ((self.consumption_kwh - self.grid_import_kwh) / self.consumption_kwh * 100.0)
                .clamp(0.0, 100.0)
        })
    }

    /// Share of own production that was used on site rather than exported.
    /// `None` when nothing was produced — see `self_sufficiency_pct`.
    pub fn self_consumption_pct(&self) -> Option<f64> {
        (self.production_kwh > 0.0).then(|| {
            ((self.production_kwh - self.grid_export_kwh) / self.production_kwh * 100.0)
                .clamp(0.0, 100.0)
        })
    }

    /// Energy that was consumed without being bought, in kWh.
    pub fn self_consumed_kwh(&self) -> f64 {
        (self.consumption_kwh - self.grid_import_kwh).max(0.0)
    }
}

/// One bar: a day for the weekly and monthly windows, a month for the yearly one.
#[derive(Clone, Debug, PartialEq)]
pub struct Bucket {
    /// Short axis label — a day number, or a three-letter month.
    pub label: String,
    pub totals: Totals,
    /// False when no rollup row covered this bucket at all. Distinguished from a
    /// genuine zero so the view can leave a gap rather than draw a zero bar.
    pub has_data: bool,
}

/// Everything the statistics view needs to draw itself.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub window: StatsWindow,
    /// Whole periods back from the current one. 0 is the period containing today;
    /// positive values move into the past.
    pub offset: u32,
    /// Human-readable name of the period being shown, e.g. "July 2026".
    pub title: String,
    pub buckets: Vec<Bucket>,
    pub totals: Totals,
    /// Oldest local day with rolled-up data, once known. Used to stop Left from
    /// walking back through unbounded empty periods.
    pub oldest_day: Option<NaiveDate>,
}

impl Stats {
    /// Switches window. Pressing the same window's key again returns to the
    /// current period, which is the cheapest way back after browsing.
    pub fn set_window(&mut self, window: StatsWindow) {
        if self.window == window {
            self.offset = 0;
        } else {
            self.window = window;
            self.offset = 0;
        }
    }

    /// Steps one period into the past, unless that would leave the range of days
    /// that have any data at all.
    pub fn back(&mut self, today: NaiveDate) {
        let next = self.offset + 1;
        if let Some(oldest) = self.oldest_day {
            let (_, end) = range(self.window, next, today);
            if end < oldest {
                return;
            }
        }
        self.offset = next;
    }

    /// Steps one period towards the present. Stops at the current period.
    pub fn forward(&mut self) {
        self.offset = self.offset.saturating_sub(1);
    }

    /// The inclusive local-date range currently being shown.
    pub fn range(&self, today: NaiveDate) -> (NaiveDate, NaiveDate) {
        range(self.window, self.offset, today)
    }
}

/// The inclusive local-date range for `window`, `offset` whole periods before the
/// one containing `today`.
///
/// The end is clamped to nothing beyond `today`: for the current period this
/// means the range stops at today rather than running to a future Sunday or the
/// 31st, so partial periods report what has actually happened.
pub fn range(window: StatsWindow, offset: u32, today: NaiveDate) -> (NaiveDate, NaiveDate) {
    let (start, end) = match window {
        StatsWindow::Week => {
            let monday = today - Duration::days(today.weekday().num_days_from_monday() as i64);
            let start = monday - Duration::weeks(offset as i64);
            (start, start + Duration::days(6))
        }
        StatsWindow::Month => {
            let first = today.with_day(1).unwrap_or(today);
            let start = first
                .checked_sub_months(Months::new(offset))
                .unwrap_or(first);
            let end = start
                .checked_add_months(Months::new(1))
                .and_then(|d| d.pred_opt())
                .unwrap_or(start);
            (start, end)
        }
        StatsWindow::Year => {
            let year = today.year() - offset as i32;
            let start = NaiveDate::from_ymd_opt(year, 1, 1).unwrap_or(today);
            let end = NaiveDate::from_ymd_opt(year, 12, 31).unwrap_or(today);
            (start, end)
        }
    };
    (start, end.min(today))
}

/// Name of the period a range belongs to, for the view's header.
pub fn title(window: StatsWindow, start: NaiveDate) -> String {
    match window {
        StatsWindow::Week => format!(
            "Week {} — {}",
            start.iso_week().week(),
            start.format("%-d %b %Y")
        ),
        StatsWindow::Month => start.format("%B %Y").to_string(),
        StatsWindow::Year => start.format("%Y").to_string(),
    }
}

/// Folds daily rollup rows into the bars for `window` over `[start, end]`.
///
/// Every bucket in the period is present in the result, including ones with no
/// data — the view needs the full axis, and `has_data` says which bars are real.
/// Days outside the range are ignored rather than trusted, so a caller passing a
/// wider query result cannot smear energy into the wrong period.
pub fn bucketize(
    window: StatsWindow,
    start: NaiveDate,
    end: NaiveDate,
    daily: &[DailyEnergy],
) -> Vec<Bucket> {
    let mut buckets: Vec<Bucket> = Vec::new();
    let mut index: Vec<(NaiveDate, NaiveDate)> = Vec::new();

    match window {
        StatsWindow::Week | StatsWindow::Month => {
            let mut d = start;
            while d <= end {
                buckets.push(Bucket {
                    label: d.format("%-d").to_string(),
                    totals: Totals::default(),
                    has_data: false,
                });
                index.push((d, d));
                let Some(next) = d.succ_opt() else { break };
                d = next;
            }
        }
        StatsWindow::Year => {
            let mut month_start = start.with_day(1).unwrap_or(start);
            while month_start <= end {
                let month_end = month_start
                    .checked_add_months(Months::new(1))
                    .and_then(|d| d.pred_opt())
                    .unwrap_or(month_start);
                buckets.push(Bucket {
                    label: month_start.format("%b").to_string(),
                    totals: Totals::default(),
                    has_data: false,
                });
                index.push((month_start, month_end));
                let Some(next) = month_start.checked_add_months(Months::new(1)) else {
                    break;
                };
                month_start = next;
            }
        }
    }

    for d in daily {
        if d.day < start || d.day > end {
            continue;
        }
        if let Some(i) = index.iter().position(|(s, e)| d.day >= *s && d.day <= *e) {
            buckets[i].totals.add(d);
            buckets[i].has_data = true;
        }
    }
    buckets
}

/// Reloads the statistics view's data for the window and period currently
/// selected in `state`.
///
/// The one function here that touches the database; everything above it is pure
/// so that the period arithmetic can be tested on its own. Called when the user
/// changes window or period, and periodically so the current period keeps up with
/// the day as it accrues.
///
/// Failures leave the previous data in place rather than blanking the view: a
/// transient query error should not look like "you used no energy".
pub async fn refresh(pool: &sqlx::SqlitePool, state: &crate::app::SharedState) {
    let (window, offset) = {
        let app = state.read().unwrap();
        (app.stats.window, app.stats.offset)
    };
    let today = chrono::Local::now().date_naive();
    let (start, end) = range(window, offset, today);

    let daily = match crate::db::query_daily_energy(pool, start, end).await {
        Ok(rows) => rows,
        Err(_) => return,
    };
    let oldest = crate::db::oldest_energy_day(pool).await.ok().flatten();
    let buckets = bucketize(window, start, end, &daily);
    let totals = total(&buckets);
    let period_title = title(window, start);

    let mut app = state.write().unwrap();
    // Knowing how far back history goes is independent of which period was
    // requested, so record it either way.
    app.stats.oldest_day = oldest;
    // The user may have moved on while the query ran. Don't replace what they
    // are looking at now with results for the period they left.
    if app.stats.window != window || app.stats.offset != offset {
        return;
    }
    app.stats.title = period_title;
    app.stats.buckets = buckets;
    app.stats.totals = totals;
}

/// Sums every bucket into one period total.
pub fn total(buckets: &[Bucket]) -> Totals {
    let mut t = Totals::default();
    for b in buckets {
        t.consumption_kwh += b.totals.consumption_kwh;
        t.production_kwh += b.totals.production_kwh;
        t.grid_import_kwh += b.totals.grid_import_kwh;
        t.grid_export_kwh += b.totals.grid_export_kwh;
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn de(d: &str, consumption: f64, production: f64, import: f64, export: f64) -> DailyEnergy {
        DailyEnergy {
            day: day(d),
            consumption_kwh: consumption,
            production_kwh: production,
            grid_import_kwh: import,
            grid_export_kwh: export,
        }
    }

    // ── range ────────────────────────────────────────────────────────────────

    #[test]
    fn week_range_starts_on_monday_and_stops_at_today() {
        // 2026-08-17 is a Monday, 2026-08-19 a Wednesday.
        let (s, e) = range(StatsWindow::Week, 0, day("2026-08-19"));
        assert_eq!(s, day("2026-08-17"), "week must start on Monday");
        // Current week is partial: it must not run on to a future Sunday.
        assert_eq!(e, day("2026-08-19"));
    }

    #[test]
    fn past_week_range_is_a_full_monday_to_sunday() {
        let (s, e) = range(StatsWindow::Week, 1, day("2026-08-19"));
        assert_eq!(s, day("2026-08-10"));
        assert_eq!(e, day("2026-08-16"));
        assert_eq!(e.weekday(), chrono::Weekday::Sun);
    }

    #[test]
    fn week_range_of_a_sunday_uses_the_week_that_sunday_ends() {
        // A Sunday is day 7 of its week, not the start of a new one.
        let (s, e) = range(StatsWindow::Week, 0, day("2026-08-16"));
        assert_eq!(s, day("2026-08-10"));
        assert_eq!(e, day("2026-08-16"));
    }

    #[test]
    fn month_range_covers_the_calendar_month() {
        let (s, e) = range(StatsWindow::Month, 0, day("2026-08-17"));
        assert_eq!(s, day("2026-08-01"));
        assert_eq!(e, day("2026-08-17"), "current month stops at today");

        let (s, e) = range(StatsWindow::Month, 1, day("2026-08-17"));
        assert_eq!(s, day("2026-07-01"));
        assert_eq!(e, day("2026-07-31"));
    }

    #[test]
    fn month_range_handles_short_months_and_year_boundaries() {
        // Stepping back from a 31-day month into February must not overflow.
        let (s, e) = range(StatsWindow::Month, 1, day("2026-03-31"));
        assert_eq!(s, day("2026-02-01"));
        assert_eq!(e, day("2026-02-28"));

        // Back across New Year.
        let (s, e) = range(StatsWindow::Month, 2, day("2026-01-15"));
        assert_eq!(s, day("2025-11-01"));
        assert_eq!(e, day("2025-11-30"));
    }

    #[test]
    fn month_range_covers_a_leap_february() {
        let (s, e) = range(StatsWindow::Month, 0, day("2024-02-29"));
        assert_eq!(s, day("2024-02-01"));
        assert_eq!(e, day("2024-02-29"));
    }

    #[test]
    fn year_range_covers_the_calendar_year() {
        let (s, e) = range(StatsWindow::Year, 0, day("2026-08-17"));
        assert_eq!(s, day("2026-01-01"));
        assert_eq!(e, day("2026-08-17"), "current year stops at today");

        let (s, e) = range(StatsWindow::Year, 1, day("2026-08-17"));
        assert_eq!(s, day("2025-01-01"));
        assert_eq!(e, day("2025-12-31"));
    }

    // ── browsing ─────────────────────────────────────────────────────────────

    #[test]
    fn forward_stops_at_the_current_period() {
        let mut s = Stats::default();
        s.forward();
        assert_eq!(s.offset, 0, "must not walk into the future");
        s.offset = 2;
        s.forward();
        s.forward();
        s.forward();
        assert_eq!(s.offset, 0);
    }

    #[test]
    fn back_stops_once_the_period_predates_all_data() {
        let today = day("2026-08-17");
        let mut s = Stats {
            window: StatsWindow::Month,
            oldest_day: Some(day("2026-07-01")),
            ..Default::default()
        };
        s.back(today); // August -> July, still has data
        assert_eq!(s.offset, 1);
        s.back(today); // June would end 2026-06-30, before the oldest day
        assert_eq!(s.offset, 1, "must not browse past the start of history");
    }

    #[test]
    fn back_is_unbounded_until_the_oldest_day_is_known() {
        let mut s = Stats::default();
        s.back(day("2026-08-17"));
        assert_eq!(s.offset, 1);
    }

    #[test]
    fn set_window_resets_to_the_current_period() {
        let mut s = Stats {
            window: StatsWindow::Month,
            offset: 5,
            ..Default::default()
        };
        s.set_window(StatsWindow::Year);
        assert_eq!(s.window, StatsWindow::Year);
        assert_eq!(s.offset, 0);

        // Pressing the same window's key again is the way back to now.
        s.offset = 3;
        s.set_window(StatsWindow::Year);
        assert_eq!(s.offset, 0);
    }

    // ── bucketize ────────────────────────────────────────────────────────────

    #[test]
    fn weekly_buckets_are_one_per_day_with_gaps_marked() {
        let start = day("2026-08-10");
        let end = day("2026-08-16");
        let daily = vec![
            de("2026-08-10", 1.0, 2.0, 0.5, 1.0),
            de("2026-08-12", 3.0, 0.0, 3.0, 0.0),
        ];
        let b = bucketize(StatsWindow::Week, start, end, &daily);

        assert_eq!(b.len(), 7, "a full week is seven bars");
        assert!(b[0].has_data);
        assert_eq!(b[0].totals.consumption_kwh, 1.0);
        assert!(
            !b[1].has_data,
            "a day with no rollup row is a gap, not a zero"
        );
        assert_eq!(b[1].totals.consumption_kwh, 0.0);
        assert!(b[2].has_data);
        assert_eq!(b[2].totals.grid_import_kwh, 3.0);
        assert_eq!(b[0].label, "10");
    }

    #[test]
    fn yearly_buckets_are_one_per_month_and_sum_their_days() {
        let daily = vec![
            de("2026-01-05", 1.0, 0.0, 0.0, 0.0),
            de("2026-01-20", 2.0, 0.0, 0.0, 0.0),
            de("2026-03-02", 4.0, 0.0, 0.0, 0.0),
        ];
        let b = bucketize(
            StatsWindow::Year,
            day("2026-01-01"),
            day("2026-03-31"),
            &daily,
        );

        assert_eq!(b.len(), 3, "Jan, Feb, Mar");
        assert_eq!(b[0].label, "Jan");
        assert_eq!(b[0].totals.consumption_kwh, 3.0, "both January days");
        assert!(!b[1].has_data, "February had nothing");
        assert_eq!(b[2].totals.consumption_kwh, 4.0);
    }

    #[test]
    fn bucketize_ignores_days_outside_the_range() {
        let daily = vec![
            de("2026-08-09", 99.0, 0.0, 0.0, 0.0), // before
            de("2026-08-11", 1.0, 0.0, 0.0, 0.0),  // inside
            de("2026-08-20", 99.0, 0.0, 0.0, 0.0), // after
        ];
        let b = bucketize(
            StatsWindow::Week,
            day("2026-08-10"),
            day("2026-08-16"),
            &daily,
        );
        assert_eq!(
            total(&b).consumption_kwh,
            1.0,
            "out-of-range days must not leak in"
        );
    }

    #[test]
    fn a_week_straddling_the_start_of_history_marks_the_days_before_it() {
        // Browsing back to the week recording began in: only the days from the
        // first recorded day onwards have data. The earlier ones must be gaps,
        // not zero bars that would drag the average down.
        let start = day("2026-06-29"); // Monday
        let end = day("2026-07-05");
        let daily = vec![
            de("2026-07-01", 20.0, 30.0, 1.0, 10.0),
            de("2026-07-02", 22.0, 28.0, 2.0, 8.0),
        ];
        let b = bucketize(StatsWindow::Week, start, end, &daily);

        assert_eq!(b.len(), 7);
        assert!(!b[0].has_data, "29 Jun predates the data");
        assert!(!b[1].has_data, "30 Jun predates the data");
        assert!(b[2].has_data, "1 Jul is the first recorded day");
        assert!(b[3].has_data);
        assert!(!b[4].has_data);
        // Totals count only the days that actually have data.
        let t = total(&b);
        assert_eq!(t.consumption_kwh, 42.0);
        assert_eq!(t.self_sufficiency_pct(), Some((42.0 - 3.0) / 42.0 * 100.0));
    }

    #[test]
    fn totals_sum_every_bucket() {
        let daily = vec![
            de("2026-08-10", 1.0, 2.0, 3.0, 4.0),
            de("2026-08-11", 10.0, 20.0, 30.0, 40.0),
        ];
        let t = total(&bucketize(
            StatsWindow::Week,
            day("2026-08-10"),
            day("2026-08-16"),
            &daily,
        ));
        assert_eq!(t.consumption_kwh, 11.0);
        assert_eq!(t.production_kwh, 22.0);
        assert_eq!(t.grid_import_kwh, 33.0);
        assert_eq!(t.grid_export_kwh, 44.0);
    }

    // ── ratios ───────────────────────────────────────────────────────────────

    #[test]
    fn self_sufficiency_is_the_share_not_bought_from_the_grid() {
        let t = Totals {
            consumption_kwh: 100.0,
            production_kwh: 80.0,
            grid_import_kwh: 40.0,
            grid_export_kwh: 20.0,
        };
        // 60 of 100 kWh consumed did not come from the grid.
        assert_eq!(t.self_sufficiency_pct(), Some(60.0));
        // 60 of 80 kWh produced was used rather than exported.
        assert_eq!(t.self_consumption_pct(), Some(75.0));
        assert_eq!(t.self_consumed_kwh(), 60.0);
    }

    #[test]
    fn ratios_are_none_rather_than_zero_when_there_is_nothing_to_divide_by() {
        // A period with no data must read as "unknown", not as 0% — which would
        // claim everything was bought from the grid.
        let empty = Totals::default();
        assert_eq!(empty.self_sufficiency_pct(), None);
        assert_eq!(empty.self_consumption_pct(), None);

        // Consumption but no solar at all: self-sufficiency is a real 0%,
        // while self-consumption has no denominator.
        let no_solar = Totals {
            consumption_kwh: 10.0,
            grid_import_kwh: 10.0,
            ..Default::default()
        };
        assert_eq!(no_solar.self_sufficiency_pct(), Some(0.0));
        assert_eq!(no_solar.self_consumption_pct(), None);
    }

    #[test]
    fn ratios_stay_within_bounds_when_the_data_is_inconsistent() {
        // Metrics come from independent 2s series, so rounding can put import
        // slightly above consumption over a period. Clamp rather than render a
        // negative or above-100 percentage.
        let noisy = Totals {
            consumption_kwh: 10.0,
            production_kwh: 10.0,
            grid_import_kwh: 10.5,
            grid_export_kwh: 10.5,
        };
        assert_eq!(noisy.self_sufficiency_pct(), Some(0.0));
        assert_eq!(noisy.self_consumption_pct(), Some(0.0));
        assert_eq!(noisy.self_consumed_kwh(), 0.0);
    }
}
