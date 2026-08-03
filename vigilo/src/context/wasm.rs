//! Wasm evaluator runtime context.
//!
//! This module prepares evaluator artifacts for publishing, compiles registry
//! components, and executes evaluator tests inside a Wasmtime component-model
//! sandbox. Keep WIT mapping here aligned with `wit/evaluator.wit`; resource
//! limits, fuel, timeout, and log caps must be enforced for every invocation.

use std::{
    env::consts::ARCH,
    fs,
    hash::{
        DefaultHasher,
        Hash,
        Hasher,
    },
    io::ErrorKind,
    path::{
        Path,
        PathBuf,
    },
    sync::Arc,
    thread,
    time::{
        Duration,
        SystemTime,
    },
};

use cargo_metadata::MetadataCommand;
use cargo_toml::Manifest;
use serde::{
    Deserialize,
    Serialize,
};
use serde_json::Value;
use tokio::sync::{
    OnceCell,
    OwnedSemaphorePermit,
    Semaphore,
};
use tracing::{
    debug,
    warn,
};
use wasmparser::{
    Parser,
    Payload,
};
use wasmtime::{
    Config as EngineConfig,
    Engine,
    Store,
    StoreLimits,
    StoreLimitsBuilder,
    component,
    component::ResourceTable,
};
use wasmtime_wasi::{
    WasiCtx,
    WasiCtxBuilder,
    WasiCtxView,
    WasiView,
};

use super::super::manifest::{
    Wit,
    read_manifest,
};
use crate::contracts::evaluator::{
    EvaluationDimension,
    EvaluationStatus,
    EvaluatorFinding,
    EvaluatorIdentity,
    EvaluatorInput,
    EvaluatorOutput,
    PreferenceOutcome,
    Score,
    Severity,
};

mod evaluator_test_bindings {
    wasmtime::component::bindgen!({
        path: "../wit/evaluator.wit",
        world: "evaluator-world",
    });
}

struct EvaluatorTestHost {
    table: ResourceTable,
    ctx: WasiCtx,
    limits: StoreLimits,
    max_log_message_bytes: usize,
    max_log_messages: u32,
    log_messages: u32,
}

impl WasiView for EvaluatorTestHost {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl evaluator_test_bindings::vigilo::evaluator::executor::Host for EvaluatorTestHost {
    fn trace(&mut self, msg: String) {
        if let Some(msg) = self.capped_log_message(msg) {
            debug!("evaluator.trace: {}", msg);
        }
    }

    fn debug(&mut self, msg: String) {
        if let Some(msg) = self.capped_log_message(msg) {
            debug!("evaluator.debug: {}", msg);
        }
    }

    fn warn(&mut self, msg: String) {
        if let Some(msg) = self.capped_log_message(msg) {
            warn!("evaluator.warn: {}", msg);
        }
    }

    fn error(&mut self, msg: String) {
        if let Some(msg) = self.capped_log_message(msg) {
            warn!("evaluator.error: {}", msg);
        }
    }

    fn send_http_request(
        &mut self,
        _req: evaluator_test_bindings::vigilo::evaluator::executor::HttpRequest,
    ) -> Result<evaluator_test_bindings::vigilo::evaluator::executor::HttpResponse, String> {
        Err("send_http_request is not enabled yet; outbound HTTP policy enforcement is not configured".to_string())
    }
}

impl EvaluatorTestHost {
    fn capped_log_message(&mut self, msg: String) -> Option<String> {
        if self.max_log_messages == 0 || self.log_messages >= self.max_log_messages {
            return None;
        }

        self.log_messages += 1;
        Some(truncate_utf8_bytes(msg, self.max_log_message_bytes))
    }
}

fn truncate_utf8_bytes(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }

    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

/// Parse JSON payload from raw str.
fn parse_json_payload(field_name: &str, raw: &str) -> anyhow::Result<Value> {
    if raw.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }

    serde_json::from_str(raw).map_err(|err| anyhow::anyhow!("invalid {} JSON: {}", field_name, err))
}

fn serialize_json_payload(field_name: &str, value: &Value) -> anyhow::Result<String> {
    serde_json::to_string(value)
        .map_err(|err| anyhow::anyhow!("invalid {} JSON value: {}", field_name, err))
}

fn serialize_optional_json_payload(
    field_name: &str,
    value: &Option<Value>,
) -> anyhow::Result<Option<String>> {
    value
        .as_ref()
        .map(|v| serialize_json_payload(field_name, v))
        .transpose()
}

