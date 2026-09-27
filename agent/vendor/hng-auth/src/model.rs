use crate::{
    acl::Rule,
    jwt::{self, PublicKey, Unverified},
    request::Target,
    Error, Result,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    ClientCertificate,
    Passkey,
    ServiceSignature,
    Presigned,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Claims {
    pub iss: String,
    pub sub: String,
    pub name: String,
    pub aud: String,
    pub iat: u64,
    pub exp: u64,
    pub amr: BTreeSet<Method>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<Target>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    pub services: BTreeSet<String>,
    pub server_name: String,
    pub certificate_serials: BTreeSet<String>,
    pub keys: Vec<PublicKey>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub name: String,
    pub keys: Vec<PublicKey>,
    pub domain: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub revision: u64,
    pub audience: String,
    pub hosts: BTreeMap<String, Host>,
    pub services: BTreeMap<String, Service>,
    pub gateway_keys: Vec<PublicKey>,
    pub acl: Vec<Rule>,
}

/// These variants must only be constructed from the accepted listener / mTLS
/// identity. Forwarded headers never set them.
#[derive(Debug, Clone, Copy)]
pub enum Connection<'a> {
    Local(&'a str),
    Peer(&'a str),
    Gateway,
}

pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
}
impl Snapshot {
    pub fn validate(&self) -> Result<()> {
        if !self.audience.starts_with("hng:") || self.audience.len() <= 4 {
            return Err(Error("invalid audience"));
        }
        for (id, s) in &self.services {
            if !valid_id(id) || s.name.is_empty() {
                return Err(Error("invalid service"));
            }
            validate_keys(&s.keys)?;
        }
        let mut serials = BTreeSet::new();
        let mut names = BTreeSet::new();
        for (id, h) in &self.hosts {
            if !valid_id(id)
                || h.server_name.is_empty()
                || !names.insert(&h.server_name)
                || h.certificate_serials.is_empty()
            {
                return Err(Error("invalid host"));
            }
            for serial in &h.certificate_serials {
                if serial.is_empty()
                    || !serial.bytes().all(|b| b.is_ascii_hexdigit())
                    || !serials.insert(serial)
                {
                    return Err(Error("invalid or duplicate certificate serial"));
                }
            }
            if h.services.iter().any(|s| !self.services.contains_key(s)) {
                return Err(Error("unknown host service"));
            }
            validate_keys(&h.keys)?;
        }
        validate_keys(&self.gateway_keys)?;
        for r in &self.acl {
            r.validate(&self.services)?;
        }
        Ok(())
    }
    pub fn member(&self, host: &str, service: &str) -> bool {
        self.hosts
            .get(host)
            .is_some_and(|h| h.services.contains(service))
    }
    pub fn verify(
        &self,
        token: &str,
        connection: Connection<'_>,
        target: &Target,
        query_carrier: bool,
        now: u64,
    ) -> Result<Claims> {
        let jwt = Unverified::<Claims>::parse(token, jwt::AUTH_TYPE)?;
        let iss = &jwt.claims.iss;
        let keys = if iss == "gateway" {
            &self.gateway_keys
        } else if let Some(s) = iss.strip_prefix("service:") {
            &self
                .services
                .get(s)
                .ok_or(Error("unknown service issuer"))?
                .keys
        } else if let Some(h) = iss.strip_prefix("connector:") {
            &self
                .hosts
                .get(h)
                .ok_or(Error("unknown connector issuer"))?
                .keys
        } else {
            return Err(Error("unknown issuer"));
        };
        let candidates: Vec<_> = keys.iter().filter(|k| k.kid == jwt.kid()).collect();
        if candidates.len() != 1 {
            return Err(Error("unknown or ambiguous issuer key"));
        }
        let c = jwt.verify(candidates[0])?;
        jwt::validate_time(c.iat, c.exp, now)?;
        if c.aud != self.audience || c.sub.is_empty() || c.name.is_empty() {
            return Err(Error("invalid audience or identity"));
        }
        if !self.services.contains_key(&target.service) {
            return Err(Error("unknown destination"));
        }
        if let Connection::Local(h) | Connection::Peer(h) = connection {
            if !self.hosts.contains_key(h) {
                return Err(Error("unknown connection host"));
            }
        }
        if let Some(s) = c.iss.strip_prefix("service:") {
            if c.sub != format!("service#{s}")
                || c.amr != BTreeSet::from([Method::ServiceSignature])
            {
                return Err(Error("invalid service claims"));
            }
            if let Connection::Local(h) | Connection::Peer(h) = connection {
                if !self.member(h, s) {
                    return Err(Error("source host cannot claim service"));
                }
            }
        } else if let Some(h) = c.iss.strip_prefix("connector:") {
            let s = c
                .sub
                .strip_prefix("service#")
                .ok_or(Error("invalid delegated subject"))?;
            if !self.member(h, s)
                || c.amr != BTreeSet::from([Method::Presigned])
                || c.request.as_ref().is_none_or(|r| r.service != s)
            {
                return Err(Error("invalid presigned claims"));
            }
        } else if !matches!(connection, Connection::Gateway)
            || c.sub.starts_with("service#")
            || !c.amr.contains(&Method::ClientCertificate)
            || c.amr
                .iter()
                .any(|a| !matches!(a, Method::ClientCertificate | Method::Passkey))
        {
            return Err(Error("invalid gateway claims or connection"));
        }
        if query_carrier && !c.amr.contains(&Method::Presigned) {
            return Err(Error("query requires presigned claims"));
        }
        if c.request.as_ref().is_some_and(|r| r != target) {
            return Err(Error("request restriction mismatch"));
        }
        Ok(c)
    }
}
fn validate_keys(keys: &[PublicKey]) -> Result<()> {
    let mut kids = BTreeSet::new();
    for k in keys {
        k.bytes()?;
        if !kids.insert(&k.kid) {
            return Err(Error("duplicate kid"));
        }
    }
    Ok(())
}
