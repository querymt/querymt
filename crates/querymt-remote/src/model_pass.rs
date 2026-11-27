//! Model passes: a prompting peer lends its own model access to one session host.
//!
//! A pass carries no authority of its own. It tells the model node to evaluate
//! the *issuer's* current grant for a call made by the named host. It is signed
//! with the issuer's mesh identity key under a signing domain of its own, so a
//! pass, a trust token and a membership token cannot be replayed as one another.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Default lifetime of an issued pass. There is no renewal in v1.
pub const DEFAULT_MODEL_PASS_TTL_SECS: u64 = 3600;
/// Longest lifetime a verifier accepts, whatever the issuer signed.
pub const MAX_MODEL_PASS_TTL_SECS: u64 = 24 * 3600;
/// Tolerated clock difference between issuer and verifier for `issued_at`.
pub const MODEL_PASS_CLOCK_SKEW_SECS: u64 = 300;

#[cfg(feature = "kameo-mesh")]
const SIGNING_DOMAIN: &[u8] = b"querymt/model-pass/v1\0";
#[cfg(feature = "kameo-mesh")]
const PASS_VERSION: u8 = 1;
#[cfg(feature = "kameo-mesh")]
const MAX_FIELD_LEN: usize = 1024;

/// Why a pass was refused. None of these allow falling back to another grant.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ModelPassError {
    #[error("unsupported model pass version {0}")]
    UnsupportedVersion(u8),
    #[error("invalid model pass: {0}")]
    Malformed(String),
    #[error("model pass signature is invalid")]
    BadSignature,
    #[error("model pass has expired")]
    Expired,
    #[error("model pass is not yet valid")]
    NotYetValid,
    #[error("model pass names a different {0}")]
    Mismatch(&'static str),
}

/// What a verifier expects of a pass, beyond its signature and validity period.
/// A `None` field is not checked. The model node does not know the session.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelPassExpectation<'a> {
    /// The peer that submitted the request carrying the pass.
    pub issuer: Option<&'a str>,
    /// The transport-authenticated sender, which the pass must name as audience.
    pub audience: Option<&'a str>,
    pub model_node: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
    pub session_id: Option<&'a str>,
}

/// A signed statement by `issuer` that `audience` may use one model for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPass {
    pub version: u8,
    /// The prompting peer whose access is lent.
    pub issuer: String,
    /// The session host that may present the pass.
    pub audience: String,
    /// The node serving the model.
    pub model_node: String,
    pub provider: String,
    pub model: String,
    pub session_id: String,
    pub issued_at: u64,
    pub expires_at: u64,
    /// Hex-encoded signature by `issuer`.
    pub signature: String,
}

/// The fields a pass is issued for.
#[derive(Debug, Clone, Copy)]
pub struct ModelPassClaim<'a> {
    pub audience: &'a str,
    pub model_node: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub session_id: &'a str,
}

#[cfg(feature = "kameo-mesh")]
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(feature = "kameo-mesh")]
fn push_field(buf: &mut Vec<u8>, value: &str) {
    buf.extend_from_slice(&(value.len() as u32).to_be_bytes());
    buf.extend_from_slice(value.as_bytes());
}

