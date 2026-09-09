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

//! The active/standby role decision for a two-node Dom cluster.
//!
//! See SPECS.md, "High availability: a two-node active/standby cluster", for the full design and
//! the reasoning behind every rule below. This module is only the pure decision at the heart of
//! it — [`decide_role`] takes what one node currently knows and returns what it should do next —
//! deliberately kept free of any I/O so every branch can be exercised directly, the same way
//! `main::elapsed_window` and `devices::keba::eco_decision` are.
//!
//! Two nodes with no third arbiter cannot, in general, tell "my peer has failed" apart from "I
//! cannot reach my peer" — this module does not attempt to. It leans instead on a fact specific
//! to this application: every actuation Dom performs is an idempotent level-command ("relay on",
//! "set current to N"), not a transaction, so two nodes both acting is harmless and only
//! *disagreement* is a hazard. The rules below exist to avoid disagreement, and to fail toward
//! [`Role::Paused`] — measuring continues, nothing actuates — whenever they cannot be sure.

/// What this node should currently be doing.
///
/// `Paused` is not "broken" — it is the deliberately safe answer when a node cannot confirm it is
/// safe to actuate. Whether measurement continues while paused is the caller's concern, not this
/// module's: `decide_role` only ever says what to do about *actuation*.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Role {
    #[default]
    Active,
    Standby,
    Paused,
}

/// What is known about the peer node, from this node's own point of view.
///
/// `NotPaired` is not a degenerate case of `Unreachable` — a node with no configured peer has
/// nothing to be ambiguous about, and is exactly today's single-node install. Keeping it as its
/// own variant is what makes solo mode `decide_role`'s trivial, unconditional first branch rather
/// than something that falls out of the other rules by coincidence.
///
/// `Establishing` is similarly not a degenerate case of `Unreachable`, even though both mean "no
/// current claim from the peer is available" — collapsing them would reintroduce a real
/// split-brain window: two nodes that boot together (or recover from a shared power outage) start
/// with no heartbeat history at all, and if that read as `Unreachable`, `decide_role`'s
/// unconditional takeover rule would make *both* claim `Active` before either has heard from the
/// other. `Establishing` instead defers to the static primary/backup designation, so only one
/// side ever claims `Active` during that window. `Unreachable` is reserved for a peer that *was*
/// reachable and is now confirmed gone — the real failover trigger, which must not wait on that
/// designation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerStatus {
    NotPaired,
    /// Configured, but no heartbeat has ever succeeded yet — see the type-level doc above.
    Establishing,
    /// Was reachable at some point; consecutive heartbeat failures have now crossed the caller's
    /// tolerance. Distinct from `Establishing` — see the type-level doc above.
    Unreachable,
    /// The peer is reachable, and last said whether *it* currently considers itself active.
    Reachable {
        claims_active: bool,
    },
}

/// One request in a heartbeat exchange — sent by the node initiating contact, whether that's an
/// ordinary paired heartbeat or an unpaired discovery probe (see `main::heartbeat_once`, which
/// makes both the same call). `node_id` is informational only, for the listener's own logs — the
/// listener never authenticates the requester (see the module-level "unpinned verification" note
/// on [`verify_reply`]); `nonce` is what matters: fresh, unpredictable per call, and echoed back
/// inside the signed reply so a captured old reply can never be replayed to pass as a fresh one.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct HeartbeatRequest {
    pub node_id: String,
    /// Hex-encoded random bytes, generated fresh by the requester for this call only.
    pub nonce: String,
}

/// The signed reply to a [`HeartbeatRequest`] — the replying node's identity and current role
/// claim, bound to the request that prompted it. Plain data, not I/O: the actual TCP exchange
/// lives in `main.rs`; build one with [`sign_reply`], check one with [`verify_reply`].
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct HeartbeatReply {
    pub node_id: String,
    /// Hex-encoded Ed25519 public key of the replying node — what a discovery probe surfaces to
    /// be pinned, and what an ongoing heartbeat checks against the pin already on file.
    pub public_key: String,
    pub claims_active: bool,
    /// Echoed straight from the `HeartbeatRequest` that prompted this reply.
    pub request_nonce: String,
    /// Hex-encoded Ed25519 signature over [`signable_bytes`] of every field above.
    pub signature: String,
}

