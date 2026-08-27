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

//! Where the energy came from, when the battery is in the way.
//!
//! The battery reports four powers, and they satisfy one identity — verified
//! against 42,092 recorded samples to a mean residual of 29 W, which is the four
//! being sampled a fraction apart rather than any disagreement:
//!
//! ```text
//! production + battery = consumption + grid
//! ```
//!
//! with the battery positive when discharging and the grid positive when
//! exporting. `consumption` is house load alone: charging the battery is not
//! consumption, it is a separate sink.
//!
//! # Why this module exists
//!
//! Self-sufficiency is the share of consumption that did not come from the grid,
//! and with a battery that only ever charges from the sun, `consumption −
//! grid_import` is exactly right. It stops being right the moment the battery
//! charges from the grid — which it will, overnight, in winter.
//!
//! Consider a night that imports 10 kWh into the battery while the house uses 2,
//! and a following day that runs entirely off the battery:
//!
//! | | consumption | import | `consumption − import` |
//! |---|---|---|---|
//! | night | 2 | 12 | −10, clamped to 0% |
//! | day | 10 | 0 | 100% |
//! | both | 12 | 12 | 0% |
//!
//! The two-day total is right. Each individual day is badly wrong, and the daily
//! bars are what the statistics view actually draws. The night reads as a total
//! failure and the day as perfect, when what happened is that every kilowatt-hour
//! came from the grid.
//!
//! # What this computes
//!
//! Two quantities per interval:
//!
//! - `grid_to_house_wh` — grid energy that served the house, whether it went
//!   there directly or by way of the battery. This is what self-sufficiency is
//!   measured against.
//! - `grid_to_battery_wh` — grid energy that went into the battery. Not part of
//!   consumption, and worth showing on its own: it is what explains a winter day
//!   with a large import bill and unchanged self-sufficiency.
//!
//! Getting the first requires knowing whether the energy leaving the battery
//! originally came from the sun or from the grid. A battery is a tank and its
//! contents do not carry labels, so [`GridOrigin`] tracks how much of what is
//! stored arrived from the grid, and discharge is attributed in proportion.

/// State of charge, in percent, at or below which the battery is treated as
/// holding nothing.
///
/// The Sonnen reserves the bottom of its range and this installation's series
/// bottoms out at exactly 5.0. Reaching it is the one hard observation available
/// about provenance — an empty battery holds no grid energy, whatever the
/// running total says — so it is used to resynchronise. See [`GridOrigin`].
const EMPTY_RSOC: f64 = 6.0;

/// One instant's power flows, in the battery's own sign convention.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Flows {
    pub production_w: f64,
    /// House load. Does not include charging the battery.
    pub consumption_w: f64,
    /// Positive discharging, negative charging.
    pub battery_w: f64,
    /// Positive exporting, negative importing.
    pub grid_w: f64,
}

/// Where one instant's grid power is going, and how fast the battery is emptying.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Split {
    /// Imported power serving the house directly.
    pub grid_to_load_w: f64,
    /// Imported power charging the battery.
    pub grid_to_battery_w: f64,
    /// Power leaving the battery, whatever its origin.
    pub discharge_w: f64,
}

/// Attributes one instant's flows.
///
/// The rule is that the sun is used before the grid, at every point: solar serves
/// the load first, whatever is left over charges the battery, and only what
/// neither covers is imported. That is not merely a convention — it is how the
/// inverter actually behaves, and it makes the split a function of the four
/// measured powers rather than of anything remembered.
///
/// Deliberately takes a single instant. An interval's flows cannot be split from
/// their average: the grid changes direction within 9.6% of consecutive samples
/// here, and an interval that both imported and exported averages to something
/// that did neither. Callers split each end and integrate the results.
pub fn split(f: Flows) -> Split {
    let import_w = (-f.grid_w).max(0.0);
    let charge_w = (-f.battery_w).max(0.0);

    // Solar serves the house first, and only the surplus charges the battery.
    let solar_to_load_w = f.production_w.max(0.0).min(f.consumption_w.max(0.0));
    let solar_surplus_w = (f.production_w.max(0.0) - solar_to_load_w).max(0.0);

    let charge_from_solar_w = charge_w.min(solar_surplus_w);
    let grid_to_battery_w = (charge_w - charge_from_solar_w).min(import_w);

    Split {
        grid_to_load_w: (import_w - grid_to_battery_w).max(0.0),
        grid_to_battery_w,
        discharge_w: f.battery_w.max(0.0),
    }
}

