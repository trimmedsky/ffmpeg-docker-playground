//! Small reusable service/backend helpers. Services do not need the fabric model.
use crate::{
    jwt::{self, BackendClaims, Jwks, SigningKey},
    model::{Claims, Method},
    Error, Result,
};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

pub struct BackendVerifier {
    path: PathBuf,
    audience: String,
    cache: Mutex<Option<(Instant, Jwks)>>,
}
impl BackendVerifier {
    pub fn from_env() -> Result<Option<Self>> {
        let Some(path) = std::env::var_os("HNG_BACKEND_JWKS") else {
            return Ok(None);
        };
        let audience =
            std::env::var("HNG_SERVICE_ID").map_err(|_| Error("HNG_SERVICE_ID required"))?;
        if !crate::model::valid_id(&audience) {
            return Err(Error("invalid service ID"));
        }
        Ok(Some(Self::new(PathBuf::from(path), audience)))
    }
    pub fn verify_headers(&self, headers: &http::HeaderMap, now: u64) -> Result<BackendClaims> {
        let mut credentials = headers.get_all("authorization").iter();
        let token = credentials
            .next()
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(Error("missing credential"))?;
        if credentials.next().is_some() {
            return Err(Error("duplicate credential"));
        }
        self.verify(token, now)
    }

    pub fn new(path: impl Into<PathBuf>, audience: String) -> Self {
        Self {
            path: path.into(),
            audience,
            cache: Mutex::new(None),
        }
    }
    pub fn verify(&self, token: &str, now: u64) -> Result<BackendClaims> {
        let unverified = jwt::Unverified::<BackendClaims>::parse(token, jwt::BACKEND_TYPE)?;
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| Error("verifier lock poisoned"))?;
        if cache.as_ref().is_none_or(|(at, keys)| {
            at.elapsed() > Duration::from_secs(5)
                || !keys.keys.iter().any(|k| k.kid == unverified.kid())
        }) {
            *cache = None;
            let bytes = std::fs::read(&self.path).map_err(|_| Error("cannot read backend JWKS"))?;
            let keys: Jwks =
                serde_json::from_slice(&bytes).map_err(|_| Error("invalid backend JWKS"))?;
            let mut seen = std::collections::BTreeSet::new();
            for k in &keys.keys {
                k.bytes()?;
                if !seen.insert(&k.kid) {
                    return Err(Error("duplicate backend kid"));
                }
            }
            *cache = Some((Instant::now(), keys));
        }
        let (_, keys) = cache.as_ref().ok_or(Error("backend keys unavailable"))?;
        jwt::verify_backend(token, keys, &self.audience, now)
    }
}
pub struct ServiceIdentity {
    key: SigningKey,
    service: String,
    name: String,
    audience: String,
    cache: Mutex<Option<(u64, String)>>,
}
impl ServiceIdentity {
    pub fn from_env() -> Result<Option<Self>> {
        let Some(path) = std::env::var_os("HNG_SERVICE_KEY") else {
            return Ok(None);
        };
        let service =
            std::env::var("HNG_SERVICE_ID").map_err(|_| Error("HNG_SERVICE_ID required"))?;
        let kid =
            std::env::var("HNG_SERVICE_KID").map_err(|_| Error("HNG_SERVICE_KID required"))?;
        let audience = std::env::var("HNG_AUDIENCE").map_err(|_| Error("HNG_AUDIENCE required"))?;
        let name = std::env::var("HNG_SERVICE_NAME").unwrap_or_else(|_| service.clone());
        Self::from_der_file(Path::new(&path), kid, service, name, audience).map(Some)
    }

    pub fn new(key: SigningKey, service: String, name: String, audience: String) -> Result<Self> {
        if !crate::model::valid_id(&service) || name.is_empty() || !audience.starts_with("hng:") {
            return Err(Error("invalid service identity"));
        }
        Ok(Self {
            key,
            service,
            name,
            audience,
            cache: Mutex::new(None),
        })
    }
    pub fn from_der_file(
        path: &Path,
        kid: String,
        service: String,
        name: String,
        audience: String,
    ) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|_| Error("cannot read service key"))?;
        Self::new(
            SigningKey::from_pkcs8(kid, &bytes)?,
            service,
            name,
            audience,
        )
    }
    pub fn token(&self, now: u64) -> Result<String> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| Error("identity lock poisoned"))?;
        if let Some((until, token)) = &*cache {
            if now < until.saturating_sub(30) {
                return Ok(token.clone());
            }
        }
        let exp = now.checked_add(300).ok_or(Error("invalid clock"))?;
        let c = Claims {
            iss: format!("service:{}", self.service),
            sub: format!("service#{}", self.service),
            name: self.name.clone(),
            aud: self.audience.clone(),
            iat: now,
            exp,
            amr: std::collections::BTreeSet::from([Method::ServiceSignature]),
            request: None,
        };
        let token = self.key.sign(jwt::AUTH_TYPE, &c)?;
        *cache = Some((exp, token.clone()));
        Ok(token)
    }
}
