use crate::backend::StoreRow;
use crate::config::schema::IndexingPolicyConfig;
use crate::error::Error;
use crate::ids::new_ulid;
use crate::ingestion::now_rfc3339;
use crate::types::StoreVisibility;

pub fn default_store_row(
    name: &str,
    visibility: StoreVisibility,
    indexing_policy: &IndexingPolicyConfig,
    policy_version: &str,
) -> Result<StoreRow, Error> {
    Ok(StoreRow {
        id: new_ulid(),
        name: name.to_string(),
        visibility,
        backend: "libsql".to_string(),
        indexing_policy: serde_json::to_string(indexing_policy).map_err(|e| Error::Internal {
            message: format!("cannot serialize indexing policy: {e}"),
            correlation_id: "store_factory_serialize".into(),
        })?,
        policy_version: policy_version.to_string(),
        acl: "{}".to_string(),
        created_at: now_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
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
        assert_eq!(row.acl, "{}");
        assert_eq!(row.created_at, "2026-06-10T12:00:00Z");

        Ok(())
    }
}
