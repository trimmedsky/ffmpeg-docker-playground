//! Request authentication.
//!
//! [`Authenticator`] is the single decision point used by the HTTP middleware. Each
//! scheme is one variant, so adding a scheme is adding a variant and its configuration.
//! The agent fails closed: without an authentication configuration it does not start,
//! and the only way to serve without authentication is the explicit
//! `FFMPEG_AGENT_INSECURE_NO_AUTH=1`.

mod jwks;
mod jwt;

use axum::http::{HeaderMap, header::AUTHORIZATION};
use std::{collections::BTreeSet, ffi::OsString, fmt};

pub use jwt::Verified;

const JWKS_FILE: &str = "FFMPEG_AGENT_AUTH_JWKS_FILE";
const AUDIENCE: &str = "FFMPEG_AGENT_AUTH_AUDIENCE";
const ISSUER: &str = "FFMPEG_AGENT_AUTH_ISSUER";
const TYP: &str = "FFMPEG_AGENT_AUTH_TYP";
const MAX_LIFETIME: &str = "FFMPEG_AGENT_AUTH_MAX_LIFETIME_SECS";
const SUBJECTS: &str = "FFMPEG_AGENT_AUTH_SUBJECTS";
const JWT_SETTINGS: [&str; 5] = [AUDIENCE, ISSUER, TYP, MAX_LIFETIME, SUBJECTS];
const INSECURE: &str = "FFMPEG_AGENT_INSECURE_NO_AUTH";
/// The static bearer token of earlier versions. Refused so that an old configuration
/// fails loudly instead of being read as something else.
const RETIRED_TOKEN_FILE: &str = "FFMPEG_AGENT_TOKEN_FILE";
const MAX_LIFETIME_LIMIT: u64 = 86400;

/// Why a request was not authenticated. Logged by the agent; the client only sees 401.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    MissingCredential,
    DuplicateCredential,
    MalformedAuthorization,
    TokenTooLarge,
    MalformedToken,
    InvalidHeader,
    UnsupportedAlgorithm,
    MissingKid,
    TypeMismatch,
    UnknownKey,
    KeysUnavailable,
    BadSignature,
    InvalidClaims,
    MissingClaim(&'static str),
    InvalidTimes,
    Expired,
    IssuedInFuture,
    NotYetValid,
    LifetimeTooLong,
    WrongIssuer,
    WrongAudience,
    SubjectNotAllowed,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::MissingCredential => "no Authorization header",
            Self::DuplicateCredential => "more than one Authorization header",
            Self::MalformedAuthorization => "Authorization is not a Bearer credential",
            Self::TokenTooLarge => "token too large",
            Self::MalformedToken => "not a JWS compact serialization",
            Self::InvalidHeader => "invalid or unsupported JOSE header",
            Self::UnsupportedAlgorithm => "alg is not ES256",
            Self::MissingKid => "no kid in JOSE header",
            Self::TypeMismatch => "typ header does not match",
            Self::UnknownKey => "kid not in JWKS",
            Self::KeysUnavailable => "JWKS unavailable",
            Self::BadSignature => "invalid signature",
            Self::InvalidClaims => "invalid claims",
            Self::MissingClaim(name) => return write!(f, "missing claim {name}"),
            Self::InvalidTimes => "iat is not before exp",
            Self::Expired => "token expired",
            Self::IssuedInFuture => "iat is in the future",
            Self::NotYetValid => "nbf is in the future",
            Self::LifetimeTooLong => "exp - iat exceeds the maximum lifetime",
            Self::WrongIssuer => "issuer does not match",
            Self::WrongAudience => "audience does not match",
            Self::SubjectNotAllowed => "subject not allowed",
        };
        f.write_str(text)
    }
}

pub enum Authenticator {
    /// `Authorization: Bearer <JWT>`, ES256, keys from a JWKS file.
    Jwt(Box<jwt::JwtVerifier>),
    /// Only with `FFMPEG_AGENT_INSECURE_NO_AUTH=1`, for isolated local tests.
    InsecureNoAuth,
}

/// The authenticated caller, or `None` when authentication is disabled.
pub type Principal = Option<Verified>;

