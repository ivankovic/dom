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

//! Outbound calls to public online services.
//!
//! Everything else Dom talks to is on the LAN. These three are not, which makes
//! them the only places the app reaches the internet:
//!
//! - [`geocode`] turns an address into coordinates, via swisstopo's federal
//!   search API.
//! - [`weather`] reads outdoor temperature from MeteoSwiss open data.
//! - [`forecast`] reads the solar-irradiance forecast that predicted production
//!   is computed from, via Open-Meteo.
//!
//! All three are plain unauthenticated GETs over TLS, and all three are
//! optional: with no location configured, nothing here is ever called.
//!
//! Note that [`forecast`] is the one whose terms bind a *licensee* rather than
//! just this program — Open-Meteo's free tier is non-commercial. See SPECS.md.

pub mod forecast;
pub mod geocode;
pub mod https;
pub mod weather;