impl ModelPass {
    /// The bytes that are signed. Every field is length-prefixed so adjacent
    /// fields cannot be shifted into one another.
    #[cfg(feature = "kameo-mesh")]
    fn signable_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(SIGNING_DOMAIN);
        buf.push(self.version);
        push_field(&mut buf, &self.issuer);
        push_field(&mut buf, &self.audience);
        push_field(&mut buf, &self.model_node);
        push_field(&mut buf, &self.provider);
        push_field(&mut buf, &self.model);
        push_field(&mut buf, &self.session_id);
        buf.extend_from_slice(&self.issued_at.to_be_bytes());
        buf.extend_from_slice(&self.expires_at.to_be_bytes());
        buf
    }

    /// Check the shape of the pass without verifying the signature.
    #[cfg(feature = "kameo-mesh")]
    fn check_shape(&self) -> Result<(), ModelPassError> {
        if self.version != PASS_VERSION {
            return Err(ModelPassError::UnsupportedVersion(self.version));
        }
        for (name, value) in [
            ("issuer", &self.issuer),
            ("audience", &self.audience),
            ("model_node", &self.model_node),
            ("provider", &self.provider),
            ("model", &self.model),
            ("session_id", &self.session_id),
        ] {
            if value.is_empty() || value.len() > MAX_FIELD_LEN {
                return Err(ModelPassError::Malformed(format!("invalid {name}")));
            }
        }
        if self.expires_at <= self.issued_at {
            return Err(ModelPassError::Malformed(
                "a pass must expire after it is issued".into(),
            ));
        }
        if self.expires_at - self.issued_at > MAX_MODEL_PASS_TTL_SECS {
            return Err(ModelPassError::Malformed("lifetime is too long".into()));
        }
        Ok(())
    }

    /// Whether the pass has expired at `now` (unix seconds).
    pub fn is_expired_at(&self, now: u64) -> bool {
        now >= self.expires_at
    }

    /// Check the signature, validity period and every field in `expect`.
    /// `now` is unix seconds on the verifier's clock.
    #[cfg(feature = "kameo-mesh")]
    pub fn verify(
        &self,
        now: u64,
        expect: &ModelPassExpectation<'_>,
    ) -> Result<(), ModelPassError> {
        self.check_shape()?;

        let issuer: libp2p::PeerId = self
            .issuer
            .parse()
            .map_err(|e| ModelPassError::Malformed(format!("invalid issuer: {e}")))?;
        let public_key = libp2p::identity::PublicKey::try_decode_protobuf(&issuer.to_bytes()[2..])
            .map_err(|_| ModelPassError::BadSignature)?;
        let signature = hex::decode(&self.signature).map_err(|_| ModelPassError::BadSignature)?;
        if !public_key.verify(&self.signable_bytes(), &signature) {
            return Err(ModelPassError::BadSignature);
        }

        if self.is_expired_at(now) {
            return Err(ModelPassError::Expired);
        }
        if self.issued_at > now.saturating_add(MODEL_PASS_CLOCK_SKEW_SECS) {
            return Err(ModelPassError::NotYetValid);
        }

        for (name, want, have) in [
            ("issuer", expect.issuer, &self.issuer),
            ("audience", expect.audience, &self.audience),
            ("model node", expect.model_node, &self.model_node),
            ("provider", expect.provider, &self.provider),
            ("model", expect.model, &self.model),
            ("session", expect.session_id, &self.session_id),
        ] {
            if want.is_some_and(|want| want != have) {
                return Err(ModelPassError::Mismatch(name));
            }
        }
        Ok(())
    }

    /// Sign a pass as `issuer` for `claim`, valid for `ttl_secs` from `now`.
    #[cfg(feature = "kameo-mesh")]
    pub fn issue(
        issuer: &libp2p::identity::Keypair,
        claim: ModelPassClaim<'_>,
        now: u64,
        ttl_secs: u64,
    ) -> Result<Self, ModelPassError> {
        let mut pass = Self {
            version: PASS_VERSION,
            issuer: issuer.public().to_peer_id().to_string(),
            audience: claim.audience.to_string(),
            model_node: claim.model_node.to_string(),
            provider: claim.provider.to_string(),
            model: claim.model.to_string(),
            session_id: claim.session_id.to_string(),
            issued_at: now,
            expires_at: now.saturating_add(ttl_secs),
            signature: String::new(),
        };
        pass.check_shape()?;
        let signature = issuer
            .sign(&pass.signable_bytes())
            .map_err(|e| ModelPassError::Malformed(format!("signing failed: {e}")))?;
        pass.signature = hex::encode(signature);
        Ok(pass)
    }

    /// Sign a pass valid for [`DEFAULT_MODEL_PASS_TTL_SECS`] from the current time.
    #[cfg(feature = "kameo-mesh")]
    pub fn issue_now(
        issuer: &libp2p::identity::Keypair,
        claim: ModelPassClaim<'_>,
    ) -> Result<Self, ModelPassError> {
        Self::issue(issuer, claim, now_secs(), DEFAULT_MODEL_PASS_TTL_SECS)
    }

    /// Verify against the current wall clock.
    #[cfg(feature = "kameo-mesh")]
    pub fn verify_now(&self, expect: &ModelPassExpectation<'_>) -> Result<(), ModelPassError> {
        self.verify(now_secs(), expect)
    }
}

#[cfg(all(test, feature = "kameo-mesh"))]
mod tests {
    use super::*;
    use libp2p::identity::Keypair;

    struct Fixture {
        issuer: Keypair,
        host: String,
        node: String,
    }

    fn fixture() -> Fixture {
        Fixture {
            issuer: Keypair::generate_ed25519(),
            host: Keypair::generate_ed25519()
                .public()
                .to_peer_id()
                .to_string(),
            node: Keypair::generate_ed25519()
                .public()
                .to_peer_id()
                .to_string(),
        }
    }