fn map_input_to_wit_input(
    input: EvaluatorInput,
) -> anyhow::Result<evaluator_test_bindings::vigilo::evaluator::types::Input> {
    let tool_calls = input
        .actual
        .tool_calls
        .into_iter()
        .map(|call| {
            Ok(
                evaluator_test_bindings::vigilo::evaluator::types::ToolCall {
                    name: call.name,
                    arguments_json: serialize_json_payload("tool-call.arguments", &call.arguments)?,
                    result_json: serialize_optional_json_payload("tool-call.result", &call.result)?,
                },
            )
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let trace = input
        .actual
        .trace
        .into_iter()
        .map(|event| {
            Ok(
                evaluator_test_bindings::vigilo::evaluator::types::AgentTraceEvent {
                    kind: event.kind,
                    name: event.name,
                    payload_json: serialize_json_payload("trace.payload", &event.payload)?,
                },
            )
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    Ok(evaluator_test_bindings::vigilo::evaluator::types::Input {
        run_id: input.run_id,
        execution_id: input.execution_id,
        attempt_id: input.attempt_id,
        test_case: evaluator_test_bindings::vigilo::evaluator::types::TestCase {
            id: input.case.id,
            task_type: input.case.task_type,
            case_group: input.case.case_group,
            input_json: serialize_json_payload("case.input", &input.case.input)?,
            expected_json: serialize_optional_json_payload("case.expected", &input.case.expected)?,
            context_json: serialize_optional_json_payload("case.context", &input.case.context)?,
            tags: input.case.tags,
            metadata_json: serialize_json_payload(
                "case.metadata",
                &serde_json::to_value(input.case.metadata)?,
            )?,
        },
        actual: evaluator_test_bindings::vigilo::evaluator::types::AgentOutput {
            text: input.actual.text,
            structured_json: serialize_optional_json_payload(
                "actual.structured",
                &input.actual.structured,
            )?,
            tool_calls,
            trace,
            raw_json: serialize_json_payload("actual.raw", &input.actual.raw)?,
            metadata_json: serialize_json_payload("actual.metadata", &input.actual.metadata)?,
        },
        evaluator_config_json: serialize_json_payload("evaluator_config", &input.evaluator_config)?,
    })
}

/// Map bound evaluator dimension to evaluation dimension type.
fn map_dimension(
    dimension: evaluator_test_bindings::vigilo::evaluator::types::EvaluationDimension,
) -> EvaluationDimension {
    use evaluator_test_bindings::vigilo::evaluator::types::EvaluationDimension as BindingDimension;

    match dimension {
        BindingDimension::Correctness => EvaluationDimension::Correctness,
        BindingDimension::Format => EvaluationDimension::Format,
        BindingDimension::Safety => EvaluationDimension::Safety,
        BindingDimension::Quality => EvaluationDimension::Quality,
        BindingDimension::Latency => EvaluationDimension::Latency,
        BindingDimension::ToolUse => EvaluationDimension::ToolUse,
        BindingDimension::Calibration => EvaluationDimension::Calibration,
        BindingDimension::Other(value) => EvaluationDimension::Other(value),
    }
}

/// Map bound evaluator status to evaluation status type.
fn map_status(
    status: evaluator_test_bindings::vigilo::evaluator::types::EvaluationStatus,
) -> EvaluationStatus {
    use evaluator_test_bindings::vigilo::evaluator::types::EvaluationStatus as BindingStatus;

    match status {
        BindingStatus::Passed => EvaluationStatus::Passed,
        BindingStatus::Failed => EvaluationStatus::Failed,
        BindingStatus::Error => EvaluationStatus::Error,
        BindingStatus::Skipped => EvaluationStatus::Skipped,
    }
}

/// Map bound evaluator severity to evaluation severity type.
fn map_severity(severity: evaluator_test_bindings::vigilo::evaluator::types::Severity) -> Severity {
    use evaluator_test_bindings::vigilo::evaluator::types::Severity as BindingSeverity;

    match severity {
        BindingSeverity::None => Severity::None,
        BindingSeverity::Low => Severity::Low,
        BindingSeverity::Medium => Severity::Medium,
        BindingSeverity::High => Severity::High,
        BindingSeverity::Critical => Severity::Critical,
    }
}

/// Map bound evaluator preference outcome to evaluation preference outcome type.
fn map_preference_outcome(
    outcome: evaluator_test_bindings::vigilo::evaluator::types::PreferenceOutcome,
) -> PreferenceOutcome {
    use evaluator_test_bindings::vigilo::evaluator::types::PreferenceOutcome as BindingPreferenceOutcome;

    match outcome {
        BindingPreferenceOutcome::Preferred => PreferenceOutcome::Preferred,
        BindingPreferenceOutcome::Tie => PreferenceOutcome::Tie,
        BindingPreferenceOutcome::NotPreferred => PreferenceOutcome::NotPreferred,
    }
}

/// Map bound evaluator score to evaluation score type.
fn map_score(score: evaluator_test_bindings::vigilo::evaluator::types::Score) -> Score {
    use evaluator_test_bindings::vigilo::evaluator::types::Score as BindingScore;

    match score {
        BindingScore::Binary(passed) => Score::Binary { passed },
        BindingScore::Range((value, min, max)) => Score::Range { value, min, max },
        BindingScore::Normalized(value) => Score::Normalized { value },
        BindingScore::SeverityMapped(severity) => Score::SeverityMapped {
            severity: map_severity(severity),
        },
        BindingScore::Preference(outcome) => Score::Preference {
            outcome: map_preference_outcome(outcome),
        },
        BindingScore::Informational => Score::Informational,
    }
}

/// Map WIT evaluator output to host output struct.
fn map_wit_output_to_output(
    output: evaluator_test_bindings::vigilo::evaluator::types::Output,
) -> anyhow::Result<EvaluatorOutput> {
    let metadata = parse_json_payload("metadata-json", &output.metadata_json)?;

    let results = output
        .results
        .into_iter()
        .map(|finding| {
            Ok(EvaluatorFinding {
                dimension: map_dimension(finding.dimension),
                status: map_status(finding.status),
                score: map_score(finding.score),
                blocking: finding.blocking,
                severity: map_severity(finding.severity),
                failure_category: finding.failure_category,
                reason: finding.reason,
                evidence: parse_json_payload("evidence-json", &finding.evidence_json)?,
                tags: finding.tags,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    Ok(EvaluatorOutput {
        evaluator: EvaluatorIdentity {
            namespace: output.evaluator.namespace,
            name: output.evaluator.name,
            version: output.evaluator.version,
            content_hash: output.evaluator.content_hash,
            interface_version: output.evaluator.interface_version,
        },
        results,
        metadata,
    })
}

struct PackageMetadata {
    name: String,
    version: String,
    target_dir: PathBuf,
    modified: SystemTime,
    description: Option<String>,
    tags: Vec<String>,
    metadata: Option<Value>,
}

struct EvaluatorMetadata {
    description: Option<String>,
    tags: Value,
    metadata: Value,
}

struct WitWorld {
    name: String,
    exports: Vec<String>,
}

struct WitDocument {
    package: String,
    version: Option<String>,
    worlds: Vec<WitWorld>,
}

struct WitMetadata {
    interface_name: Option<String>,
    interface_version: Option<String>,
    wit_world: Option<String>,
}

const PACKAGE_METADATA_SECTION: &str = "vigilo.package";
const WASM_RUNTIME_NAME: &str = "wasmtime";

#[derive(Deserialize)]
struct CargoLockDocument {
    package: Vec<CargoLockPackage>,
}

#[derive(Deserialize)]
struct CargoLockPackage {
    name: String,
    version: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct EmbeddedPackageMetadata {
    name: String,
    version: String,
}

/// Read custom section in wasm bytes.
///
/// Retrieves evaluator metadata from wasm bytes (e.g., name and version).
fn read_embedded_package_metadata(
    wasm_bytes: &[u8],
) -> anyhow::Result<Option<EmbeddedPackageMetadata>> {
    for payload in Parser::new(0).parse_all(wasm_bytes) {
        if let Payload::CustomSection(section) = payload?
            && section.name() == PACKAGE_METADATA_SECTION
        {
            let metadata = serde_json::from_slice::<EmbeddedPackageMetadata>(section.data())
                .map_err(|err| {
                    anyhow::anyhow!(
                        "failed to decode {} metadata: {}",
                        PACKAGE_METADATA_SECTION,
                        err
                    )
                })?;
            return Ok(Some(metadata));
        }
    }

    Ok(None)
}

/// Push LEB128 (Little Endian Base 128) compressed value into vector buffer.
fn push_u32_leb128(buf: &mut Vec<u8>, mut value: u32) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;

        if value != 0 {
            byte |= 0x80;
        }

        buf.push(byte);

        if value == 0 {
            break;
        }
    }
}

/// Appends custom section of metadata to wasm bytes.
///
/// Used to attach evaluator metadata (e.g., name and version).
fn append_custom_section(
    wasm_bytes: &[u8],
    section_name: &str,
    section_data: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let mut payload = Vec::new();
    let section_name_len = u32::try_from(section_name.len())
        .map_err(|_| anyhow::anyhow!("custom section name too long"))?;
    push_u32_leb128(&mut payload, section_name_len);
    payload.extend_from_slice(section_name.as_bytes());
    payload.extend_from_slice(section_data);

    let payload_len = u32::try_from(payload.len())
        .map_err(|_| anyhow::anyhow!("custom section payload too long"))?;

    let mut out = Vec::with_capacity(wasm_bytes.len() + payload.len() + 8);
    out.extend_from_slice(wasm_bytes);
    out.push(0);
    push_u32_leb128(&mut out, payload_len);
    out.extend_from_slice(&payload);

    Ok(out)
}

/// Ensure that custom section evaluator metadata has been appended to wasm bytes.
fn ensure_embedded_package_metadata(
    wasm_bytes: Vec<u8>,
    package_name: &str,
    package_version: &str,
) -> anyhow::Result<Vec<u8>> {
    match read_embedded_package_metadata(&wasm_bytes)? {
        Some(existing) => {
            if existing.name != package_name || existing.version != package_version {
                anyhow::bail!(
                    "embedded {} mismatch (found {}@{}, expected {}@{})",
                    PACKAGE_METADATA_SECTION,
                    existing.name,
                    existing.version,
                    package_name,
                    package_version,
                );
            }

            Ok(wasm_bytes)
        }
        None => {
            let metadata = EmbeddedPackageMetadata {
                name: package_name.to_string(),
                version: package_version.to_string(),
            };
            let encoded = serde_json::to_vec(&metadata)?;
            let out = append_custom_section(&wasm_bytes, PACKAGE_METADATA_SECTION, &encoded)?;

            // verify append/read-back so we fail fast on malformed custom section writes.
            let embedded = read_embedded_package_metadata(&out)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "failed to read back embedded {} metadata",
                    PACKAGE_METADATA_SECTION
                )
            })?;

            if embedded.name != package_name || embedded.version != package_version {
                anyhow::bail!(
                    "embedded {} mismatch after write (found {}@{}, expected {}@{})",
                    PACKAGE_METADATA_SECTION,
                    embedded.name,
                    embedded.version,
                    package_name,
                    package_version,
                );
            }

            Ok(out)
        }
    }
}

/// Parse wit file and return struct.
fn parse_wit_file(path: &PathBuf) -> anyhow::Result<WitDocument> {
    let content = fs::read_to_string(path)?;

    let mut package: Option<String> = None;
    let mut version: Option<String> = None;
    let mut worlds: Vec<WitWorld> = Vec::new();

    let mut current_world: Option<WitWorld> = None;
    let mut world_depth: i32 = 0;

    for raw in content.lines() {
        let line = raw.split("//").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }

        if package.is_none() && line.starts_with("package ") && line.ends_with(';') {
            let body = line
                .trim_start_matches("package ")
                .trim_end_matches(';')
                .trim();

            if let Some((pkg, ver)) = body.split_once('@') {
                package = Some(pkg.trim().to_string());
                version = Some(ver.trim().to_string());
            } else {
                package = Some(body.to_string());
            }
            continue;
        }

        if current_world.is_none() && line.starts_with("world ") && line.contains('{') {
            let name_part = line
                .trim_start_matches("world ")
                .split('{')
                .next()
                .unwrap_or("")
                .trim();

            if !name_part.is_empty() {
                current_world = Some(WitWorld {
                    name: name_part.to_string(),
                    exports: Vec::new(),
                });
                world_depth = 1;
                continue;
            }
        }

        if let Some(world) = current_world.as_mut() {
            if line.starts_with("export ") && line.ends_with(';') {
                let export_name = line
                    .trim_start_matches("export ")
                    .trim_end_matches(';')
                    .trim();
                if !export_name.is_empty() {
                    world.exports.push(export_name.to_string());
                }
            }

            let opens = line.chars().filter(|c| *c == '{').count() as i32;
            let closes = line.chars().filter(|c| *c == '}').count() as i32;
            world_depth += opens - closes;

            if world_depth <= 0 {
                let finished = current_world.take().expect("world exists");
                worlds.push(finished);
                world_depth = 0;
            }
        }
    }

    let package = package.ok_or_else(|| {
        anyhow::anyhow!("missing package declaration in WIT file {}", path.display())
    })?;

    Ok(WitDocument {
        package,
        version,
        worlds,
    })
}

/// Resolve wit file metadata and return as struct.
fn resolve_wit_metadata(
    package_path: &Path,
    manifest_wit: Option<&Wit>,
) -> anyhow::Result<WitMetadata> {
    let Some(wit) = manifest_wit else {
        return Ok(WitMetadata {
            interface_name: None,
            interface_version: None,
            wit_world: None,
        });
    };

    let wit_path = package_path.join(&wit.path);
    let parsed = parse_wit_file(&wit_path)?;

    let world = parsed.worlds.iter().find(|w| w.name == wit.world);

    let has_interface_export = world
        .map(|w| w.exports.iter().any(|e| e == &wit.interface))
        .unwrap_or(false);

    let package_matches = parsed.package == wit.package;
    let version_matches = parsed.version.as_deref() == Some(wit.version.as_str());
    let world_matches = world.is_some();
    let interface_matches = has_interface_export;

    if wit.strict {
        if !package_matches {
            return Err(anyhow::anyhow!(
                "WIT package mismatch (config={}, file={})",
                wit.package,
                parsed.package,
            ));
        }
        if !version_matches {
            return Err(anyhow::anyhow!(
                "WIT version mismatch (config={}, file={})",
                wit.version,
                parsed.version.unwrap_or_else(|| "<missing>".to_string()),
            ));
        }
        if !world_matches {
            return Err(anyhow::anyhow!(
                "WIT world '{}' not found in {}",
                wit.world,
                wit_path.display(),
            ));
        }
        if !interface_matches {
            return Err(anyhow::anyhow!(
                "WIT interface '{}' is not exported by world '{}'",
                wit.interface,
                wit.world,
            ));
        }
    } else if !package_matches || !version_matches || !world_matches || !interface_matches {
        warn!(
            "WIT config does not fully match {} (strict=false), continuing with configured values",
            wit_path.display()
        );
    }

    Ok(WitMetadata {
        interface_name: Some(format!("{}/{}", wit.package, wit.interface)),
        interface_version: Some(wit.version.clone()),
        wit_world: Some(wit.world.clone()),
    })
}

/// Convert toml value to serde value.
fn value_from_toml(value: &toml::Value) -> anyhow::Result<Value> {
    serde_json::to_value(value)
        .map_err(|err| anyhow::anyhow!("failed to encode TOML value to JSON: {}", err))
}

/// Resolve evaluator metadata and return as struct.
fn resolve_evaluator_metadata(
    package: &super::super::manifest::Package,
    cargo: &PackageMetadata,
) -> anyhow::Result<EvaluatorMetadata> {
    let description = package
        .description
        .clone()
        .or_else(|| cargo.description.clone());

    let tags = if package.tags.is_empty() {
        cargo.tags.clone()
    } else {
        package.tags.clone()
    };

    let metadata = match &package.metadata {
        Some(value) => value_from_toml(value)?,
        None => cargo
            .metadata
            .clone()
            .unwrap_or_else(|| Value::Object(Default::default())),
    };

    Ok(EvaluatorMetadata {
        description,
        tags: Value::Array(tags.into_iter().map(Value::String).collect()),
        metadata,
    })
}

/// Get metadata from package manifest defined in Vigilo.toml.
fn get_package_metadata(
    package_path: &Path,
    manifest_file: &str,
) -> anyhow::Result<PackageMetadata> {
    match manifest_file {
        "Cargo.toml" => {
            let manifest_path = package_path.join(manifest_file);
            let manifest_content = fs::read_to_string(&manifest_path)?;
            let manifest_value: toml::Value = toml::from_str(&manifest_content)?;

            let fs_metadata = fs::metadata(&manifest_path)?;

            let metadata = MetadataCommand::new()
                .manifest_path(&manifest_path)
                .exec()?;

            let target_dir = metadata.target_directory;

            let manifest = Manifest::from_path(&manifest_path)?;

            let package = manifest
                .package
                .ok_or_else(|| anyhow::anyhow!("no [package] section found in Cargo.toml"))?;

            let package_table = manifest_value
                .get("package")
                .and_then(|value| value.as_table());

            let description = package_table
                .and_then(|pkg| pkg.get("description"))
                .and_then(|value| value.as_str())
                .map(|value| value.to_string());

            let tags = package_table
                .and_then(|pkg| pkg.get("keywords"))
                .and_then(|value| value.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| item.as_str())
                        .map(|item| item.to_string())
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default();

            let metadata = package_table
                .and_then(|pkg| pkg.get("metadata"))
                .and_then(|value| value.as_table())
                .and_then(|metadata| metadata.get("vigilo"))
                .map(value_from_toml)
                .transpose()?;

            Ok(PackageMetadata {
                name: package.name,
                version: package.version.get()?.to_string(),
                target_dir: target_dir.into_std_path_buf(),
                modified: fs_metadata.modified()?,
                description,
                tags,
                metadata,
            })
        }
        _ => Err(anyhow::anyhow!(
            "Vigilo.toml [package] manifest {} is unsupported",
            manifest_file
        )),
    }
}

/// Get fingerprint for wasmtime engine.
fn get_engine_fingerprint(engine: &Engine) -> String {
    let mut hasher = DefaultHasher::new();
    engine.precompile_compatibility_hash().hash(&mut hasher);
    format!("{:x}-{}", hasher.finish(), ARCH)
}

/// Resolve wasm runtime version from `Cargo.lock` file.
fn resolve_runtime_version() -> anyhow::Result<String> {
    let lock_content = include_str!("../../../Cargo.lock");
    let lock = toml::from_str::<CargoLockDocument>(lock_content)
        .map_err(|err| anyhow::anyhow!("failed to parse embedded Cargo.lock: {}", err))?;

    lock.package
        .into_iter()
        .find(|pkg| pkg.name == WASM_RUNTIME_NAME)
        .map(|pkg| pkg.version)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{} dependency was not found in embedded Cargo.lock",
                WASM_RUNTIME_NAME
            )
        })
}