/// How much of what the battery is holding arrived from the grid.
///
/// Path-dependent, so it is carried forward rather than derived, and every
/// source of error accumulates into it: round-trip losses, the residual between
/// the four measurements, intervals skipped because Dom was not running. Two
/// things stop that becoming permanent drift, and both are observations rather
/// than estimates:
///
/// - the total can never exceed what the battery is holding, so it is clamped to
///   the reported remaining capacity on every update;
/// - an empty battery holds no grid energy at all, so reaching [`EMPTY_RSOC`]
///   resets it to zero outright.
///
/// Without the second, one bad week would colour every day after it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GridOrigin {
    /// Watt-hours of the stored energy that came from the grid.
    grid_wh: f64,
}

/// What one accounted interval contributed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Accounted {
    /// Grid energy that served the house, directly or via the battery.
    pub grid_to_house_wh: f64,
    /// Grid energy that went into the battery.
    pub grid_to_battery_wh: f64,
}

impl GridOrigin {
    pub fn new(grid_wh: f64) -> Self {
        Self {
            grid_wh: grid_wh.max(0.0),
        }
    }

    /// Watt-hours of grid-origin energy currently stored.
    pub fn stored_grid_wh(self) -> f64 {
        self.grid_wh
    }

    /// The share of the battery's contents that came from the grid.
    fn share(self, stored_wh: f64) -> f64 {
        if stored_wh <= 0.0 {
            return 0.0;
        }
        (self.grid_wh / stored_wh).clamp(0.0, 1.0)
    }

    /// Rescales after the battery moved without Dom watching.
    ///
    /// A poll-loop gap — a restart, a network outage, an interval too long to
    /// integrate — leaves the battery at a state of charge that arrived from an
    /// unknown mixture. Holding the *share* constant across the gap is the
    /// assumption that makes no claim: it says the unobserved energy looked like
    /// what was already there. Treating it as all solar would flatter
    /// self-sufficiency and treating it as all grid would punish it, and neither
    /// is known.
    ///
    /// Both ends are required. Taking only the new total would compute the share
    /// from the number it is about to scale by, which is arithmetically a no-op —
    /// the gap would silently do nothing at all.
    pub fn resync(&mut self, stored_before_wh: f64, stored_after_wh: f64) {
        let share = self.share(stored_before_wh);
        self.grid_wh = (share * stored_after_wh).clamp(0.0, stored_after_wh.max(0.0));
    }

