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

//! The solar forecast, end to end against a real database: record production,
//! record a forecast, calibrate against it, and predict.

mod common;

use chrono::{Duration, Local, NaiveDate, TimeZone, Utc};
use dom::{db, online::forecast, solar};
use sqlx::SqlitePool;

/// The array these tests pretend to have: 9 kW at standard test conditions, so
/// `k` is 9000 / 1000 / 4 steps per hour.
const TRUE_K: f64 = 2.25;

async fn battery(pool: &SqlitePool) -> i64 {
    let ip = "172.16.20.8".parse().unwrap();
    dom::devices::sonnen_batterie::save_device(pool, ip, "battery", None)
        .await
        .unwrap();
    dom::devices::sonnen_batterie::load_all(pool).await.unwrap()[0].id
}

/// The instant a local day starts, in UTC.
///
/// Everything here is anchored to local midnight rather than to UTC midnight,
/// because that is what the completeness screens group by — a fixture written
/// across a UTC day covers only 22 or 23 hours of the local one, and would fail
/// the screen for a reason that has nothing to do with what is being tested.
fn local_midnight(day: NaiveDate) -> chrono::DateTime<Utc> {
    Local
        .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .unwrap()
        .with_timezone(&Utc)
}

/// A day-shaped irradiance curve: zero at night, a smooth arc across the middle.
fn gti_at(step_of_day: usize) -> f64 {
    let hour = step_of_day as f64 / 4.0;
    if !(6.0..20.0).contains(&hour) {
        return 0.0;
    }
    let x = (hour - 13.0) / 7.0;
    (950.0 * (1.0 - x * x)).max(0.0)
}

/// Writes a whole day of per-minute production consistent with `gti_at`, as the
/// rollup would have. Full `span_secs`, so it passes the completeness screens.
async fn record_production_day(pool: &SqlitePool, device_id: i64, day: NaiveDate) {
    for step in 0..96 {
        let gti = gti_at(step);
        let step_wh = solar::predict_wh(gti, 20.0, TRUE_K);
        for m in 0..15 {
            let at = local_midnight(day) + Duration::minutes((step * 15 + m) as i64);
            sqlx::query(
                "INSERT INTO EnergyMinute
                     (device_id, minute, metric, energy_ws, energy_ws_pos, energy_ws_neg,
                      span_secs, peak_w)
                 VALUES (?, ?, 'production', ?, ?, 0, 60, ?)",
            )
            .bind(device_id)
            .bind(at.format("%Y-%m-%d %H:%M:00").to_string())
            .bind(step_wh / 15.0 * 3600.0)
            .bind(step_wh / 15.0 * 3600.0)
            .bind(step_wh * 4.0)
            .execute(pool)
            .await
            .unwrap();
        }
    }
}

/// The weather that produced that day, as the forecast would have delivered it.
fn weather_for(day: NaiveDate) -> Vec<solar::ForecastPoint> {
    (0..96)
        .map(|step| solar::ForecastPoint {
            valid_at: local_midnight(day) + Duration::minutes(step as i64 * 15),
            gti_w_m2: gti_at(step),
            temperature_c: 20.0,
            cloud_cover_pct: 10.0,
            precipitation_mm: 0.0,
        })
        .collect()
}

#[tokio::test]
async fn production_is_read_back_as_fifteen_minute_steps() {
    let pool = common::db().await;
    let id = battery(&pool).await;
    let day = NaiveDate::from_ymd_opt(2026, 7, 2).unwrap();
    record_production_day(&pool, id, day).await;

    let days = db::query_complete_production_days(&pool, day, day)
        .await
        .unwrap();
    let steps = db::query_production_15min(&pool, &days).await.unwrap();

    // A whole day, at four steps an hour — allowing for the local-day window
    // covering part of the neighbouring UTC days.
    assert_eq!(days, vec![day], "a complete day should qualify");
    assert_eq!(steps.len(), 96, "a whole day at four steps an hour");

    // Energy is preserved: the steps sum to what the minutes held.
    let from_steps: f64 = steps.values().sum();
    let from_minutes: f64 = sqlx::query_scalar::<_, f64>(
        "SELECT SUM(energy_ws) / 3600.0 FROM EnergyMinute WHERE metric = 'production'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        (from_steps - from_minutes).abs() / from_minutes < 0.05,
        "{from_steps} vs {from_minutes}"
    );
}

