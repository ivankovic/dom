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

//! Outbound calls to public online services.
//!
//! Everything else Dom talks to is on the LAN. These two are not, which makes
//! them the only places the app reaches the internet:
//!
//! - [`geocode`] turns an address into coordinates, via swisstopo's federal
//!   search API.
//! - [`weather`] reads outdoor temperature from MeteoSwiss open data.
//!
//! Both are plain unauthenticated GETs over TLS, and both are optional: with no
//! location configured, nothing here is ever called.

pub mod geocode;
pub mod https;
pub mod weather;
