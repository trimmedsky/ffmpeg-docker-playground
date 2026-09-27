//! HNG authentication contracts. Transport identity is supplied by the TLS
//! acceptor, never by a request header. Authorization is a separate decision.
#![forbid(unsafe_code)]

pub mod acl;
pub mod jwt;
pub mod model;
pub mod request;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Error(pub &'static str);
pub type Result<T> = std::result::Result<T, Error>;

pub mod client;
