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

//! Turning a solar-irradiance forecast into predicted PV production.
//!
//! Pure arithmetic — no database, no network, no clock. Everything here is a
//! function of numbers passed in, which is what makes the model testable and the
//! calibration reproducible.
//!
//! # The model
//!
//! One free parameter. Production over a 15-minute step is
//!
//! ```text
//! Wh = k · GTI · (1 + γ·(T_cell − 25))      T_cell ≈ T_air + 0.03·GTI
//! ```
//!
//! `GTI` is plane-of-array irradiance in W/m², which Open-Meteo computes for a
//! given tilt and azimuth, so no solar geometry is implemented here. `γ` is the
//! power temperature coefficient of crystalline silicon. `k` is everything else
//! at once: panel area, module efficiency, inverter losses, soiling, shading,
//! wiring — fitted against the house's own recorded production rather than
//! entered from a datasheet.
//!
//! That single fitted number is why this works. Guessing the array from nameplate
//! specs lands within perhaps ±20%; fitting `k` against 40 days of measured
//! production predicted held-out days to 6.7% mean absolute error, and recovered
//! a peak of 9.2 kW against an observed 9.55 kW it had never been shown.
//!
//! # What the model deliberately leaves out
//!
//! **Cloud cover.** GTI already encodes cloud attenuation. Regressing on both
//! would fit the same physics twice and produce a confident, wrong coefficient.
//! Cloud cover is worth *showing* — it is what a person reads off a forecast —
//! but it does not belong in the fit.
//!
//! **Inverter clipping.** Checked against real data: output per unit of
//! irradiance stays flat across the whole GTI range with no ceiling, so the
//! response is linear and a `min(…, P_max)` term would only add a parameter that
//! fits noise. Revisit if the array is ever extended past the inverter.
//!
//! **Recent-days correction.** Tempting, and measurably harmful — see
//! [`Accuracy`].

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};

/// Power temperature coefficient of crystalline silicon, per °C of cell
/// temperature above the 25 °C standard-test condition. Negative: a hot panel
/// produces less.
pub const TEMPERATURE_COEFFICIENT: f64 = -0.004;

/// Cell temperature rise above ambient, per W/m² of irradiance. Equivalent to a
/// nominal operating cell temperature of ~44 °C, which is typical for a
/// roof-mounted array with air behind it.
pub const CELL_RISE_PER_IRRADIANCE: f64 = 0.03;

/// Forecast steps per hour. The irradiance variables Open-Meteo serves at
/// 15-minute resolution are native model output rather than interpolated hourly
/// values, which is the reason to work at this step at all.
pub const STEPS_PER_HOUR: f64 = 4.0;

/// Irradiance at standard test conditions, W/m². Used only to express a fitted
/// `k` as a peak power a person can recognise.
const STC_IRRADIANCE: f64 = 1000.0;

/// One forecast step: the weather expected over a 15-minute interval.
///
/// `cloud_cover_pct` and `precipitation_mm` are carried for display only. See the
/// module docs for why they are not in the fit.
#[derive(Clone, Debug, PartialEq)]
pub struct ForecastPoint {
    /// Start of the 15-minute interval this describes.
    pub valid_at: DateTime<Utc>,
    /// Plane-of-array irradiance, W/m², for the calibrated tilt and azimuth.
    pub gti_w_m2: f64,
    /// Air temperature at 2 m, °C.
    pub temperature_c: f64,
    pub cloud_cover_pct: f64,
    pub precipitation_mm: f64,
}

/// One (weather, measured production) pair, the input to a calibration.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// Start of the step. Carried so error can be summarised per day as well as
    /// per step — which criterion is used to choose a plane changes the answer;
    /// see [`daily_rmse_kwh`].
    pub at: DateTime<Utc>,
    pub gti_w_m2: f64,
    pub temperature_c: f64,
    /// What the array actually produced over the step, in watt-hours.
    pub actual_wh: f64,
}

