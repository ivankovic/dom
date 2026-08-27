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

//! Outdoor temperature from MeteoSwiss open data.
//!
//! `data.geo.admin.ch` publishes the current 10-minute mean air temperature for
//! every station in SwissMetNet, the federal automatic monitoring network, as one
//! JSON document refreshed every ten minutes. It is Open Government Data: free of
//! charge, machine-readable, and reusable with attribution — which is what makes
//! it usable here regardless of what Dom itself is licensed under.
//!
//! The document covers the whole country, so the work here is picking the station
//! nearest the configured location. Station positions are LV95 (EPSG:2056), the
//! same frame `geocode` asks for, so "nearest" is Pythagoras in metres.
//!
//! What this gives is a real measurement rather than a model — but from a station
//! that may be some distance away and at a very different altitude. Both are
//! reported alongside the reading so the number is never presented as though it
//! were measured at the house.

use anyhow::{Context, bail};
use serde::Deserialize;

const HOST: &str = "data.geo.admin.ch";
const PATH: &str = "/ch.meteoschweiz.messwerte-lufttemperatur-10min/\
                    ch.meteoschweiz.messwerte-lufttemperatur-10min_en.json";

/// One SwissMetNet station's current reading.
#[derive(Debug, Clone, PartialEq)]
pub struct Station {
    /// Three-letter station abbreviation, e.g. "BER".
    pub id: String,
    pub name: String,
    /// Air temperature, 10-minute mean, in degrees Celsius.
    pub temperature_c: f64,
    /// Station altitude in metres. Worth showing: a reading from 1880 m means
    /// something different from one at 550 m.
    pub altitude_m: f64,
    /// LV95 easting, metres.
    pub east: f64,
    /// LV95 northing, metres.
    pub north: f64,
    /// When the station measured it.
    pub measured_at: chrono::DateTime<chrono::Utc>,
}

/// A reading, with how far away it was taken.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub station: Station,
    /// Straight-line distance from the configured location, in kilometres.
    pub distance_km: f64,
}

#[derive(Deserialize)]
struct FeatureCollection {
    features: Vec<Feature>,
}

#[derive(Deserialize)]
struct Feature {
    id: String,
    geometry: Geometry,
    properties: Properties,
}

#[derive(Deserialize)]
struct Geometry {
    /// `[easting, northing]` in LV95 — the opposite order to what swisstopo's
    /// search API reports. See `geocode::Attrs`.
    coordinates: [f64; 2],
}

#[derive(Deserialize)]
struct Properties {
    station_name: String,
    /// Absent or null when a station is not currently reporting.
    value: Option<f64>,
    /// A string in the published data, not a number.
    altitude: Option<String>,
    reference_ts: Option<String>,
}

/// Parses the published document into the stations that are currently reporting.
///
/// Stations with no value are dropped rather than defaulted: a station that is
/// down must not be selectable as the nearest one and then report 0 °C.
pub fn parse_stations(json: &str) -> anyhow::Result<Vec<Station>> {
    let parsed: FeatureCollection =
        serde_json::from_str(json).context("parsing MeteoSwiss station data")?;
    let mut out = Vec::with_capacity(parsed.features.len());
    for f in parsed.features {
        let Some(temperature_c) = f.properties.value else {
            continue;
        };
        let Some(ts) = f.properties.reference_ts.as_deref() else {
            continue;
        };
        let Ok(measured_at) = chrono::DateTime::parse_from_rfc3339(ts) else {
            continue;
        };
        out.push(Station {
            id: f.id,
            name: f.properties.station_name,
            temperature_c,
            altitude_m: f
                .properties
                .altitude
                .as_deref()
                .and_then(|a| a.trim().parse().ok())
                .unwrap_or(f64::NAN),
            east: f.geometry.coordinates[0],
            north: f.geometry.coordinates[1],
            measured_at: measured_at.with_timezone(&chrono::Utc),
        });
    }
    Ok(out)
}