/// Wasm component wrapper.
pub struct Component {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub tags: Value,
    pub metadata: Value,
    pub interface_name: Option<String>,
    pub interface_version: Option<String>,
    pub wit_world: Option<String>,
    pub runtime: String,
    pub runtime_version: String,
    pub runtime_fingerprint: String,
    #[allow(dead_code)]
    pub component: component::Component,
    pub wasm_hash: String,
    pub wasm_bytes: Vec<u8>,
    #[allow(dead_code)]
    pub serialized: Vec<u8>,
}

pub(crate) const DEFAULT_MAX_MEMORY_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const DEFAULT_MAX_TABLE_ELEMENTS: u64 = 10_000;
pub(crate) const DEFAULT_MAX_INSTANCES: u64 = 3;
pub(crate) const DEFAULT_MAX_MEMORIES: u64 = 1;
pub(crate) const DEFAULT_MAX_TABLES: u64 = 2;
pub(crate) const DEFAULT_FUEL_PER_EVALUATION: u64 = 50_000_000;
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 5_000;
pub(crate) const DEFAULT_EPOCH_TICK_INTERVAL_MS: u64 = 10;
pub(crate) const DEFAULT_MAX_CONCURRENT_EVALUATIONS: u64 = 8;
pub(crate) const DEFAULT_MAX_LOG_MESSAGE_BYTES: u64 = 4 * 1024;
pub(crate) const DEFAULT_MAX_LOG_MESSAGES: u32 = 128;

