//! API-key authentication (spec §6.1 "Auth and API keys", §18 kill switches).
//!
//! Keys are presented as `Authorization: Bearer sk_live_...`. We store only the
//! SHA-256 hash; the plaintext is shown once at creation.
//!
//! The PTY route also takes a terminal token, `tt1.<expires_at_ms>.<signature>`:
//! base64url HMAC-SHA256 over `terminal\0<sandbox_id>\0<expires_at_ms>`, keyed
//! with the stored key hash (hex text) of a key in the sandbox's org. It opens
//! that one sandbox's terminal until it expires, and nothing else.

use crate::store::Store;
use crate::usage::{ApiKey, Org, OrgStatus};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

const TERMINAL_TOKEN_PREFIX: &str = "tt1";

pub fn hash_key(plaintext: &str) -> String {
    let mut h = Sha256::new();
    h.update(plaintext.as_bytes());
    hex::encode(h.finalize())
}

/// Resolved identity for a request.
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub org_id: String,
    pub admin: bool,
}

#[derive(Debug)]
pub enum AuthOutcome {
    Ok(AuthContext),
    Missing,
    Invalid,
    /// Org suspended via kill switch (spec §18).
    Suspended,
}

/// Verify a presented bearer token against the store.
pub fn authenticate(store: &Store, bearer: Option<&str>) -> AuthOutcome {
    let token = match bearer {
        Some(t) if !t.is_empty() => t,
        _ => return AuthOutcome::Missing,
    };
    let hash = hash_key(token);
    let key: ApiKey = match store.get_api_key(&hash) {
        Ok(Some(k)) => k,
        _ => return AuthOutcome::Invalid,
    };
    if key.disabled {
        return AuthOutcome::Invalid;
    }
    org_outcome(store, key)
}

/// Verify a terminal token for `sandbox_id` (PTY route only). An unknown
/// sandbox and a bad signature both return `Invalid`, so ids cannot be probed.
pub fn authenticate_terminal(
    store: &Store,
    token: &str,
    sandbox_id: &str,
    now_ms: i64,
) -> AuthOutcome {
    let Some((expires_at, signature)) = parse_terminal_token(token) else {
        return AuthOutcome::Invalid;
    };
    if expires_at <= now_ms {
        return AuthOutcome::Invalid;
    }
    let org_id = match store.get_sandbox(sandbox_id) {
        Ok(Some(sb)) => sb.org_id,
        _ => return AuthOutcome::Invalid,
    };
    let keys = store.list_api_keys_for_org(&org_id).unwrap_or_default();
    let Some(key) = keys.into_iter().find(|k| {
        !k.disabled
            && terminal_mac(&k.key_hash, sandbox_id, expires_at)
                .verify_slice(&signature)
                .is_ok()
    }) else {
        return AuthOutcome::Invalid;
    };
    match org_outcome(store, key) {
        // The token never carries admin scope, even when signed with an admin key.
        AuthOutcome::Ok(ctx) => AuthOutcome::Ok(AuthContext {
            org_id: ctx.org_id,
            admin: false,
        }),
        other => other,
    }
}

fn org_outcome(store: &Store, key: ApiKey) -> AuthOutcome {
    match store.get_org(&key.org_id) {
        // Admin keys are never locked out by suspension — otherwise suspending
        // an org (or the admin's own org) would lock the operator out of the
        // very endpoint that lifts the suspension.
        Ok(Some(org)) if org.status == OrgStatus::Suspended && !key.admin => AuthOutcome::Suspended,
        Ok(Some(_)) => AuthOutcome::Ok(AuthContext {
            org_id: key.org_id,
            admin: key.admin,
        }),
        _ => AuthOutcome::Invalid,
    }
}

fn parse_terminal_token(token: &str) -> Option<(i64, Vec<u8>)> {
    let mut parts = token.split('.');
    let (Some(TERMINAL_TOKEN_PREFIX), Some(expiry), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let expires_at = expiry.parse::<i64>().ok()?;
    let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
    Some((expires_at, signature))
}

/// `verify_slice` on the result compares in constant time.
fn terminal_mac(key_hash: &str, sandbox_id: &str, expires_at: i64) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key_hash.as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(format!("terminal\0{sandbox_id}\0{expires_at}").as_bytes());
    mac
}

/// Seed the bootstrap org + admin key on first boot. Returns the plaintext key
/// if a new one was generated (so the installer can print it once).
pub fn bootstrap(
    store: &Store,
    org_id: &str,
    provided_key: Option<&str>,
) -> anyhow::Result<Option<String>> {
    if store.get_org(org_id)?.is_none() {
        store.put_org(&Org {
            id: org_id.to_string(),
            name: "admin".to_string(),
            status: OrgStatus::Active,
            prepaid_credits_usd: 1_000_000_000.0, // admin org is effectively uncapped
            spent_usd: 0.0,
            quota_units: 0.0, // convention: <= 0 means unlimited (see Org::quota_unlimited)
            created_at: chrono::Utc::now(),
        })?;
    }
    // If an admin key already exists for this org, do nothing.
    // (We can't enumerate by org cheaply; instead store a marker in meta.)
    let marker = format!("admin_key_seeded:{org_id}");
    if store.get_meta(&marker)?.is_some() {
        return Ok(None);
    }
    let plaintext = provided_key
        .filter(|k| !k.is_empty())
        .map(|k| k.to_string())
        .unwrap_or_else(crate::ids::api_key);
    store.put_api_key(&ApiKey {
        key_hash: hash_key(&plaintext),
        org_id: org_id.to_string(),
        name: "bootstrap-admin".to_string(),
        admin: true,
        disabled: false,
        created_at: chrono::Utc::now(),
    })?;
    store.set_meta(&marker, "1")?;
    Ok(Some(plaintext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_is_stable() {
        assert_eq!(hash_key("abc"), hash_key("abc"));
        assert_ne!(hash_key("abc"), hash_key("abd"));
    }

    #[test]
    fn bootstrap_then_authenticate() {
        let store = Store::open_in_memory().unwrap();
        let key = bootstrap(&store, "org_admin", None).unwrap().unwrap();
        match authenticate(&store, Some(&key)) {
            AuthOutcome::Ok(ctx) => {
                assert_eq!(ctx.org_id, "org_admin");
                assert!(ctx.admin);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        assert!(matches!(
            authenticate(&store, Some("nope")),
            AuthOutcome::Invalid
        ));
        assert!(matches!(authenticate(&store, None), AuthOutcome::Missing));
    }

    /// Pinned to the same vector as Broods core's signer, so both sides agree on the bytes.
    #[test]
    fn terminal_token_matches_shared_vector() {
        let token = "tt1.4102444800000.6Skbp31ryI8i4j9JJotL4ED4JAHrgNEGKNMm8uiopXc";
        let (expires_at, signature) = parse_terminal_token(token).unwrap();
        assert_eq!(expires_at, 4_102_444_800_000);
        let mac = terminal_mac(&hash_key("tenant-key"), "sbx_handler", expires_at);
        assert!(mac.verify_slice(&signature).is_ok());
        let other = terminal_mac(&hash_key("tenant-key"), "sbx_other", expires_at);
        assert!(other.verify_slice(&signature).is_err());
    }

    #[test]
    fn terminal_token_parse_rejects_malformed() {
        for token in [
            "",
            "tt1",
            "tt1.123",
            "tt2.123.AAAA",
            "tt1.soon.AAAA",
            "tt1.123.not base64!",
            "tt1.123.AAAA.extra",
        ] {
            assert!(parse_terminal_token(token).is_none(), "{token}");
        }
    }
}