/// The exact bytes a [`HeartbeatReply`] signs and a verifier reconstructs — length-prefixed
/// concatenation of each field, deliberately not `serde_json::to_vec`. JSON's byte encoding is an
/// implementation detail of `serde_json` (field order, escaping, whitespace), not a contract; the
/// moment a field is added or a serde version changes it, "the same JSON bytes" quietly stops
/// being the same bytes, and a signature scheme that relied on that fails in a way nothing catches
/// at compile time. Explicit length prefixes (rather than plain concatenation) prevent two
/// different field splits from producing the same bytes (`"ab"+"c"` vs `"a"+"bc"`).
fn signable_bytes(
    node_id: &str,
    public_key: &str,
    claims_active: bool,
    request_nonce: &str,
) -> Vec<u8> {
    fn push_field(buf: &mut Vec<u8>, field: &[u8]) {
        buf.extend_from_slice(&(field.len() as u32).to_be_bytes());
        buf.extend_from_slice(field);
    }
    let mut buf = Vec::new();
    push_field(&mut buf, node_id.as_bytes());
    push_field(&mut buf, public_key.as_bytes());
    buf.push(u8::from(claims_active));
    push_field(&mut buf, request_nonce.as_bytes());
    buf
}

pub(crate) fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

pub(crate) fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// This node's own hex-encoded Ed25519 public key, derived from its persistent keypair
/// (`db::get_or_create_cluster_keypair`) — what gets sent out in every `HeartbeatReply`, and what
/// a human pins when they confirm pairing.
#[must_use]
pub fn public_key_hex(keypair: &ring::signature::Ed25519KeyPair) -> String {
    use ring::signature::KeyPair;
    to_hex(keypair.public_key().as_ref())
}

/// A fresh random nonce for one heartbeat exchange — see `HeartbeatRequest`'s doc for why this
/// has to be unpredictable and different every call, not just present.
#[must_use]
pub fn random_nonce_hex() -> String {
    let bytes: [u8; 16] = rand::random();
    to_hex(&bytes)
}

/// Builds and signs a reply to a request that carried `request_nonce`, using this node's own
/// persistent keypair and current `node_id`/`claims_active`.
#[must_use]
pub fn sign_reply(
    keypair: &ring::signature::Ed25519KeyPair,
    node_id: &str,
    claims_active: bool,
    request_nonce: &str,
) -> HeartbeatReply {
    let public_key = public_key_hex(keypair);
    let bytes = signable_bytes(node_id, &public_key, claims_active, request_nonce);
    let signature = to_hex(keypair.sign(&bytes).as_ref());
    HeartbeatReply {
        node_id: node_id.to_string(),
        public_key,
        claims_active,
        request_nonce: request_nonce.to_string(),
        signature,
    }
}

/// Checks that `reply` really was signed by the private key matching its own `public_key` field,
/// over exactly the fields it claims. This is an *internal consistency* check only — it proves
/// the replier holds some private key matching what it claims, not that it is the intended peer.
/// Real trust starts only once a human pins a specific `public_key` (see SPECS.md); an unpinned
/// discovery probe calling this and getting `true` back has confirmed nothing more than "this is
/// a Dom instance that isn't lying about its own key," which is expected of literally anything
/// running this code, malicious or not.
#[must_use]
pub fn verify_reply(reply: &HeartbeatReply) -> bool {
    let (Some(public_key), Some(signature)) =
        (from_hex(&reply.public_key), from_hex(&reply.signature))
    else {
        return false;
    };
    let bytes = signable_bytes(
        &reply.node_id,
        &reply.public_key,
        reply.claims_active,
        &reply.request_nonce,
    );
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &public_key)
        .verify(&bytes, &signature)
        .is_ok()
}

