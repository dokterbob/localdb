use axum::{extract::State, Extension, Json};
use serde::Serialize;

use localdb_core::auth::Principal;
use localdb_core::Error as CoreError;

use super::require_principal;
use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct ConfigResponse {
    pub yaml_config: serde_json::Value,
    pub effective_stores: Vec<EffectiveStoreView>,
}

#[derive(Debug, Serialize)]
pub struct EffectiveStoreView {
    pub name: String,
    pub visibility: String,
    pub backend: String,
}

/// `GET /v1/config`.
///
/// Admin-only (specs/05-surfaces.md §3.1) — the resolved config isn't
/// meaningfully store-scoped, and members are readers of search/store
/// content in this phase, not of server configuration.
///
/// `yaml_config` reflects **startup state**: the config file is read once
/// when the daemon starts and never reloaded (specs/03-config.md §5 — the
/// hot-reload watcher was removed in T3). Edits to the file take effect on
/// the next daemon restart.
pub async fn get_config(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
) -> Result<Json<ConfigResponse>, ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    let yaml = state.yaml_config();
    let effective = state.effective_config().await?;

    let yaml_value = serde_json::to_value(yaml).map_err(|e| {
        ApiError(CoreError::Internal {
            message: format!("cannot serialize config: {}", e),
            correlation_id: "config_serialize".to_string(),
        })
    })?;

    let effective_stores = effective
        .stores
        .iter()
        .map(|s| EffectiveStoreView {
            name: s.name.clone(),
            visibility: s.visibility.clone(),
            backend: s.backend.clone(),
        })
        .collect();

    Ok(Json(ConfigResponse {
        yaml_config: yaml_value,
        effective_stores,
    }))
}
