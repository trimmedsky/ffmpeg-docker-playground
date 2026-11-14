//! Verification of ES256-signed JWTs (RFC 7519) in JWS compact serialization
//! (RFC 7515), following the JWT best current practices (RFC 8725):
//! the algorithm is fixed by the verifier rather than chosen by the token, keys come
//! only from the configured JWKS, and the audience is always checked.

use super::{Rejection, jwks::KeyStore};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};
use serde::Deserialize;
use std::collections::BTreeSet;

/// Largest accepted compact token.
pub const MAX_TOKEN_BYTES: usize = 16 * 1024;
/// Tolerated clock difference for `iat` and `nbf` in the future. `exp` gets none.
pub const CLOCK_SKEW_SECS: u64 = 15;
pub const DEFAULT_MAX_LIFETIME_SECS: u64 = 300;

/// What a token must satisfy besides a valid signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Required: the token's `aud` (a string or an array) must contain it.
    pub audience: String,
    /// Optional: when set, `iss` must be present and equal.
    pub issuer: Option<String>,
    /// Optional: when set, the JOSE `typ` header must be present and equal.
    pub typ: Option<String>,
    /// Upper bound on `exp - iat`.
    pub max_lifetime_secs: u64,
    /// Optional allow-list for `sub`.
    pub subjects: Option<BTreeSet<String>>,
}

/// The JOSE header. Members other than these three are refused, which covers the
/// key-carrying `jwk`, `jku`, `x5u` and `x5c`, `crit` extensions, `b64: false` and `zip`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    alg: String,
    kid: Option<String>,
    typ: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

/// Registered claims the verifier reads. Other claims are allowed and ignored.
/// NumericDate values must be non-negative integers.
#[derive(Deserialize)]
struct Claims {
    sub: Option<String>,
    iss: Option<String>,
    aud: Option<Audience>,
    exp: Option<u64>,
    iat: Option<u64>,
    nbf: Option<u64>,
}

/// The authenticated caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub subject: String,
}

fn segment(value: &str) -> Result<Vec<u8>, Rejection> {
    if value.is_empty() {
        return Err(Rejection::MalformedToken);
    }
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| Rejection::MalformedToken)
}

/// Verifies `token` with keys from `key`. Every check that a token can fail is a
/// separate [`Rejection`], so each one is covered by its own test.
pub fn verify(
    policy: &Policy,
    token: &str,
    now: u64,
    key: impl FnOnce(&str) -> Result<super::jwks::VerifyingKey, Rejection>,
) -> Result<Verified, Rejection> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(Rejection::TokenTooLarge);
    }
    let mut parts = token.split('.');
    let (Some(header_b64), Some(payload_b64), Some(signature_b64), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Rejection::MalformedToken);
    };
    let header = segment(header_b64)?;
    let payload = segment(payload_b64)?;
    let signature = segment(signature_b64)?;

    let header: Header = serde_json::from_slice(&header).map_err(|_| Rejection::InvalidHeader)?;
    if header.alg != "ES256" {
        return Err(Rejection::UnsupportedAlgorithm);
    }
    let kid = header
        .kid
        .filter(|kid| !kid.is_empty())
        .ok_or(Rejection::MissingKid)?;
    if let Some(required) = &policy.typ
        && header.typ.as_ref() != Some(required)
    {
        return Err(Rejection::TypeMismatch);
    }

    let key = key(&kid)?;
    let signing_input = &token[..header_b64.len() + 1 + payload_b64.len()];
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &key.sec1)
        .verify(signing_input.as_bytes(), &signature)
        .map_err(|_| Rejection::BadSignature)?;

    // Only signed content is parsed past this point.
    let claims: Claims = serde_json::from_slice(&payload).map_err(|_| Rejection::InvalidClaims)?;
    let exp = claims.exp.ok_or(Rejection::MissingClaim("exp"))?;
    let iat = claims.iat.ok_or(Rejection::MissingClaim("iat"))?;
    let subject = claims
        .sub
        .filter(|sub| !sub.is_empty())
        .ok_or(Rejection::MissingClaim("sub"))?;
    if iat >= exp {
        return Err(Rejection::InvalidTimes);
    }
    if exp <= now {
        return Err(Rejection::Expired);
    }
    if iat > now.saturating_add(CLOCK_SKEW_SECS) {
        return Err(Rejection::IssuedInFuture);
    }
    if claims
        .nbf
        .is_some_and(|nbf| nbf > now.saturating_add(CLOCK_SKEW_SECS))
    {
        return Err(Rejection::NotYetValid);
    }
    if exp - iat > policy.max_lifetime_secs {
        return Err(Rejection::LifetimeTooLong);
    }
    if let Some(issuer) = &policy.issuer
        && claims.iss.as_ref() != Some(issuer)
    {
        return Err(Rejection::WrongIssuer);
    }
    let audience_matches = match &claims.aud {
        Some(Audience::One(aud)) => *aud == policy.audience,
        Some(Audience::Many(auds)) => auds.contains(&policy.audience),
        None => false,
    };
    if !audience_matches {
        return Err(Rejection::WrongAudience);
    }
    if policy
        .subjects
        .as_ref()
        .is_some_and(|allowed| !allowed.contains(&subject))
    {
        return Err(Rejection::SubjectNotAllowed);
    }
    Ok(Verified { subject })
}

