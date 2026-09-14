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