#[tokio::test]
async fn an_incomplete_day_is_not_calibrated_against() {
    let pool = common::db().await;
    let id = battery(&pool).await;
    let day = NaiveDate::from_ymd_opt(2026, 7, 2).unwrap();
    record_production_day(&pool, id, day).await;

    // Take out four hours from the middle of the day, as a stalled poll loop would.
    let gap_from = local_midnight(day) + Duration::hours(10);
    sqlx::query("DELETE FROM EnergyMinute WHERE minute >= ? AND minute < ?")
        .bind(gap_from.format("%Y-%m-%d %H:%M:00").to_string())
        .bind(
            (gap_from + Duration::hours(4))
                .format("%Y-%m-%d %H:%M:00")
                .to_string(),
        )
        .execute(&pool)
        .await
        .unwrap();

    let days = db::query_complete_production_days(&pool, day, day)
        .await
        .unwrap();
    assert!(
        days.is_empty(),
        "a day with a four-hour hole must not qualify"
    );
}

#[tokio::test]
async fn a_partly_recorded_step_is_dropped_but_its_neighbours_are_kept() {
    let pool = common::db().await;
    let id = battery(&pool).await;
    let day = NaiveDate::from_ymd_opt(2026, 7, 2).unwrap();
    record_production_day(&pool, id, day).await;

    let days = db::query_complete_production_days(&pool, day, day)
        .await
        .unwrap();
    let before = db::query_production_15min(&pool, &days).await.unwrap();

    // One minute inside the midday step recorded a single sample — the signature
    // of a poll-loop gap. That step goes; the rest of the day stays.
    let dropped = local_midnight(day) + Duration::hours(12);
    sqlx::query("UPDATE EnergyMinute SET span_secs = 2 WHERE minute = ?")
        .bind(
            (dropped + Duration::minutes(7))
                .format("%Y-%m-%d %H:%M:00")
                .to_string(),
        )
        .execute(&pool)
        .await
        .unwrap();
    let after = db::query_production_15min(&pool, &days).await.unwrap();

    assert!(!before.is_empty());
    assert_eq!(after.len(), before.len() - 1, "exactly one step should go");
    assert!(before.contains_key(&dropped) && !after.contains_key(&dropped));
}

#[tokio::test]
async fn calibrating_against_recorded_production_recovers_the_array() {
    let pool = common::db().await;
    let id = battery(&pool).await;
    let days: Vec<_> = (2..=17)
        .map(|d| NaiveDate::from_ymd_opt(2026, 7, d).unwrap())
        .collect();
    for day in &days {
        record_production_day(&pool, id, *day).await;
    }

    let complete = db::query_complete_production_days(&pool, days[0], *days.last().unwrap())
        .await
        .unwrap();
    let produced = db::query_production_15min(&pool, &complete).await.unwrap();
    let weather: Vec<_> = complete.iter().flat_map(|d| weather_for(*d)).collect();

    let fit = forecast::best_fit(
        [(35, 0, weather.as_slice())],
        &produced,
        complete.len(),
        Utc::now(),
    )
    .expect("a fit");

    // The array was never named to the fit; it comes back out of the production.
    assert!((fit.k - TRUE_K).abs() < 1e-6, "recovered {}", fit.k);
    assert!((fit.peak_w() - 9_000.0).abs() < 1.0, "{} W", fit.peak_w());
    assert!(fit.daily_rmse_kwh < 0.01, "{}", fit.daily_rmse_kwh);
}

