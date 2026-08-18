//! Harness-owned artifact paths, atomic persistence, leases, and digests.
//!
//! All generated state is confined to `target/perf`. Path validation and the
//! host campaign lease guard destructive replacement, while atomic writers keep
//! interrupted campaigns from publishing partially written machine contracts.

use std::{
    collections::BTreeMap,
    fs::{
        self,
        File,
        OpenOptions,
    },
    io::{
        Read,
        Write,
    },
    path::{
        Component,
        Path,
        PathBuf,
    },
    time::{
        SystemTime,
        UNIX_EPOCH,
    },
};

use anyhow::{
    Context,
    Result,
    bail,
};
use serde::Serialize;

/// Locates the enclosing Vigilo Cargo workspace from the current directory.
pub fn workspace_root() -> Result<PathBuf> {
    let current = std::env::current_dir().context("resolve current directory")?;
    for ancestor in current.ancestors() {
        let manifest = ancestor.join("Cargo.toml");
        if manifest.is_file()
            && fs::read_to_string(&manifest)
                .with_context(|| format!("read {}", manifest.display()))?
                .contains("[workspace]")
        {
            return Ok(ancestor.to_path_buf());
        }
    }
    bail!("run cargo perf from the agent-vigilo workspace")
}

/// Resolves an artifact path and proves that it remains under `target/perf`.
pub fn require_artifact_path(root: &Path, path: &Path) -> Result<PathBuf> {
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        bail!("artifact path cannot contain '..': {}", path.display());
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let allowed = root.join("target").join("perf");
    if !absolute.starts_with(&allowed) {
        bail!(
            "artifact path must be under {}: {}",
            allowed.display(),
            absolute.display()
        );
    }
    Ok(absolute)
}

/// Resolves a non-root artifact path beneath a named `target/perf` category.
pub fn require_artifact_subpath(root: &Path, path: &Path, category: &str) -> Result<PathBuf> {
    let absolute = require_artifact_path(root, path)?;
    let category_root = root.join("target").join("perf").join(category);
    if absolute == category_root || !absolute.starts_with(&category_root) {
        bail!(
            "artifact path must be a child of {}: {}",
            category_root.display(),
            absolute.display()
        );
    }
    Ok(absolute)
}

/// Creates an empty run directory, generating a run ID when no path is supplied.
pub fn create_run_dir(root: &Path, requested: Option<&Path>, prefix: &str) -> Result<PathBuf> {
    let path = match requested {
        Some(path) => require_artifact_subpath(root, path, "runs")?,
        None => root
            .join("target/perf/runs")
            .join(format!("{prefix}-{}", run_id())),
    };
    if path.exists() && path.read_dir()?.next().is_some() {
        bail!("run directory is not empty: {}", path.display());
    }
    fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
    Ok(path)
}

/// Generates a timestamped, process-scoped identifier for a harness run.
pub fn run_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{millis}-{}", std::process::id())
}

/// Exclusive host lease preventing concurrent build or measurement campaigns.
pub struct CampaignLease {
    path: PathBuf,
    owner: String,
}

impl CampaignLease {
    /// Acquires the workspace performance lease or rejects an active owner.
    pub fn acquire(root: &Path) -> Result<Self> {
        let directory = root.join("target/perf");
        fs::create_dir_all(&directory)?;
        let path = directory.join("campaign.lock");
        let owner = std::process::id().to_string();
        for _ in 0..3 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "{owner}")?;
                    file.sync_all()?;
                    return Ok(Self { path, owner });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let mut contents = String::new();
                    File::open(&path)?.read_to_string(&mut contents)?;
                    let active_owner = contents.trim().parse::<u32>().ok();
                    if active_owner.is_some_and(process_is_alive) {
                        bail!(
                            "another cargo perf build/run/compare owns the host lease (PID {})",
                            active_owner.unwrap_or_default()
                        );
                    }
                    let _ = fs::remove_file(&path);
                }
                Err(error) => return Err(error).context("create performance campaign lease"),
            }
        }
        bail!("could not acquire performance campaign lease")
    }
}

