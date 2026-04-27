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
use tokio::sync::OnceCell;
use tracing::debug;
use wasmtime::{
    component,
    Config as EngineConfig,
    Engine,
};

use super::super::manifest::read_manifest;


struct PackageMetadata {
    name: String,
    version: String,
    target_dir: PathBuf,
    modified: SystemTime,
}

fn get_package_metadata(package_path: &PathBuf, manifest_file: &String) -> anyhow::Result<PackageMetadata> {
    match manifest_file.as_str() {
        "Cargo.toml" => {
            let manifest_path = package_path.join(manifest_file);

            let fs_metadata = fs::metadata(&manifest_path)?;

            let metadata = MetadataCommand::new()
                .manifest_path(&manifest_path)
                .exec()?;

            let target_dir = metadata.target_directory;

            let manifest = Manifest::from_path(&manifest_path)?;

            let package = manifest.package.ok_or_else(|| {
                anyhow::anyhow!("no [package] section found in Cargo.toml")
            })?;

            Ok(
                PackageMetadata {
                    name: package.name,
                    version: package.version.get()?.to_string(),
                    target_dir: target_dir.into_std_path_buf(),
                    modified: fs_metadata.modified()?,
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

pub struct Component {
    pub name: String,
    pub version: String,
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

    pub fn build(&self, package_path: PathBuf, profile: String) -> anyhow::Result<Component> {
        let manifest = read_manifest(&package_path)?;
        let manifest_profile = manifest.get_profile(profile)?;

        let package_metadata = get_package_metadata(
            &package_path,
            &manifest.package.manifest,
        )?;

        let wasm_path = package_metadata.target_dir.join(&manifest_profile.wasm);

        let fs_wasm_metadata = fs::metadata(&wasm_path)?;
        let wasm_modified = fs_wasm_metadata.modified()?;
        if package_metadata.modified > wasm_modified {
            return Err(anyhow::anyhow!("evaluation manifest was modified after wasm build"));
        }
        let wasm_bytes = fs::read(wasm_path)?;
        let wasm_hash = blake3::hash(&wasm_bytes).to_hex().to_string();

        let component = component::Component::new(
            &self.engine,
            &wasm_bytes,
        )?;
        let serialized = component.serialize()?;

        Ok(
            Component{
                name: package_metadata.name,
                version: package_metadata.version,
                component,
                wasm_hash,
                wasm_bytes,
                serialized,
            }
        )
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