/// The irradiance a panel actually responds to: incident irradiance, derated for
/// how hot it makes the cell.
///
/// Separated from [`predict_wh`] because it is also the regressor the fit runs
/// on — the model is linear in exactly this quantity, so fitting and predicting
/// must not be able to disagree about it.
pub fn response(gti_w_m2: f64, temperature_c: f64) -> f64 {
    if gti_w_m2 <= 0.0 {
        return 0.0;
    }
    let cell_c = temperature_c + CELL_RISE_PER_IRRADIANCE * gti_w_m2;
    gti_w_m2 * (1.0 + TEMPERATURE_COEFFICIENT * (cell_c - 25.0))
}

/// Predicted production over one 15-minute step, in watt-hours.
pub fn predict_wh(gti_w_m2: f64, temperature_c: f64, k: f64) -> f64 {
    (k * response(gti_w_m2, temperature_c)).max(0.0)
}

/// A fitted array response, and the evidence behind it.
#[derive(Clone, Debug, PartialEq)]
pub struct Calibration {
    /// Watt-hours per 15-minute step per unit of [`response`].
    pub k: f64,
    /// The plane the fit was run against.
    pub tilt_deg: i32,
    pub azimuth_deg: i32,
    /// How much was fitted, so a thin calibration can be labelled as one.
    pub days: usize,
    pub samples: usize,
    /// Root-mean-square error over the fitted steps, in watts.
    pub rmse_w: f64,
    /// Root-mean-square error of the fitted *daily* totals, in kWh. The figure
    /// planes are chosen by, and the closest thing to an error bar available
    /// before enough forecasts have been made and checked — see
    /// [`daily_rmse_kwh`] and [`Accuracy`].
    pub daily_rmse_kwh: f64,
    pub fitted_at: DateTime<Utc>,
}

impl Calibration {
    /// The array's peak output, in watts, at standard test conditions.
    ///
    /// Not a number the fit is given — it falls out of `k` — which makes it the
    /// cheapest sanity check there is: it should look like the array on the roof.
    pub fn peak_w(&self) -> f64 {
        self.k * STC_IRRADIANCE * STEPS_PER_HOUR
    }

    /// Whether there is enough behind this fit to show a prediction without
    /// qualifying it heavily.
    pub fn is_well_founded(&self) -> bool {
        self.days >= MIN_CALIBRATION_DAYS
    }
}

/// Fewest days of production history a calibration is trusted on.
///
/// Two weeks spans enough weather to separate the array's response from a run of
/// similar days. Below it a fit still happens — a rough number beats none — but
/// the view says so.
pub const MIN_CALIBRATION_DAYS: usize = 14;

/// How many RMS residuals a sample may sit from the fit before the second pass
/// drops it. Loose enough that ordinary scatter — passing cloud, an edge-of-cloud
/// irradiance spike — survives; tight enough to catch a physically impossible one.
const OUTLIER_REJECTION_SIGMA: f64 = 4.0;

/// Least-squares scale through the origin: the `k` minimising squared error.
///
/// Through the origin deliberately. An intercept would let the model claim
/// production in the dark, and the one thing known for certain about this system
/// is that it makes nothing at night.
///
/// Fitted in two passes. Least squares is not robust, and one impossible sample
/// really does move the answer: a single 200 kW step among two hundred clean ones
/// dragged `k` 18% off its true value in test. The caller screens whole days for
/// completeness before getting here, but that catches gaps, not a lone corrupt
/// reading — so the second pass drops whatever sits further than
/// `OUTLIER_REJECTION_SIGMA` RMS residuals from the first fit and refits without
/// it. One extra pass, not iterated to convergence: the aim is to survive a bad
/// sample, not to reshape the data until it agrees.
///
/// `None` when no sample carries any irradiance, which is what a run of nights,
/// an unlit array, or an empty slice all look like.
pub fn fit_scale(samples: &[Sample]) -> Option<f64> {
    let first = least_squares(samples.iter())?;
    let tolerance = OUTLIER_REJECTION_SIGMA * rmse_wh(samples, first);
    if tolerance <= 0.0 {
        return Some(first);
    }
    let kept = samples.iter().filter(|s| {
        (predict_wh(s.gti_w_m2, s.temperature_c, first) - s.actual_wh).abs() <= tolerance
    });
    // Refuse to fall back on a fit of nothing: if the trim leaves no lit sample,
    // the first pass is the better answer available.
    least_squares(kept).or(Some(first))
}