impl Drop for CampaignLease {
    fn drop(&mut self) {
        let owned =
            fs::read_to_string(&self.path).is_ok_and(|contents| contents.trim() == self.owner);
        if owned {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn process_is_alive(process_id: u32) -> bool {
    let result = unsafe { libc::kill(process_id as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_is_alive(process_id: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle,
            GetLastError,
            STILL_ACTIVE,
        },
        System::Threading::{
            GetExitCodeProcess,
            OpenProcess,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
    };

    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        // Access denied is safer to treat as live; invalid PID means the lease is stale.
        return unsafe { GetLastError() } != 87;
    }
    let mut exit_code = 0;
    let alive = unsafe { GetExitCodeProcess(process, &raw mut exit_code) } != 0
        && exit_code == STILL_ACTIVE as u32;
    unsafe { CloseHandle(process) };
    alive
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(_process_id: u32) -> bool {
    true
}

/// Atomically serializes a value as pretty JSON.
pub fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    atomic_bytes(path, &bytes)
}

/// Atomically serializes values as newline-delimited JSON records.
pub fn atomic_jsonl<T: Serialize>(path: &Path, values: &[T]) -> Result<()> {
    let mut bytes = Vec::new();
    for value in values {
        serde_json::to_writer(&mut bytes, value)?;
        bytes.push(b'\n');
    }
    atomic_bytes(path, &bytes)
}

/// Atomically replaces a UTF-8 text artifact.
pub fn atomic_text(path: &Path, value: &str) -> Result<()> {
    atomic_bytes(path, value.as_bytes())
}

fn atomic_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("missing parent for {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("artifact"),
        std::process::id()
    ));
    let mut file = File::create(&temp).with_context(|| format!("create {}", temp.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    commit_temp(&temp, path)?;
    Ok(())
}

#[cfg(unix)]
fn commit_temp(temp: &Path, path: &Path) -> Result<()> {
    fs::rename(temp, path).with_context(|| format!("commit {}", path.display()))?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(windows)]
fn commit_temp(temp: &Path, path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING,
        MOVEFILE_WRITE_THROUGH,
        MoveFileExW,
    };

    let mut source: Vec<u16> = temp.as_os_str().encode_wide().collect();
    source.push(0);
    let mut destination: Vec<u16> = path.as_os_str().encode_wide().collect();
    destination.push(0);
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        bail!(
            "commit {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn commit_temp(temp: &Path, path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temp, path).with_context(|| format!("commit {}", path.display()))
}

/// Computes the BLAKE3 digest of one file.
pub fn digest_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// Computes a deterministic BLAKE3 digest over a directory tree.
pub fn digest_tree(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok(blake3::hash(b"").to_hex().to_string());
    }
    let mut files = Vec::new();
    collect_files(path, path, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut hasher = blake3::Hasher::new();
    for (relative, file) in files {
        hasher.update(relative.to_string_lossy().replace('\\', "/").as_bytes());
        hasher.update(b"\0");
        hasher.update(&fs::read(file)?);
        hasher.update(b"\0");
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn collect_files(root: &Path, path: &Path, files: &mut Vec<(PathBuf, PathBuf)>) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(root, &entry.path(), files)?;
        } else if file_type.is_file() {
            files.push((entry.path().strip_prefix(root)?.to_path_buf(), entry.path()));
        }
    }
    Ok(())
}

/// Recursively copies a source tree without following external state.
pub fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    for entry in fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir_all(&destination)?;
            copy_tree(&entry.path(), &destination)?;
        } else {
            fs::create_dir_all(target)?;
            fs::copy(entry.path(), &destination)?;
        }
    }
    Ok(())
}

/// Returns the total byte size of regular files beneath a directory.
pub fn directory_bytes(path: &Path) -> Result<u64> {
    let mut total = 0;
    if !path.exists() {
        return Ok(0);
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            total += directory_bytes(&entry.path())?;
        } else {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

/// Creates an empty additive-extension map for a versioned document.
pub fn no_extra() -> BTreeMap<String, serde_json::Value> {
    BTreeMap::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_paths_cannot_escape_target_perf() {
        let root = Path::new("C:/workspace");
        assert!(require_artifact_path(root, Path::new("target/perf/run")).is_ok());
        assert!(require_artifact_path(root, Path::new("target/run")).is_err());
        assert!(require_artifact_path(root, Path::new("target/perf/../escape")).is_err());
        assert!(require_artifact_subpath(root, Path::new("target/perf/builds"), "builds").is_err());
        assert!(
            require_artifact_subpath(root, Path::new("target/perf/builds/candidate"), "builds")
                .is_ok()
        );
    }

    #[test]
    fn campaign_lease_is_exclusive_and_released() {
        let root = tempfile::tempdir().unwrap();
        let first = CampaignLease::acquire(root.path()).unwrap();
        assert!(CampaignLease::acquire(root.path()).is_err());
        drop(first);
        assert!(CampaignLease::acquire(root.path()).is_ok());
    }

    #[test]
    fn atomic_writer_replaces_a_committed_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("artifact.jsonl");
        atomic_text(&path, "first\n").unwrap();
        atomic_text(&path, "second\n").unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "second\n");
    }

    #[test]
    fn tree_copy_digests_and_sizes_are_deterministic() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let target = directory.path().join("target");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("a.txt"), b"alpha").unwrap();
        fs::write(source.join("nested/b.txt"), b"beta").unwrap();

        let digest = digest_tree(&source).unwrap();
        copy_tree(&source, &target).unwrap();
        assert_eq!(digest_tree(&target).unwrap(), digest);
        assert_eq!(directory_bytes(&target).unwrap(), 9);
        assert_eq!(
            digest_file(&target.join("a.txt")).unwrap(),
            blake3::hash(b"alpha").to_hex().to_string()
        );
        assert_eq!(
            directory_bytes(&directory.path().join("missing")).unwrap(),
            0
        );
        assert_eq!(
            digest_tree(&directory.path().join("missing")).unwrap(),
            blake3::hash(b"").to_hex().to_string()
        );
    }

    #[test]
    fn json_jsonl_and_run_directories_use_owned_paths() {
        let root = tempfile::tempdir().unwrap();
        let run = create_run_dir(root.path(), None, "unit").unwrap();
        assert!(run.starts_with(root.path().join("target/perf/runs")));
        fs::write(run.join("occupied"), "data").unwrap();
        assert!(create_run_dir(root.path(), Some(&run), "unit").is_err());

        let json = run.join("value.json");
        let jsonl = run.join("values.jsonl");
        atomic_json(&json, &serde_json::json!({"value": 1})).unwrap();
        atomic_jsonl(
            &jsonl,
            &[
                serde_json::json!({"value": 1}),
                serde_json::json!({"value": 2}),
            ],
        )
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(json).unwrap()).unwrap()["value"],
            1
        );
        assert_eq!(fs::read_to_string(jsonl).unwrap().lines().count(), 2);
        let id = run_id();
        let (timestamp, process) = id.split_once('-').unwrap();
        assert!(timestamp.parse::<u128>().is_ok());
        assert_eq!(process, std::process::id().to_string());
        assert!(no_extra().is_empty());
    }
}
