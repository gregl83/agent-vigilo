use sqlx::types::JsonValue;

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct Evaluator {
    pub(crate) id: String,
    pub(crate) namespace: String,
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) content_hash: String,
    pub(crate) wasm_bytes: Vec<u8>,
    pub(crate) wasm_size_bytes: i64,
    pub(crate) interface_name: Option<String>,
    pub(crate) interface_version: Option<String>,
    pub(crate) wit_world: Option<String>,
    pub(crate) runtime: Option<String>,
    pub(crate) runtime_version: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) tags: JsonValue,
    pub(crate) metadata: JsonValue,
    pub(crate) is_active: bool,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

#[derive(Debug, Clone)]
pub(crate) struct NewEvaluator {
    pub(crate) namespace: String,
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) content_hash: String,
    pub(crate) wasm_bytes: Vec<u8>,
    pub(crate) interface_name: Option<String>,
    pub(crate) interface_version: Option<String>,
    pub(crate) wit_world: Option<String>,
    pub(crate) runtime: Option<String>,
    pub(crate) runtime_version: Option<String>,
    pub(crate) description: Option<String>,
}