/// The station closest to an LV95 position, with its distance in kilometres.
///
/// LV95 is a metric projection, so Euclidean distance is metres on the ground —
/// no great-circle arithmetic needed. Over the span of Switzerland the projection
/// error is far smaller than the distance to the nearest station.
pub fn nearest_station(stations: &[Station], east: f64, north: f64) -> Option<Observation> {
    stations
        .iter()
        .map(|s| {
            let d = ((s.east - east).powi(2) + (s.north - north).powi(2)).sqrt();
            (s, d)
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(s, d)| Observation {
            station: s.clone(),
            distance_km: d / 1000.0,
        })
}

/// Fetches the current readings and returns the one nearest the given position.
pub async fn fetch_nearest(east: f64, north: f64) -> anyhow::Result<Observation> {
    let body = super::https::get(HOST, PATH).await?;
    let stations = parse_stations(&body)?;
    if stations.is_empty() {
        bail!("no MeteoSwiss station is currently reporting a temperature");
    }
    nearest_station(&stations, east, north).context("could not determine the nearest station")
}

/// Fetches the outdoor temperature for the configured location, records it, and
/// publishes it into shared state.
///
/// The one function here that touches the database and the UI state; everything
/// above it is pure parsing and arithmetic. Called both by the periodic task and
/// immediately after the user sets an address, so a new location shows a reading
/// without waiting for the next tick.
///
/// Does nothing when no location is configured — this is the only part of Dom that
/// reaches the internet, and it stays idle unless asked.
pub async fn refresh(pool: &sqlx::SqlitePool, state: &crate::app::SharedState) {
    let location = match crate::db::get_location(pool).await {
        Ok(Some(loc)) => loc,
        Ok(None) => {
            state.write().unwrap().location = None;
            return;
        }
        Err(e) => {
            log::warn!("reading the configured location failed: {e:#}");
            return;
        }
    };
    state.write().unwrap().location = Some(location.clone());

    match fetch_nearest(location.east, location.north).await {
        Ok(obs) => {
            let _ = crate::db::insert_outdoor_temperature(
                pool,
                obs.station.measured_at,
                &obs.station.id,
                &obs.station.name,
                obs.station.temperature_c,
                obs.station.altitude_m,
                obs.distance_km,
            )
            .await;
            let today = chrono::Local::now().date_naive();
            let range = crate::db::outdoor_range_for_day(pool, today)
                .await
                .unwrap_or(None);

            let mut app = state.write().unwrap();
            app.outdoor = Some(crate::app::OutdoorReading {
                station_name: obs.station.name,
                temperature_c: obs.station.temperature_c,
                altitude_m: obs.station.altitude_m,
                distance_km: obs.distance_km,
                measured_at: obs.station.measured_at,
            });
            app.outdoor_today = range;
            app.last_weather_error = None;
        }
        Err(e) => {
            log::warn!("fetching the outdoor temperature failed: {e:#}");
            state.write().unwrap().last_weather_error = Some(format!("{e:#}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/meteoswiss_temperature_10min.json");

    #[test]
    fn parses_real_station_data() {
        let s = parse_stations(FIXTURE).unwrap();
        assert!(!s.is_empty());
        let arosa = s.iter().find(|s| s.id == "ARO").expect("ARO in fixture");
        assert_eq!(arosa.name, "Arosa");
        assert!((arosa.altitude_m - 1880.0).abs() < 0.5, "{arosa:?}");
        // Swiss air temperature is never anywhere near these bounds.
        assert!((-50.0..50.0).contains(&arosa.temperature_c), "{arosa:?}");
    }

    #[test]
    fn nearest_station_to_bern_is_the_bern_station() {
        // The test that catches an easting/northing transposition. Swapping them
        // still yields a valid-looking coordinate and a confident answer — just
        // the wrong station, on the far side of the country.
        let s = parse_stations(FIXTURE).unwrap();
        // Bundesplatz, Bern, in LV95.
        let obs = nearest_station(&s, 2_600_423.0, 1_199_521.0).unwrap();
        assert_eq!(obs.station.id, "BER", "picked {:?}", obs.station);
        assert!(obs.distance_km < 15.0, "Bern's station is close: {obs:?}");
    }

    #[test]
    fn nearest_station_to_lugano_is_the_lugano_station() {
        let s = parse_stations(FIXTURE).unwrap();
        // Lugano, LV95.
        let obs = nearest_station(&s, 2_717_874.0, 1_095_884.0).unwrap();
        assert_eq!(obs.station.id, "LUG", "picked {:?}", obs.station);
    }

    #[test]
    fn distance_is_reported_in_kilometres() {
        let s = parse_stations(FIXTURE).unwrap();
        let here = s.iter().find(|s| s.id == "BER").unwrap();
        // Exactly 5 km east of the station.
        let obs = nearest_station(&s, here.east + 5000.0, here.north).unwrap();
        assert_eq!(obs.station.id, "BER");
        assert!((obs.distance_km - 5.0).abs() < 0.001, "{obs:?}");
    }

    #[test]
    fn stations_without_a_reading_are_dropped() {
        // A station that is down must not become the nearest one and then report
        // a temperature of zero.
        let json = r#"{"features":[
            {"id":"AAA","geometry":{"coordinates":[2600000.0,1200000.0]},
             "properties":{"station_name":"Down","value":null,"altitude":"500.00",
                           "reference_ts":"2026-08-18T15:50:00Z"}},
            {"id":"BBB","geometry":{"coordinates":[2700000.0,1200000.0]},
             "properties":{"station_name":"Up","value":21.0,"altitude":"600.00",
                           "reference_ts":"2026-08-18T15:50:00Z"}}
        ]}"#;
        let s = parse_stations(json).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].id, "BBB");
        // Nearest to the down station's own position is still the working one.
        let obs = nearest_station(&s, 2_600_000.0, 1_200_000.0).unwrap();
        assert_eq!(obs.station.id, "BBB");
    }

    #[test]
    fn stations_with_an_unparseable_timestamp_are_dropped() {
        let json = r#"{"features":[
            {"id":"AAA","geometry":{"coordinates":[2600000.0,1200000.0]},
             "properties":{"station_name":"Bad ts","value":21.0,"altitude":"500.00",
                           "reference_ts":"not a timestamp"}}
        ]}"#;
        assert!(parse_stations(json).unwrap().is_empty());
    }

    #[test]
    fn no_stations_yields_no_observation() {
        assert!(nearest_station(&[], 2_600_000.0, 1_200_000.0).is_none());
    }

    #[test]
    fn rejects_a_document_that_is_not_the_expected_shape() {
        assert!(parse_stations("not json").is_err());
        assert!(parse_stations(r#"{"nope":1}"#).is_err());
    }
}
