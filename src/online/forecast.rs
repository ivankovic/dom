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

//! Solar-irradiance and temperature forecasts from Open-Meteo.
//!
//! Two endpoints, one parser. `api.open-meteo.com` serves the forward forecast;
//! `historical-forecast-api.open-meteo.com` serves the same variables for past
//! dates, which is what a calibration is fitted against.
//!
//! # Why this service and not MeteoSwiss directly
//!
//! MeteoSwiss publishes its own point forecasts as Open Government Data, and it
//! was the obvious first choice — same host as the outdoor temperature already
//! read in [`super::weather`], same parameter codes. It was measured and
//! rejected: the CSVs are one file per parameter covering every one of ~6000
//! Swiss locations for nine days, and the hourly 2 m temperature file alone is
//! 32.5 MB. Extracting ~216 numbers for one house from 32.5 MB every hour is
//! roughly 23 GB a month, and the rows are ordered by time rather than by
//! location so there is no early exit. The same request here is about 7 KB.
//!
//! Open-Meteo runs MeteoSwiss's own ICON-CH1 and ICON-CH2 models — 1 km and 2 km
//! over the Alps — blending to a wider-area model as the lead time outruns them,
//! so this is the federal forecast, delivered in a shape a house can afford.
//! No model is pinned for exactly that reason: ICON-CH1 only reaches 33 hours,
//! and pinning it would truncate tomorrow.
//!
//! # Resolution
//!
//! 15 minutes. That is genuinely native for irradiance — the values carry
//! sub-hourly structure no interpolation would produce. Temperature at the same
//! step is interpolated from hourly, and is taken anyway because it comes free in
//! the same document and the model consumes both together.
//!
//! Ten-minute forecasts do not exist. Weather models emit hourly steps; anything
//! finer is nowcasting, which stops six hours out and so cannot say anything
//! about tomorrow.
//!
//! # Attribution
//!
//! Open-Meteo data is CC BY 4.0 and the licence asks for a credit shown next to
//! it. [`ATTRIBUTION`] is that credit, and the Environment view displays it.

use anyhow::Context;
use chrono::Timelike;
use serde::Deserialize;

use crate::solar::{Calibration, ForecastPoint, Sample};

/// Forward forecast.
const HOST_FORECAST: &str = "api.open-meteo.com";
/// The same variables for dates that have already passed.
const HOST_ARCHIVE: &str = "historical-forecast-api.open-meteo.com";

/// Credit the data licence requires wherever the data is shown.
pub const ATTRIBUTION: &str = "Weather data by Open-Meteo.com";

/// How far ahead the forward forecast reaches. Two days covers the rest of today
/// and all of tomorrow from any hour, which is the question being asked.
const FORECAST_DAYS: u8 = 2;

/// The variables fetched, in both directions.
///
/// `cloud_cover` and `precipitation` are for the view, not the model — plane-of
/// -array irradiance already encodes cloud attenuation, and regressing on both
/// would fit the same physics twice.
const VARIABLES: &str = "global_tilted_irradiance,temperature_2m,cloud_cover,precipitation";

/// UTC throughout. The forecast is joined against stored measurements, which are
/// UTC, and local days are derived once at the point of display — asking the API
/// for local time would put a second timezone conversion in the middle of a join.
const QUERY_TAIL: &str = "timezone=GMT";

/// Open-Meteo's 15-minute block, as delivered.
#[derive(Deserialize)]
struct Response {
    minutely_15: Series,
}

/// Parallel arrays, one entry per step. Any of the values may be null where the
/// model has no answer, so each is optional and a step missing what the model
/// needs is dropped rather than defaulted.
#[derive(Deserialize)]
struct Series {
    time: Vec<String>,
    global_tilted_irradiance: Vec<Option<f64>>,
    temperature_2m: Vec<Option<f64>>,
    cloud_cover: Vec<Option<f64>>,
    precipitation: Vec<Option<f64>>,
}

/// Query path for the forward forecast at a given plane.
fn forecast_path(latitude: f64, longitude: f64, tilt_deg: i32, azimuth_deg: i32) -> String {
    format!(
        "/v1/forecast?latitude={latitude}&longitude={longitude}\
         &minutely_15={VARIABLES}&tilt={tilt_deg}&azimuth={azimuth_deg}\
         &forecast_days={FORECAST_DAYS}&{QUERY_TAIL}"
    )
}

