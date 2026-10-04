//! The verification keys: a JWKS file (RFC 7517) holding ES256 public keys.
//!
//! The file is the only source of keys. Keys or key URLs inside a token are never
//! consulted (the JOSE header rejects `jwk`, `jku`, `x5u` and `x5c`; see `jwt.rs`).
//! The file is checked on every lookup and re-read when it changes, and at least every
//! [`REFRESH`], so keys can be rotated by replacing the file without a restart.
//! A file that cannot be read or parsed rejects every request until it is fixed.

use super::Rejection;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{agreement, rand::SystemRandom};
use serde::Deserialize;
use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant, SystemTime},
};

/// Upper bound on the JWKS file size, so a wrong path cannot make the agent read a
/// large file into memory.
const MAX_FILE_BYTES: u64 = 1 << 20;
const MAX_KEYS: usize = 64;
/// Re-read the file at least this often even if its metadata looks unchanged.
const REFRESH: Duration = Duration::from_secs(5);

/// A P-256 public key as an uncompressed SEC1 point (`0x04 || x || y`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyingKey {
    pub kid: String,
    pub sec1: [u8; 65],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    keys: Vec<Jwk>,
}

/// Only the members needed for an EC public key are accepted. `alg` and `use` are
/// optional, but if present they must agree with ES256 signature verification.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Jwk {
    kty: String,
    crv: String,
    x: String,
    y: String,
    kid: String,
    alg: Option<String>,
    #[serde(rename = "use")]
    use_: Option<String>,
    /// Present only so that a private key gets a precise error instead of a generic one.
    d: Option<serde::de::IgnoredAny>,
}

fn coordinate(value: &str) -> Result<[u8; 32], String> {
    URL_SAFE_NO_PAD
        .decode(value)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| "EC coordinates must be 32-byte base64url values".to_string())
}

/// ring validates the peer key of an ECDH agreement completely (on the curve, not the
/// point at infinity). Running one ephemeral agreement is the public API for that
/// check, so an invalid key is reported when the file is loaded, not on first use.
fn valid_point(sec1: &[u8; 65]) -> bool {
    let rng = SystemRandom::new();
    let Ok(ephemeral) = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)
    else {
        return false;
    };
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, sec1);
    agreement::agree_ephemeral(ephemeral, &peer, |_| ()).is_ok()
}

/// Parses and validates a whole JWKS document. Any invalid key rejects the document.
pub fn parse(bytes: &[u8]) -> Result<Vec<VerifyingKey>, String> {
    let document: Document = serde_json::from_slice(bytes)
        .map_err(|e| format!("not a JWKS document with only EC public keys: {e}"))?;
    if document.keys.len() > MAX_KEYS {
        return Err(format!("more than {MAX_KEYS} keys"));
    }
    let mut seen = HashSet::new();
    let mut keys = Vec::with_capacity(document.keys.len());
    for jwk in document.keys {
        if jwk.d.is_some() {
            return Err("contains a private key (member \"d\"); publish public keys only".into());
        }
        if jwk.kid.is_empty() || jwk.kid.chars().any(char::is_control) {
            return Err("every key needs a non-empty \"kid\"".into());
        }
        if jwk.kty != "EC" || jwk.crv != "P-256" {
            return Err(format!("key {:?} is not an EC P-256 key", jwk.kid));
        }
        if jwk.alg.as_deref().is_some_and(|alg| alg != "ES256") {
            return Err(format!(
                "key {:?} declares an alg other than ES256",
                jwk.kid
            ));
        }
        if jwk.use_.as_deref().is_some_and(|u| u != "sig") {
            return Err(format!("key {:?} declares a use other than sig", jwk.kid));
        }
        let mut sec1 = [0u8; 65];
        sec1[0] = 4;
        sec1[1..33].copy_from_slice(&coordinate(&jwk.x)?);
        sec1[33..].copy_from_slice(&coordinate(&jwk.y)?);
        if !valid_point(&sec1) {
            return Err(format!("key {:?} is not a valid P-256 point", jwk.kid));
        }
        if !seen.insert(jwk.kid.clone()) {
            // Two keys with one kid make the lookup ambiguous.
            return Err(format!("duplicate kid {:?}", jwk.kid));
        }
        keys.push(VerifyingKey { kid: jwk.kid, sec1 });
    }
    Ok(keys)
}

