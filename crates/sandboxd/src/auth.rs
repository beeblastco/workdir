//! API-key authentication (spec §6.1 "Auth and API keys", §18 kill switches).
//!
//! Keys are presented as `Authorization: Bearer sk_live_...`. We store only the
//! SHA-256 hash; the plaintext is shown once at creation.
//!
//! The PTY route also takes a one-time terminal token minted by
//! `POST /v1/sandboxes/:id/pty-token` (see [`TerminalTokens`]).

use crate::store::Store;
use crate::usage::{ApiKey, Org, OrgStatus};
use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;

/// Fixed by the host; the caller cannot ask for longer.
pub const TERMINAL_TOKEN_TTL_SECS: i64 = 120;
/// Open (unused, unexpired) terminal tokens one org may hold at once.
pub const TERMINAL_TOKENS_PER_ORG: usize = 64;

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

/// One-time terminal tokens, held only in memory so neither the DB nor its
/// backups can open a terminal. Keyed by the token's SHA-256, like API keys, so
/// lookup timing says nothing about the token. A restart forgets open tokens.
#[derive(Default)]
pub struct TerminalTokens {
    open: Mutex<HashMap<String, TerminalGrant>>,
}

struct TerminalGrant {
    sandbox_id: String,
    org_id: String,
    expires_at: DateTime<Utc>,
}

impl TerminalTokens {
    /// Verify and consume `token` for the PTY of `sandbox_id`. Any presentation
    /// uses the token up. Every failure but suspension is `Invalid`.
    pub fn authenticate(
        &self,
        store: &Store,
        token: &str,
        sandbox_id: &str,
        now: DateTime<Utc>,
    ) -> AuthOutcome {
        let Some(org_id) = self.redeem(token, sandbox_id, now) else {
            return AuthOutcome::Invalid;
        };
        match store.get_sandbox(sandbox_id) {
            Ok(Some(sb)) if sb.org_id == org_id => {}
            _ => return AuthOutcome::Invalid,
        }
        match store.get_org(&org_id) {
            Ok(Some(org)) if org.status == OrgStatus::Suspended => AuthOutcome::Suspended,
            Ok(Some(_)) => AuthOutcome::Ok(AuthContext {
                org_id,
                admin: false,
            }),
            _ => AuthOutcome::Invalid,
        }
    }

    /// Mint a token for `sandbox_id` owned by `org_id`. `None` when the org
    /// already holds [`TERMINAL_TOKENS_PER_ORG`] open tokens.
    pub fn mint(
        &self,
        sandbox_id: &str,
        org_id: &str,
        now: DateTime<Utc>,
    ) -> Option<(String, DateTime<Utc>)> {
        let mut open = self.open.lock().unwrap();
        open.retain(|_, grant| grant.expires_at > now);
        if open.values().filter(|grant| grant.org_id == org_id).count() >= TERMINAL_TOKENS_PER_ORG {
            return None;
        }
        let token = crate::ids::terminal_token();
        let expires_at = now + Duration::seconds(TERMINAL_TOKEN_TTL_SECS);
        open.insert(
            hash_key(&token),
            TerminalGrant {
                sandbox_id: sandbox_id.to_string(),
                org_id: org_id.to_string(),
                expires_at,
            },
        );
        Some((token, expires_at))
    }

    fn redeem(&self, token: &str, sandbox_id: &str, now: DateTime<Utc>) -> Option<String> {
        let grant = self.open.lock().unwrap().remove(&hash_key(token))?;
        (grant.sandbox_id == sandbox_id && grant.expires_at > now).then_some(grant.org_id)
    }
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

    #[test]
    fn terminal_token_is_single_use_and_bound_to_its_sandbox() {
        let tokens = TerminalTokens::default();
        let now = Utc::now();
        let (token, expires_at) = tokens.mint("sbx_a", "org_a", now).unwrap();
        assert_eq!(expires_at - now, Duration::seconds(TERMINAL_TOKEN_TTL_SECS));
        assert_eq!(token.len(), "wtt_".len() + 64);
        assert_eq!(
            tokens.redeem(&token, "sbx_a", now).as_deref(),
            Some("org_a")
        );
        assert_eq!(tokens.redeem(&token, "sbx_a", now), None);

        let (token, _) = tokens.mint("sbx_a", "org_a", now).unwrap();
        assert_eq!(tokens.redeem(&token, "sbx_b", now), None);
        assert_eq!(
            tokens.redeem(&token, "sbx_a", now),
            None,
            "a wrong-sandbox try uses it up"
        );
        assert_eq!(tokens.redeem("wtt_unknown", "sbx_a", now), None);
    }

    #[test]
    fn terminal_token_expires_after_the_fixed_lifetime() {
        let tokens = TerminalTokens::default();
        let now = Utc::now();
        let (token, expires_at) = tokens.mint("sbx_a", "org_a", now).unwrap();
        assert_eq!(tokens.redeem(&token, "sbx_a", expires_at), None);
    }

    #[test]
    fn terminal_tokens_are_capped_per_org_until_they_expire() {
        let tokens = TerminalTokens::default();
        let now = Utc::now();
        for _ in 0..TERMINAL_TOKENS_PER_ORG {
            tokens.mint("sbx_a", "org_a", now).unwrap();
        }
        assert!(tokens.mint("sbx_a", "org_a", now).is_none());
        assert!(
            tokens.mint("sbx_b", "org_b", now).is_some(),
            "the cap is per org"
        );
        let later = now + Duration::seconds(TERMINAL_TOKEN_TTL_SECS);
        assert!(
            tokens.mint("sbx_a", "org_a", later).is_some(),
            "expired tokens are dropped"
        );
        assert_eq!(tokens.open.lock().unwrap().len(), 1);
    }
}
