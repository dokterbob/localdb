//! Token minting/verification and PKCE S256 (D1).
//!
//! Secrets are 32 raw bytes from `OsRng`, `ldb_`-prefixed, base64url
//! (no padding) encoded, and shown to the caller exactly once. Only the
//! blake3 hash of the secret is ever persisted (`AuthStore`) — the
//! plaintext never touches storage.

use std::time::{SystemTime, UNIX_EPOCH};

use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// Prefix on every minted opaque secret — access/refresh tokens, API keys,
/// and invite secrets all share it.
pub const TOKEN_PREFIX: &str = "ldb_";

/// Access token TTL (D1): 1 hour.
pub const ACCESS_TOKEN_TTL_SECS: i64 = 60 * 60;

/// Refresh token TTL (D1): 30 days, rotated on use.
pub const REFRESH_TOKEN_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// Authorization-code TTL (T4, specs/05-surfaces.md §3.1 R5): 10 minutes,
/// single-use.
pub const AUTH_CODE_TTL_SECS: i64 = 10 * 60;

/// A freshly minted secret: the plaintext (shown once to the caller) and its
/// blake3 hash (the only form persisted at rest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedSecret {
    pub secret: String,
    pub hash: String,
}

/// Mint a new opaque bearer secret: 32 bytes from `OsRng`, `ldb_`-prefixed,
/// base64url (no padding) encoded. Returns both the plaintext (show-once)
/// and its blake3 hash for persistence.
pub fn mint_secret() -> MintedSecret {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let secret = format!("{TOKEN_PREFIX}{}", base64url_encode(&bytes));
    let hash = hash_secret(&secret);
    MintedSecret { secret, hash }
}

/// blake3 hex hash of a secret — the only form ever persisted at rest.
pub fn hash_secret(secret: &str) -> String {
    blake3::hash(secret.as_bytes()).to_hex().to_string()
}

/// Verify a presented secret against a persisted hash by re-hashing and
/// comparing.
pub fn verify_secret(secret: &str, hash: &str) -> bool {
    hash_secret(secret) == hash
}

/// PKCE S256 verification (RFC 7636 §4.6): `challenge` must equal
/// base64url(no-pad)(SHA-256(verifier)).
pub fn verify_pkce_s256(verifier: &str, challenge: &str) -> bool {
    let digest = Sha256::digest(verifier.as_bytes());
    base64url_encode(&digest) == challenge
}

/// Generate a fresh PKCE (RFC 7636) verifier/challenge pair: `verifier` is
/// 32 random bytes from `OsRng`, base64url (no-pad) encoded (43 chars —
/// within the 43-128 char range RFC 7636 §4.1 requires); `challenge` is
/// `S256(verifier)`, guaranteed to round-trip against [`verify_pkce_s256`]
/// since both share the same `base64url_encode`. Used by `localdb login`
/// (`cli::cmds::login`) to drive the authorization-code + PKCE flow.
pub fn generate_pkce_pair() -> (String, String) {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let verifier = base64url_encode(&bytes);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64url_encode(&digest);
    (verifier, challenge)
}

const B64URL_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Base64url encoding without padding (RFC 4648 §5).
fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64URL_ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(B64URL_ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64URL_ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64URL_ALPHABET[(n & 0x3F) as usize] as char);
        }
    }
    out
}

/// Current time as Unix epoch seconds.
///
/// Deliberately *real* wall-clock time, even under `cfg(test)` — unlike
/// `crate::ingestion::now_rfc3339`, which freezes to a fixed string
/// crate-wide under test. Token expiry tests need small positive/negative
/// offsets from "now", not a single frozen instant.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Format a Unix epoch-seconds value as an RFC 3339 UTC timestamp
/// (`YYYY-MM-DDTHH:MM:SSZ`, no sub-second precision).
fn format_unix(secs: i64) -> String {
    let secs = secs.max(0) as u64;
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;

    // Civil-from-days (Howard Hinnant's algorithm): days-since-epoch -> y/m/d.
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };

    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// An RFC 3339 UTC timestamp `delta_secs` from now. Negative deltas produce
/// a timestamp in the past — used to mint already-expired tokens in tests.
pub fn rfc3339_from_now(delta_secs: i64) -> String {
    format_unix(now_unix() + delta_secs)
}

/// Is the given RFC 3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) in the past
/// relative to now?
///
/// Relies on lexicographic string ordering matching chronological ordering
/// for this fixed-width, zero-padded format — the same convention
/// `core::store::MetadataFilter::FetchedAfter`/`FetchedBefore` rely on.
pub fn is_expired(expires_at: &str) -> bool {
    expires_at < rfc3339_from_now(0).as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_secret_has_ldb_prefix() {
        let minted = mint_secret();
        assert!(minted.secret.starts_with(TOKEN_PREFIX));
    }

    #[test]
    fn mint_secret_body_is_43_chars_from_32_bytes() {
        // 32 bytes, base64url no-pad: ceil(32*8/6) = 43 chars.
        let minted = mint_secret();
        assert_eq!(minted.secret.len(), TOKEN_PREFIX.len() + 43);
    }

    #[test]
    fn mint_secret_hash_matches_hash_secret() {
        let minted = mint_secret();
        assert_eq!(minted.hash, hash_secret(&minted.secret));
    }

    #[test]
    fn two_minted_secrets_differ() {
        let a = mint_secret();
        let b = mint_secret();
        assert_ne!(a.secret, b.secret);
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn verify_secret_round_trip() {
        let minted = mint_secret();
        assert!(verify_secret(&minted.secret, &minted.hash));
    }

    #[test]
    fn verify_secret_rejects_wrong_secret() {
        let minted = mint_secret();
        assert!(!verify_secret("ldb_totally-wrong-secret", &minted.hash));
    }

    #[test]
    fn pkce_s256_round_trip_rfc7636_test_vector() {
        // RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(verify_pkce_s256(verifier, challenge));
    }

    #[test]
    fn pkce_s256_rejects_wrong_challenge() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert!(!verify_pkce_s256(verifier, "not-the-right-challenge"));
    }

    #[test]
    fn pkce_s256_rejects_wrong_verifier() {
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(!verify_pkce_s256("some-other-verifier", challenge));
    }

    #[test]
    fn is_expired_true_for_past_timestamp() {
        assert!(is_expired(&rfc3339_from_now(-10)));
    }

    #[test]
    fn is_expired_false_for_future_timestamp() {
        assert!(!is_expired(&rfc3339_from_now(3600)));
    }

    #[test]
    fn generate_pkce_pair_round_trips_with_verify() {
        let (verifier, challenge) = generate_pkce_pair();
        assert!(verify_pkce_s256(&verifier, &challenge));
        assert_eq!(verifier.len(), 43, "32 bytes base64url no-pad is 43 chars");
    }

    #[test]
    fn generate_pkce_pair_differs_each_call() {
        let (v1, _) = generate_pkce_pair();
        let (v2, _) = generate_pkce_pair();
        assert_ne!(v1, v2);
    }
}
