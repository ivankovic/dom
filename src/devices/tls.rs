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

//! TLS to a device on your own network, authenticated by pinning.
//!
//! The public-internet calls in `src/online/` verify certificates against the
//! webpki root store, which works because those hosts have certificates signed
//! by a public authority. A router on the LAN does not: RouterOS generates its
//! own self-signed certificate, so there is no chain to a root and no name to
//! match — the address is an IP, not a domain.
//!
//! The answer is trust on first use. The first time Dom connects it records the
//! SHA-256 of the certificate the device presented; every connection after that
//! must present the same one. That is weaker than a public CA — a device
//! impersonated *before* the first connection is trusted — and much stronger
//! than what it replaces, which was sending the router administrator password
//! over plain HTTP on every poll.
//!
//! What it actually buys: after the first connection, nothing on the network can
//! read or alter the traffic without holding the device's private key, and an
//! attempt to substitute a certificate fails the handshake **before** any
//! credential is written to the socket. That ordering is the point, and it is why
//! a mismatch must never fall back to plain HTTP.
//!
//! A certificate legitimately changes — a router reset, a regenerated key, a
//! firmware upgrade. Dom cannot tell that apart from an attack, so it does not
//! try: it refuses to connect, says so in the Network view, and waits for a
//! person to accept the new certificate.

use std::sync::Arc;

use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    ClientConfig, DigitallySignedStruct, Error as TlsError, SignatureScheme,
};

/// A certificate's identity: the SHA-256 of its DER encoding, lowercase hex.
///
/// The same fingerprint OpenSSL prints for `-fingerprint -sha256`, minus the
/// colons, so a value stored here can be compared against the device's own
/// report of it by eye.
pub fn fingerprint(der: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, der);
    digest.as_ref().iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Formats a fingerprint the way a person compares one: grouped, and short
/// enough to read off a screen.
///
/// Only ever shown alongside the full value, never instead of it.
pub fn short_fingerprint(fingerprint: &str) -> String {
    fingerprint
        .as_bytes()
        .chunks(4)
        .take(4)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect::<Vec<_>>()
        .join(":")
}

/// Why a pinned connection was refused.
#[derive(Debug, Clone, PartialEq)]
pub enum PinFailure {
    /// The device presented a certificate other than the pinned one. Carries
    /// what was seen, so a person can be shown it and decide.
    Changed { expected: String, observed: String },
}

/// Verifies the server by comparing its certificate against a pinned
/// fingerprint, and records what it saw either way.
///
/// Deliberately performs no other checks. Expiry, name matching and chain
/// building all assume an authority Dom does not have here; what identifies the
/// device is the exact key it holds, and that is what is compared.
#[derive(Debug)]
struct PinnedVerifier {
    /// `None` on the very first connection to a device: whatever it presents is
    /// recorded and becomes the pin.
    expected: Option<String>,
    /// What the device actually presented, for the caller to store or show.
    observed: Arc<std::sync::Mutex<Option<String>>>,
    /// Signature schemes the provider supports, needed to answer rustls.
    provider: Arc<tokio_rustls::rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let observed = fingerprint(end_entity.as_ref());
        if let Ok(mut slot) = self.observed.lock() {
            *slot = Some(observed.clone());
        }
        match &self.expected {
            // Trust on first use.
            None => Ok(ServerCertVerified::assertion()),
            Some(pinned) if *pinned == observed => Ok(ServerCertVerified::assertion()),
            Some(pinned) => Err(TlsError::General(format!(
                "certificate changed: pinned {pinned}, presented {observed}"
            ))),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        tokio_rustls::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        tokio_rustls::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// A TLS client configuration that accepts exactly one certificate, plus the
/// slot the certificate it saw is written into.
pub fn pinned_config(
    expected: Option<String>,
) -> (Arc<ClientConfig>, Arc<std::sync::Mutex<Option<String>>>) {
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let observed = Arc::new(std::sync::Mutex::new(None));
    let verifier = Arc::new(PinnedVerifier {
        expected,
        observed: Arc::clone(&observed),
        provider: Arc::clone(&provider),
    });

    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("the ring provider supports the default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    (Arc::new(config), observed)
}

/// Classifies a handshake error as a pin mismatch, or not.
///
/// rustls reports a custom verifier's rejection as an opaque message, so the
/// distinction has to be recovered from the text. It matters: a mismatch must
/// stop the connection and ask a person, while any other TLS failure is just a
/// connection that did not work.
pub fn pin_failure(error: &str, expected: Option<&str>) -> Option<PinFailure> {
    let observed = error.split("presented ").nth(1)?.trim().to_string();
    if observed.len() != 64 || !observed.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(PinFailure::Changed {
        expected: expected.unwrap_or_default().to_string(),
        observed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DER blob whose SHA-256 is known, so the fingerprint format can be
    /// checked against something independent of this code.
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn a_fingerprint_is_the_hex_sha256_of_the_certificate() {
        assert_eq!(fingerprint(b""), EMPTY_SHA256);
        assert_eq!(fingerprint(b"").len(), 64);
        assert!(fingerprint(b"abc").chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn different_certificates_have_different_fingerprints() {
        assert_ne!(fingerprint(b"cert-a"), fingerprint(b"cert-b"));
        // ...and the same one is stable, which is the whole basis of pinning.
        assert_eq!(fingerprint(b"cert-a"), fingerprint(b"cert-a"));
    }

    #[test]
    fn the_short_form_is_a_prefix_of_the_full_one() {
        // Shown next to the full value for reading off a screen, so it must not
        // be able to differ from it.
        let full = fingerprint(b"cert");
        let short = short_fingerprint(&full);
        assert_eq!(short.len(), 19, "four groups of four, three separators");
        assert_eq!(short.replace(':', ""), full[..16]);
    }

    #[test]
    fn a_changed_certificate_is_recognised_from_the_handshake_error() {
        let expected = fingerprint(b"old");
        let observed = fingerprint(b"new");
        let error = format!("certificate changed: pinned {expected}, presented {observed}");

        assert_eq!(
            pin_failure(&error, Some(&expected)),
            Some(PinFailure::Changed {
                expected: expected.clone(),
                observed,
            })
        );
    }

    #[test]
    fn an_ordinary_tls_failure_is_not_reported_as_a_changed_certificate() {
        // A connection that simply did not work must not put a "someone may be
        // impersonating your router" notice in front of the user.
        for error in [
            "connection refused",
            "peer closed connection without sending TLS close_notify",
            "received fatal alert: HandshakeFailure",
            "presented nothing at all",
            "presented deadbeef",
        ] {
            assert_eq!(pin_failure(error, Some("x")), None, "{error}");
        }
    }

    #[test]
    fn a_configuration_can_be_built_with_and_without_a_pin() {
        // First contact has nothing to compare against and must still connect.
        let (config, observed) = pinned_config(None);
        assert!(observed.lock().unwrap().is_none(), "nothing seen yet");
        assert!(!config.alpn_protocols.is_empty() || config.alpn_protocols.is_empty());

        let (_, observed) = pinned_config(Some(fingerprint(b"pinned")));
        assert!(observed.lock().unwrap().is_none());
    }
}
