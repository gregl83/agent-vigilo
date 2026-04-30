use chrono::{
    DateTime,
    Utc,
};
use serde::{
    Deserialize,
    Serialize,
};
use uuid::Uuid;


#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "evaluator_state", rename_all = "lowercase")]
pub(crate) enum EvaluatorState {
    Active,
    Yanked,
    Deprecated,
    Disabled,
    Removed,
}

impl std::str::FromStr for EvaluatorState {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "active" => Ok(Self::Active),
            "yanked" => Ok(Self::Yanked),
            "deprecated" => Ok(Self::Deprecated),
            "disabled" => Ok(Self::Disabled),
            "removed" => Ok(Self::Removed),
            _ => anyhow::bail!("invalid evaluator state '{}'; expected one of: active, yanked, deprecated, disabled, removed", s),
        }
    }
}


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
    pub(crate) state: EvaluatorState,
    pub(crate) state_reason: Option<String>,
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
    pub(crate) state: EvaluatorState,
    pub(crate) state_reason: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct EvaluatorSummary {
    pub(crate) namespace: String,
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) description: Option<String>,
    pub(crate) tags: serde_json::Value,
    pub(crate) metadata: serde_json::Value,
    pub(crate) state: EvaluatorState,
    pub(crate) state_reason: Option<String>,
}