#[tokio::test]
async fn a_calibration_survives_a_round_trip() {
    let pool = common::db().await;
    let fitted = solar::Calibration {
        k: 2.296_278_723_820_724,
        tilt_deg: 35,
        azimuth_deg: 60,
        days: 44,
        samples: 4096,
        rmse_w: 1_196.768_865_846_388_7,
        daily_rmse_kwh: 4.284_830_638_944_775,
        fitted_at: Utc.with_ymd_and_hms(2026, 8, 22, 9, 45, 31).unwrap(),
    };
    assert_eq!(db::get_calibration(&pool).await.unwrap(), None);
    db::set_calibration(&pool, &fitted).await.unwrap();
    assert_eq!(db::get_calibration(&pool).await.unwrap(), Some(fitted));

    // Half a calibration is not a calibration: a missing key reads as none
    // rather than as a fit with defaults filled in.
    sqlx::query("DELETE FROM Config WHERE key = 'solar_k'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(db::get_calibration(&pool).await.unwrap(), None);
}

#[tokio::test]
async fn a_forecast_is_never_rewritten_once_its_time_has_passed() {
    let pool = common::db().await;
    let day = NaiveDate::from_ymd_opt(2026, 7, 2).unwrap();
    let noon = local_midnight(day) + Duration::hours(12);

    // Issued in the morning, for the whole day.
    let morning = weather_for(day);
    db::insert_forecast(
        &pool,
        &morning,
        noon - Duration::hours(6),
        noon - Duration::hours(6),
    )
    .await
    .unwrap();

    // Reissued at noon, now claiming twice the irradiance everywhere.
    let revised: Vec<_> = morning
        .iter()
        .map(|p| solar::ForecastPoint {
            gti_w_m2: p.gti_w_m2 * 2.0,
            ..p.clone()
        })
        .collect();
    db::insert_forecast(&pool, &revised, noon, noon)
        .await
        .unwrap();

    let stored = db::query_forecast(&pool, day, day).await.unwrap();
    let by_time: std::collections::HashMap<_, _> =
        stored.iter().map(|p| (p.valid_at, p.gti_w_m2)).collect();

    // Before noon the morning's forecast still stands — that is what makes the
    // forecast-versus-actual comparison a comparison of forecasts.
    let before = local_midnight(day) + Duration::hours(10);
    let after = local_midnight(day) + Duration::hours(14);
    assert_eq!(by_time[&before], gti_at(40));
    assert_eq!(by_time[&after], gti_at(56) * 2.0);
}

#[tokio::test]
async fn a_predicted_day_can_be_compared_with_what_was_produced() {
    let pool = common::db().await;
    let id = battery(&pool).await;
    let day = NaiveDate::from_ymd_opt(2026, 7, 2).unwrap();
    record_production_day(&pool, id, day).await;
    let issued = local_midnight(day) - Duration::hours(12);
    db::insert_forecast(&pool, &weather_for(day), issued, issued)
        .await
        .unwrap();

    // The daily total, as the rollup would have left it. Written directly: the
    // rollup reads the 2s series, which this test does not fabricate, and it is
    // covered by its own tests in `tests/db.rs`.
    let produced_wh: f64 = sqlx::query_scalar(
        "SELECT SUM(energy_ws) / 3600.0 FROM EnergyMinute WHERE metric = 'production'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO EnergyDaily (device_id, day, metric, energy_wh)
         VALUES (?, ?, 'production', ?)",
    )
    .bind(id)
    .bind(day.to_string())
    .bind(produced_wh)
    .execute(&pool)
    .await
    .unwrap();

    let stored = db::query_forecast(&pool, day, day).await.unwrap();
    let predicted = solar::daily_kwh(&stored, TRUE_K);
    let actual = db::query_daily_production_kwh(&pool, day, day)
        .await
        .unwrap();

    let (p, a) = (predicted[&day], actual[&day]);
    assert!(p > 20.0, "the day should predict real production, got {p}");
    assert!((p - a).abs() / a < 0.05, "predicted {p}, produced {a}");

    let accuracy = solar::Accuracy::from_pairs(&[(p, a)]);
    assert_eq!(accuracy.within_10pct, 1);
    assert!(!accuracy.is_meaningful(), "one day is not enough to quote");
}

#[tokio::test]
async fn a_day_whose_forecast_only_covers_part_of_it_is_not_scored() {
    // The trap this guards: forecast rows exist only for steps Dom was running
    // for, so a day it started partway through holds a fraction of a forecast.
    // Compared against that day's whole production it reads as a huge miss, and
    // one such day is enough to drag the error band the view shows well off.
    let pool = common::db().await;
    let id = battery(&pool).await;
    let state = dom::app::new_shared();

    let today = Local::now().date_naive();
    let full = today - Duration::days(2);
    let partial = today - Duration::days(1);

    for day in [full, partial] {
        record_production_day(&pool, id, day).await;
        let produced_wh = solar::daily_kwh(&weather_for(day), TRUE_K)
            .values()
            .sum::<f64>()
            * 1000.0;
        sqlx::query(
            "INSERT INTO EnergyDaily (device_id, day, metric, energy_wh)
             VALUES (?, ?, 'production', ?)",
        )
        .bind(id)
        .bind(day.to_string())
        .bind(produced_wh)
        .execute(&pool)
        .await
        .unwrap();
    }

    // One day forecast in full; the next only from midday, as a restart leaves it.
    let issued = local_midnight(full) - Duration::hours(12);
    db::insert_forecast(&pool, &weather_for(full), issued, issued)
        .await
        .unwrap();
    let midday = local_midnight(partial) + Duration::hours(12);
    db::insert_forecast(&pool, &weather_for(partial), midday, midday)
        .await
        .unwrap();

    let calibration = solar::Calibration {
        k: TRUE_K,
        tilt_deg: 35,
        azimuth_deg: 0,
        days: 44,
        samples: 4096,
        rmse_w: 100.0,
        daily_rmse_kwh: 1.0,
        fitted_at: Utc::now(),
    };
    forecast::republish(&pool, &state, Some(calibration), Utc::now()).await;

    let app = state.read().unwrap();
    let days: Vec<_> = app.solar.recent.iter().map(|(d, _, _)| *d).collect();
    assert_eq!(
        days,
        vec![full],
        "only the fully forecast day is comparable"
    );

    // And the day that is scored is scored correctly.
    let (_, predicted, actual) = app.solar.recent[0];
    assert!(
        (predicted - actual).abs() / actual < 0.01,
        "{predicted} vs {actual}"
    );
}