/// One ordinary least-squares pass through the origin.
fn least_squares<'a>(samples: impl IntoIterator<Item = &'a Sample>) -> Option<f64> {
    let mut numerator = 0.0;
    let mut denominator = 0.0;
    for s in samples {
        let x = response(s.gti_w_m2, s.temperature_c);
        numerator += x * s.actual_wh;
        denominator += x * x;
    }
    (denominator > 0.0).then(|| numerator / denominator)
}

/// Root-mean-square residual in watt-hours per step, the unit the samples are in.
fn rmse_wh(samples: &[Sample], k: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples
        .iter()
        .map(|s| {
            let e = predict_wh(s.gti_w_m2, s.temperature_c, k) - s.actual_wh;
            e * e
        })
        .sum();
    (sum / samples.len() as f64).sqrt()
}

/// Root-mean-square error of a fit, converted from watt-hours per step to watts
/// so it can be read against the array's peak.
pub fn rmse_w(samples: &[Sample], k: f64) -> f64 {
    rmse_wh(samples, k) * STEPS_PER_HOUR
}

/// Root-mean-square error of *daily* totals, in kWh.
///
/// The criterion a plane is chosen by, and deliberately not [`rmse_w`]. The two
/// disagree, and measurably: over 44 days of real production, picking the plane
/// with the lowest per-step error gave 10.2% mean absolute error on held-out
/// days, while picking the lowest daily error gave 8.6% — against 8.0% for the
/// best plane in the grid. Per-step error is dominated by exactly when a cloud
/// arrives, which is noise at the scale anyone asks the question; a daily total
/// averages that out and is what the view actually reports.
///
/// The scale `k` is still fitted per step, which is the right estimator for it.
/// Only the choice between planes is made here.
pub fn daily_rmse_kwh(samples: &[Sample], k: f64) -> f64 {
    let mut per_day: BTreeMap<NaiveDate, (f64, f64)> = BTreeMap::new();
    for s in samples {
        let day = s.at.with_timezone(&chrono::Local).date_naive();
        let e = per_day.entry(day).or_default();
        e.0 += predict_wh(s.gti_w_m2, s.temperature_c, k);
        e.1 += s.actual_wh;
    }
    if per_day.is_empty() {
        return f64::INFINITY;
    }
    let sum: f64 = per_day
        .values()
        .map(|(p, a)| ((p - a) / 1000.0).powi(2))
        .sum();
    (sum / per_day.len() as f64).sqrt()
}

/// Sums 15-minute predictions into kWh per local day.
pub fn daily_kwh<'a>(
    points: impl IntoIterator<Item = &'a ForecastPoint>,
    k: f64,
) -> BTreeMap<NaiveDate, f64> {
    let mut out: BTreeMap<NaiveDate, f64> = BTreeMap::new();
    for p in points {
        let day = p.valid_at.with_timezone(&chrono::Local).date_naive();
        *out.entry(day).or_default() += predict_wh(p.gti_w_m2, p.temperature_c, k) / 1000.0;
    }
    out
}