/// Query path for past dates at a given plane, inclusive of both ends.
fn archive_path(
    latitude: f64,
    longitude: f64,
    tilt_deg: i32,
    azimuth_deg: i32,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
) -> String {
    format!(
        "/v1/forecast?latitude={latitude}&longitude={longitude}\
         &minutely_15={VARIABLES}&tilt={tilt_deg}&azimuth={azimuth_deg}\
         &start_date={from}&end_date={to}&{QUERY_TAIL}"
    )
}

/// Parses a response into forecast steps.
///
/// Steps whose timestamp will not parse, or whose irradiance or temperature the
/// model did not supply, are dropped. A dropped step simply is not predicted
/// against; a step defaulted to zero irradiance would be predicted as darkness,
/// which is a different and worse claim.
pub fn parse(json: &str) -> anyhow::Result<Vec<ForecastPoint>> {
    let r: Response = serde_json::from_str(json).context("parsing the Open-Meteo forecast")?;
    let s = r.minutely_15;
    let n = s.time.len();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let (Some(gti), Some(temperature_c)) = (
            s.global_tilted_irradiance.get(i).copied().flatten(),
            s.temperature_2m.get(i).copied().flatten(),
        ) else {
            continue;
        };
        let Ok(naive) = chrono::NaiveDateTime::parse_from_str(&s.time[i], "%Y-%m-%dT%H:%M") else {
            continue;
        };
        out.push(ForecastPoint {
            valid_at: naive.and_utc(),
            // The model reports a small negative at the horizon; clamped here so
            // every consumer sees the same floor.
            gti_w_m2: gti.max(0.0),
            temperature_c,
            cloud_cover_pct: s.cloud_cover.get(i).copied().flatten().unwrap_or(0.0),
            precipitation_mm: s.precipitation.get(i).copied().flatten().unwrap_or(0.0),
        });
    }
    Ok(out)
}

/// Fetches the forward forecast for a plane.
pub async fn fetch_forecast(
    latitude: f64,
    longitude: f64,
    tilt_deg: i32,
    azimuth_deg: i32,
) -> anyhow::Result<Vec<ForecastPoint>> {
    let path = forecast_path(latitude, longitude, tilt_deg, azimuth_deg);
    parse(&super::https::get(HOST_FORECAST, &path).await?)
}

/// Fetches past weather for a plane, for calibration.
pub async fn fetch_archive(
    latitude: f64,
    longitude: f64,
    tilt_deg: i32,
    azimuth_deg: i32,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
) -> anyhow::Result<Vec<ForecastPoint>> {
    let path = archive_path(latitude, longitude, tilt_deg, azimuth_deg, from, to);
    parse(&super::https::get(HOST_ARCHIVE, &path).await?)
}

/// Pairs forecast steps with what was actually produced in them.
///
/// Steps with no matching measurement are dropped rather than treated as zero
/// production: an unrecorded step is unknown, and calling it zero would teach the
/// fit that the array makes nothing in sunshine.
pub fn pair_with_production(
    weather: &[ForecastPoint],
    produced_wh: &std::collections::HashMap<chrono::DateTime<chrono::Utc>, f64>,
) -> Vec<Sample> {
    weather
        .iter()
        .filter_map(|p| {
            Some(Sample {
                at: p.valid_at,
                gti_w_m2: p.gti_w_m2,
                temperature_c: p.temperature_c,
                actual_wh: *produced_wh.get(&p.valid_at)?,
            })
        })
        .collect()
}

/// Picks the plane and scale that best explain the measured production.
///
/// The orientation is searched rather than configured because nobody reliably
/// knows their roof to ten degrees, and because what is wanted is not the roof
/// anyway: it is the *effective* plane, which absorbs shading, soiling and an
/// array split across faces. The optimum is broad — several nearby planes score
/// within a few tenths of a percent of each other — so this reports the best of
/// them without implying the roof has been surveyed.
///
/// One request per candidate plane, so the grid is deliberately coarse and this
/// runs on the order of once a week, not once a tick.
pub fn best_fit<'a>(
    candidates: impl IntoIterator<Item = (i32, i32, &'a [ForecastPoint])>,
    produced_wh: &std::collections::HashMap<chrono::DateTime<chrono::Utc>, f64>,
    days: usize,
    fitted_at: chrono::DateTime<chrono::Utc>,
) -> Option<Calibration> {
    let mut best: Option<Calibration> = None;
    for (tilt_deg, azimuth_deg, weather) in candidates {
        let samples = pair_with_production(weather, produced_wh);
        let Some(k) = crate::solar::fit_scale(&samples) else {
            continue;
        };
        // Scored on daily error, not on the per-step `rmse_w` also recorded —
        // see `solar::daily_rmse_kwh` for why they differ and which one predicts
        // days better.
        let candidate = Calibration {
            k,
            tilt_deg,
            azimuth_deg,
            days,
            samples: samples.len(),
            rmse_w: crate::solar::rmse_w(&samples, k),
            daily_rmse_kwh: crate::solar::daily_rmse_kwh(&samples, k),
            fitted_at,
        };
        if best
            .as_ref()
            .is_none_or(|b| candidate.daily_rmse_kwh < b.daily_rmse_kwh)
        {
            best = Some(candidate);
        }
    }
    best
}

