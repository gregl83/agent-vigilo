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
//
//
// fn engine_fingerprint(engine: &Engine) -> String {
//     let mut hasher = DefaultHasher::new();
//     engine.precompile_compatibility_hash().hash(&mut hasher);
//     format!("{:x}-{}", hasher.finish(), std::env::consts::ARCH)
// }
//
// pub struct Wasm {
//     engine: Engine,
//     component: Component,
//     fingerprint: String,
//     hash: String, // store as string in postgres?  BYTEA
//     serialized: Vec<u8>,
// }
//
// fn build(path: &PathBuf) -> anyhow::Result<(String, String, Vec<u8>, Vec<u8>)> {
//     let wasm_bytes = fs::read(path)?;
//
//     let mut config = Config::new();
//     config.wasm_component_model(true);
//
//     let engine = Engine::new(&config)?;
//
//     let component = Component::new(
//         &engine,
//         &wasm_bytes
//     )?;
//
//     Ok((
//         engine_fingerprint(&engine),
//         blake3::hash(&wasm_bytes).to_hex().to_string(),
//         wasm_bytes,
//         component.serialize()?
//     ))
// }


use std::sync::Mutex;
use tokio::sync::OnceCell;
use tracing::{debug, error};
use wasmtime::{Config, Engine};

pub struct Wasm {
    engine: Mutex<Engine>,
}

impl Wasm {
    pub fn new() -> Self {
        Self {
            engine: Mutex::new(
                Engine::new(
                    &Config::default(),
                ).unwrap()
            )
        }
    }

    // todo - wasm build functionality
}

pub struct Context {
    pub(crate) cell: OnceCell<Wasm>,
}

impl Context {
    pub async fn get(&self) -> anyhow::Result<&Wasm> {
        self.cell.get_or_try_init(|| async {
            debug!("initializing wasm engine");
            Ok(Wasm::new())
        }).await
    }
}