impl Authenticator {
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    fn from_lookup(get: impl Fn(&str) -> Option<OsString>) -> Result<Self, String> {
        if get(RETIRED_TOKEN_FILE).is_some() {
            return Err(format!(
                "{RETIRED_TOKEN_FILE} is no longer supported; configure JWT authentication with {JWKS_FILE} and {AUDIENCE}"
            ));
        }
        let text = |name: &str| -> Result<Option<String>, String> {
            let Some(value) = get(name) else {
                return Ok(None);
            };
            let value = value
                .into_string()
                .map_err(|_| format!("{name} is not valid UTF-8"))?;
            if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
                return Err(format!(
                    "{name} must be non-empty, without surrounding whitespace or control characters"
                ));
            }
            Ok(Some(value))
        };
        let jwt_settings: Vec<&str> = JWT_SETTINGS
            .into_iter()
            .filter(|name| get(name).is_some())
            .collect();
        match (text(JWKS_FILE)?, text(INSECURE)?) {
            (Some(_), Some(_)) => Err(format!("{INSECURE} cannot be combined with {JWKS_FILE}")),
            (None, Some(flag)) if flag != "1" => Err(format!(
                "{INSECURE} must be exactly 1; unset it to require authentication"
            )),
            (None, insecure) if !jwt_settings.is_empty() => Err(format!(
                "{} set without {JWKS_FILE}{}",
                jwt_settings.join(", "),
                if insecure.is_some() {
                    format!(" (and {INSECURE} disables authentication)")
                } else {
                    String::new()
                }
            )),
            (None, Some(_)) => Ok(Self::InsecureNoAuth),
            (None, None) => Err(format!(
                "authentication is required: set {JWKS_FILE} and {AUDIENCE} \
                 ({INSECURE}=1 disables authentication for isolated local tests only)"
            )),
            (Some(path), None) => {
                let audience = text(AUDIENCE)?.ok_or_else(|| format!("{AUDIENCE} is required"))?;
                let max_lifetime_secs = match text(MAX_LIFETIME)? {
                    None => jwt::DEFAULT_MAX_LIFETIME_SECS,
                    Some(value) => value
                        .parse::<u64>()
                        .ok()
                        .filter(|secs| (1..=MAX_LIFETIME_LIMIT).contains(secs))
                        .ok_or_else(|| {
                            format!("{MAX_LIFETIME} must be in 1..={MAX_LIFETIME_LIMIT}")
                        })?,
                };
                let subjects = match text(SUBJECTS)? {
                    None => None,
                    Some(list) => {
                        let items: Vec<&str> = list.split(',').map(str::trim).collect();
                        if items.iter().any(|item| item.is_empty()) {
                            return Err(format!(
                                "{SUBJECTS} must be a comma-separated list without empty entries"
                            ));
                        }
                        Some(items.into_iter().map(String::from).collect::<BTreeSet<_>>())
                    }
                };
                let policy = jwt::Policy {
                    audience,
                    issuer: text(ISSUER)?,
                    typ: text(TYP)?,
                    max_lifetime_secs,
                    subjects,
                };
                Ok(Self::Jwt(Box::new(jwt::JwtVerifier::new(
                    policy,
                    jwks::KeyStore::open(path)?,
                ))))
            }
        }
    }

    /// One line for the startup log. Contains no secrets.
    pub fn describe(&self) -> String {
        match self {
            Self::InsecureNoAuth => format!("authentication DISABLED ({INSECURE}=1)"),
            Self::Jwt(verifier) => {
                let p = verifier.policy();
                format!(
                    "JWT authentication: ES256, {} key(s), audience {:?}, issuer {}, typ {}, max lifetime {}s, subjects {}",
                    verifier.key_count(),
                    p.audience,
                    p.issuer
                        .as_deref()
                        .map_or("any".into(), |i| format!("{i:?}")),
                    p.typ.as_deref().map_or("any".into(), |t| format!("{t:?}")),
                    p.max_lifetime_secs,
                    p.subjects
                        .as_ref()
                        .map_or("any".into(), |s| format!("{s:?}")),
                )
            }
        }
    }

    /// `now` is seconds since the Unix epoch.
    pub fn authenticate(&self, headers: &HeaderMap, now: u64) -> Result<Principal, Rejection> {
        match self {
            Self::InsecureNoAuth => Ok(None),
            Self::Jwt(verifier) => verifier.verify(bearer(headers)?, now).map(Some),
        }
    }
}

/// The token of exactly one `Authorization: Bearer <token>` header (RFC 6750 2.1).
/// Credentials anywhere else (query string, other headers) are never read.
fn bearer(headers: &HeaderMap) -> Result<&str, Rejection> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let value = values.next().ok_or(Rejection::MissingCredential)?;
    if values.next().is_some() {
        return Err(Rejection::DuplicateCredential);
    }
    if value.len() > jwt::MAX_TOKEN_BYTES + "Bearer ".len() {
        return Err(Rejection::TokenTooLarge);
    }
    let value = value
        .to_str()
        .map_err(|_| Rejection::MalformedAuthorization)?;
    let (scheme, token) = value
        .split_once(' ')
        .ok_or(Rejection::MalformedAuthorization)?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() {
        return Err(Rejection::MalformedAuthorization);
    }
    if !token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(Rejection::MalformedToken);
    }
    Ok(token)
}