// ── Orchestration ─────────────────────────────────────────────────────────────

/// Plane assumed before anything has been fitted: a moderate tilt facing south.
/// Only ever used to get the first forecast on screen; the first calibration
/// replaces it with whatever actually explains the production.
const DEFAULT_TILT_DEG: i32 = 30;
const DEFAULT_AZIMUTH_DEG: i32 = 0;

/// How far back a calibration reads. Long enough to span varied weather and to
/// track soiling and the sun's seasonal drift, and short enough that a real
/// change to the array works its way in within a season.
const CALIBRATION_WINDOW_DAYS: i64 = 90;

/// Window the orientation search runs over. Shorter than the calibration window
/// because it costs one request per candidate plane; the winning plane is then
/// refitted over the full window.
const ORIENTATION_SEARCH_DAYS: i64 = 21;

/// Fewest complete days before fitting at all. Below this the answer would be a
/// description of one week's weather rather than of the array.
const MIN_DAYS_TO_FIT: usize = 3;

/// How often the scale is refitted, and how often the plane is searched again.
/// Refitting is one request; searching the plane is one per candidate, so it
/// happens far less often.
const REFIT_AFTER_HOURS: i64 = 24;
const RESEARCH_ORIENTATION_AFTER_DAYS: i64 = 7;

/// Pause between the requests of an orientation search, so a background fit never
/// arrives as a burst.
const SEARCH_PACING: std::time::Duration = std::time::Duration::from_millis(500);

/// How many past days the forecast-versus-actual comparison looks back over.
const COMPARISON_DAYS: i64 = 21;

/// Forecast steps a past day must have, out of the 96 a whole one holds, before
/// it may be compared against what was produced.
///
/// The counterpart of `db::MIN_STEP_COVERAGE_SECS`, and for the same reason:
/// comparing part of a forecast against all of a day is not a measurement of
/// anything. A few missing steps at the edges are tolerated because a fetch that
/// straddles midnight legitimately leaves them.
const MIN_FORECAST_STEPS_PER_DAY: usize = 92;

/// Fetches the forecast, records it, and republishes the outlook.
///
/// Does nothing until a location is set — like everything else in this module,
/// it stays off the network unless asked.
pub async fn refresh(pool: &sqlx::SqlitePool, state: &crate::app::SharedState) {
    let Ok(Some(location)) = crate::db::get_location(pool).await else {
        state.write().unwrap().solar = Default::default();
        return;
    };

    let calibration = crate::db::get_calibration(pool).await.unwrap_or(None);
    let (tilt, azimuth) = calibration
        .as_ref()
        .map(|c| (c.tilt_deg, c.azimuth_deg))
        .unwrap_or((DEFAULT_TILT_DEG, DEFAULT_AZIMUTH_DEG));

    let now = chrono::Utc::now();
    match fetch_forecast(location.latitude, location.longitude, tilt, azimuth).await {
        Ok(points) => {
            // `now` as the cutoff as well as the issue time: steps that have
            // already happened keep whatever was last predicted for them, which
            // is what makes the comparison below a comparison of forecasts.
            if let Err(e) = crate::db::insert_forecast(pool, &points, now, now).await {
                log::warn!("recording the solar forecast failed: {e:#}");
            }
            state.write().unwrap().solar.last_error = None;
        }
        Err(e) => {
            log::warn!("fetching the solar forecast failed: {e:#}");
            state.write().unwrap().solar.last_error = Some(format!("{e:#}"));
        }
    }

    republish(pool, state, calibration, now).await;
}