#[derive(Debug, Clone)]
pub struct Config {
    pub max_memory_bytes: u64,
    pub max_table_elements: u64,
    pub max_instances: u64,
    pub max_memories: u64,
    pub max_tables: u64,
    pub fuel_per_evaluation: u64,
    pub timeout_ms: u64,
    pub epoch_tick_interval_ms: u64,
    pub max_concurrent_evaluations: u64,
    pub max_log_message_bytes: u64,
    pub max_log_messages: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
            max_table_elements: DEFAULT_MAX_TABLE_ELEMENTS,
            max_instances: DEFAULT_MAX_INSTANCES,
            max_memories: DEFAULT_MAX_MEMORIES,
            max_tables: DEFAULT_MAX_TABLES,
            fuel_per_evaluation: DEFAULT_FUEL_PER_EVALUATION,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            epoch_tick_interval_ms: DEFAULT_EPOCH_TICK_INTERVAL_MS,
            max_concurrent_evaluations: DEFAULT_MAX_CONCURRENT_EVALUATIONS,
            max_log_message_bytes: DEFAULT_MAX_LOG_MESSAGE_BYTES,
            max_log_messages: DEFAULT_MAX_LOG_MESSAGES,
        }
    }
}

impl Config {
    fn store_limits(&self) -> anyhow::Result<StoreLimits> {
        Ok(StoreLimitsBuilder::new()
            .memory_size(to_usize("wasm max memory bytes", self.max_memory_bytes)?)
            .table_elements(to_usize(
                "wasm max table elements",
                self.max_table_elements,
            )?)
            .instances(to_usize("wasm max instances", self.max_instances)?)
            .memories(to_usize("wasm max memories", self.max_memories)?)
            .tables(to_usize("wasm max tables", self.max_tables)?)
            .trap_on_grow_failure(true)
            .build())
    }

