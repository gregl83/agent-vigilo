use std::{
    env::consts::ARCH,
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    io::ErrorKind,
    path::PathBuf,
    time::SystemTime,
};

use cargo_metadata::MetadataCommand;
use cargo_toml::Manifest;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::OnceCell;
use tracing::{debug, warn};
use wasmparser::{Parser, Payload};
use wasmtime::{Config as EngineConfig, Engine, Store, component, component::ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::contracts::evaluator::{
    EvaluationDimension, EvaluationStatus, EvaluatorFinding, EvaluatorIdentity, EvaluatorInput,
    EvaluatorOutput, PreferenceOutcome, Score, Severity,
};

use super::super::manifest::{Wit, read_manifest};

mod evaluator_test_bindings {
    wasmtime::component::bindgen!({
        path: "../wit/evaluator.wit",
        world: "evaluator-world",
    });
}

struct EvaluatorTestHost {
    table: ResourceTable,
    ctx: WasiCtx,
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
        debug!("evaluator.trace: {}", msg);
    }

    fn debug(&mut self, msg: String) {
        debug!("evaluator.debug: {}", msg);
    }

    fn warn(&mut self, msg: String) {
        warn!("evaluator.warn: {}", msg);
    }

    fn error(&mut self, msg: String) {
        warn!("evaluator.error: {}", msg);
    }

    fn send_http_request(
        &mut self,
        _req: evaluator_test_bindings::vigilo::evaluator::executor::HttpRequest,
    ) -> Result<evaluator_test_bindings::vigilo::evaluator::executor::HttpResponse, String> {
        Err("send_http_request is not enabled yet; outbound HTTP policy enforcement is not configured".to_string())
    }
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
        if let Payload::CustomSection(section) = payload? {
            if section.name() == PACKAGE_METADATA_SECTION {
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
    package_path: &PathBuf,
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
    package_path: &PathBuf,
    manifest_file: &String,
) -> anyhow::Result<PackageMetadata> {
    match manifest_file.as_str() {
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
    pub component: component::Component,
    pub wasm_hash: String,
    pub wasm_bytes: Vec<u8>,
    pub serialized: Vec<u8>,
}

#[derive(Clone)]
pub struct Config {}

impl Default for Config {
    fn default() -> Self {
        Self {}
    }
}

/// Wasm engine wrapper.
pub struct Wasm {
    engine: Engine,
    fingerprint: String,
}

impl Wasm {
    pub fn new(_config: Config) -> anyhow::Result<Self> {
        let mut engine_config = EngineConfig::new();
        engine_config.wasm_component_model(true);

        let engine = Engine::new(&engine_config)?;
        let fingerprint = get_engine_fingerprint(&engine);

        Ok(Self {
            engine,
            fingerprint,
        })
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

    /// Run evaluator in test mode.
    pub fn test_evaluator(
        &self,
        wasm_bytes: &[u8],
        input: EvaluatorInput,
    ) -> anyhow::Result<EvaluatorOutput> {
        let component = component::Component::new(&self.engine, wasm_bytes)?;
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
            },
        );
        let bindings =
            evaluator_test_bindings::EvaluatorWorld::instantiate(&mut store, &component, &linker)?;

        let input = map_input_to_wit_input(input)?;

        let output = bindings
            .vigilo_evaluator_evaluator()
            .call_evaluate(&mut store, &input)?
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
                Ok(Wasm::new(self.config.clone())?)
            })
            .await
    }
}