/// Rebuilds the outlook from what is stored, without touching the network.
///
/// Separate from [`refresh`] so the view can be brought up to date from a
/// changed calibration, or at startup, without spending a request — and so the
/// assembly can be exercised against a database rather than against the weather.
pub async fn republish(
    pool: &sqlx::SqlitePool,
    state: &crate::app::SharedState,
    calibration: Option<Calibration>,
    now: chrono::DateTime<chrono::Utc>,
) {
    let today = now.with_timezone(&chrono::Local).date_naive();
    let Some(tomorrow) = today.succ_opt() else {
        return;
    };
    let from = today - chrono::Duration::days(COMPARISON_DAYS);

    let stored = crate::db::query_forecast(pool, from, tomorrow)
        .await
        .unwrap_or_default();
    let actual = crate::db::query_daily_production_kwh(pool, from, today)
        .await
        .unwrap_or_default();

    // With no calibration there is a forecast but no way to turn it into power;
    // the weather is still worth showing, so the outlook keeps it and leaves the
    // predictions empty.
    let k = calibration.as_ref().map(|c| c.k).unwrap_or(0.0);
    let predicted = crate::solar::daily_kwh(&stored, k);

    let curve = stored
        .iter()
        .filter(|p| p.valid_at.with_timezone(&chrono::Local).date_naive() == tomorrow)
        .map(|p| {
            let local = p.valid_at.with_timezone(&chrono::Local);
            let hour = local.hour() as f64 + local.minute() as f64 / 60.0;
            let kw = crate::solar::predict_wh(p.gti_w_m2, p.temperature_c, k)
                * crate::solar::STEPS_PER_HOUR
                / 1000.0;
            (hour, kw)
        })
        .collect();

    // Only days that are over are compared: today is still accruing, so its
    // actual is not yet a total and would read as a large under-production.
    //
    // And only days whose forecast covers them. Past rows exist only for steps
    // Dom was running for when the covering fetch happened — that is what the
    // freeze rule buys — so a day Dom started partway through has a fraction of a
    // forecast, which against a whole day's production reads as a huge miss.
    // First run at 14:00 would enter the next day at roughly 30% of actual, and
    // one such day is enough to drag a genuine ±8% band towards ±18%.
    let mut steps_per_day: std::collections::HashMap<chrono::NaiveDate, usize> =
        std::collections::HashMap::new();
    for p in &stored {
        *steps_per_day
            .entry(p.valid_at.with_timezone(&chrono::Local).date_naive())
            .or_default() += 1;
    }
    let recent: Vec<_> = predicted
        .iter()
        .filter(|(day, _)| **day < today)
        .filter(|(day, _)| {
            steps_per_day.get(day).copied().unwrap_or(0) >= MIN_FORECAST_STEPS_PER_DAY
        })
        .filter_map(|(day, p)| actual.get(day).map(|a| (*day, *p, *a)))
        .collect();
    let accuracy = crate::solar::Accuracy::from_pairs(
        &recent.iter().map(|(_, p, a)| (*p, *a)).collect::<Vec<_>>(),
    );

    let daylight: Vec<_> = stored
        .iter()
        .filter(|p| {
            p.valid_at.with_timezone(&chrono::Local).date_naive() == tomorrow && p.gti_w_m2 > 20.0
        })
        .collect();
    let tomorrow_cloud_pct = (!daylight.is_empty())
        .then(|| daylight.iter().map(|p| p.cloud_cover_pct).sum::<f64>() / daylight.len() as f64);

    // Today is half over, so its expectation is what has already been produced
    // plus what is still forecast — not the sum of the whole day's rows. Those
    // rows only exist for steps Dom was running for: on a first run, or after any
    // downtime, summing them would quietly report a fraction of the day as though
    // it were the whole of it. Adding the measured part also makes the figure
    // better than a pure forecast as the day goes on.
    let rest_of_today: Vec<_> = stored
        .iter()
        .filter(|p| {
            p.valid_at >= now && p.valid_at.with_timezone(&chrono::Local).date_naive() == today
        })
        .cloned()
        .collect();
    let today_actual_kwh = actual.get(&today).copied().unwrap_or(0.0);

    let mut app = state.write().unwrap();
    let solar = &mut app.solar;
    solar.today_kwh = calibration
        .as_ref()
        .map(|_| crate::solar::expected_for_partial_day(today_actual_kwh, &rest_of_today, k));
    solar.tomorrow_kwh = predicted.get(&tomorrow).copied();
    solar.today_actual_kwh = today_actual_kwh;
    solar.tomorrow_curve = curve;
    solar.tomorrow_cloud_pct = tomorrow_cloud_pct;
    solar.recent = recent;
    solar.accuracy = accuracy;
    solar.calibration = calibration;
}