    /// Accounts for one interval, and advances the stored provenance.
    ///
    /// `start` and `end` are the readings bracketing the interval; each is split
    /// on its own and the results integrated, so an interval that changed
    /// direction is still attributed correctly.
    ///
    /// Both stored totals are taken because the two are used for different
    /// things and confusing them is silent: the mixture is read from what the
    /// battery held *before* it was drawn on, while the result is capped by what
    /// it holds *after*.
    pub fn account(
        &mut self,
        start: Flows,
        end: Flows,
        dt_secs: f64,
        stored_before_wh: f64,
        stored_after_wh: f64,
        rsoc: f64,
    ) -> Accounted {
        if dt_secs <= 0.0 {
            return Accounted {
                grid_to_house_wh: 0.0,
                grid_to_battery_wh: 0.0,
            };
        }
        let hours = dt_secs / 3600.0;
        let (a, b) = (split(start), split(end));
        let mean = |x: f64, y: f64| (x + y) / 2.0 * hours;

        let grid_to_load_wh = mean(a.grid_to_load_w, b.grid_to_load_w);
        let grid_to_battery_wh = mean(a.grid_to_battery_w, b.grid_to_battery_w);
        let discharge_wh = mean(a.discharge_w, b.discharge_w);

        // Attributed against the mix as it stood before this interval drew on it.
        let from_grid_wh = discharge_wh * self.share(stored_before_wh);

        self.grid_wh =
            (self.grid_wh + grid_to_battery_wh - from_grid_wh).clamp(0.0, stored_after_wh.max(0.0));
        if rsoc <= EMPTY_RSOC {
            // An empty battery holds nothing, whatever the running total says.
            self.grid_wh = 0.0;
        }

        Accounted {
            grid_to_house_wh: grid_to_load_wh + from_grid_wh,
            grid_to_battery_wh,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flows(production: f64, consumption: f64, battery: f64, grid: f64) -> Flows {
        Flows {
            production_w: production,
            consumption_w: consumption,
            battery_w: battery,
            grid_w: grid,
        }
    }

    /// Asserts the identity the battery's own numbers satisfy, so a test that
    /// describes an impossible situation is caught rather than believed.
    fn assert_balanced(f: Flows) {
        let residual = f.production_w + f.battery_w - f.consumption_w - f.grid_w;
        assert!(
            residual.abs() < 1e-6,
            "unbalanced flows: {f:?} ({residual})"
        );
    }

    /// A steady interval, with the battery moving from `before` to `after`.
    fn run(
        state: &mut GridOrigin,
        f: Flows,
        seconds: f64,
        before: f64,
        after: f64,
        rsoc: f64,
    ) -> Accounted {
        assert_balanced(f);
        state.account(f, f, seconds, before, after, rsoc)
    }

    // ── Splitting one instant ─────────────────────────────────────────────────

    #[test]
    fn sunshine_covering_the_house_imports_nothing() {
        let f = flows(3000.0, 1000.0, -2000.0, 0.0);
        assert_balanced(f);
        let s = split(f);
        assert_eq!(s.grid_to_load_w, 0.0);
        assert_eq!(
            s.grid_to_battery_w, 0.0,
            "the surplus charged it, not the grid"
        );
        assert_eq!(s.discharge_w, 0.0);
    }

    #[test]
    fn charging_at_night_is_charged_to_the_grid() {
        // The winter case: no sun, house drawing 500 W, battery taking 3 kW.
        let f = flows(0.0, 500.0, -3000.0, -3500.0);
        assert_balanced(f);
        let s = split(f);
        assert_eq!(s.grid_to_battery_w, 3000.0);
        assert_eq!(s.grid_to_load_w, 500.0);
    }

    #[test]
    fn partial_sun_charges_the_battery_from_both() {
        // 2 kW of sun, 500 W of house: 1.5 kW spare, but the battery wants 3 kW.
        let f = flows(2000.0, 500.0, -3000.0, -1500.0);
        assert_balanced(f);
        let s = split(f);
        assert_eq!(
            s.grid_to_battery_w, 1500.0,
            "only what the sun could not cover"
        );
        assert_eq!(s.grid_to_load_w, 0.0, "the sun covered the house first");
    }

    #[test]
    fn the_sun_serves_the_house_before_it_charges_the_battery() {
        // The ordering that makes the split a function of the measurements alone.
        let f = flows(1000.0, 800.0, -200.0, 0.0);
        assert_balanced(f);
        let s = split(f);
        assert_eq!(s.grid_to_load_w, 0.0);
        assert_eq!(s.grid_to_battery_w, 0.0);
    }

    #[test]
    fn discharging_to_cover_the_house_imports_nothing() {
        let f = flows(0.0, 1000.0, 1000.0, 0.0);
        assert_balanced(f);
        let s = split(f);
        assert_eq!(s.discharge_w, 1000.0);
        assert_eq!(s.grid_to_load_w, 0.0);
    }

    #[test]
    fn a_shortfall_the_battery_cannot_cover_is_imported() {
        let f = flows(0.0, 3000.0, 1000.0, -2000.0);
        assert_balanced(f);
        let s = split(f);
        assert_eq!(s.grid_to_load_w, 2000.0);
        assert_eq!(s.grid_to_battery_w, 0.0, "it is discharging, not charging");
    }

    #[test]
    fn exporting_is_not_importing() {
        let f = flows(5000.0, 1000.0, 0.0, 4000.0);
        assert_balanced(f);
        let s = split(f);
        assert_eq!(s.grid_to_load_w, 0.0);
        assert_eq!(s.grid_to_battery_w, 0.0);
    }

    // ── Provenance across an interval ─────────────────────────────────────────

    #[test]
    fn grid_energy_stored_overnight_is_still_grid_energy_in_the_morning() {
        // The whole reason this module exists.
        let mut state = GridOrigin::default();

        let night = run(
            &mut state,
            flows(0.0, 0.0, -1000.0, -1000.0),
            3600.0,
            0.0,
            1000.0,
            50.0,
        );
        assert!(
            (night.grid_to_battery_wh - 1000.0).abs() < 1e-6,
            "{night:?}"
        );
        assert_eq!(night.grid_to_house_wh, 0.0, "nothing reached the house yet");
        assert!((state.stored_grid_wh() - 1000.0).abs() < 1e-6);

        let morning = run(
            &mut state,
            flows(0.0, 1000.0, 1000.0, 0.0),
            3600.0,
            1000.0,
            0.0,
            50.0,
        );
        assert!(
            (morning.grid_to_house_wh - 1000.0).abs() < 1e-6,
            "the grid served the house, a night late: {morning:?}"
        );
        assert!(state.stored_grid_wh() < 1e-6, "and no longer holds it");
    }

    #[test]
    fn solar_stored_and_released_never_counts_as_grid() {
        let mut state = GridOrigin::default();
        run(
            &mut state,
            flows(1000.0, 0.0, -1000.0, 0.0),
            3600.0,
            0.0,
            1000.0,
            50.0,
        );
        assert_eq!(state.stored_grid_wh(), 0.0);

        let evening = run(
            &mut state,
            flows(0.0, 1000.0, 1000.0, 0.0),
            3600.0,
            1000.0,
            0.0,
            50.0,
        );
        assert_eq!(evening.grid_to_house_wh, 0.0);
    }

    #[test]
    fn a_mixed_battery_is_drawn_down_in_proportion() {
        // A tank does not label its contents: half grid, half sun, so half of
        // whatever comes out is grid.
        let mut state = GridOrigin::new(500.0);
        let out = run(
            &mut state,
            flows(0.0, 400.0, 400.0, 0.0),
            3600.0,
            1000.0,
            600.0,
            50.0,
        );
        assert!((out.grid_to_house_wh - 200.0).abs() < 1e-6, "{out:?}");
        assert!((state.stored_grid_wh() - 300.0).abs() < 1e-6);
    }

    #[test]
    fn an_empty_battery_holds_no_grid_energy_whatever_the_running_total_says() {
        // The anchor that stops drift becoming permanent. Every error — losses,
        // the residual between four measurements, intervals missed while Dom was
        // not running — accumulates here, and reaching the floor clears it.
        let mut state = GridOrigin::new(4000.0);
        run(
            &mut state,
            flows(0.0, 0.0, 0.0, 0.0),
            2.0,
            100.0,
            100.0,
            EMPTY_RSOC,
        );
        assert_eq!(state.stored_grid_wh(), 0.0);
    }

    #[test]
    fn the_stored_total_can_never_exceed_what_the_battery_holds() {
        let mut state = GridOrigin::new(10_000.0);
        run(
            &mut state,
            flows(0.0, 0.0, -5000.0, -5000.0),
            3600.0,
            800.0,
            800.0,
            50.0,
        );
        assert!(
            state.stored_grid_wh() <= 800.0,
            "{}",
            state.stored_grid_wh()
        );
    }

    #[test]
    fn a_gap_keeps_the_mixture_rather_than_guessing_at_it() {
        // Half the battery was grid energy; while Dom was not looking it filled
        // up. Claiming the new energy was solar would flatter self-sufficiency,
        // claiming it was grid would punish it, and neither is known.
        let mut state = GridOrigin::new(500.0);
        state.resync(1000.0, 1000.0);
        assert!(
            (state.stored_grid_wh() - 500.0).abs() < 1e-6,
            "nothing moved"
        );

        state.resync(1000.0, 2000.0);
        assert!(
            (state.stored_grid_wh() - 1000.0).abs() < 1e-6,
            "the share is held, not the amount: {}",
            state.stored_grid_wh()
        );
    }

    #[test]
    fn resyncing_needs_both_ends_to_do_anything_at_all() {
        // Taking only the new total would compute the share from the very number
        // it then scales by, which cannot change the answer — the gap would
        // silently do nothing, which is the bug this signature prevents.
        let mut state = GridOrigin::new(500.0);
        state.resync(1000.0, 4000.0);
        assert!(state.stored_grid_wh() > 500.0, "{}", state.stored_grid_wh());
    }

    #[test]
    fn an_interval_that_changed_direction_is_split_at_both_ends() {
        // The grid reverses within 9.6% of consecutive samples on this
        // installation. Averaging first would describe an interval that neither
        // imported nor exported; splitting each end and integrating does not.
        let importing = flows(0.0, 1000.0, 0.0, -1000.0);
        let exporting = flows(2000.0, 1000.0, 0.0, 1000.0);
        assert_balanced(importing);
        assert_balanced(exporting);

        let mut state = GridOrigin::default();
        let out = state.account(importing, exporting, 3600.0, 1000.0, 1000.0, 50.0);
        // Half an hour importing 1 kW to the house, half an hour exporting.
        assert!((out.grid_to_house_wh - 500.0).abs() < 1e-6, "{out:?}");

        // Averaging the flows first would have hidden it entirely.
        let averaged = split(flows(1000.0, 1000.0, 0.0, 0.0));
        assert_eq!(averaged.grid_to_load_w, 0.0, "which is the wrong answer");
    }

    #[test]
    fn an_interval_of_no_time_accounts_for_nothing() {
        let mut state = GridOrigin::new(100.0);
        let f = flows(0.0, 1000.0, 1000.0, 0.0);
        for dt in [0.0, -1.0] {
            let out = state.account(f, f, dt, 1000.0, 1000.0, 50.0);
            assert_eq!(out.grid_to_house_wh, 0.0);
            assert_eq!(out.grid_to_battery_wh, 0.0);
        }
        assert_eq!(state.stored_grid_wh(), 100.0, "and leaves the state alone");
    }

    #[test]
    fn nothing_is_ever_attributed_to_the_grid_that_the_grid_did_not_supply() {
        // A sweep: whatever the flows, grid energy accounted for cannot exceed
        // what was actually imported over the interval.
        let mut state = GridOrigin::new(2000.0);
        for production in [0.0, 500.0, 3000.0] {
            for consumption in [0.0, 800.0, 4000.0] {
                for battery in [-2000.0, 0.0, 1500.0] {
                    // The grid is whatever balances the other three.
                    let grid = production + battery - consumption;
                    let f = flows(production, consumption, battery, grid);
                    assert_balanced(f);
                    let imported_wh = (-grid).max(0.0);
                    let out = state.account(f, f, 3600.0, 4000.0, 4000.0, 50.0);
                    assert!(
                        out.grid_to_battery_wh <= imported_wh + 1e-6,
                        "{f:?} imported {imported_wh} but charged {out:?}"
                    );
                    assert!(out.grid_to_house_wh >= -1e-6, "{f:?} -> {out:?}");
                    assert!(out.grid_to_battery_wh >= -1e-6, "{f:?} -> {out:?}");
                }
            }
        }
    }
}
