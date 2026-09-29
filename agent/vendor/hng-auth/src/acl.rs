use crate::{
    model::{Claims, Method, Service},
    request::{validate_path, Target},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Internet,
    Service(String),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub source: Source,
    pub destination: String,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub methods: BTreeSet<String>,
    #[serde(default)]
    pub required_authentication: BTreeSet<Method>,
    #[serde(default)]
    pub allow_presigned: bool,
}
impl Rule {
    pub fn validate(&self, services: &BTreeMap<String, Service>) -> Result<()> {
        if !services.contains_key(&self.destination)
            || matches!(&self.source, Source::Service(s) if !services.contains_key(s))
        {
            return Err(Error("unknown ACL service"));
        }
        if let Some(p) = &self.path_prefix {
            validate_path(p)?;
            if p.len() > 1 && p.ends_with('/') {
                return Err(Error("ACL prefix trailing slash"));
            }
        }
        if self
            .methods
            .iter()
            .any(|m| m.parse::<http::Method>().is_err() || *m != m.to_ascii_uppercase())
        {
            return Err(Error("invalid ACL method"));
        }
        Ok(())
    }
}
pub fn source(c: &Claims) -> Source {
    c.sub
        .strip_prefix("service#")
        .map(|s| Source::Service(s.into()))
        .unwrap_or(Source::Internet)
}
fn prefix_matches(prefix: &str, path: &str) -> bool {
    prefix == "/"
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|r| r.starts_with('/'))
}
/// Select path before evaluating methods/authentication. A specific prefix
/// cannot fall back to a broad rule because a method or Passkey is missing.
pub fn candidates<'a>(
    rules: &'a [Rule],
    source: &Source,
    target: &Target,
    presigned: bool,
) -> Vec<&'a Rule> {
    let matching: Vec<_> = rules
        .iter()
        .filter(|r| {
            r.source == *source
                && r.destination == target.service
                && (!presigned || r.allow_presigned)
                && prefix_matches(r.path_prefix.as_deref().unwrap_or("/"), &target.path)
        })
        .collect();
    let longest = matching
        .iter()
        .map(|r| r.path_prefix.as_deref().unwrap_or("/").len())
        .max();
    matching
        .into_iter()
        .filter(|r| {
            Some(r.path_prefix.as_deref().unwrap_or("/").len()) == longest
                && (r.methods.is_empty() || r.methods.contains(&target.method))
        })
        .collect()
}
pub fn authorize(rules: &[Rule], claims: &Claims, target: &Target) -> Result<()> {
    if candidates(
        rules,
        &source(claims),
        target,
        claims.amr.contains(&Method::Presigned),
    )
    .iter()
    .any(|r| r.required_authentication.is_subset(&claims.amr))
    {
        Ok(())
    } else {
        Err(Error("ACL denied"))
    }
}
