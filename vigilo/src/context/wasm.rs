// use std::fs;
// use std::hash::{DefaultHasher, Hash, Hasher};
// use std::path::{Path, PathBuf};
// use moka::future::Cache;
// use wasmtime::component::Component;
// use wasmtime::{Config, Engine};
// use sqlx::PgPool;
// use std::sync::Arc;
//
// pub struct Registry {
//     engine: Engine,
//     pool: PgPool,
//     cache: Cache<String, Component>,
// }
//
// impl Registry {
//     pub fn new(engine: Engine, pool: PgPool) -> Self {
//         let cache = Cache::builder()
//             .weigher(|_key: &String, _component: &Component| {
//                 // Component doesn't expose size directly — use a fixed weight
//                 // or track size separately; tune based on your module sizes
//                 1u32
//             })
//             .max_capacity(512)  // max 512 components in memory
//             .build();
//
//         Self { engine, pool, cache }
//     }
//
//     /// Install a wasm component into postgres and warm the cache
//     pub async fn install(
//         &self,
//         name: &str,
//         version: &str,
//         wasm_bytes: Vec<u8>,
//     ) -> anyhow::Result<()> {
//         // 1. validate before touching db
//         Component::validate(&self.engine, &wasm_bytes)?;
//
//         // 2. hash for dedup + cache key
//         let hash = blake3::hash(&wasm_bytes).to_hex().to_string();
//
//         // 3. compile to get the component
//         let component = Component::new(&self.engine, &wasm_bytes)?;
//
//         // 4. serialize compiled artifact
//         let compiled = component.serialize()?;
//
//         // 5. fingerprint for cache invalidation across versions/arches
//         let fingerprint = engine_fingerprint(&self.engine);
//
//         // 6. store everything in postgres
//         sqlx::query!(
//             r#"
//             INSERT INTO wasm_modules (name, version, wasm_bytes, compiled_cache, compiled_for, hash)
//             VALUES ($1, $2, $3, $4, $5, $6)
//             ON CONFLICT (name, version) DO UPDATE
//                 SET wasm_bytes    = EXCLUDED.wasm_bytes,
//                     compiled_cache = EXCLUDED.compiled_cache,
//                     compiled_for  = EXCLUDED.compiled_for,
//                     hash          = EXCLUDED.hash
//             "#,
//             name,
//             version,
//             &wasm_bytes,
//             &compiled,
//             fingerprint,
//             hash,
//         )
//             .execute(&self.pool)
//             .await?;
//
//         // 7. warm the cache immediately — skip cold start on first invocation
//         self.cache.insert(hash, component).await;
//
//         Ok(())
//     }
//
//     /// Load a component — cache first, then compiled_cache, then recompile
//     pub async fn load(
//         &self,
//         name: &str,
//         version: &str,
//     ) -> anyhow::Result<Component> {
//         // 1. fetch hash only — cheap, avoids pulling bytes on cache hit
//         let row = sqlx::query!(
//             "SELECT hash FROM wasm_modules WHERE name = $1 AND version = $2 AND is_active = true",
//             name,
//             version,
//         )
//             .fetch_one(&self.pool)
//             .await?;
//
//         let hash = row.hash;
//
//         // 2. check moka — instant hit
//         if let Some(component) = self.cache.get(&hash).await {
//             return Ok(component);
//         }
//
//         // 3. cache miss — fetch full row
//         let row = sqlx::query!(
//             r#"
//             SELECT wasm_bytes, compiled_cache, compiled_for
//             FROM wasm_modules
//             WHERE name = $1 AND version = $2 AND is_active = true
//             "#,
//             name,
//             version,
//         )
//             .fetch_one(&self.pool)
//             .await?;
//
//         let fingerprint = engine_fingerprint(&self.engine);
//
//         // 4. try compiled_cache first — fast deserialization
//         let component = if let (Some(compiled), Some(compiled_for)) =
//             (row.compiled_cache, row.compiled_for)
//         {
//             if compiled_for == fingerprint {
//                 // SAFETY: bytes were produced by our own serialization
//                 // and fingerprint confirms same engine version + arch
//                 unsafe { Component::deserialize(&self.engine, &compiled)? }
//             } else {
//                 // fingerprint mismatch — recompile and update cache
//                 self.recompile(&row.wasm_bytes, name, version, &fingerprint).await?
//             }
//         } else {
//             // no compiled cache — first time on this arch/version
//             self.recompile(&row.wasm_bytes, name, version, &fingerprint).await?
//         };
//
//         // 5. insert into moka
//         self.cache.insert(hash, component.clone()).await;
//
//         Ok(component)
//     }
//
//     /// Recompile from source and write compiled artifact back to postgres
//     async fn recompile(
//         &self,
//         wasm_bytes: &[u8],
//         name: &str,
//         version: &str,
//         fingerprint: &str,
//     ) -> anyhow::Result<Component> {
//         let component = Component::new(&self.engine, wasm_bytes)?;
//         let compiled = component.serialize()?;
//
//         // write back async — don't block the caller
//         let pool = self.pool.clone();
//         let fingerprint = fingerprint.to_string();
//         let name = name.to_string();
//         let version = version.to_string();
//
//         tokio::spawn(async move {
//             let _ = sqlx::query!(
//                 r#"
//                 UPDATE wasm_modules
//                 SET compiled_cache = $1, compiled_for = $2
//                 WHERE name = $3 AND version = $4
//                 "#,
//                 &compiled,
//                 fingerprint,
//                 name,
//                 version,
//             )
//                 .execute(&pool)
//                 .await;
//         });
//
//         Ok(component)
//     }
//
//     /// Deactivate an evaluator — soft delete
//     pub async fn remove(&self, name: &str, version: &str) -> anyhow::Result<()> {
//         // invalidate cache first
//         let row = sqlx::query!(
//             "SELECT hash FROM wasm_modules WHERE name = $1 AND version = $2",
//             name,
//             version,
//         )
//             .fetch_optional(&self.pool)
//             .await?;
//
//         if let Some(row) = row {
//             self.cache.invalidate(&row.hash).await;
//         }
//
//         sqlx::query!(
//             "UPDATE wasm_modules SET is_active = false WHERE name = $1 AND version = $2",
//             name,
//             version,
//         )
//             .execute(&self.pool)
//             .await?;
//
//         Ok(())
//     }
// }