/// Expected production for a day that is already partly over: what was measured,
/// plus what is still forecast.
///
/// Not the same as summing the day's forecast rows, and better than it in two
/// ways. Forecast rows only exist for steps Dom was running for, so after a
/// restart or any downtime a plain sum reports a fraction of the day as though it
/// were the whole of it. And half of today is no longer a prediction — folding in
/// the measurement replaces a forecast with a fact, so the figure sharpens as the
/// day goes on instead of staying as uncertain as it was at dawn.
pub fn expected_for_partial_day(produced_kwh: f64, remaining: &[ForecastPoint], k: f64) -> f64 {
    produced_kwh
        + remaining
            .iter()
            .map(|p| predict_wh(p.gti_w_m2, p.temperature_c, k) / 1000.0)
            .sum::<f64>()
}

/// How well past forecasts turned out, measured against what was produced.
///
/// Built from days that have both a forecast made before the day happened and a
/// complete production record, so it reports the accuracy of the whole chain —
/// weather forecast included — rather than of the panel model alone. That
/// distinction is the point: against irradiance that actually occurred the model
/// lands within 6.7%, but a real day-ahead forecast is ~8.5%, and the difference
/// is the atmosphere, not anything a better fit could recover.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Accuracy {
    /// Days compared.
    pub days: usize,
    /// Mean absolute percentage error over those days.
    pub mape: f64,
    /// Mean signed error, kWh. Persistent non-zero means the forecast leans.
    pub bias_kwh: f64,
    /// How many landed within 10%.
    pub within_10pct: usize,
}

/// Fewest compared days before an accuracy figure is worth showing.
pub const MIN_ACCURACY_DAYS: usize = 7;

/// Days below this produced total are left out of the error statistics.
///
/// A percentage error on a day that made 0.4 kWh says nothing about the model and
/// everything about the divisor.
const MIN_DAY_KWH_FOR_ERROR: f64 = 5.0;

impl Accuracy {
    /// Summarises paired (predicted, actual) daily totals in kWh.
    pub fn from_pairs(pairs: &[(f64, f64)]) -> Self {
        let scored: Vec<_> = pairs
            .iter()
            .filter(|(_, actual)| *actual >= MIN_DAY_KWH_FOR_ERROR)
            .collect();
        if scored.is_empty() {
            return Self::default();
        }
        let n = scored.len() as f64;
        Self {
            days: scored.len(),
            mape: scored.iter().map(|(p, a)| (p - a).abs() / a).sum::<f64>() / n,
            bias_kwh: scored.iter().map(|(p, a)| p - a).sum::<f64>() / n,
            within_10pct: scored
                .iter()
                .filter(|(p, a)| (*p - *a).abs() / *a <= 0.10)
                .count(),
        }
    }

    /// Whether enough days have been compared to quote the figure.
    pub fn is_meaningful(&self) -> bool {
        self.days >= MIN_ACCURACY_DAYS
    }
}

/// Tilts tried when searching for the array's effective orientation, in degrees
/// from horizontal.
pub const TILT_CANDIDATES: [i32; 5] = [15, 25, 35, 45, 55];

/// Azimuths tried, in degrees from south — negative east, positive west. Open
/// -Meteo uses the same convention, so these pass straight through.
pub const AZIMUTH_CANDIDATES: [i32; 5] = [-60, -30, 0, 30, 60];

/// Half the spacing of each axis of the grid above, so a refinement pass lands
/// between the coarse points rather than back on them.
const TILT_REFINE_STEP: i32 = 5;
const AZIMUTH_REFINE_STEP: i32 = 15;

