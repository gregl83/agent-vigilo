use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
};

use serde::Deserialize;


#[derive(Deserialize)]
struct Package {
    manifest: String,
}

#[derive(Deserialize)]
struct Profile {
    wasm: String,
}

#[derive(Deserialize)]
struct Manifest {
    package: Package,
    profile: HashMap<String, Profile>,
}

fn read_manifest(crate_path: PathBuf) -> anyhow::Result<Manifest> {
    let content = fs::read_to_string(
        crate_path.join("Vigilo.toml")
    )?;
    let manifest: Manifest = toml::from_str(&content)?;
    Ok(manifest)
}