/// A condition worth telling a person about, via `alarm::AlarmSink`.
///
/// `SplitBrainDetected` is distinct from the other two: it means something has *already* gone
/// slightly wrong (both nodes briefly agreed they were active) rather than a currently-safe
/// precaution against something that might. Both nodes involved raise it, even the one that keeps
/// `Role::Active` — a human should learn it happened even though it self-corrected.
///
/// `PeerIdentityMismatch` means a heartbeat reply came back internally valid (see
/// [`verify_reply`]) but signed by a different key than the one pinned for this peer — most
/// plausibly a peer that was reinstalled or factory-reset and generated a new keypair, not
/// necessarily anything hostile. Treated as a heartbeat failure for role-decision purposes (never
/// trust an unpinned claim), but raised as its own alarm rather than folded silently into
/// `PeerUnreachable`, because "the peer answers but isn't who I pinned" needs a human to look and
/// decide whether to re-pair — see `main::cluster_task`'s handling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlarmCondition {
    GatewayUnreachable,
    PeerUnreachable,
    SplitBrainDetected,
    PeerIdentityMismatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    pub role: Role,
    pub alarm: Option<AlarmCondition>,
}

/// Decides this node's role for the next tick.
///
/// - `currently_active`: this node's own last-known role (`Role::Active` or not) — failback is
///   sticky (see SPECS.md), so the state machine needs to know what it was doing, not just what
///   it can currently observe.
/// - `self_is_designated_primary`: the static priority used only to break ties — an election with
///   no other information (a fresh pairing, or both nodes recovering from a gateway outage
///   together) and, separately, to decide which side yields in a detected split brain.
/// - `gateway_reachable`: whether this node can currently reach its own LAN gateway. This is the
///   partition discriminator: a node that cannot reach the gateway cannot tell "peer is down" from
///   "I am the one cut off", so it must not guess either way.
/// - `peer`: what is known about the peer — see [`PeerStatus`].
///
/// Rule order (see SPECS.md for the reasoning behind each):
/// 1. No peer configured at all → always `Active`, never alarms. This is every existing
///    single-node install, unchanged.
/// 2. Gateway unreachable → `Paused` + `GatewayUnreachable`, regardless of anything else known
///    about the peer — an unreachable gateway makes every other input unreliable.
/// 3. Gateway reachable, peer still establishing (no heartbeat has ever succeeded) → the
///    designated primary claims `Active`, the other stays `Standby`. No alarm — this is ordinary
///    startup, not a fault. Gated by priority specifically so two nodes booting together never
///    both claim `Active` — see `PeerStatus::Establishing`'s doc.
/// 4. Gateway reachable, peer confirmed unreachable (was reachable, heartbeats have since failed
///    past the caller's tolerance) → `Active` + `PeerUnreachable`, **unconditionally** — this is
///    the real failover trigger, and applies the same whether this node is primary or backup: it
///    covers both "just took over" and "still covering for a peer that has been down for a
///    while" identically, and is worth alarming on even for the primary, since it means there is
///    currently no backup either.
/// 5. Gateway reachable, peer reachable:
///    - both sides currently believe they are active → split brain. The lower-priority node
///      (`!self_is_designated_primary`) yields to `Standby`; the higher-priority one stays
///      `Active`. Both raise `SplitBrainDetected`.
///    - neither side currently believes it is active (a fresh election, or both recovering from a
///      shared gateway outage together) → the designated primary claims `Active`, the other stays
///      `Standby`. No alarm: this is ordinary operation, not a fault.
///    - otherwise the two sides already disagree in the ordinary way (one active, one standby) —
///      stay exactly as `currently_active` says. No alarm: this is the sticky-failback case
///      working as intended.
#[must_use]
pub fn decide_role(
    currently_active: bool,
    self_is_designated_primary: bool,
    gateway_reachable: bool,
    peer: PeerStatus,
) -> Decision {
    if peer == PeerStatus::NotPaired {
        return Decision {
            role: Role::Active,
            alarm: None,
        };
    }
    if !gateway_reachable {
        return Decision {
            role: Role::Paused,
            alarm: Some(AlarmCondition::GatewayUnreachable),
        };
    }
    if peer == PeerStatus::Establishing {
        let role = if self_is_designated_primary {
            Role::Active
        } else {
            Role::Standby
        };
        return Decision { role, alarm: None };
    }
    let PeerStatus::Reachable { claims_active } = peer else {
        // Only `Unreachable` remains at this point.
        return Decision {
            role: Role::Active,
            alarm: Some(AlarmCondition::PeerUnreachable),
        };
    };

    if currently_active && claims_active {
        let role = if self_is_designated_primary {
            Role::Active
        } else {
            Role::Standby
        };
        return Decision {
            role,
            alarm: Some(AlarmCondition::SplitBrainDetected),
        };
    }
    if !currently_active && !claims_active {
        let role = if self_is_designated_primary {
            Role::Active
        } else {
            Role::Standby
        };
        return Decision { role, alarm: None };
    }
    Decision {
        role: if currently_active {
            Role::Active
        } else {
            Role::Standby
        },
        alarm: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_keypair() -> ring::signature::Ed25519KeyPair {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
    }

    #[test]
    fn hex_round_trips_through_arbitrary_bytes() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(from_hex(&to_hex(&bytes)), Some(bytes));
        assert_eq!(from_hex(""), Some(Vec::new()));
        assert_eq!(from_hex("abc"), None, "odd length is not valid hex");
        assert_eq!(from_hex("zz"), None, "non-hex characters");
    }

    #[test]
    fn a_reply_verifies_against_its_own_signature() {
        let keypair = test_keypair();
        let reply = sign_reply(&keypair, "node-a", true, "deadbeef");
        assert!(verify_reply(&reply));
        assert_eq!(reply.public_key, public_key_hex(&keypair));
    }

    #[test]
    fn tampering_with_any_field_breaks_verification() {
        let keypair = test_keypair();
        let original = sign_reply(&keypair, "node-a", true, "deadbeef");

        let mut wrong_claim = original.clone();
        wrong_claim.claims_active = false;
        assert!(!verify_reply(&wrong_claim), "flipped claims_active");

        let mut wrong_node = original.clone();
        wrong_node.node_id = "node-b".to_string();
        assert!(!verify_reply(&wrong_node), "swapped node_id");

        let mut wrong_nonce = original.clone();
        wrong_nonce.request_nonce = "0000".to_string();
        assert!(!verify_reply(&wrong_nonce), "swapped request_nonce");

        let mut wrong_key = original.clone();
        wrong_key.public_key = public_key_hex(&test_keypair());
        assert!(
            !verify_reply(&wrong_key),
            "claiming a different key entirely"
        );

        let mut malformed = original;
        malformed.signature = "not-hex".to_string();
        assert!(
            !verify_reply(&malformed),
            "malformed signature is rejected, not panicked on"
        );
    }

    #[test]
    fn a_signature_from_one_key_does_not_verify_under_another() {
        let signer = test_keypair();
        let mut reply = sign_reply(&signer, "node-a", true, "deadbeef");
        // Claim a different key's identity while keeping the original signature.
        reply.public_key = public_key_hex(&test_keypair());
        assert!(!verify_reply(&reply));
    }

    #[test]
    fn no_peer_configured_is_always_active_and_never_alarms() {
        for currently_active in [true, false] {
            for primary in [true, false] {
                for gateway in [true, false] {
                    let d = decide_role(currently_active, primary, gateway, PeerStatus::NotPaired);
                    assert_eq!(
                        d,
                        Decision {
                            role: Role::Active,
                            alarm: None
                        },
                        "currently_active={currently_active} primary={primary} gateway={gateway}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_unreachable_gateway_always_pauses_regardless_of_the_peer() {
        for peer in [
            PeerStatus::Establishing,
            PeerStatus::Unreachable,
            PeerStatus::Reachable {
                claims_active: true,
            },
            PeerStatus::Reachable {
                claims_active: false,
            },
        ] {
            for currently_active in [true, false] {
                for primary in [true, false] {
                    let d = decide_role(currently_active, primary, false, peer);
                    assert_eq!(
                        d,
                        Decision {
                            role: Role::Paused,
                            alarm: Some(AlarmCondition::GatewayUnreachable)
                        },
                        "peer={peer:?} currently_active={currently_active} primary={primary}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_unreachable_peer_with_a_reachable_gateway_takes_over() {
        for currently_active in [true, false] {
            for primary in [true, false] {
                let d = decide_role(currently_active, primary, true, PeerStatus::Unreachable);
                assert_eq!(
                    d,
                    Decision {
                        role: Role::Active,
                        alarm: Some(AlarmCondition::PeerUnreachable)
                    },
                    "currently_active={currently_active} primary={primary}"
                );
            }
        }
    }

    #[test]
    fn an_establishing_peer_defers_to_the_static_priority_without_alarming() {
        for currently_active in [true, false] {
            assert_eq!(
                decide_role(currently_active, true, true, PeerStatus::Establishing),
                Decision {
                    role: Role::Active,
                    alarm: None
                },
                "the designated primary claims active while still establishing contact, currently_active={currently_active}"
            );
            assert_eq!(
                decide_role(currently_active, false, true, PeerStatus::Establishing),
                Decision {
                    role: Role::Standby,
                    alarm: None
                },
                "the non-primary stays standby while still establishing contact, currently_active={currently_active}"
            );
        }
    }

    /// The cold-start bug this module exists to prevent: two nodes booting together (or
    /// recovering from a shared power outage) have no heartbeat history yet, so both see
    /// `PeerStatus::Establishing`. If that state were treated the same as a confirmed-gone peer
    /// (`Unreachable`, which unconditionally claims `Active`), both nodes would claim `Active`
    /// simultaneously. Simulating exactly that boot — both sides `currently_active: false`, both
    /// seeing `Establishing` — must never produce two `Active`s.
    #[test]
    fn simultaneous_cold_start_never_produces_two_active_nodes() {
        let primary = decide_role(false, true, true, PeerStatus::Establishing);
        let backup = decide_role(false, false, true, PeerStatus::Establishing);
        assert!(
            !(primary.role == Role::Active && backup.role == Role::Active),
            "primary={primary:?} backup={backup:?}"
        );
        assert_eq!(primary.role, Role::Active);
        assert_eq!(backup.role, Role::Standby);
    }

    #[test]
    fn the_ordinary_sticky_case_holds_its_role_and_does_not_alarm() {
        // I am active, peer (rightly) says it is not.
        assert_eq!(
            decide_role(
                true,
                false,
                true,
                PeerStatus::Reachable {
                    claims_active: false
                }
            ),
            Decision {
                role: Role::Active,
                alarm: None
            }
        );
        // I am standby, peer (rightly) says it is active.
        assert_eq!(
            decide_role(
                false,
                true,
                true,
                PeerStatus::Reachable {
                    claims_active: true
                }
            ),
            Decision {
                role: Role::Standby,
                alarm: None
            }
        );
    }

    #[test]
    fn a_fresh_election_is_broken_by_static_priority_without_alarming() {
        let peer = PeerStatus::Reachable {
            claims_active: false,
        };
        assert_eq!(
            decide_role(false, true, true, peer),
            Decision {
                role: Role::Active,
                alarm: None
            },
            "the designated primary claims active"
        );
        assert_eq!(
            decide_role(false, false, true, peer),
            Decision {
                role: Role::Standby,
                alarm: None
            },
            "the non-primary yields"
        );
    }

    #[test]
    fn a_detected_split_brain_yields_the_lower_priority_side_and_both_alarm() {
        let peer = PeerStatus::Reachable {
            claims_active: true,
        };
        assert_eq!(
            decide_role(true, true, true, peer),
            Decision {
                role: Role::Active,
                alarm: Some(AlarmCondition::SplitBrainDetected)
            },
            "the designated primary keeps acting, but still alarms"
        );
        assert_eq!(
            decide_role(true, false, true, peer),
            Decision {
                role: Role::Standby,
                alarm: Some(AlarmCondition::SplitBrainDetected)
            },
            "the non-primary yields, and alarms"
        );
    }
}