/// `Config` keys recording when each half of a calibration was last *attempted*,
/// as opposed to when one last succeeded.
///
/// Attempts rather than successes, because a failure leaves nothing stored and a
/// schedule keyed on the stored fit would then retry on every tick. The
/// orientation search is 33 requests; retrying it every half hour against a
/// 10,000-a-day allowance is how an outage turns into a rate-limit ban.
const LAST_FIT_KEY: &str = "solar_last_fit_attempt";
const LAST_SEARCH_KEY: &str = "solar_last_orientation_search";

/// Whether an attempt recorded under `key` is old enough to make another.
async fn due(
    pool: &sqlx::SqlitePool,
    key: &str,
    interval: chrono::Duration,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    match crate::db::get_config(pool, key).await {
        Ok(Some(raw)) => crate::db::parse_timestamp(&raw).is_none_or(|t| now - t >= interval),
        // Never attempted, or the read failed — attempting is the safe answer;
        // the attempt itself is recorded, so a failure cannot loop.
        _ => true,
    }
}

/// Refits the array against recorded production, searching the plane when the
/// stored one is stale or absent.
///
/// Returns the calibration if one was fitted, and `None` when it is not yet due
/// or there is not enough recorded production to fit. Safe to call on every tick:
/// it decides for itself whether anything is owed.
///
/// Costs one request when only the scale is refitted, and one per candidate plane
/// when the orientation is searched too — which is why the two are scheduled
/// separately.
pub async fn calibrate(pool: &sqlx::SqlitePool) -> anyhow::Result<Option<Calibration>> {
    let Some(location) = crate::db::get_location(pool).await? else {
        return Ok(None);
    };
    let now = chrono::Utc::now();
    if !due(
        pool,
        LAST_FIT_KEY,
        chrono::Duration::hours(REFIT_AFTER_HOURS),
        now,
    )
    .await
    {
        return Ok(None);
    }
    // Recorded before the work, not after: an attempt that fails must still push
    // the next one out.
    crate::db::set_config(pool, LAST_FIT_KEY, &crate::devices::ts(now)).await?;
    let today = chrono::Local::now().date_naive();
    let Some(yesterday) = today.pred_opt() else {
        return Ok(None);
    };

    let days = crate::db::query_complete_production_days(
        pool,
        today - chrono::Duration::days(CALIBRATION_WINDOW_DAYS),
        yesterday,
    )
    .await?;
    if days.len() < MIN_DAYS_TO_FIT {
        log::info!(
            "solar calibration deferred: {} complete days of production, {MIN_DAYS_TO_FIT} needed",
            days.len()
        );
        return Ok(None);
    }
    let produced = crate::db::query_production_15min(pool, &days).await?;
    let (first, last) = (days[0], days[days.len() - 1]);

    // The plane is searched on its own, much slower schedule, and keyed on when
    // one was last *attempted* — see `LAST_SEARCH_KEY`. Keying it on the stored
    // fit would mean a run of failed calibrations retried all 33 requests every
    // half hour.
    let previous = crate::db::get_calibration(pool).await?;
    let search_due = due(
        pool,
        LAST_SEARCH_KEY,
        chrono::Duration::days(RESEARCH_ORIENTATION_AFTER_DAYS),
        now,
    )
    .await;
    let plane = match &previous {
        Some(c) if !search_due => (c.tilt_deg, c.azimuth_deg),
        _ => {
            crate::db::set_config(pool, LAST_SEARCH_KEY, &crate::devices::ts(now)).await?;
            // A search that finds nothing keeps whatever plane was already
            // fitted; falling back to due south would throw away a known answer.
            search_orientation(&location, &days, &produced, now)
                .await?
                .or_else(|| previous.as_ref().map(|c| (c.tilt_deg, c.azimuth_deg)))
                .unwrap_or((DEFAULT_TILT_DEG, DEFAULT_AZIMUTH_DEG))
        }
    };

    // Final fit at the chosen plane, over the whole window.
    let weather = fetch_archive(
        location.latitude,
        location.longitude,
        plane.0,
        plane.1,
        first,
        last,
    )
    .await?;
    let Some(fitted) = best_fit(
        [(plane.0, plane.1, weather.as_slice())],
        &produced,
        days.len(),
        now,
    ) else {
        log::warn!("solar calibration found no overlap between forecast history and production");
        return Ok(None);
    };

    crate::db::set_calibration(pool, &fitted).await?;
    log::info!(
        "solar calibrated: {:.0} W peak at {}° tilt / {}° azimuth, from {} days ({} steps), \
         RMSE {:.0} W",
        fitted.peak_w(),
        fitted.tilt_deg,
        fitted.azimuth_deg,
        fitted.days,
        fitted.samples,
        fitted.rmse_w
    );
    Ok(Some(fitted))
}

