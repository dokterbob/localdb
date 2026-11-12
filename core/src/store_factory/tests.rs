use super::*;

#[test]
fn default_store_row_uses_explicit_context_for_libsql_row() -> Result<(), Error> {
    let policy = IndexingPolicyConfig::default();

    let row = default_store_row("test", StoreVisibility::Private, &policy, "v1")?;

    assert_eq!(row.id.len(), 26);
    assert!(row.id.chars().all(|c| c.is_ascii_alphanumeric()));
    assert_eq!(row.name, "test");
    assert_eq!(row.visibility, StoreVisibility::Private);
    assert_eq!(row.backend, "libsql");
    assert_eq!(row.indexing_policy, serde_json::to_string(&policy).unwrap());
    assert_eq!(row.policy_version, "v1");
    assert_eq!(row.created_at, "2026-06-10T12:00:00Z");

    Ok(())
}