use std::{
    env::consts::ARCH,
    fs,
    hash::{
        DefaultHasher,
        Hash,
        Hasher,
    },
    path::PathBuf,
};
use std::time::SystemTime;
use cargo_metadata::MetadataCommand;
use cargo_toml::Manifest;
use serde::{
    Deserialize,
    Serialize,
};
use serde_json::Value;
use tokio::sync::OnceCell;
use tracing::{
    debug,
    warn,
};
use wasmparser::{
    Parser,
    Payload,
};
use wasmtime::{
    component,
    Config as EngineConfig,
    Engine,
    Store,
};

use super::super::manifest::{
    Wit,
    read_manifest,
};

mod evaluator_test_bindings {
    wasmtime::component::bindgen!({
        path: "../wit/evaluator.wit",
        world: "evaluator-world",
    });
}

struct EvaluatorTestHost;

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

fn read_embedded_package_metadata(wasm_bytes: &[u8]) -> anyhow::Result<Option<EmbeddedPackageMetadata>> {
    for payload in Parser::new(0).parse_all(wasm_bytes) {
        if let Payload::CustomSection(section) = payload? {
            if section.name() == PACKAGE_METADATA_SECTION {
                let metadata = serde_json::from_slice::<EmbeddedPackageMetadata>(section.data())
                    .map_err(|err| anyhow::anyhow!("failed to decode {} metadata: {}", PACKAGE_METADATA_SECTION, err))?;
                return Ok(Some(metadata));
            }
        }
    }

    Ok(None)
}

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

