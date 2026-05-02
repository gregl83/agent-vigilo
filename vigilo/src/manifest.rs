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
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub metadata: Option<toml::Value>,
}

#[derive(Deserialize)]
pub(crate) struct Profile {
    pub wasm: String,
}

fn default_false() -> bool {
    false
}

#[derive(Deserialize)]
pub(crate) struct Wit {
    pub path: String,
    pub world: String,
    pub package: String,
    pub version: String,
    pub interface: String,
    #[serde(default = "default_false")]
    pub strict: bool,
}

#[derive(Deserialize)]
pub(crate) struct Manifest {
    pub package: Package,
    pub wit: Option<Wit>,
    profile: HashMap<String, Profile>,
}

impl Manifest {
    pub fn get_profile(&self, profile_name: &str) -> anyhow::Result<&Profile> {
        self.profile
            .get(profile_name)
            .ok_or(anyhow!("manifest profile {} not supported", profile_name))
    }
}

pub(crate) fn read_manifest(crate_path: &PathBuf) -> anyhow::Result<Manifest> {
    let content = fs::read_to_string(crate_path.join("Vigilo.toml"))?;
    let manifest: Manifest = toml::from_str(&content)?;
    Ok(manifest)
}
