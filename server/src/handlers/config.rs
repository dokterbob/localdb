use axum::{extract::State, Json};
use serde::Serialize;

use localdb_core::Error as CoreError;

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

pub async fn get_config(State(state): State<AppState>) -> Result<Json<ConfigResponse>, ApiError> {
    let yaml = state.yaml_config().await;
    let effective = state.effective_config().await?;

    let yaml_value = serde_json::to_value(&yaml).map_err(|e| {
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