fn append_custom_section(wasm_bytes: &[u8], section_name: &str, section_data: &[u8]) -> anyhow::Result<Vec<u8>> {
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
            let embedded = read_embedded_package_metadata(&out)?
                .ok_or_else(|| anyhow::anyhow!("failed to read back embedded {} metadata", PACKAGE_METADATA_SECTION))?;

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

    let package = package.ok_or_else(|| anyhow::anyhow!("missing package declaration in WIT file {}", path.display()))?;

    Ok(WitDocument {
        package,
        version,
        worlds,
    })
}

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

    let world = parsed
        .worlds
        .iter()
        .find(|w| w.name == wit.world);

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

fn value_from_toml(value: &toml::Value) -> anyhow::Result<Value> {
    serde_json::to_value(value)
        .map_err(|err| anyhow::anyhow!("failed to encode TOML value to JSON: {}", err))
}

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
        None => cargo.metadata.clone().unwrap_or_else(|| Value::Object(Default::default())),
    };

    Ok(EvaluatorMetadata {
        description,
        tags: Value::Array(tags.into_iter().map(Value::String).collect()),
        metadata,
    })
}

fn get_package_metadata(package_path: &PathBuf, manifest_file: &String) -> anyhow::Result<PackageMetadata> {
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

            let package = manifest.package.ok_or_else(|| {
                anyhow::anyhow!("no [package] section found in Cargo.toml")
            })?;

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

            Ok(
                PackageMetadata {
                    name: package.name,
                    version: package.version.get()?.to_string(),
                    target_dir: target_dir.into_std_path_buf(),
                    modified: fs_metadata.modified()?,
                    description,
                    tags,
                    metadata,
                }
            )
        }
        _ => {
            Err(anyhow::anyhow!("Vigilo.toml [package] manifest {} is unsupported", manifest_file))
        }
    }
}

fn get_engine_fingerprint(engine: &Engine) -> String {
    let mut hasher = DefaultHasher::new();
    engine.precompile_compatibility_hash().hash(&mut hasher);
    format!("{:x}-{}", hasher.finish(), ARCH)
}

