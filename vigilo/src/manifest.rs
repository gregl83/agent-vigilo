use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
};
use anyhow::anyhow;
use serde::Deserialize;


#[derive(Deserialize)]
pub(crate) struct Package {
    pub manifest: String,
}

#[derive(Deserialize)]
pub(crate) struct Profile {
    pub wasm: String,
}

#[derive(Deserialize)]
pub(crate) struct Manifest {
    pub package: Package,
    profile: HashMap<String, Profile>,
}

impl Manifest {
    pub fn get_profile(&self, profile_name: String) -> anyhow::Result<&Profile> {
        self.profile.get(&profile_name).ok_or(
            anyhow!("manifest profile {} not supported", profile_name)
        )
    }
}

pub(crate) fn read_manifest(crate_path: &PathBuf) -> anyhow::Result<Manifest> {
    let content = fs::read_to_string(
        crate_path.join("Vigilo.toml")
    )?;
    let manifest: Manifest = toml::from_str(&content)?;
    Ok(manifest)
}