/// What identifies one version of the file without reading it. A replacement by
/// rename changes the inode; an in-place rewrite changes the length or mtime.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    len: u64,
    modified: Option<SystemTime>,
    inode: u64,
}

impl Fingerprint {
    fn of(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(metadata);
        #[cfg(not(unix))]
        let inode = 0;
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            inode,
        }
    }
}

struct Loaded {
    at: Instant,
    fingerprint: Fingerprint,
    keys: Vec<VerifyingKey>,
}

fn load(path: &Path) -> Result<Loaded, String> {
    let file = fs::File::open(path).map_err(|e| format!("cannot open: {e}"))?;
    let metadata = file.metadata().map_err(|e| format!("cannot stat: {e}"))?;
    if !metadata.is_file() {
        return Err("not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read: {e}"))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!("larger than {MAX_FILE_BYTES} bytes"));
    }
    Ok(Loaded {
        at: Instant::now(),
        fingerprint: Fingerprint::of(&metadata),
        keys: parse(&bytes)?,
    })
}

pub struct KeyStore {
    path: PathBuf,
    refresh: Duration,
    loaded: Mutex<Option<Loaded>>,
}

impl KeyStore {
    /// Loads the file once so that a missing, invalid or empty key set stops startup.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        Self::with_refresh(path.into(), REFRESH)
    }

    fn with_refresh(path: PathBuf, refresh: Duration) -> Result<Self, String> {
        let loaded = load(&path).map_err(|e| format!("JWKS file {}: {e}", path.display()))?;
        if loaded.keys.is_empty() {
            return Err(format!("JWKS file {} contains no keys", path.display()));
        }
        Ok(Self {
            path,
            refresh,
            loaded: Mutex::new(Some(loaded)),
        })
    }

    pub fn key_count(&self) -> usize {
        self.loaded
            .lock()
            .ok()
            .and_then(|loaded| loaded.as_ref().map(|l| l.keys.len()))
            .unwrap_or(0)
    }

    /// The key whose `kid` equals `kid`. Kids are unique within a loaded file.
    pub fn key(&self, kid: &str) -> Result<VerifyingKey, Rejection> {
        let mut loaded = self.loaded.lock().map_err(|_| Rejection::KeysUnavailable)?;
        let current = match fs::metadata(&self.path) {
            Ok(metadata) => Fingerprint::of(&metadata),
            Err(error) => {
                // A removed file revokes every key.
                eprintln!("JWKS file {}: cannot stat: {error}", self.path.display());
                return Err(Rejection::KeysUnavailable);
            }
        };
        let stale = loaded
            .as_ref()
            .is_none_or(|l| l.fingerprint != current || l.at.elapsed() >= self.refresh);
        if stale {
            *loaded = None;
            match load(&self.path) {
                Ok(fresh) => *loaded = Some(fresh),
                Err(error) => {
                    eprintln!("JWKS file {}: {error}", self.path.display());
                    return Err(Rejection::KeysUnavailable);
                }
            }
        }
        loaded
            .as_ref()
            .and_then(|l| l.keys.iter().find(|k| k.kid == kid))
            .cloned()
            .ok_or(Rejection::UnknownKey)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::testing::Signer;
    use serde_json::json;

    fn write(path: &Path, value: &serde_json::Value) {
        // Replace atomically, as a rotation would.
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, serde_json::to_vec(value).unwrap()).unwrap();
        fs::rename(&temporary, path).unwrap();
    }

    fn rejects(value: serde_json::Value, expected: &str) {
        let error = parse(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(error.contains(expected), "{error:?} lacks {expected:?}");
    }

    #[test]
    fn accepts_public_keys_with_optional_alg_and_use() {
        let a = Signer::new("a");
        let mut b = a.jwk_value("b");
        b["alg"] = json!("ES256");
        b["use"] = json!("sig");
        let keys = parse(&serde_json::to_vec(&json!({"keys":[a.jwk(), b]})).unwrap()).unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].sec1, keys[1].sec1);
        assert_eq!(keys[0].sec1, a.public_sec1());
    }

    #[test]
    fn rejects_every_unsafe_or_ambiguous_key_set() {
        let s = Signer::new("k");
        let with = |field: &str, value: serde_json::Value| {
            let mut key = s.jwk();
            key[field] = value;
            json!({"keys":[key]})
        };
        rejects(json!({"keys":[s.jwk(), s.jwk()]}), "duplicate kid");
        rejects(with("d", json!("AAAA")), "private key");
        rejects(with("kty", json!("RSA")), "not an EC P-256 key");
        rejects(with("crv", json!("P-384")), "not an EC P-256 key");
        rejects(with("alg", json!("ES384")), "alg other than ES256");
        rejects(with("alg", json!("none")), "alg other than ES256");
        rejects(with("use", json!("enc")), "use other than sig");
        rejects(with("kid", json!("")), "non-empty \"kid\"");
        rejects(with("x", json!("AAAA")), "32-byte");
        rejects(
            with("x", json!(URL_SAFE_NO_PAD.encode([0u8; 32]) + "=")),
            "32-byte",
        );
        rejects(
            with("y", json!(URL_SAFE_NO_PAD.encode([1u8; 32]))),
            "valid P-256 point",
        );
        rejects(with("x5u", json!("https://example.org/key")), "not a JWKS");
        rejects(with("key_ops", json!(["verify"])), "not a JWKS");
        let mut missing = s.jwk();
        missing.as_object_mut().unwrap().remove("kid");
        rejects(json!({"keys":[missing]}), "not a JWKS");
        rejects(json!({"keys":[s.jwk()],"extra":1}), "not a JWKS");
        rejects(json!([s.jwk()]), "not a JWKS");
        let many: Vec<_> = (0..65).map(|n| s.jwk_value(&n.to_string())).collect();
        rejects(json!({ "keys": many }), "more than 64 keys");
    }

    #[test]
    fn startup_requires_a_readable_non_empty_key_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jwks.json");
        assert!(KeyStore::open(&path).err().unwrap().contains("cannot open"));
        write(&path, &json!({"keys":[]}));
        assert!(KeyStore::open(&path).err().unwrap().contains("no keys"));
        fs::write(&path, vec![b' '; MAX_FILE_BYTES as usize + 1]).unwrap();
        assert!(KeyStore::open(&path).err().unwrap().contains("larger than"));
        assert!(KeyStore::open(dir.path()).is_err());
        write(&path, &json!({"keys":[Signer::new("a").jwk()]}));
        assert_eq!(KeyStore::open(&path).unwrap().key_count(), 1);
    }

    #[test]
    fn follows_rotation_and_fails_closed_on_a_broken_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jwks.json");
        let (a, b) = (Signer::new("a"), Signer::new("b"));
        write(&path, &json!({"keys":[a.jwk()]}));
        // A long refresh interval proves that changes are seen through the metadata.
        let store = KeyStore::with_refresh(path.clone(), Duration::from_secs(3600)).unwrap();
        assert_eq!(store.key("a").unwrap().sec1, a.public_sec1());
        assert_eq!(store.key("b"), Err(Rejection::UnknownKey));

        // Publishing a new key makes it usable at once.
        write(&path, &json!({"keys":[a.jwk(), b.jwk()]}));
        assert_eq!(store.key("b").unwrap().sec1, b.public_sec1());
        // Retiring a key revokes it at once, too.
        write(&path, &json!({"keys":[b.jwk()]}));
        assert_eq!(store.key("a"), Err(Rejection::UnknownKey));

        // A broken or missing file rejects everything; it never keeps stale keys.
        fs::write(&path, b"{not json").unwrap();
        assert_eq!(store.key("b"), Err(Rejection::KeysUnavailable));
        write(&path, &json!({"keys":[b.jwk(), b.jwk()]}));
        assert_eq!(store.key("b"), Err(Rejection::KeysUnavailable));
        fs::remove_file(&path).unwrap();
        assert_eq!(store.key("b"), Err(Rejection::KeysUnavailable));
        write(&path, &json!({"keys":[b.jwk()]}));
        assert_eq!(store.key("b").unwrap().sec1, b.public_sec1());
        // An emptied file is valid and revokes every key.
        write(&path, &json!({"keys":[]}));
        assert_eq!(store.key("b"), Err(Rejection::UnknownKey));
    }

    #[test]
    fn rereads_periodically_even_if_metadata_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jwks.json");
        write(&path, &json!({"keys":[Signer::new("a").jwk()]}));
        let store = KeyStore::with_refresh(path.clone(), Duration::ZERO).unwrap();
        // Swap the cached copy for a different one without touching the file: a zero
        // refresh interval must make the next lookup read the file again.
        store.loaded.lock().unwrap().as_mut().unwrap().keys.clear();
        assert!(store.key("a").is_ok());
    }
}