fn resolve_runtime_version() -> anyhow::Result<String> {
    let lock_content = include_str!("../../../Cargo.lock");
    let lock = toml::from_str::<CargoLockDocument>(lock_content)
        .map_err(|err| anyhow::anyhow!("failed to parse embedded Cargo.lock: {}", err))?;

    lock.package
        .into_iter()
        .find(|pkg| pkg.name == WASM_RUNTIME_NAME)
        .map(|pkg| pkg.version)
        .ok_or_else(|| anyhow::anyhow!("{} dependency was not found in embedded Cargo.lock", WASM_RUNTIME_NAME))
}

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

        Ok(
            Self {
                engine,
                fingerprint,
            }
        )
    }

    pub fn prepare_evaluator(&self, package_path: PathBuf, profile: String) -> anyhow::Result<Component> {
        let manifest = read_manifest(&package_path)?;
        let manifest_profile = manifest.get_profile(profile)?;
        let wit_metadata = resolve_wit_metadata(&package_path, manifest.wit.as_ref())?;

        let package_metadata = get_package_metadata(
            &package_path,
            &manifest.package.manifest,
        )?;
        let evaluator_metadata = resolve_evaluator_metadata(&manifest.package, &package_metadata)?;

        let wasm_path = package_metadata.target_dir.join(&manifest_profile.wasm);

        let fs_wasm_metadata = fs::metadata(&wasm_path)?;
        let wasm_modified = fs_wasm_metadata.modified()?;
        if package_metadata.modified > wasm_modified {
            return Err(anyhow::anyhow!("evaluation manifest was modified after wasm build"));
        }
        let wasm_bytes = fs::read(wasm_path)?;
        let wasm_bytes = ensure_embedded_package_metadata(
            wasm_bytes,
            &package_metadata.name,
            &package_metadata.version,
        )?;
        let wasm_hash = blake3::hash(&wasm_bytes).to_hex().to_string();

        let component = component::Component::new(
            &self.engine,
            &wasm_bytes,
        )?;
        let serialized = component.serialize()?;

        let runtime_version = resolve_runtime_version()?;

        Ok(
            Component{
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
            }
        )
    }

    pub fn test_evaluator(&self, wasm_bytes: &[u8], db_context: String) -> anyhow::Result<String> {
        let component = component::Component::new(&self.engine, wasm_bytes)?;
        let mut linker = component::Linker::new(&self.engine);

        evaluator_test_bindings::vigilo::evaluator::executor::add_to_linker::<_, wasmtime::component::HasSelf<EvaluatorTestHost>>(
            &mut linker,
            |host: &mut EvaluatorTestHost| host,
        )?;

        let mut store = Store::new(&self.engine, EvaluatorTestHost);
        let bindings = evaluator_test_bindings::EvaluatorWorld::instantiate(
            &mut store,
            &component,
            &linker,
        )?;

        let input = evaluator_test_bindings::vigilo::evaluator::types::Input {
            context: evaluator_test_bindings::vigilo::evaluator::types::Context {
                db: db_context,
            },
        };

        let output = bindings
            .vigilo_evaluator_evaluator()
            .call_evaluate(&mut store, &input)?
            .map_err(|err| anyhow::anyhow!("evaluator returned error: {}", err))?;

        Ok(output.data.val)
    }
}

pub(crate) struct Context {
    pub(crate) cell: OnceCell<Wasm>,
    pub(crate) config: Config,
}

impl Context {
    pub async fn get(&self) -> anyhow::Result<&Wasm> {
        self.cell.get_or_try_init(|| async {
            debug!("initializing wasm engine");
            Ok(
                Wasm::new(self.config.clone())?
            )
        }).await
    }
}

/*
use std::path::{Path, PathBuf};
use cargo_metadata::MetadataCommand;

impl Wasm {
    pub fn find_wasm_artifact(crate_root: &Path, is_release: bool) -> anyhow::Result<PathBuf> {
        // 1. Fetch metadata for the crate
        let metadata = MetadataCommand::new()
            .manifest_path(crate_root.join("Cargo.toml"))
            .exec()?;

        // 2. Get the target directory (absolute path)
        let target_dir = metadata.target_directory.as_std_path();

        // 3. Identify the package name
        let root_package = metadata.root_package()
            .ok_or_else(|| anyhow::anyhow!("Could not find root package"))?;

        // 4. Transform name: Cargo replaces '-' with '_' for filenames
        let file_name = format!("{}.wasm", root_package.name.replace("-", "_"));

        // 5. Construct the final path
        // Common target for components is wasm32-wasip1
        let profile = if is_release { "release" } else { "debug" };

        let wasm_path = target_dir
            .join("wasm32-wasip1")
            .join(profile)
            .join(file_name);

        if !wasm_path.exists() {
            return Err(anyhow::anyhow!(
                "WASM artifact not found at {:?}. Did you run 'cargo component build'?",
                wasm_path
            ));
        }

        Ok(wasm_path)
    }
}


use clap::Parser;

#[derive(Parser)]
pub struct Cli {
    /// Build artifacts in release mode, with optimizations
    #[arg(short, long)]
    pub release: bool,

    /// Build artifacts with the specified Cargo profile
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,

    // ... other args like path ...
}

impl Cli {
    /// Resolve which folder name to look for in the target directory
    pub fn resolve_profile(&self) -> &str {
        if let Some(ref p) = self.profile {
            p
        } else if self.release {
            "release"
        } else {
            "debug"
        }
    }
}
 */