    fn epoch_deadline_ticks(&self) -> u64 {
        if self.timeout_ms == 0 {
            return 0;
        }

        self.timeout_ms
            .div_ceil(self.epoch_tick_interval_ms.max(1))
            .max(1)
    }
}

fn to_usize(name: &str, value: u64) -> anyhow::Result<usize> {
    usize::try_from(value).map_err(|_| anyhow::anyhow!("{} is too large: {}", name, value))
}

fn start_epoch_ticker(engine: Engine, tick_interval_ms: u64) {
    if tick_interval_ms == 0 {
        return;
    }

    let interval = Duration::from_millis(tick_interval_ms);
    let _ = thread::Builder::new()
        .name("vigilo-wasm-epoch-ticker".to_string())
        .spawn(move || {
            loop {
                thread::sleep(interval);
                #[cfg(target_has_atomic = "64")]
                engine.increment_epoch();
            }
        });
}

/// Wasm engine wrapper.
#[derive(Clone)]
pub struct Wasm {
    engine: Engine,
    fingerprint: String,
    config: Config,
    evaluation_semaphore: Arc<Semaphore>,
}

impl Wasm {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let mut engine_config = EngineConfig::new();
        engine_config.wasm_component_model(true);
        engine_config.consume_fuel(true);
        if config.timeout_ms > 0 {
            engine_config.epoch_interruption(true);
        }

        let engine = Engine::new(&engine_config)?;
        let fingerprint = get_engine_fingerprint(&engine);
        if config.timeout_ms > 0 {
            start_epoch_ticker(engine.clone(), config.epoch_tick_interval_ms);
        }
        let max_concurrent_evaluations = to_usize(
            "wasm max concurrent evaluations",
            config.max_concurrent_evaluations,
        )?;

        Ok(Self {
            engine,
            fingerprint,
            config,
            evaluation_semaphore: Arc::new(Semaphore::new(max_concurrent_evaluations)),
        })
    }

    pub(crate) async fn acquire_evaluation_permit(&self) -> anyhow::Result<OwnedSemaphorePermit> {
        self.evaluation_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|err| anyhow::anyhow!("wasm evaluation semaphore closed: {}", err))
    }

    /// Prepare evaluator for execution.
    pub fn prepare_evaluator(
        &self,
        package_path: PathBuf,
        profile: String,
    ) -> anyhow::Result<Component> {
        let manifest = read_manifest(&package_path)?;
        let manifest_profile = manifest.get_profile(&profile)?;
        let wit_metadata = resolve_wit_metadata(&package_path, manifest.wit.as_ref())?;

        let package_metadata = get_package_metadata(&package_path, &manifest.package.manifest)?;
        let evaluator_metadata = resolve_evaluator_metadata(&manifest.package, &package_metadata)?;

        let wasm_path = package_metadata.target_dir.join(&manifest_profile.wasm);

        let fs_wasm_metadata = match fs::metadata(&wasm_path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                let release_flag = if profile == "release" {
                    " --release"
                } else {
                    ""
                };

                anyhow::bail!(
                    "configured wasm artifact was not found at {} (profile '{}'); build it first with: cargo build --manifest-path {} --target wasm32-wasip2{}",
                    wasm_path.display(),
                    profile,
                    package_path.join(&manifest.package.manifest).display(),
                    release_flag,
                );
            }
            Err(err) => {
                return Err(anyhow::anyhow!(
                    "failed to read wasm metadata at {}: {}",
                    wasm_path.display(),
                    err
                ));
            }
        };
        let wasm_modified = fs_wasm_metadata.modified()?;
        if package_metadata.modified > wasm_modified {
            return Err(anyhow::anyhow!(
                "evaluation manifest was modified after wasm build"
            ));
        }
        let wasm_bytes = fs::read(&wasm_path).map_err(|err| {
            anyhow::anyhow!(
                "failed to read wasm bytes at {}: {}",
                wasm_path.display(),
                err
            )
        })?;
        let wasm_bytes = ensure_embedded_package_metadata(
            wasm_bytes,
            &package_metadata.name,
            &package_metadata.version,
        )?;
        let wasm_hash = blake3::hash(&wasm_bytes).to_hex().to_string();

        let component = component::Component::new(&self.engine, &wasm_bytes)?;
        let serialized = component.serialize()?;

        let runtime_version = resolve_runtime_version()?;

        Ok(Component {
            name: package_metadata.name,
            version: package_metadata.version,
            description: evaluator_metadata.description,
            tags: evaluator_metadata.tags,
            metadata: evaluator_metadata.metadata,
            interface_name: wit_metadata.interface_name,
            interface_version: wit_metadata.interface_version,
            wit_world: wit_metadata.wit_world,
            runtime: WASM_RUNTIME_NAME.to_string(),
            runtime_version,
            runtime_fingerprint: self.fingerprint.clone(),
            component,
            wasm_hash,
            wasm_bytes,
            serialized,
        })
    }

    /// Compile evaluator wasm bytes into a component for registry caching.
    pub fn compile_component(&self, wasm_bytes: &[u8]) -> anyhow::Result<component::Component> {
        Ok(component::Component::new(&self.engine, wasm_bytes)?)
    }

    /// Run evaluator in test mode.
    pub fn test_evaluator(
        &self,
        wasm_bytes: &[u8],
        input: EvaluatorInput,
    ) -> anyhow::Result<EvaluatorOutput> {
        let component = component::Component::new(&self.engine, wasm_bytes)?;
        self.test_evaluator_component(&component, input)
    }

    /// Run evaluator in test mode using a precompiled component.
    pub fn test_evaluator_component(
        &self,
        component: &component::Component,
        input: EvaluatorInput,
    ) -> anyhow::Result<EvaluatorOutput> {
        let mut linker = component::Linker::new(&self.engine);

        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;

        evaluator_test_bindings::vigilo::evaluator::executor::add_to_linker::<
            _,
            wasmtime::component::HasSelf<EvaluatorTestHost>,
        >(&mut linker, |host: &mut EvaluatorTestHost| host)?;

        let mut store = Store::new(
            &self.engine,
            EvaluatorTestHost {
                table: ResourceTable::new(),
                ctx: WasiCtxBuilder::new().build(),
                limits: self.config.store_limits()?,
                max_log_message_bytes: to_usize(
                    "wasm max log message bytes",
                    self.config.max_log_message_bytes,
                )?,
                max_log_messages: self.config.max_log_messages,
                log_messages: 0,
            },
        );
        store.limiter(|host| &mut host.limits);
        store.set_fuel(self.config.fuel_per_evaluation)?;
        if self.config.timeout_ms > 0 {
            #[cfg(target_has_atomic = "64")]
            {
                store.set_epoch_deadline(self.config.epoch_deadline_ticks());
                store.epoch_deadline_trap();
            }
        }
        let bindings =
            evaluator_test_bindings::EvaluatorWorld::instantiate(&mut store, component, &linker)?;

        let input = map_input_to_wit_input(input)?;

        let output = bindings
            .vigilo_evaluator_evaluator()
            .call_evaluate(&mut store, &input)
            .map_err(|err| anyhow::anyhow!("evaluator trapped in wasm sandbox: {}", err))?
            .map_err(|err| anyhow::anyhow!("evaluator returned error: {}", err))?;

        map_wit_output_to_output(output)
    }
}