    fn claim<'a>(f: &'a Fixture) -> ModelPassClaim<'a> {
        ModelPassClaim {
            audience: &f.host,
            model_node: &f.node,
            provider: "example",
            model: "model-1",
            session_id: "session-1",
        }
    }

    fn expect<'a>(f: &'a Fixture, issuer: &'a str) -> ModelPassExpectation<'a> {
        ModelPassExpectation {
            issuer: Some(issuer),
            audience: Some(&f.host),
            model_node: Some(&f.node),
            provider: Some("example"),
            model: Some("model-1"),
            session_id: None,
        }
    }

    #[test]
    fn model_pass_admits_only_what_it_names() {
        let f = fixture();
        let issuer_id = f.issuer.public().to_peer_id().to_string();
        let pass = ModelPass::issue(&f.issuer, claim(&f), 1_000, 600).unwrap();
        pass.verify(1_100, &expect(&f, &issuer_id)).unwrap();

        // Each named field must match.
        let other = Keypair::generate_ed25519()
            .public()
            .to_peer_id()
            .to_string();
        let mut e = expect(&f, &issuer_id);
        e.audience = Some(&other);
        assert_eq!(
            pass.verify(1_100, &e),
            Err(ModelPassError::Mismatch("audience"))
        );
        let mut e = expect(&f, &issuer_id);
        e.model_node = Some(&other);
        assert_eq!(
            pass.verify(1_100, &e),
            Err(ModelPassError::Mismatch("model node"))
        );
        let mut e = expect(&f, &issuer_id);
        e.provider = Some("other");
        assert_eq!(
            pass.verify(1_100, &e),
            Err(ModelPassError::Mismatch("provider"))
        );
        let mut e = expect(&f, &issuer_id);
        e.model = Some("model-2");
        assert_eq!(
            pass.verify(1_100, &e),
            Err(ModelPassError::Mismatch("model"))
        );
        let mut e = expect(&f, &issuer_id);
        e.session_id = Some("session-2");
        assert_eq!(
            pass.verify(1_100, &e),
            Err(ModelPassError::Mismatch("session"))
        );
        // A pass issued by someone else than the submitter is refused.
        let mut e = expect(&f, &issuer_id);
        e.issuer = Some(&other);
        assert_eq!(
            pass.verify(1_100, &e),
            Err(ModelPassError::Mismatch("issuer"))
        );

        // Expiry is exclusive and checked on the verifier's clock.
        assert_eq!(
            pass.verify(1_600, &expect(&f, &issuer_id)),
            Err(ModelPassError::Expired)
        );
        pass.verify(1_599, &expect(&f, &issuer_id)).unwrap();
        assert_eq!(
            pass.verify(
                1_000 - MODEL_PASS_CLOCK_SKEW_SECS - 1,
                &expect(&f, &issuer_id)
            ),
            Err(ModelPassError::NotYetValid)
        );
    }

    #[test]
    fn model_pass_rejects_altered_and_forged_passes() {
        let f = fixture();
        let issuer_id = f.issuer.public().to_peer_id().to_string();
        let pass = ModelPass::issue(&f.issuer, claim(&f), 1_000, 600).unwrap();

        for tamper in [
            |p: &mut ModelPass| p.model = "model-2".into(),
            |p: &mut ModelPass| p.provider = "other".into(),
            |p: &mut ModelPass| p.session_id = "session-2".into(),
            |p: &mut ModelPass| p.audience = "x".into(),
            |p: &mut ModelPass| p.expires_at += 1,
            |p: &mut ModelPass| p.issued_at -= 1,
        ] {
            let mut altered = pass.clone();
            tamper(&mut altered);
            assert!(
                altered
                    .verify(1_100, &ModelPassExpectation::default())
                    .is_err(),
                "tampered pass must not verify"
            );
        }

        // A pass signed by another key but naming the issuer is refused.
        let forger = Keypair::generate_ed25519();
        let mut forged = ModelPass::issue(&forger, claim(&f), 1_000, 600).unwrap();
        forged.issuer = issuer_id;
        assert_eq!(
            forged.verify(1_100, &ModelPassExpectation::default()),
            Err(ModelPassError::BadSignature)
        );

        // Garbage signature, unsupported version and over-long lifetimes.
        let mut bad = pass.clone();
        bad.signature = "zz".into();
        assert_eq!(
            bad.verify(1_100, &ModelPassExpectation::default()),
            Err(ModelPassError::BadSignature)
        );
        let mut bad = pass;
        bad.version = 2;
        assert_eq!(
            bad.verify(1_100, &ModelPassExpectation::default()),
            Err(ModelPassError::UnsupportedVersion(2))
        );
        assert!(
            ModelPass::issue(&f.issuer, claim(&f), 1_000, MAX_MODEL_PASS_TTL_SECS + 1).is_err()
        );
        assert!(ModelPass::issue(&f.issuer, claim(&f), 1_000, 0).is_err());
    }

    #[test]
    fn model_pass_signature_is_not_a_membership_or_trust_token_signature() {
        // The signing domain is distinct: the signed bytes start with the pass domain,
        // so they cannot equal the signable bytes of a membership or trust token.
        let f = fixture();
        let pass = ModelPass::issue(&f.issuer, claim(&f), 1_000, 600).unwrap();
        assert!(pass.signable_bytes().starts_with(SIGNING_DOMAIN));
        let token = crate::invite::MembershipToken::issue(
            format!("trust:{}:eng", pass.issuer),
            &f.host,
            &f.issuer,
            "trust".into(),
            crate::invite::InvitePermissions::default(),
            0,
        )
        .unwrap();
        // A token signature does not verify as a pass signature.
        let mut forged = pass;
        forged.signature = token.signature;
        assert_eq!(
            forged.verify(1_100, &ModelPassExpectation::default()),
            Err(ModelPassError::BadSignature)
        );
    }

    #[test]
    fn model_pass_round_trips_through_serde() {
        let f = fixture();
        let pass = ModelPass::issue(&f.issuer, claim(&f), 1_000, 600).unwrap();
        let json = serde_json::to_string(&pass).unwrap();
        let back: ModelPass = serde_json::from_str(&json).unwrap();
        assert_eq!(back, pass);
        back.verify(1_100, &ModelPassExpectation::default())
            .unwrap();
    }
}
