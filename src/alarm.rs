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

//! Escalating a `cluster::AlarmCondition` to a person, when pausing automation on its own would
//! otherwise pass silently.
//!
//! Nothing else in Dom alerts a human to anything today — the README lists Household/alarm
//! devices as an explicitly unimplemented category — so this is a new concern, not an extension
//! of an existing one. [`AlarmSink`] exists so *how* that alert reaches a person is a separate,
//! swappable decision from the cluster logic that decides *whether* one is owed; `cluster::
//! decide_role` never touches a sink directly, it only returns a `Decision` for the caller to act
//! on.
//!
//! The one implementation shipped here, [`LogAlarm`], is deliberately the least capable one
//! possible: every node has it, with no hardware and no configuration. A Raspberry Pi's GPIO
//! driving an LED is the natural next `AlarmSink` — local, and needing no other system to be
//! working — but it needs real hardware to verify against and is not implemented yet (see
//! SPECS.md). A cloud or push-notification sink is deliberately *not* planned as the only
//! channel, on any node: the conditions this module reports are frequently exactly "the network
//! is the problem", and the node most in need of alerting is often the one that cannot reach
//! anything external to say so.

use crate::cluster::AlarmCondition;

/// Something that can tell a person about a `cluster::AlarmCondition`, and tell them again once
/// it has cleared.
///
/// `clear` is a distinct call rather than `raise` simply not being called again: a sink driving
/// physical hardware (an LED) needs to be told explicitly to turn back off, not left to infer it
/// from silence.
pub trait AlarmSink {
    fn raise(&self, condition: AlarmCondition);
    fn clear(&self);
}

/// The always-available floor: writes to the log every other background task already writes to.
/// `SplitBrainDetected` logs at `error!` — it means two nodes already briefly disagreed, not a
/// currently-safe precaution — the other two conditions log at `warn!`.
pub struct LogAlarm;

impl AlarmSink for LogAlarm {
    fn raise(&self, condition: AlarmCondition) {
        match condition {
            AlarmCondition::SplitBrainDetected => {
                log::error!("cluster alarm: {condition:?}");
            }
            AlarmCondition::GatewayUnreachable
            | AlarmCondition::PeerUnreachable
            | AlarmCondition::PeerIdentityMismatch => {
                log::warn!("cluster alarm: {condition:?}");
            }
        }
    }

    fn clear(&self) {
        log::info!("cluster alarm cleared");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raising_and_clearing_every_condition_does_not_panic() {
        let sink = LogAlarm;
        for condition in [
            AlarmCondition::GatewayUnreachable,
            AlarmCondition::PeerUnreachable,
            AlarmCondition::SplitBrainDetected,
            AlarmCondition::PeerIdentityMismatch,
        ] {
            sink.raise(condition);
            sink.clear();
        }
    }
}