pub(crate) struct Context {
    pub(crate) cell: OnceCell<Wasm>,
    pub(crate) config: Config,
}

impl Context {
    pub async fn get(&self) -> anyhow::Result<&Wasm> {
        self.cell
            .get_or_try_init(|| async {
                debug!("initializing wasm engine");
                Wasm::new(self.config.clone())
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        time::SystemTime,
    };

    use serde_json::json;
    use tempfile::tempdir;

    use super::{
        evaluator_test_bindings::vigilo::evaluator::{
            executor,
            types as wit_types,
        },
        *,
    };
    use crate::{
        contracts::evaluator::{
            AgentOutput,
            AgentTraceEvent,
            TestCase,
            ToolCall,
        },
        manifest::Package,
    };

    const TEST_WIT: &str = r#"
        package vigilo:evaluator@0.1.0;

        interface evaluator {}

        world evaluator-world {
            export evaluator;
        }
    "#;

    fn minimal_wasm_module() -> Vec<u8> {
        b"\0asm\x01\0\0\0".to_vec()
    }

    fn test_host(max_log_message_bytes: usize, max_log_messages: u32) -> EvaluatorTestHost {
        EvaluatorTestHost {
            table: ResourceTable::new(),
            ctx: WasiCtxBuilder::new().build(),
            limits: Config::default().store_limits().unwrap(),
            max_log_message_bytes,
            max_log_messages,
            log_messages: 0,
        }
    }

    fn evaluator_input() -> EvaluatorInput {
        EvaluatorInput {
            run_id: "run-1".to_string(),
            execution_id: "execution-1".to_string(),
            attempt_id: "attempt-1".to_string(),
            case: TestCase {
                id: "case-1".to_string(),
                task_type: "classification".to_string(),
                case_group: Some("sentiment".to_string()),
                input: json!({"text": "hello"}),
                expected: Some(json!({"label": "positive"})),
                context: None,
                tags: vec!["example".to_string()],
                metadata: BTreeMap::from([("source".to_string(), json!("test"))]),
            },
            actual: AgentOutput {
                text: Some("positive".to_string()),
                structured: Some(json!({"label": "positive"})),
                tool_calls: vec![ToolCall {
                    name: "lookup".to_string(),
                    arguments: json!({"id": 1}),
                    result: Some(json!({"found": true})),
                }],
                trace: vec![AgentTraceEvent {
                    kind: "tool_call".to_string(),
                    name: Some("lookup".to_string()),
                    payload: json!({"step": 1}),
                }],
                raw: json!({"provider": "test"}),
                metadata: json!({"latency_ms": 12}),
            },
            evaluator_config: json!({"threshold": 0.8}),
        }
    }

    fn wit_output() -> wit_types::Output {
        wit_types::Output {
            evaluator: wit_types::EvaluatorIdentity {
                namespace: "vigilo".to_string(),
                name: "example".to_string(),
                version: "1.0.0".to_string(),
                content_hash: Some("hash".to_string()),
                interface_version: Some("0.1.0".to_string()),
            },
            results: vec![wit_types::EvaluatorFinding {
                dimension: wit_types::EvaluationDimension::Other("style".to_string()),
                status: wit_types::EvaluationStatus::Failed,
                score: wit_types::Score::Range((0.4, 0.0, 1.0)),
                blocking: true,
                severity: wit_types::Severity::High,
                failure_category: Some("tone".to_string()),
                reason: Some("too terse".to_string()),
                evidence_json: r#"{"span":"answer"}"#.to_string(),
                tags: vec!["style".to_string()],
            }],
            metadata_json: r#"{"duration_ms":4}"#.to_string(),
        }
    }

    fn manifest_wit(
        package: &str,
        version: &str,
        world: &str,
        interface: &str,
        strict: bool,
    ) -> Wit {
        Wit {
            path: "evaluator.wit".to_string(),
            world: world.to_string(),
            package: package.to_string(),
            version: version.to_string(),
            interface: interface.to_string(),
            strict,
        }
    }

    fn cargo_package_metadata() -> PackageMetadata {
        PackageMetadata {
            name: "example".to_string(),
            version: "1.0.0".to_string(),
            target_dir: PathBuf::from("target"),
            modified: SystemTime::UNIX_EPOCH,
            description: Some("Cargo description".to_string()),
            tags: vec!["cargo".to_string()],
            metadata: Some(json!({"owner": "cargo"})),
        }
    }

    fn error_message<T>(result: anyhow::Result<T>) -> String {
        match result {
            Ok(_) => panic!("expected operation to fail"),
            Err(err) => err.to_string(),
        }
    }

    #[test]
    fn utf8_truncation_respects_character_boundaries() {
        let cases = [
            ("hello", 5, "hello"),
            ("hello", 3, "hel"),
            ("aéb", 2, "a"),
            ("aéb", 3, "aé"),
            ("hello", 0, ""),
        ];

        for (value, max_bytes, expected) in cases {
            assert_eq!(truncate_utf8_bytes(value.to_string(), max_bytes), expected);
        }
    }

    #[test]
    fn evaluator_logs_enforce_message_and_byte_limits() {
        let mut host = test_host(3, 2);

        assert_eq!(
            host.capped_log_message("hello".to_string()).as_deref(),
            Some("hel")
        );
        assert_eq!(
            host.capped_log_message("éé".to_string()).as_deref(),
            Some("é")
        );
        assert_eq!(host.capped_log_message("ignored".to_string()), None);

        let mut disabled = test_host(10, 0);
        assert_eq!(disabled.capped_log_message("ignored".to_string()), None);
    }

    #[test]
    fn evaluator_http_requests_are_rejected_until_policy_is_configured() {
        let mut host = test_host(10, 1);
        let request = executor::HttpRequest {
            method: "GET".to_string(),
            uri: "https://example.test".to_string(),
            headers: Vec::new(),
            body: None,
            timeout_ms: None,
        };

        let err = executor::Host::send_http_request(&mut host, request).unwrap_err();

        assert!(err.contains("outbound HTTP policy enforcement is not configured"));
    }

    #[test]
    fn json_payload_parsing_accepts_blank_objects_and_labels_errors() {
        assert_eq!(parse_json_payload("metadata", "  ").unwrap(), json!({}));
        assert_eq!(
            parse_json_payload("metadata", r#"{"ok":true}"#).unwrap(),
            json!({"ok": true})
        );

        let err = parse_json_payload("evidence", "{").unwrap_err();
        assert!(err.to_string().contains("invalid evidence JSON"));
    }

    #[test]
    fn evaluator_input_mapping_preserves_structured_fields() {
        let mapped = map_input_to_wit_input(evaluator_input()).unwrap();

        assert_eq!(mapped.run_id, "run-1");
        assert_eq!(mapped.test_case.case_group.as_deref(), Some("sentiment"));
        assert_eq!(
            parse_json_payload("input", &mapped.test_case.input_json).unwrap(),
            json!({"text": "hello"})
        );
        assert_eq!(mapped.actual.tool_calls[0].name, "lookup");
        assert_eq!(
            parse_json_payload("arguments", &mapped.actual.tool_calls[0].arguments_json).unwrap(),
            json!({"id": 1})
        );
        assert_eq!(mapped.actual.trace[0].kind, "tool_call");
        assert_eq!(
            parse_json_payload("config", &mapped.evaluator_config_json).unwrap(),
            json!({"threshold": 0.8})
        );
    }

    #[test]
    fn wit_enum_mappings_cover_every_contract_variant() {
        use wit_types::{
            EvaluationDimension as WitDimension,
            EvaluationStatus as WitStatus,
            PreferenceOutcome as WitPreference,
            Severity as WitSeverity,
        };

        let dimensions = [
            (WitDimension::Correctness, EvaluationDimension::Correctness),
            (WitDimension::Format, EvaluationDimension::Format),
            (WitDimension::Safety, EvaluationDimension::Safety),
            (WitDimension::Quality, EvaluationDimension::Quality),
            (WitDimension::Latency, EvaluationDimension::Latency),
            (WitDimension::ToolUse, EvaluationDimension::ToolUse),
            (WitDimension::Calibration, EvaluationDimension::Calibration),
            (
                WitDimension::Other("custom".to_string()),
                EvaluationDimension::Other("custom".to_string()),
            ),
        ];
        for (input, expected) in dimensions {
            assert_eq!(map_dimension(input), expected);
        }

        let statuses = [
            (WitStatus::Passed, EvaluationStatus::Passed),
            (WitStatus::Failed, EvaluationStatus::Failed),
            (WitStatus::Error, EvaluationStatus::Error),
            (WitStatus::Skipped, EvaluationStatus::Skipped),
        ];
        for (input, expected) in statuses {
            assert_eq!(map_status(input), expected);
        }

        let severities = [
            (WitSeverity::None, Severity::None),
            (WitSeverity::Low, Severity::Low),
            (WitSeverity::Medium, Severity::Medium),
            (WitSeverity::High, Severity::High),
            (WitSeverity::Critical, Severity::Critical),
        ];
        for (input, expected) in severities {
            assert_eq!(map_severity(input), expected);
        }

        let preferences = [
            (WitPreference::Preferred, PreferenceOutcome::Preferred),
            (WitPreference::Tie, PreferenceOutcome::Tie),
            (WitPreference::NotPreferred, PreferenceOutcome::NotPreferred),
        ];
        for (input, expected) in preferences {
            assert_eq!(map_preference_outcome(input), expected);
        }
    }

    #[test]
    fn wit_score_mapping_covers_every_contract_variant() {
        let cases = [
            (
                wit_types::Score::Binary(true),
                Score::Binary { passed: true },
            ),
            (
                wit_types::Score::Range((0.5, 0.0, 1.0)),
                Score::Range {
                    value: 0.5,
                    min: 0.0,
                    max: 1.0,
                },
            ),
            (
                wit_types::Score::Normalized(0.75),
                Score::Normalized { value: 0.75 },
            ),
            (
                wit_types::Score::SeverityMapped(wit_types::Severity::Medium),
                Score::SeverityMapped {
                    severity: Severity::Medium,
                },
            ),
            (
                wit_types::Score::Preference(wit_types::PreferenceOutcome::Tie),
                Score::Preference {
                    outcome: PreferenceOutcome::Tie,
                },
            ),
            (wit_types::Score::Informational, Score::Informational),
        ];

        for (input, expected) in cases {
            assert_eq!(
                serde_json::to_value(map_score(input)).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
    }

    #[test]
    fn wit_output_mapping_preserves_findings_and_json_payloads() {
        let output = map_wit_output_to_output(wit_output()).unwrap();

        assert_eq!(output.evaluator.namespace, "vigilo");
        assert_eq!(output.metadata, json!({"duration_ms": 4}));
        assert_eq!(output.results.len(), 1);
        let finding = &output.results[0];
        assert_eq!(
            finding.dimension,
            EvaluationDimension::Other("style".to_string())
        );
        assert_eq!(finding.status, EvaluationStatus::Failed);
        assert_eq!(finding.severity, Severity::High);
        assert_eq!(finding.evidence, json!({"span": "answer"}));
        assert!(finding.blocking);
    }

    #[test]
    fn wit_output_mapping_labels_invalid_json_fields() {
        let mut output = wit_output();
        output.metadata_json = "{".to_string();
        let err = map_wit_output_to_output(output).unwrap_err();
        assert!(err.to_string().contains("invalid metadata-json JSON"));

        let mut output = wit_output();
        output.results[0].evidence_json = "{".to_string();
        let err = map_wit_output_to_output(output).unwrap_err();
        assert!(err.to_string().contains("invalid evidence-json JSON"));
    }

    #[test]
    fn package_metadata_embedding_is_idempotent_and_rejects_conflicts() {
        let original = minimal_wasm_module();
        let embedded =
            ensure_embedded_package_metadata(original.clone(), "example", "1.0.0").unwrap();
        let metadata = read_embedded_package_metadata(&embedded).unwrap().unwrap();

        assert_eq!(metadata.name, "example");
        assert_eq!(metadata.version, "1.0.0");
        assert!(embedded.len() > original.len());
        assert_eq!(
            ensure_embedded_package_metadata(embedded.clone(), "example", "1.0.0").unwrap(),
            embedded
        );

        let err = ensure_embedded_package_metadata(embedded, "other", "1.0.0").unwrap_err();
        assert!(err.to_string().contains("embedded vigilo.package mismatch"));
    }

    #[test]
    fn malformed_embedded_package_metadata_is_rejected() {
        let wasm = append_custom_section(
            &minimal_wasm_module(),
            PACKAGE_METADATA_SECTION,
            b"not-json",
        )
        .unwrap();

        let err = read_embedded_package_metadata(&wasm).unwrap_err();

        assert!(
            err.to_string()
                .contains("failed to decode vigilo.package metadata")
        );
    }

    #[test]
    fn strict_wit_metadata_requires_the_declared_contract() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("evaluator.wit"), TEST_WIT).unwrap();
        let valid = manifest_wit(
            "vigilo:evaluator",
            "0.1.0",
            "evaluator-world",
            "evaluator",
            true,
        );

        let metadata = resolve_wit_metadata(dir.path(), Some(&valid)).unwrap();
        assert_eq!(
            metadata.interface_name.as_deref(),
            Some("vigilo:evaluator/evaluator")
        );
        assert_eq!(metadata.interface_version.as_deref(), Some("0.1.0"));
        assert_eq!(metadata.wit_world.as_deref(), Some("evaluator-world"));

        let mismatches = [
            (
                "other:package",
                "0.1.0",
                "evaluator-world",
                "evaluator",
                "package mismatch",
            ),
            (
                "vigilo:evaluator",
                "9.0.0",
                "evaluator-world",
                "evaluator",
                "version mismatch",
            ),
            (
                "vigilo:evaluator",
                "0.1.0",
                "missing-world",
                "evaluator",
                "not found",
            ),
            (
                "vigilo:evaluator",
                "0.1.0",
                "evaluator-world",
                "missing-interface",
                "is not exported",
            ),
        ];
        for (package, version, world, interface, expected) in mismatches {
            let wit = manifest_wit(package, version, world, interface, true);
            let err = error_message(resolve_wit_metadata(dir.path(), Some(&wit)));
            assert!(err.contains(expected), "expected '{expected}' in '{err}'");
        }
    }

    #[test]
    fn optional_wit_metadata_accepts_missing_or_non_strict_contracts() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("evaluator.wit"), TEST_WIT).unwrap();

        let absent = resolve_wit_metadata(dir.path(), None).unwrap();
        assert!(absent.interface_name.is_none());
        assert!(absent.interface_version.is_none());
        assert!(absent.wit_world.is_none());

        let configured = manifest_wit("other:package", "9.0.0", "missing", "missing", false);
        let metadata = resolve_wit_metadata(dir.path(), Some(&configured)).unwrap();
        assert_eq!(
            metadata.interface_name.as_deref(),
            Some("other:package/missing")
        );
        assert_eq!(metadata.interface_version.as_deref(), Some("9.0.0"));
        assert_eq!(metadata.wit_world.as_deref(), Some("missing"));
    }

    #[test]
    fn wit_parser_rejects_files_without_a_package_declaration() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("evaluator.wit");
        fs::write(&path, "world evaluator-world {}\n").unwrap();

        let err = error_message(parse_wit_file(&path));

        assert!(err.contains("missing package declaration"));
    }

