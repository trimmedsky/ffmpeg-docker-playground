use crate::{Error, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ring::{
    rand::SystemRandom,
    signature::{self, EcdsaKeyPair, KeyPair},
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub const AUTH_TYPE: &str = "hng-auth+jwt";
pub const BACKEND_TYPE: &str = "hng-backend+jwt";
/// Lifetime (`exp - iat`) of every backend JWT the connector mints.
pub const BACKEND_TOKEN_LIFETIME_SECS: u64 = 60;
/// Largest `exp - iat` a backend verifier accepts. Backends trust nothing but
/// the local connector key, so a mis-minted or leaked long-lived token must not
/// stay usable for as long as it claims. Every SDK verifier (`sdk/typescript`,
/// `sdk/python`, `sdk/go`) enforces the same value, pinned by the shared
/// `sdk/fixtures/backend.json` conformance vectors. This bound is specific to
/// backend JWTs: fabric JWTs (`hng-auth+jwt`, including pre-signed URLs) have
/// their own lifetimes and only use [`validate_time`].
pub const BACKEND_MAX_LIFETIME_SECS: u64 = 300;
const _: () = assert!(BACKEND_TOKEN_LIFETIME_SECS <= BACKEND_MAX_LIFETIME_SECS);
const MAX_TOKEN: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicKey {
    pub kty: String,
    pub crv: String,
    pub alg: String,
    pub kid: String,
    pub x: String,
    pub y: String,
}

impl PublicKey {
    pub fn from_sec1(kid: String, bytes: &[u8]) -> Result<Self> {
        if kid.is_empty() || bytes.len() != 65 || bytes[0] != 4 {
            return Err(Error("invalid ES256 public key"));
        }
        Ok(Self {
            kty: "EC".into(),
            crv: "P-256".into(),
            alg: "ES256".into(),
            kid,
            x: B64.encode(&bytes[1..33]),
            y: B64.encode(&bytes[33..65]),
        })
    }

    pub fn bytes(&self) -> Result<Vec<u8>> {
        if self.kty != "EC" || self.crv != "P-256" || self.alg != "ES256" || self.kid.is_empty() {
            return Err(Error("invalid public key"));
        }
        let x = decode(&self.x)?;
        let y = decode(&self.y)?;
        if x.len() != 32 || y.len() != 32 {
            return Err(Error("invalid public key size"));
        }
        Ok([&[4u8][..], &x, &y].concat())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Jwks {
    pub keys: Vec<PublicKey>,
}

pub struct SigningKey {
    key: EcdsaKeyPair,
    kid: String,
}
impl SigningKey {
    pub fn generate() -> Result<Vec<u8>> {
        EcdsaKeyPair::generate_pkcs8(
            &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &SystemRandom::new(),
        )
        .map(|d| d.as_ref().to_vec())
        .map_err(|_| Error("key generation failed"))
    }
    pub fn from_pkcs8(kid: String, der: &[u8]) -> Result<Self> {
        if kid.is_empty() {
            return Err(Error("empty kid"));
        }
        let key = EcdsaKeyPair::from_pkcs8(
            &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            der,
            &SystemRandom::new(),
        )
        .map_err(|_| Error("invalid signing key"))?;
        Ok(Self { key, kid })
    }
    pub fn public_key(&self) -> PublicKey {
        let p = self.key.public_key().as_ref();
        PublicKey {
            kty: "EC".into(),
            crv: "P-256".into(),
            alg: "ES256".into(),
            kid: self.kid.clone(),
            x: B64.encode(&p[1..33]),
            y: B64.encode(&p[33..65]),
        }
    }
    pub fn sign(&self, typ: &str, claims: &impl Serialize) -> Result<String> {
        let h = Header {
            alg: "ES256".into(),
            typ: typ.into(),
            kid: self.kid.clone(),
        };
        let input = format!(
            "{}.{}",
            B64.encode(serde_json::to_vec(&h).map_err(|_| Error("encode header"))?),
            B64.encode(serde_json::to_vec(claims).map_err(|_| Error("encode claims"))?)
        );
        let sig = self
            .key
            .sign(&SystemRandom::new(), input.as_bytes())
            .map_err(|_| Error("sign failed"))?;
        Ok(format!("{input}.{}", B64.encode(sig.as_ref())))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    alg: String,
    typ: String,
    kid: String,
}

fn decode(s: &str) -> Result<Vec<u8>> {
    B64.decode(s).map_err(|_| Error("invalid base64url"))
}

/// The decoded claims are untrusted until `verify` succeeds. Key lookup is
/// restricted to registered (issuer, kid); token-provided URLs/JWKs are rejected.
pub struct Unverified<T> {
    pub claims: T,
    header: Header,
    input: String,
    signature: Vec<u8>,
}
impl<T: DeserializeOwned> Unverified<T> {
    pub fn parse(token: &str, typ: &str) -> Result<Self> {
        if token.len() > MAX_TOKEN {
            return Err(Error("token too large"));
        }
        let parts: Vec<_> = token.split('.').collect();
        if parts.len() != 3 {
            return Err(Error("invalid compact JWT"));
        }
        let header: Header =
            serde_json::from_slice(&decode(parts[0])?).map_err(|_| Error("invalid JOSE header"))?;
        if header.alg != "ES256" || header.typ != typ || header.kid.is_empty() {
            return Err(Error("invalid JWT algorithm/type/kid"));
        }
        let claims =
            serde_json::from_slice(&decode(parts[1])?).map_err(|_| Error("invalid claims"))?;
        Ok(Self {
            claims,
            header,
            input: format!("{}.{}", parts[0], parts[1]),
            signature: decode(parts[2])?,
        })
    }
    pub fn kid(&self) -> &str {
        &self.header.kid
    }
    pub fn verify(self, key: &PublicKey) -> Result<T> {
        if self.header.kid != key.kid {
            return Err(Error("unknown kid"));
        }
        signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, key.bytes()?)
            .verify(self.input.as_bytes(), &self.signature)
            .map_err(|_| Error("invalid signature"))?;
        Ok(self.claims)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BackendClaims {
    pub sub: String,
    pub name: String,
    pub aud: String,
    pub iat: u64,
    pub exp: u64,
}

pub fn validate_time(iat: u64, exp: u64, now: u64) -> Result<()> {
    if iat >= exp || iat > now.saturating_add(15) || exp <= now {
        return Err(Error("invalid token time"));
    }
    Ok(())
}
/// [`validate_time`] plus the backend-only [`BACKEND_MAX_LIFETIME_SECS`] bound.
pub fn validate_backend_time(iat: u64, exp: u64, now: u64) -> Result<()> {
    validate_time(iat, exp, now)?;
    if exp - iat > BACKEND_MAX_LIFETIME_SECS {
        return Err(Error("backend token lifetime too long"));
    }
    Ok(())
}
pub fn verify_backend(token: &str, keys: &Jwks, audience: &str, now: u64) -> Result<BackendClaims> {
    let jwt = Unverified::<BackendClaims>::parse(token, BACKEND_TYPE)?;
    let candidates: Vec<_> = keys.keys.iter().filter(|k| k.kid == jwt.kid()).collect();
    if candidates.len() != 1 {
        return Err(Error("unknown or ambiguous kid"));
    }
    let c = jwt.verify(candidates[0])?;
    validate_backend_time(c.iat, c.exp, now)?;
    if c.aud != audience || c.sub.is_empty() || c.name.is_empty() {
        return Err(Error("invalid backend identity"));
    }
    Ok(c)
}