/// The eight planes surrounding a coarse winner, for a second pass.
///
/// The coarse grid straddles the optimum rather than hitting it: on real data its
/// best plane predicted held-out days to 8.0% where a plane between two of its
/// points reached 6.7%. Eight more requests, run as rarely as the search itself,
/// close most of that.
///
/// Tilts are clamped to the horizon. Azimuths are not wrapped — a search that
/// ends up against the edge of the grid is reporting that the array faces
/// somewhere the grid does not reach, and silently wrapping to the far side would
/// turn that into a confident wrong answer.
pub fn refinement_around(tilt_deg: i32, azimuth_deg: i32) -> Vec<(i32, i32)> {
    let mut out = Vec::with_capacity(8);
    for dt in [-TILT_REFINE_STEP, 0, TILT_REFINE_STEP] {
        for da in [-AZIMUTH_REFINE_STEP, 0, AZIMUTH_REFINE_STEP] {
            if (dt, da) == (0, 0) {
                continue;
            }
            let tilt = (tilt_deg + dt).clamp(0, 90);
            out.push((tilt, azimuth_deg + da));
        }
    }
    // Clamping at the horizon can collapse two rows onto one another, and each
    // duplicate is a wasted request. Sorted first because `dedup` only removes
    // neighbours.
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(gti: f64, temp: f64, wh: f64) -> Sample {
        sample_at(0, gti, temp, wh)
    }

    /// A sample `steps` quarter-hours after a fixed instant, for the tests that
    /// care which day it lands in.
    fn sample_at(steps: i64, gti: f64, temp: f64, wh: f64) -> Sample {
        Sample {
            at: DateTime::from_timestamp(1_756_000_000, 0).unwrap()
                + chrono::Duration::minutes(15 * steps),
            gti_w_m2: gti,
            temperature_c: temp,
            actual_wh: wh,
        }
    }

    fn point(hhmm: &str, gti: f64) -> ForecastPoint {
        ForecastPoint {
            valid_at: DateTime::parse_from_rfc3339(hhmm).unwrap().to_utc(),
            gti_w_m2: gti,
            temperature_c: 20.0,
            cloud_cover_pct: 0.0,
            precipitation_mm: 0.0,
        }
    }

    #[test]
    fn nothing_is_produced_in_the_dark() {
        assert_eq!(response(0.0, 20.0), 0.0);
        assert_eq!(predict_wh(0.0, 20.0, 2.3), 0.0);
        // Open-Meteo can report a small negative at the horizon; it is still night.
        assert_eq!(response(-5.0, 20.0), 0.0);
    }

    #[test]
    fn a_hot_panel_produces_less_than_a_cold_one() {
        let cold = response(800.0, 0.0);
        let hot = response(800.0, 30.0);
        assert!(hot < cold, "cold {cold}, hot {hot}");
        // 30 °C of air temperature costs 12% of output, at 0.4%/°C.
        assert!(
            (cold / hot - 1.0 / (1.0 - 0.12)).abs() < 0.01,
            "{cold} {hot}"
        );
    }

    #[test]
    fn at_standard_test_conditions_response_is_the_irradiance() {
        // 25 °C *cell*, which at 1000 W/m² means much cooler air.
        let air = 25.0 - CELL_RISE_PER_IRRADIANCE * STC_IRRADIANCE;
        assert!((response(STC_IRRADIANCE, air) - STC_IRRADIANCE).abs() < 1e-9);
    }

    #[test]
    fn the_fit_recovers_a_scale_it_was_not_told() {
        let truth = 2.3011;
        let samples: Vec<_> = [(0.0, 12.0), (180.0, 15.0), (640.0, 22.0), (930.0, 28.0)]
            .iter()
            .map(|&(gti, t)| sample(gti, t, predict_wh(gti, t, truth)))
            .collect();
        let k = fit_scale(&samples).unwrap();
        assert!((k - truth).abs() < 1e-9, "recovered {k}");
        assert!(rmse_w(&samples, k) < 1e-9);
    }

    #[test]
    fn the_fit_is_not_derailed_by_a_single_wild_reading() {
        // Least squares is not robust, so this pins how much damage one bad
        // sample does rather than claiming immunity: the caller screens days for
        // completeness before fitting, and this is what happens if one slips by.
        let truth = 2.3;
        let mut samples: Vec<_> = (0..200)
            .map(|i| {
                let gti = i as f64 * 5.0;
                sample(gti, 20.0, predict_wh(gti, 20.0, truth))
            })
            .collect();
        let clean = fit_scale(&samples).unwrap();
        samples.push(sample(500.0, 20.0, 50_000.0));
        let dirty = fit_scale(&samples).unwrap();
        assert!((clean - truth).abs() < 1e-9);
        assert!(
            (dirty - truth).abs() < 0.1,
            "one outlier moved k to {dirty}"
        );
    }

    #[test]
    fn a_night_of_samples_yields_no_fit() {
        assert_eq!(fit_scale(&[]), None);
        assert_eq!(
            fit_scale(&[sample(0.0, 10.0, 0.0), sample(0.0, 9.0, 0.0)]),
            None
        );
    }

    #[test]
    fn prediction_never_goes_negative() {
        // A very hot cell drives the derate term negative; the array still does
        // not consume power.
        assert_eq!(predict_wh(900.0, 300.0, 2.3), 0.0);
    }

    #[test]
    fn peak_power_falls_out_of_the_fitted_scale() {
        let c = Calibration {
            k: 2.3011,
            tilt_deg: 40,
            azimuth_deg: 50,
            days: 40,
            samples: 3742,
            rmse_w: 900.0,
            daily_rmse_kwh: 3.4,
            fitted_at: Utc::now(),
        };
        // The measured fit against 40 days of real production; the array's
        // observed peak was 9547 W, which it was never shown.
        assert!((c.peak_w() - 9204.0).abs() < 1.0, "{}", c.peak_w());
        assert!(c.is_well_founded());
    }

    #[test]
    fn a_thin_calibration_is_marked_as_thin() {
        let c = Calibration {
            k: 2.3,
            tilt_deg: 30,
            azimuth_deg: 0,
            days: MIN_CALIBRATION_DAYS - 1,
            samples: 100,
            rmse_w: 0.0,
            daily_rmse_kwh: 0.0,
            fitted_at: Utc::now(),
        };
        assert!(!c.is_well_founded());
    }

    #[test]
    fn daily_totals_are_grouped_by_local_day() {
        // Two steps either side of local midnight in Zurich (UTC+2 in summer):
        // 21:45 UTC is still today, 22:15 UTC is tomorrow.
        let points = vec![
            point("2026-08-22T21:45:00Z", 400.0),
            point("2026-08-22T22:15:00Z", 400.0),
        ];
        let totals = daily_kwh(&points, 2.3);
        assert_eq!(totals.len(), 2, "{totals:?}");
        assert!(totals.values().all(|v| *v > 0.0));
    }

    #[test]
    fn a_partly_finished_day_counts_what_happened_plus_what_is_left() {
        let k = 2.3;
        let rest = vec![
            point("2026-08-22T14:00:00Z", 800.0),
            point("2026-08-22T14:15:00Z", 600.0),
        ];
        let expected = expected_for_partial_day(8.5, &rest, k);
        let forecast_part: f64 = rest
            .iter()
            .map(|p| predict_wh(p.gti_w_m2, p.temperature_c, k) / 1000.0)
            .sum();
        assert!((expected - (8.5 + forecast_part)).abs() < 1e-12);
        assert!(expected > 8.5, "the afternoon still has to be added");
    }

    #[test]
    fn a_day_with_nothing_left_to_forecast_is_just_what_it_produced() {
        // After sunset, and on a first run where no forecast row for today was
        // ever recorded, the answer is the measurement — not zero.
        assert_eq!(expected_for_partial_day(41.2, &[], 2.3), 41.2);
    }

    #[test]
    fn accuracy_reports_error_bias_and_hit_rate() {
        // Four days: two exact, one 20% over, one 10% under.
        let a = Accuracy::from_pairs(&[(50.0, 50.0), (40.0, 40.0), (60.0, 50.0), (45.0, 50.0)]);
        assert_eq!(a.days, 4);
        assert!(
            (a.mape - (0.0 + 0.0 + 0.2 + 0.1) / 4.0).abs() < 1e-9,
            "{a:?}"
        );
        assert!((a.bias_kwh - (10.0 - 5.0) / 4.0).abs() < 1e-9, "{a:?}");
        assert_eq!(a.within_10pct, 3, "a 10% miss counts as within 10%");
        assert!(!a.is_meaningful(), "four days is not enough to quote");
    }

    #[test]
    fn a_near_zero_day_is_left_out_of_the_error() {
        // Predicting 1.0 against an actual 0.2 is a 400% error that says nothing
        // about the model, and would swamp the average.
        let with_dark_day = Accuracy::from_pairs(&[(50.0, 50.0), (1.0, 0.2)]);
        assert_eq!(with_dark_day.days, 1);
        assert_eq!(with_dark_day.mape, 0.0);
    }

    #[test]
    fn no_comparable_days_is_not_a_perfect_score() {
        let a = Accuracy::from_pairs(&[]);
        assert_eq!(a, Accuracy::default());
        assert!(!a.is_meaningful());
        assert_eq!(a.days, 0);
    }

    #[test]
    fn daily_error_and_step_error_can_disagree() {
        // The reason planes are chosen on daily error. Two days, each one step:
        // a model that is wrong by the same amount every step but in opposite
        // directions has real step error and none at all per day.
        let k = 1.0;
        let day_one = 0;
        let day_two = 96; // 24 h later
        let samples = vec![
            sample_at(day_one, 100.0, 25.0, response(100.0, 25.0) * k + 500.0),
            sample_at(day_one, 100.0, 25.0, response(100.0, 25.0) * k - 500.0),
            sample_at(day_two, 100.0, 25.0, response(100.0, 25.0) * k),
        ];
        assert!(rmse_w(&samples, k) > 0.0, "the steps are wrong");
        assert!(
            daily_rmse_kwh(&samples, k) < 1e-9,
            "but the days are right: {}",
            daily_rmse_kwh(&samples, k)
        );
    }

    #[test]
    fn daily_error_is_infinite_with_nothing_to_score() {
        // Infinity rather than zero, so a plane with no overlapping data never
        // wins a comparison against one that does.
        assert_eq!(daily_rmse_kwh(&[], 2.3), f64::INFINITY);
    }

    #[test]
    fn refinement_lands_between_the_coarse_points() {
        let around = refinement_around(45, 30);
        assert_eq!(around.len(), 8, "the eight neighbours, not the centre");
        assert!(!around.contains(&(45, 30)), "the centre is already known");
        assert!(around.contains(&(40, 15)) && around.contains(&(50, 45)));
        // Off the coarse grid, which is the point of running it at all.
        let coarse: Vec<_> = TILT_CANDIDATES
            .iter()
            .flat_map(|t| AZIMUTH_CANDIDATES.iter().map(|a| (*t, *a)))
            .collect();
        assert!(around.iter().all(|p| !coarse.contains(p)), "{around:?}");
    }

    #[test]
    fn refinement_stays_above_the_horizon_and_does_not_wrap() {
        let flat = refinement_around(0, 0);
        assert!(flat.iter().all(|(t, _)| *t >= 0));
        // Clamping collapses the tilt row below the horizon onto the one at it;
        // each duplicate would be a wasted request.
        let mut unique = flat.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(flat, unique, "clamping must not produce duplicates");
        // An azimuth against the edge of the grid stays there rather than
        // wrapping to the far side and claiming the array faces the other way.
        assert!(refinement_around(35, -60).contains(&(35, -75)));
    }

    #[test]
    fn the_orientation_grid_spans_the_plausible_roofs() {
        // South must be a candidate, and the grid must be symmetric about it, or
        // the search would lean east or west before seeing any data.
        assert!(AZIMUTH_CANDIDATES.contains(&0));
        let sum: i32 = AZIMUTH_CANDIDATES.iter().sum();
        assert_eq!(sum, 0);
        assert!(TILT_CANDIDATES.iter().all(|t| (0..=90).contains(t)));
    }
}