    #[test]
    fn evaluator_manifest_metadata_overrides_cargo_fallbacks() {
        let fallback = Package {
            manifest: "Cargo.toml".to_string(),
            description: None,
            tags: Vec::new(),
            metadata: None,
        };
        let resolved = resolve_evaluator_metadata(&fallback, &cargo_package_metadata()).unwrap();
        assert_eq!(resolved.description.as_deref(), Some("Cargo description"));
        assert_eq!(resolved.tags, json!(["cargo"]));
        assert_eq!(resolved.metadata, json!({"owner": "cargo"}));

        let overrides = Package {
            manifest: "Cargo.toml".to_string(),
            description: Some("Manifest description".to_string()),
            tags: vec!["manifest".to_string()],
            metadata: Some(toml::from_str("owner = 'manifest'").unwrap()),
        };
        let resolved = resolve_evaluator_metadata(&overrides, &cargo_package_metadata()).unwrap();
        assert_eq!(
            resolved.description.as_deref(),
            Some("Manifest description")
        );
        assert_eq!(resolved.tags, json!(["manifest"]));
        assert_eq!(resolved.metadata, json!({"owner": "manifest"}));
    }

    #[test]
    fn wasm_configuration_uses_bounded_defaults_and_deadlines() {
        let defaults = Config::default();
        defaults.store_limits().unwrap();
        assert_eq!(defaults.max_memory_bytes, DEFAULT_MAX_MEMORY_BYTES);
        assert_eq!(
            defaults.max_concurrent_evaluations,
            DEFAULT_MAX_CONCURRENT_EVALUATIONS
        );

        let mut config = defaults.clone();
        config.timeout_ms = 11;
        config.epoch_tick_interval_ms = 5;
        assert_eq!(config.epoch_deadline_ticks(), 3);

        config.timeout_ms = 0;
        assert_eq!(config.epoch_deadline_ticks(), 0);
    }

    #[test]
    fn runtime_identity_comes_from_the_engine_and_embedded_lockfile() {
        let engine = Engine::default();
        let fingerprint = get_engine_fingerprint(&engine);
        let version = resolve_runtime_version().unwrap();

        assert!(fingerprint.ends_with(&format!("-{ARCH}")));
        assert!(!version.is_empty());
        assert!(version.chars().next().unwrap().is_ascii_digit());
    }

    #[tokio::test]
    async fn wasm_context_initializes_once_and_bounds_concurrency() {
        let config = Config {
            timeout_ms: 0,
            max_concurrent_evaluations: 1,
            ..Config::default()
        };
        let context = Context {
            cell: OnceCell::new(),
            config,
        };

        let first = context.get().await.unwrap();
        let second = context.get().await.unwrap();
        assert!(std::ptr::eq(first, second));

        let permit = first.acquire_evaluation_permit().await.unwrap();
        assert!(
            first
                .evaluation_semaphore
                .clone()
                .try_acquire_owned()
                .is_err()
        );
        drop(permit);
        let _permit = first.acquire_evaluation_permit().await.unwrap();
    }
}
