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
        created_at: now_rfc3339(),
    })
}

#[cfg(test)]
mod tests;
