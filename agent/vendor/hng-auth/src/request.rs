use crate::{model::valid_id, Error, Result};
use http::{HeaderMap, Uri};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub service: String,
    pub method: String,
    pub path: String,
    pub query: String,
}
impl Target {
    pub fn validate(&self) -> Result<()> {
        if !valid_id(&self.service)
            || self.method.parse::<http::Method>().is_err()
            || self.method != self.method.to_ascii_uppercase()
        {
            return Err(Error("invalid target"));
        }
        validate_path(&self.path)?;
        if self.query.contains('#') || self.query.chars().any(|c| c.is_control()) {
            return Err(Error("invalid query"));
        }
        Ok(())
    }
    pub fn uri(&self) -> String {
        if self.query.is_empty() {
            self.path.clone()
        } else {
            format!("{}?{}", self.path, self.query)
        }
    }
    pub fn routed_uri(&self) -> String {
        format!("/services/{}{}", self.service, self.uri())
    }
}

/// Reject ambiguous separators / dot segments before ACL and forwarding.
/// Preserve the encoded form, including UTF-8, for exact presign comparison.
pub fn validate_path(path: &str) -> Result<()> {
    if !path.starts_with('/')
        || path.contains(['\\', '?', '#'])
        || path.chars().any(|c| c.is_control())
    {
        return Err(Error("invalid path"));
    }
    for piece in path.split('%').skip(1) {
        if let Some(code) = piece.get(..2).and_then(|v| u8::from_str_radix(v, 16).ok()) {
            if code.is_ascii_alphanumeric() || b"-._~".contains(&code) {
                return Err(Error("noncanonical path escape"));
            }
        }
    }
    let decoded = percent_decode(path)?;
    if decoded.contains(['\\', '%'])
        || decoded.matches('/').count() != path.matches('/').count()
        || decoded.split('/').any(|s| s == "." || s == "..")
        || decoded.chars().any(|c| c.is_control())
    {
        return Err(Error("ambiguous path"));
    }
    Ok(())
}
fn percent_decode(s: &str) -> Result<String> {
    let mut out = Vec::new();
    let mut i = 0;
    let b = s.as_bytes();
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() {
                return Err(Error("invalid percent escape"));
            }
            let h = (b[i + 1] as char)
                .to_digit(16)
                .ok_or(Error("invalid percent escape"))?;
            let l = (b[i + 2] as char)
                .to_digit(16)
                .ok_or(Error("invalid percent escape"))?;
            let decoded = (h * 16 + l) as u8;
            out.push(decoded);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| Error("invalid UTF-8"))
}

pub struct Extracted {
    pub token: String,
    pub query_carrier: bool,
    pub target: Target,
}
pub fn extract(headers: &HeaderMap, uri: &Uri, method: &str) -> Result<Extracted> {
    let route = uri
        .path()
        .strip_prefix("/services/")
        .ok_or(Error("invalid service route"))?;
    let (service, path) = route.split_once('/').ok_or(Error("missing service path"))?;
    extract_for(
        headers,
        uri,
        Target {
            service: service.into(),
            method: method.into(),
            path: format!("/{path}"),
            query: String::new(),
        },
    )
}
pub fn extract_for(headers: &HeaderMap, uri: &Uri, mut target: Target) -> Result<Extracted> {
    let auth: Vec<_> = headers
        .get_all(http::header::AUTHORIZATION)
        .iter()
        .collect();
    if auth.len() > 1 {
        return Err(Error("duplicate Authorization"));
    }
    let mut presign = None;
    let mut query = Vec::new();
    if let Some(q) = uri.query() {
        for part in q.split('&') {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            if percent_decode(key)? == "hng_presign" {
                if presign.is_some() {
                    return Err(Error("duplicate hng_presign"));
                }
                presign = Some(percent_decode(value)?);
            } else {
                query.push(part);
            }
        }
    }
    target.query = query.join("&");
    target.validate()?;
    let query_carrier = presign.is_some();
    let token = match (auth.first(), presign) {
        (Some(_), Some(_)) => return Err(Error("ambiguous credentials")),
        (Some(h), None) => h
            .to_str()
            .map_err(|_| Error("invalid Authorization"))?
            .strip_prefix("Bearer ")
            .ok_or(Error("expected Bearer"))?
            .to_string(),
        (None, Some(p)) => p,
        _ => return Err(Error("missing credentials")),
    };
    Ok(Extracted {
        token,
        query_carrier,
        target,
    })
}