#[cfg(test)]
pub mod testing {
    //! A throwaway ES256 issuer for tests.
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use ring::{
        rand::SystemRandom,
        signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair},
    };
    use serde_json::{Value, json};

    pub struct Signer {
        pub kid: String,
        key: EcdsaKeyPair,
    }

    impl Signer {
        pub fn new(kid: &str) -> Self {
            let rng = SystemRandom::new();
            let pkcs8 =
                EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
            let key =
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
                    .unwrap();
            Self {
                kid: kid.into(),
                key,
            }
        }

        pub fn public_sec1(&self) -> [u8; 65] {
            self.key.public_key().as_ref().try_into().unwrap()
        }

        pub fn jwk_value(&self, kid: &str) -> Value {
            let point = self.public_sec1();
            json!({"kty":"EC","crv":"P-256","kid":kid,
                   "x":URL_SAFE_NO_PAD.encode(&point[1..33]),"y":URL_SAFE_NO_PAD.encode(&point[33..])})
        }

        pub fn jwk(&self) -> Value {
            self.jwk_value(&self.kid)
        }

        pub fn sign_raw(&self, header: &str, claims: &str) -> String {
            let input = format!(
                "{}.{}",
                URL_SAFE_NO_PAD.encode(header),
                URL_SAFE_NO_PAD.encode(claims)
            );
            let signature = self
                .key
                .sign(&SystemRandom::new(), input.as_bytes())
                .unwrap();
            format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
        }

        pub fn sign(&self, header: &Value, claims: &Value) -> String {
            self.sign_raw(&header.to_string(), &claims.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::Signer, *};
    use axum::http::HeaderValue;
    use serde_json::json;
    use std::{collections::HashMap, path::Path};

    const NOW: u64 = 1_800_000_000;

    fn load(vars: &[(&str, &str)]) -> Result<Authenticator, String> {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        Authenticator::from_lookup(|name| vars.get(name).cloned())
    }

    fn jwks_file(dir: &Path, signer: &Signer) -> String {
        let path = dir.join("jwks.json");
        std::fs::write(&path, json!({"keys":[signer.jwk()]}).to_string()).unwrap();
        path.to_str().unwrap().to_string()
    }

    fn error(vars: &[(&str, &str)]) -> String {
        match load(vars) {
            Err(error) => error,
            Ok(_) => panic!("{vars:?} was accepted"),
        }
    }

    #[test]
    fn fails_closed_without_configuration() {
        assert!(error(&[]).contains("authentication is required"));
        for flag in ["0", "true", "yes", "TRUE", " 1", "1 ", ""] {
            let message = error(&[(INSECURE, flag)]);
            assert!(message.contains(INSECURE), "{flag:?}: {message}");
        }
        assert!(matches!(
            load(&[(INSECURE, "1")]),
            Ok(Authenticator::InsecureNoAuth)
        ));
    }

    #[test]
    fn refuses_contradictory_or_incomplete_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let jwks = jwks_file(dir.path(), &Signer::new("k"));
        let jwks = jwks.as_str();
        assert!(
            error(&[(JWKS_FILE, jwks), (AUDIENCE, "a"), (INSECURE, "1")])
                .contains("cannot be combined")
        );
        assert!(error(&[(JWKS_FILE, jwks)]).contains("FFMPEG_AGENT_AUTH_AUDIENCE is required"));
        for setting in JWT_SETTINGS {
            assert!(
                error(&[(setting, "x")]).contains("set without"),
                "{setting}"
            );
            assert!(
                error(&[(setting, "x"), (INSECURE, "1")]).contains("set without"),
                "{setting}"
            );
        }
        assert!(
            error(&[(RETIRED_TOKEN_FILE, "/dev/null"), (INSECURE, "1")])
                .contains("no longer supported")
        );
        assert!(error(&[(RETIRED_TOKEN_FILE, "")]).contains("no longer supported"));
        assert!(
            error(&[(JWKS_FILE, "/nonexistent/jwks.json"), (AUDIENCE, "a")])
                .contains("cannot open")
        );
        for (name, value) in [
            (AUDIENCE, ""),
            (AUDIENCE, " a"),
            (ISSUER, "a\tb"),
            (TYP, ""),
            (MAX_LIFETIME, "0"),
            (MAX_LIFETIME, "86401"),
            (MAX_LIFETIME, "-1"),
            (MAX_LIFETIME, "5m"),
            (SUBJECTS, "a,,b"),
            (SUBJECTS, "a,"),
        ] {
            let mut vars = vec![(JWKS_FILE, jwks), (AUDIENCE, "a")];
            vars.retain(|(n, _)| *n != name);
            vars.push((name, value));
            assert!(error(&vars).contains(name), "{name}={value:?}");
        }
    }

    #[test]
    fn reads_the_jwt_policy() {
        let dir = tempfile::tempdir().unwrap();
        let jwks = jwks_file(dir.path(), &Signer::new("k"));
        let Ok(Authenticator::Jwt(verifier)) = load(&[
            (JWKS_FILE, &jwks),
            (AUDIENCE, "ffmpeg-agent"),
            (ISSUER, "example-issuer"),
            (TYP, "example+jwt"),
            (MAX_LIFETIME, "60"),
            (SUBJECTS, "caller-a, caller-b"),
        ]) else {
            panic!("not a JWT authenticator");
        };
        assert_eq!(
            *verifier.policy(),
            jwt::Policy {
                audience: "ffmpeg-agent".into(),
                issuer: Some("example-issuer".into()),
                typ: Some("example+jwt".into()),
                max_lifetime_secs: 60,
                subjects: Some(BTreeSet::from(["caller-a".into(), "caller-b".into()])),
            }
        );
        let Ok(Authenticator::Jwt(verifier)) =
            load(&[(JWKS_FILE, &jwks), (AUDIENCE, "ffmpeg-agent")])
        else {
            panic!("not a JWT authenticator");
        };
        assert_eq!(verifier.policy().max_lifetime_secs, 300);
        assert_eq!(verifier.policy().issuer, None);
        assert_eq!(verifier.policy().subjects, None);
    }

    fn headers(values: &[&[u8]]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(AUTHORIZATION, HeaderValue::from_bytes(value).unwrap());
        }
        headers
    }

    #[test]
    fn extracts_exactly_one_bearer_credential() {
        assert_eq!(bearer(&headers(&[b"Bearer a.b.c"])), Ok("a.b.c"));
        assert_eq!(bearer(&headers(&[b"bearer a.b.c"])), Ok("a.b.c"));
        assert_eq!(bearer(&headers(&[])), Err(Rejection::MissingCredential));
        assert_eq!(
            bearer(&headers(&[b"Bearer a.b.c", b"Bearer a.b.c"])),
            Err(Rejection::DuplicateCredential)
        );
        for value in [
            &b"Basic YTpi"[..],
            b"Bearer",
            b"Bearer ",
            b"Token a.b.c",
            b"Bearer\ta.b.c",
            b"Bearer \xff",
        ] {
            assert_eq!(
                bearer(&headers(&[value])),
                Err(Rejection::MalformedAuthorization),
                "{}",
                String::from_utf8_lossy(value)
            );
        }
        for value in [
            &b"Bearer  a.b.c"[..],
            b"Bearer a.b.c x",
            b"Bearer a+b.c",
            b"Bearer a.b.c=",
        ] {
            assert_eq!(bearer(&headers(&[value])), Err(Rejection::MalformedToken));
        }
        let large = format!("Bearer {}", "a".repeat(jwt::MAX_TOKEN_BYTES + 1));
        assert_eq!(
            bearer(&headers(&[large.as_bytes()])),
            Err(Rejection::TokenTooLarge)
        );
    }

    #[test]
    fn authenticates_requests_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let signer = Signer::new("k");
        let jwks = jwks_file(dir.path(), &signer);
        let auth = load(&[(JWKS_FILE, &jwks), (AUDIENCE, "ffmpeg-agent")]).unwrap();
        let claims = json!({"sub":"example-caller","aud":"ffmpeg-agent","iat":NOW,"exp":NOW + 60});
        let token = signer.sign(&json!({"alg":"ES256","kid":"k"}), &claims);
        let value = format!("Bearer {token}");
        let principal = auth
            .authenticate(&headers(&[value.as_bytes()]), NOW)
            .unwrap();
        assert_eq!(principal.unwrap().subject, "example-caller");
        assert_eq!(
            auth.authenticate(&headers(&[]), NOW),
            Err(Rejection::MissingCredential)
        );
        assert_eq!(
            auth.authenticate(&headers(&[value.as_bytes()]), NOW + 60),
            Err(Rejection::Expired)
        );
        // Identity headers carry no weight.
        let mut spoofed = headers(&[]);
        spoofed.insert("x-user-id", HeaderValue::from_static("example-caller"));
        assert_eq!(
            auth.authenticate(&spoofed, NOW),
            Err(Rejection::MissingCredential)
        );
        assert!(auth.describe().contains("audience \"ffmpeg-agent\""));

        let open = load(&[(INSECURE, "1")]).unwrap();
        assert_eq!(open.authenticate(&headers(&[]), NOW), Ok(None));
        assert!(open.describe().contains("DISABLED"));
    }
}