pub struct JwtVerifier {
    policy: Policy,
    keys: KeyStore,
}

impl JwtVerifier {
    pub fn new(policy: Policy, keys: KeyStore) -> Self {
        Self { policy, keys }
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn key_count(&self) -> usize {
        self.keys.key_count()
    }

    pub fn verify(&self, token: &str, now: u64) -> Result<Verified, Rejection> {
        verify(&self.policy, token, now, |kid| self.keys.key(kid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{jwks::VerifyingKey, testing::Signer};
    use serde_json::{Value, json};

    const NOW: u64 = 1_800_000_000;

    fn policy() -> Policy {
        Policy {
            audience: "ffmpeg-agent".into(),
            issuer: None,
            typ: None,
            max_lifetime_secs: DEFAULT_MAX_LIFETIME_SECS,
            subjects: None,
        }
    }

    fn claims() -> Value {
        json!({"sub":"example-caller","aud":"ffmpeg-agent","iat":NOW - 10,"exp":NOW + 50})
    }

    fn header() -> Value {
        json!({"alg":"ES256","kid":"k"})
    }

    fn check(policy: &Policy, signer: &Signer, token: &str) -> Result<Verified, Rejection> {
        verify(policy, token, NOW, |kid| {
            if kid == signer.kid {
                Ok(VerifyingKey {
                    kid: kid.into(),
                    sec1: signer.public_sec1(),
                })
            } else {
                Err(Rejection::UnknownKey)
            }
        })
    }

    fn outcome(policy: &Policy, header: Value, claims: Value) -> Result<Verified, Rejection> {
        let signer = Signer::new("k");
        check(policy, &signer, &signer.sign(&header, &claims))
    }

    fn with(mut value: Value, field: &str, member: Value) -> Value {
        value[field] = member;
        value
    }

    fn without(mut value: Value, field: &str) -> Value {
        value.as_object_mut().unwrap().remove(field);
        value
    }

    #[test]
    fn accepts_a_valid_token_and_reports_the_subject() {
        assert_eq!(
            outcome(&policy(), header(), claims()),
            Ok(Verified {
                subject: "example-caller".into()
            })
        );
    }

    #[test]
    fn accepts_standard_variations() {
        let p = policy();
        // aud as an array (RFC 7519 4.1.3), unknown claims, typ when none is required.
        let varied = with(claims(), "aud", json!(["other", "ffmpeg-agent"]));
        let varied = with(varied, "custom", json!({"nested": [1, 2]}));
        let typed = with(header(), "typ", json!("JWT"));
        assert!(outcome(&p, typed, varied).is_ok());
        // Boundaries: lifetime exactly the maximum, iat at the skew limit, nbf now.
        let at_max = json!({"sub":"s","aud":"ffmpeg-agent","iat":NOW,"exp":NOW + 300});
        assert!(outcome(&p, header(), at_max).is_ok());
        let skewed = json!({"sub":"s","aud":"ffmpeg-agent","iat":NOW + 15,"exp":NOW + 60});
        assert!(outcome(&p, header(), skewed).is_ok());
        assert!(outcome(&p, header(), with(claims(), "nbf", json!(NOW + 15))).is_ok());
    }

    #[test]
    fn rejects_malformed_compact_serialization() {
        let p = policy();
        let signer = Signer::new("k");
        let good = signer.sign(&header(), &claims());
        let parts: Vec<_> = good.split('.').collect();
        for token in [
            String::new(),
            "a.b".into(),
            format!("{good}.extra"),
            format!("{}..{}", parts[0], parts[2]),
            format!("{}.{}.", parts[0], parts[1]),
            format!("{}=.{}.{}", parts[0], parts[1], parts[2]),
            format!("{}.{}.{}", parts[0].replace('e', "+"), parts[1], parts[2]),
            format!("{}.{}.{}*", parts[0], parts[1], parts[2]),
        ] {
            assert_eq!(
                check(&p, &signer, &token),
                Err(Rejection::MalformedToken),
                "{token}"
            );
        }
        let large = format!("{good}{}", "A".repeat(MAX_TOKEN_BYTES));
        assert_eq!(check(&p, &signer, &large), Err(Rejection::TokenTooLarge));
    }

    #[test]
    fn rejects_header_problems() {
        let p = policy();
        for alg in ["none", "HS256", "RS256", "ES384", "EdDSA", "es256"] {
            let header = with(header(), "alg", json!(alg));
            assert_eq!(
                outcome(&p, header, claims()),
                Err(Rejection::UnsupportedAlgorithm)
            );
        }
        assert_eq!(
            outcome(&p, without(header(), "kid"), claims()),
            Err(Rejection::MissingKid)
        );
        assert_eq!(
            outcome(&p, with(header(), "kid", json!("")), claims()),
            Err(Rejection::MissingKid)
        );
        // Keys or key locations inside the token are never used.
        let signer = Signer::new("k");
        for (member, value) in [
            ("jwk", signer.jwk()),
            ("jku", json!("https://example.org/jwks.json")),
            ("x5u", json!("https://example.org/cert.pem")),
            ("x5c", json!(["AAAA"])),
            ("crit", json!(["exp"])),
            ("b64", json!(false)),
            ("zip", json!("DEF")),
        ] {
            let header = with(header(), member, value);
            assert_eq!(
                outcome(&p, header, claims()),
                Err(Rejection::InvalidHeader),
                "{member}"
            );
        }
        assert_eq!(
            outcome(&p, json!(["ES256"]), claims()),
            Err(Rejection::InvalidHeader)
        );
        assert_eq!(
            outcome(&p, json!({"alg":1,"kid":"k"}), claims()),
            Err(Rejection::InvalidHeader)
        );
        let duplicate = r#"{"alg":"ES256","alg":"ES256","kid":"k"}"#;
        assert_eq!(
            check(
                &p,
                &signer,
                &signer.sign_raw(duplicate, &claims().to_string())
            ),
            Err(Rejection::InvalidHeader)
        );
    }

    #[test]
    fn enforces_a_configured_typ_exactly() {
        let p = Policy {
            typ: Some("example+jwt".into()),
            ..policy()
        };
        assert!(outcome(&p, with(header(), "typ", json!("example+jwt")), claims()).is_ok());
        assert_eq!(
            outcome(&p, header(), claims()),
            Err(Rejection::TypeMismatch)
        );
        for typ in ["JWT", "Example+JWT", "application/example+jwt"] {
            let header = with(header(), "typ", json!(typ));
            assert_eq!(
                outcome(&p, header, claims()),
                Err(Rejection::TypeMismatch),
                "{typ}"
            );
        }
    }

    /// The ASN.1 DER form of a raw `r || s` signature, as some libraries emit it.
    fn der(raw: &[u8]) -> Vec<u8> {
        let integer = |half: &[u8]| {
            let trimmed: Vec<u8> = half.iter().copied().skip_while(|b| *b == 0).collect();
            let mut out = if trimmed.first().is_some_and(|b| b & 0x80 != 0) {
                vec![0]
            } else {
                vec![]
            };
            out.extend(trimmed);
            [vec![2, out.len() as u8], out].concat()
        };
        let body = [integer(&raw[..32]), integer(&raw[32..])].concat();
        [vec![0x30, body.len() as u8], body].concat()
    }

    #[test]
    fn rejects_unknown_keys_and_bad_signatures() {
        let p = policy();
        let signer = Signer::new("k");
        let other = Signer::new("k");
        // Same kid, different key.
        assert_eq!(
            check(&p, &signer, &other.sign(&header(), &claims())),
            Err(Rejection::BadSignature)
        );
        assert_eq!(
            check(
                &p,
                &signer,
                &signer.sign(&with(header(), "kid", json!("unknown")), &claims())
            ),
            Err(Rejection::UnknownKey)
        );
        // Tampering with either signed segment or the signature itself.
        let token = signer.sign(&header(), &claims());
        let parts: Vec<_> = token.split('.').collect();
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        let forged = URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&with(claims(), "sub", json!("someone-else"))).unwrap());
        let swapped_header = URL_SAFE_NO_PAD.encode(br#"{"kid":"k","alg":"ES256"}"#);
        for tampered in [
            format!("{}.{forged}.{}", parts[0], parts[2]),
            format!("{swapped_header}.{}.{}", parts[1], parts[2]),
            format!(
                "{}.{}.{}",
                parts[0],
                parts[1],
                URL_SAFE_NO_PAD.encode([0u8; 64])
            ),
            // A truncated or DER-encoded signature is not the fixed-size JWS form.
            format!(
                "{}.{}.{}",
                parts[0],
                parts[1],
                URL_SAFE_NO_PAD.encode(&signature[..63])
            ),
            format!(
                "{}.{}.{}",
                parts[0],
                parts[1],
                URL_SAFE_NO_PAD.encode(der(&signature))
            ),
        ] {
            assert_eq!(check(&p, &signer, &tampered), Err(Rejection::BadSignature));
        }
    }

    #[test]
    fn rejects_invalid_or_incomplete_claims() {
        let p = policy();
        let signer = Signer::new("k");
        for payload in [
            "[]".to_string(),
            "\"text\"".into(),
            "not json".into(),
            r#"{"sub":"s","aud":"ffmpeg-agent","iat":1,"exp":2,"exp":3}"#.into(),
        ] {
            let token = signer.sign_raw(&header().to_string(), &payload);
            assert_eq!(
                check(&p, &signer, &token),
                Err(Rejection::InvalidClaims),
                "{payload}"
            );
        }
        for (field, value) in [
            ("exp", json!(1.5)),
            ("exp", json!(-1)),
            ("exp", json!("1800000050")),
            ("iat", json!(1e30)),
            ("nbf", json!("now")),
            ("sub", json!(7)),
            ("aud", json!(7)),
            ("aud", json!(["ffmpeg-agent", 7])),
            ("iss", json!(["example-issuer"])),
        ] {
            let claims = with(claims(), field, value.clone());
            assert_eq!(
                outcome(&p, header(), claims),
                Err(Rejection::InvalidClaims),
                "{field}={value}"
            );
        }
        for field in ["exp", "iat", "sub"] {
            assert_eq!(
                outcome(&p, header(), without(claims(), field)),
                Err(Rejection::MissingClaim(field))
            );
            let null = with(claims(), field, Value::Null);
            assert_eq!(
                outcome(&p, header(), null),
                Err(Rejection::MissingClaim(field))
            );
        }
        let empty_sub = with(claims(), "sub", json!(""));
        assert_eq!(
            outcome(&p, header(), empty_sub),
            Err(Rejection::MissingClaim("sub"))
        );
    }

    #[test]
    fn rejects_tokens_outside_their_validity_window() {
        let p = policy();
        let at = |iat: u64, exp: u64| json!({"sub":"s","aud":"ffmpeg-agent","iat":iat,"exp":exp});
        assert_eq!(
            outcome(&p, header(), at(NOW - 60, NOW)),
            Err(Rejection::Expired)
        );
        assert_eq!(
            outcome(&p, header(), at(NOW - 60, NOW - 1)),
            Err(Rejection::Expired)
        );
        assert_eq!(
            outcome(&p, header(), at(NOW + 16, NOW + 60)),
            Err(Rejection::IssuedInFuture)
        );
        assert_eq!(
            outcome(&p, header(), at(NOW + 10, NOW + 10)),
            Err(Rejection::InvalidTimes)
        );
        assert_eq!(
            outcome(&p, header(), at(NOW + 10, NOW + 5)),
            Err(Rejection::InvalidTimes)
        );
        assert_eq!(
            outcome(&p, header(), at(NOW, NOW + 301)),
            Err(Rejection::LifetimeTooLong)
        );
        let nbf = with(claims(), "nbf", json!(NOW + 16));
        assert_eq!(outcome(&p, header(), nbf), Err(Rejection::NotYetValid));
        let short = Policy {
            max_lifetime_secs: 60,
            ..policy()
        };
        assert!(outcome(&short, header(), at(NOW, NOW + 60)).is_ok());
        assert_eq!(
            outcome(&short, header(), at(NOW, NOW + 61)),
            Err(Rejection::LifetimeTooLong)
        );
    }

    #[test]
    fn checks_audience_issuer_and_subject() {
        let p = policy();
        for aud in [
            json!("other"),
            json!([]),
            json!(["other"]),
            json!("FFMPEG-AGENT"),
        ] {
            let claims = with(claims(), "aud", aud.clone());
            assert_eq!(
                outcome(&p, header(), claims),
                Err(Rejection::WrongAudience),
                "{aud}"
            );
        }
        assert_eq!(
            outcome(&p, header(), without(claims(), "aud")),
            Err(Rejection::WrongAudience)
        );

        let p = Policy {
            issuer: Some("example-issuer".into()),
            ..policy()
        };
        assert!(outcome(&p, header(), with(claims(), "iss", json!("example-issuer"))).is_ok());
        assert_eq!(outcome(&p, header(), claims()), Err(Rejection::WrongIssuer));
        let other = with(claims(), "iss", json!("other-issuer"));
        assert_eq!(outcome(&p, header(), other), Err(Rejection::WrongIssuer));

        let p = Policy {
            subjects: Some(BTreeSet::from(["example-caller".to_string()])),
            ..policy()
        };
        assert!(outcome(&p, header(), claims()).is_ok());
        let other = with(claims(), "sub", json!("someone-else"));
        assert_eq!(
            outcome(&p, header(), other),
            Err(Rejection::SubjectNotAllowed)
        );
    }
}
