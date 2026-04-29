use chrono::{
    DateTime,
    Utc,
};
use serde::{
    Deserialize,
    Serialize,
};
use uuid::Uuid;


// todo - consider sqlx::types::Json<EvaluatorMetadata> 

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EvaluatorDraft {
    pub(crate) namespace: String,
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) content_hash: String,
    pub(crate) wasm_bytes: Vec<u8>,
    pub(crate) interface_name: Option<String>,
    pub(crate) interface_version: Option<String>,
    pub(crate) wit_world: Option<String>,
    pub(crate) runtime: String,
    pub(crate) runtime_version: String,
    pub(crate) runtime_fingerprint: String,
    pub(crate) description: Option<String>,
    pub(crate) tags: serde_json::Value,
    pub(crate) metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EvaluatorPatch {
    pub(crate) is_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct Evaluator {
    pub(crate) id: Uuid,
    pub(crate) namespace: String,
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) content_hash: String,
    pub(crate) wasm_bytes: Vec<u8>,
    pub(crate) wasm_size_bytes: i64,
    pub(crate) interface_name: Option<String>,
    pub(crate) interface_version: Option<String>,
    pub(crate) wit_world: Option<String>,
    pub(crate) runtime: String,
    pub(crate) runtime_version: String,
    pub(crate) runtime_fingerprint: String,
    pub(crate) description: Option<String>,
    pub(crate) tags: serde_json::Value,
    pub(crate) metadata: serde_json::Value,
    pub(crate) is_active: bool,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}
