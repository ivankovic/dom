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

//! Turning a street address into coordinates, via swisstopo's search API.
//!
//! `api3.geo.admin.ch` is the Swiss federal geodata service: no key, no account,
//! and authoritative for Swiss addresses — it resolves to individual buildings
//! from the official address register.
//!
//! Coordinates are requested in **LV95** (EPSG:2056) as well as WGS84, because
//! MeteoSwiss publishes its station positions in LV95. Asking for them directly
//! avoids reprojecting, and LV95 is metric, so distance between two points is
//! plain Pythagoras — see `weather::nearest_station`.

use anyhow::{Context, bail};
use serde::Deserialize;

const HOST: &str = "api3.geo.admin.ch";

/// A resolved location.
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    /// What the service matched, for the user to confirm — e.g.
    /// "Bundesplatz 3 3011 Bern". Markup stripped.
    pub label: String,
    /// LV95 easting, metres.
    pub east: f64,
    /// LV95 northing, metres.
    pub north: f64,
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Deserialize)]
struct SearchResponse {
    results: Vec<SearchResult>,
}

#[derive(Deserialize)]
struct SearchResult {
    attrs: Attrs,
}

#[derive(Deserialize)]
struct Attrs {
    label: String,
    lat: f64,
    lon: f64,
    /// With `sr=2056` this is the **northing**, not the easting — the service
    /// reports LV95 as (x = north, y = east), which is the reverse of the
    /// (east, north) ordering MeteoSwiss uses. Named for what it holds.
    x: f64,
    /// LV95 easting. See `x`.
    y: f64,
}

/// Removes the `<b>...</b>` emphasis swisstopo puts around the matched part of a
/// label, so it can be shown in a terminal.
fn strip_markup(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut in_tag = false;
    for c in label.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parses a search response, returning the best match.
///
/// The service orders results by its own relevance weight, so the first is the
/// best match; a full street address resolves to the exact building. Separate from
/// the request so it can be tested against a captured response.
pub fn parse_best_match(json: &str) -> anyhow::Result<Place> {
    let parsed: SearchResponse = serde_json::from_str(json).context("parsing search response")?;
    let Some(first) = parsed.results.into_iter().next() else {
        bail!("no location matched");
    };
    let a = first.attrs;
    Ok(Place {
        label: strip_markup(&a.label),
        // Note the crossover: the service's x is the northing.
        east: a.y,
        north: a.x,
        latitude: a.lat,
        longitude: a.lon,
    })
}

/// Looks up `address` and returns the best match.
pub async fn lookup(address: &str) -> anyhow::Result<Place> {
    let path = format!(
        "/rest/services/api/SearchServer?searchText={}&type=locations&sr=2056&limit=5",
        urlencode(address)
    );
    let body = super::https::get(HOST, &path).await?;
    parse_best_match(&body)
}

/// Percent-encodes everything outside the unreserved set, so an address with
/// spaces, commas or umlauts survives being put in a query string.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/swisstopo_search.json");

    #[test]
    fn parses_a_real_response() {
        let p = parse_best_match(FIXTURE).unwrap();
        assert_eq!(p.label, "Bundesplatz 3 3011 Bern");
        assert!((p.latitude - 46.9468).abs() < 0.01, "{p:?}");
        assert!((p.longitude - 7.4442).abs() < 0.01, "{p:?}");
    }

    #[test]
    fn lv95_easting_and_northing_are_not_transposed() {
        // The service reports LV95 as (x = north, y = east), the reverse of the
        // ordering MeteoSwiss uses. Getting this backwards still parses and still
        // produces plausible-looking numbers, so it is asserted directly: in
        // LV95 the easting of anywhere in Switzerland is ~2.5-2.8 million and the
        // northing ~1.07-1.3 million, so the two are never confusable by range.
        let p = parse_best_match(FIXTURE).unwrap();
        assert!(
            (2_480_000.0..2_840_000.0).contains(&p.east),
            "east {} out of range for Switzerland",
            p.east
        );
        assert!(
            (1_070_000.0..1_300_000.0).contains(&p.north),
            "north {} out of range for Switzerland",
            p.north
        );
    }

    #[test]
    fn strips_the_emphasis_markup_from_labels() {
        assert_eq!(
            strip_markup("Bundesplatz 3 <b>3011 Bern</b>"),
            "Bundesplatz 3 3011 Bern"
        );
        assert_eq!(strip_markup("<i>x</i>  y"), "x y");
        assert_eq!(strip_markup("plain"), "plain");
    }

    #[test]
    fn reports_no_match_rather_than_guessing() {
        let err = parse_best_match(r#"{"results":[]}"#).unwrap_err();
        assert!(err.to_string().contains("no location matched"), "{err}");
    }

    #[test]
    fn rejects_a_response_that_is_not_the_expected_shape() {
        assert!(parse_best_match("not json").is_err());
        assert!(parse_best_match(r#"{"unexpected":1}"#).is_err());
    }

    #[test]
    fn urlencodes_addresses_with_spaces_and_umlauts() {
        assert_eq!(urlencode("Bundesplatz 3 Bern"), "Bundesplatz+3+Bern");
        // Umlauts must survive as UTF-8 percent escapes, or Swiss place names
        // like Zürich and Grindelwaldstrasse fail to match.
        assert_eq!(urlencode("Zürich"), "Z%C3%BCrich");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
    }
}