/// Tries each candidate plane over a recent window and returns the best.
async fn search_orientation(
    location: &crate::db::Location,
    days: &[chrono::NaiveDate],
    produced: &std::collections::HashMap<chrono::DateTime<chrono::Utc>, f64>,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<Option<(i32, i32)>> {
    let window: Vec<_> = days
        .iter()
        .rev()
        .take(ORIENTATION_SEARCH_DAYS as usize)
        .rev()
        .copied()
        .collect();
    let (Some(first), Some(last)) = (window.first(), window.last()) else {
        return Ok(None);
    };

    // Coarse pass over the grid, then a finer pass around whatever won. The
    // coarse grid brackets the answer rather than landing on it — see
    // `solar::refinement_around`.
    let coarse: Vec<_> = crate::solar::TILT_CANDIDATES
        .iter()
        .flat_map(|t| crate::solar::AZIMUTH_CANDIDATES.iter().map(|a| (*t, *a)))
        .collect();
    let Some(best) = try_planes(
        location,
        &coarse,
        produced,
        *first,
        *last,
        window.len(),
        now,
    )
    .await
    else {
        return Ok(None);
    };
    log::info!(
        "solar orientation, coarse pass over {} days: {}° tilt / {}° azimuth",
        window.len(),
        best.tilt_deg,
        best.azimuth_deg
    );

    let around = crate::solar::refinement_around(best.tilt_deg, best.azimuth_deg);
    let refined = try_planes(
        location,
        &around,
        produced,
        *first,
        *last,
        window.len(),
        now,
    )
    .await;

    let chosen = better_plane(best, refined);
    log::info!(
        "solar orientation: {}° tilt / {}° azimuth",
        chosen.tilt_deg,
        chosen.azimuth_deg
    );
    Ok(Some((chosen.tilt_deg, chosen.azimuth_deg)))
}

/// Which of the two search passes to keep: the refinement, but only if it
/// explains the days better than the coarse winner already does.
///
/// Compared on `daily_rmse_kwh`, which is the same criterion `best_fit` uses to
/// pick a winner *within* each pass, and it has to be — `solar::daily_rmse_kwh`
/// records why, with numbers: over the same 44 days, choosing planes by
/// per-step error gave 10.2% held-out daily error against 8.6% for daily error.
/// This comparison used to be on `rmse_w`, so the one decision that was not made
/// by `best_fit` was the one made on the criterion the module rejects.
///
/// It also used to be preceded by a filter reading
/// `daily_rmse_kwh(&[], r.k).is_nan() || r.rmse_w < f64::INFINITY`. That was a
/// no-op wearing the shape of a validity check: `daily_rmse_kwh` of an empty
/// slice returns `INFINITY`, which is not NaN, so the first half was always
/// false and the second half always true for any fit that exists at all.
fn better_plane(coarse: Calibration, refined: Option<Calibration>) -> Calibration {
    match refined {
        Some(r) if r.daily_rmse_kwh < coarse.daily_rmse_kwh => r,
        _ => coarse,
    }
}

/// Fetches each candidate plane in turn and returns the best fit among them.
///
/// A plane that fails to fetch is skipped rather than aborting the pass: the
/// remaining planes still bracket the answer, and a search that gives up on one
/// timeout would leave the array uncalibrated for another week.
async fn try_planes(
    location: &crate::db::Location,
    planes: &[(i32, i32)],
    produced: &std::collections::HashMap<chrono::DateTime<chrono::Utc>, f64>,
    first: chrono::NaiveDate,
    last: chrono::NaiveDate,
    days: usize,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<Calibration> {
    let mut fetched = Vec::new();
    for (tilt, azimuth) in planes {
        match fetch_archive(
            location.latitude,
            location.longitude,
            *tilt,
            *azimuth,
            first,
            last,
        )
        .await
        {
            Ok(points) => fetched.push((*tilt, *azimuth, points)),
            Err(e) => log::warn!("orientation candidate {tilt}/{azimuth} failed: {e:#}"),
        }
        tokio::time::sleep(SEARCH_PACING).await;
    }
    best_fit(
        fetched.iter().map(|(t, a, p)| (*t, *a, p.as_slice())),
        produced,
        days,
        now,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/openmeteo_forecast.json");

    #[test]
    fn parses_a_real_forecast() {
        let points = parse(FIXTURE).unwrap();
        // Two days at four steps an hour.
        assert_eq!(points.len(), 2 * 24 * 4);
        assert!(points.windows(2).all(|w| w[0].valid_at < w[1].valid_at));
        assert!(
            points
                .windows(2)
                .all(|w| { (w[1].valid_at - w[0].valid_at) == chrono::Duration::minutes(15) })
        );
    }

    #[test]
    fn a_real_forecast_is_physically_sane() {
        let points = parse(FIXTURE).unwrap();
        assert!(points.iter().all(|p| (0.0..=1400.0).contains(&p.gti_w_m2)));
        assert!(
            points
                .iter()
                .all(|p| (-40.0..50.0).contains(&p.temperature_c))
        );
        assert!(
            points
                .iter()
                .all(|p| (0.0..=100.0).contains(&p.cloud_cover_pct))
        );
        assert!(points.iter().all(|p| p.precipitation_mm >= 0.0));
        // The sun rises: some step carries real irradiance, and some does not.
        assert!(points.iter().any(|p| p.gti_w_m2 > 100.0));
        assert!(points.iter().any(|p| p.gti_w_m2 == 0.0));
    }

    #[test]
    fn timestamps_are_read_as_utc() {
        // The API is asked for GMT and its timestamps carry no offset, so a
        // mis-set timezone would silently shift every step by an hour or two —
        // enough to move production into the wrong day without looking wrong.
        let points = parse(FIXTURE).unwrap();
        let first = points.first().unwrap().valid_at;
        assert_eq!(first.format("%H:%M").to_string(), "00:00");
        assert_eq!(first.time(), chrono::NaiveTime::MIN);
    }

    #[test]
    fn a_step_the_model_did_not_answer_for_is_dropped() {
        let json = r#"{"minutely_15":{
            "time":["2026-08-22T00:00","2026-08-22T00:15","2026-08-22T00:30"],
            "global_tilted_irradiance":[10.0,null,30.0],
            "temperature_2m":[15.0,15.0,null],
            "cloud_cover":[0.0,0.0,0.0],
            "precipitation":[0.0,0.0,0.0]}}"#;
        let points = parse(json).unwrap();
        assert_eq!(points.len(), 1, "{points:?}");
        assert_eq!(points[0].gti_w_m2, 10.0);
    }

    #[test]
    fn a_negative_horizon_irradiance_is_floored_at_zero() {
        let json = r#"{"minutely_15":{
            "time":["2026-08-22T05:00"],"global_tilted_irradiance":[-0.4],
            "temperature_2m":[9.0],"cloud_cover":[50.0],"precipitation":[0.0]}}"#;
        assert_eq!(parse(json).unwrap()[0].gti_w_m2, 0.0);
    }

    #[test]
    fn a_document_of_the_wrong_shape_is_an_error_not_an_empty_forecast() {
        assert!(parse("not json").is_err());
        assert!(parse(r#"{"hourly":{"time":[]}}"#).is_err());
    }

    #[test]
    fn both_paths_ask_for_the_plane_and_for_utc() {
        let f = forecast_path(47.26, 8.42, 40, 50);
        assert!(f.contains("tilt=40") && f.contains("azimuth=50"), "{f}");
        assert!(f.contains("timezone=GMT"), "{f}");
        assert!(f.contains("global_tilted_irradiance"), "{f}");
        // No model pinned: ICON-CH1 reaches only 33 hours and would truncate
        // tomorrow. See the module docs.
        assert!(!f.contains("models="), "{f}");

        let d = chrono::NaiveDate::from_ymd_opt(2026, 7, 2).unwrap();
        let a = archive_path(47.26, 8.42, 15, -60, d, d.succ_opt().unwrap());
        assert!(a.contains("start_date=2026-07-02"), "{a}");
        assert!(a.contains("end_date=2026-07-03"), "{a}");
        assert!(a.contains("azimuth=-60"), "{a}");
    }

    // ── Pairing and fitting ───────────────────────────────────────────────────

    fn at(minutes: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_756_000_000, 0).unwrap()
            + chrono::Duration::minutes(minutes)
    }

    fn weather(gti: &[f64]) -> Vec<ForecastPoint> {
        gti.iter()
            .enumerate()
            .map(|(i, g)| ForecastPoint {
                valid_at: at(i as i64 * 15),
                gti_w_m2: *g,
                temperature_c: 20.0,
                cloud_cover_pct: 0.0,
                precipitation_mm: 0.0,
            })
            .collect()
    }

    #[test]
    fn a_step_with_no_measurement_is_not_treated_as_zero_production() {
        // The failure this guards: calling an unrecorded sunny step "zero" would
        // teach the fit the array makes nothing in sunshine.
        let w = weather(&[0.0, 400.0, 800.0]);
        let produced = std::collections::HashMap::from([(at(0), 0.0), (at(30), 1_800.0)]);
        let samples = pair_with_production(&w, &produced);
        assert_eq!(samples.len(), 2);
        assert!(samples.iter().all(|s| s.gti_w_m2 != 400.0));
    }

    #[test]
    fn the_search_prefers_the_plane_that_explains_production() {
        // Production generated from the second plane at a known scale; the search
        // is given both and must pick it out, and recover the scale.
        let truth_k = 2.3;
        let wrong = weather(&[0.0, 900.0, 100.0, 500.0]);
        let right = weather(&[0.0, 200.0, 850.0, 400.0]);
        let produced = right
            .iter()
            .map(|p| {
                (
                    p.valid_at,
                    crate::solar::predict_wh(p.gti_w_m2, p.temperature_c, truth_k),
                )
            })
            .collect();

        let fit = best_fit(
            [(30, 0, wrong.as_slice()), (40, 50, right.as_slice())],
            &produced,
            40,
            at(0),
        )
        .unwrap();

        assert_eq!((fit.tilt_deg, fit.azimuth_deg), (40, 50));
        assert!((fit.k - truth_k).abs() < 1e-9, "{fit:?}");
        assert!(fit.rmse_w < 1e-6);
        assert_eq!(fit.days, 40);
        assert_eq!(fit.samples, 4);
    }

    #[test]
    fn no_overlap_between_weather_and_production_yields_no_fit() {
        let w = weather(&[0.0, 500.0]);
        assert!(best_fit([(30, 0, w.as_slice())], &Default::default(), 40, at(0)).is_none());
        // And an empty candidate list is not a fit of zero.
        assert!(best_fit([], &Default::default(), 40, at(0)).is_none());
    }

    // ── Choosing between the two search passes ────────────────────────────────

    /// A calibration that differs only in the two error figures, so a test can
    /// set them against each other.
    fn plane(tilt_deg: i32, rmse_w: f64, daily_rmse_kwh: f64) -> Calibration {
        Calibration {
            k: 0.5,
            tilt_deg,
            azimuth_deg: 0,
            days: 21,
            samples: 400,
            rmse_w,
            daily_rmse_kwh,
            fitted_at: at(0),
        }
    }

    #[test]
    fn the_refinement_is_kept_when_it_explains_the_days_better() {
        let coarse = plane(35, 200.0, 1.4);
        let refined = plane(40, 200.0, 0.9);
        assert_eq!(better_plane(coarse, Some(refined)).tilt_deg, 40);
    }

    #[test]
    fn the_coarse_winner_is_kept_when_the_refinement_does_not_improve_on_it() {
        let coarse = plane(35, 200.0, 0.9);
        let refined = plane(40, 200.0, 1.4);
        assert_eq!(better_plane(coarse, Some(refined)).tilt_deg, 35);
    }

    #[test]
    fn a_refinement_pass_that_fetched_nothing_leaves_the_coarse_winner_alone() {
        let coarse = plane(35, 200.0, 0.9);
        assert_eq!(better_plane(coarse, None).tilt_deg, 35);
    }

    #[test]
    fn the_two_passes_are_compared_on_daily_error_not_per_step_error() {
        // The regression. `solar::daily_rmse_kwh` and SPECS.md both record that
        // the two criteria disagree and that daily error is the one that
        // predicts days better — 8.6% against 10.2% over the same 44 days. This
        // comparison was made on `rmse_w`, so a refinement with the better
        // daily total was rejected for having noisier individual steps.
        let coarse = plane(35, 150.0, 1.4);
        let refined = plane(40, 260.0, 0.9);
        assert!(
            refined.rmse_w > coarse.rmse_w,
            "the refinement is worse per step",
        );
        assert_eq!(
            better_plane(coarse, Some(refined)).tilt_deg,
            40,
            "and better per day, which is what decides",
        );
    }
}
