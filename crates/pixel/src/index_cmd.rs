//! `pixel index pack` / `pixel index unpack` — portable index bundles.
//!
//! The team's sharing story: CI (or one developer) runs `pixel build-index`
//! once, `pack` freezes `.pixel`'s index files into a single `.pxpack` tar
//! with a per-file xxh3 manifest, and everyone else `unpack`s it — from a
//! path or an `https://` URL — instead of rebuilding. `history.db` stays
//! opt-in behind `--include-history`: it is the lazy index this PR made
//! demand-driven, so it is not packed by default.
//!
//! `unpack` refuses while a daemon is running for the repo (it holds open
//! handles on the files being replaced) unless `--force`, verifies every
//! file's hash before it lands, and warns — does not refuse — when the
//! pack's `repo_head` differs from the checkout's: a stale index is a
//! freshness question the incremental path already answers.

use std::io::Read;
use std::path::{Path, PathBuf};

use pixel_index::index::SHARD_DIR;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MANIFEST: &str = "manifest.json";
const PACK_FORMAT: u32 = 1;
/// URL fetch ceiling: a pack is index bytes, not a page — generous but
/// bounded so a wrong URL can't stream forever.
const FETCH_CAP: u64 = 2 * 1024 * 1024 * 1024;
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Index-bearing files packed by default. Journals, locks, targets, and
/// plan/workspace state are local and never travel.
const INDEX_FILES: &[&str] = &["graph.db", "base.shard", "calls.json", "state.json"];

#[derive(clap::Subcommand)]
pub enum IndexCmd {
    /// Freeze this repo's index into a single `.pxpack` bundle.
    Pack {
        /// Output file (e.g. index.pxpack).
        #[arg(long)]
        out: PathBuf,
        /// Also pack history.db (the on-demand facts index).
        #[arg(long)]
        include_history: bool,
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Install a packed index into this repo's `.pixel/`.
    Unpack {
        /// Pack file path or https:// URL.
        source: String,
        /// Replace the index while a daemon is running.
        #[arg(long)]
        force: bool,
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: u32,
    pixel_version: String,
    created_at_ms: i64,
    repo_head: Option<String>,
    files: Vec<ManifestFile>,
}

#[derive(Serialize, Deserialize)]
struct ManifestFile {
    path: String,
    size: u64,
    xxh3: String,
}

fn xxh3_of(bytes: &[u8]) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(bytes))
}

/// The files `pack` captures: INDEX_FILES plus `-wal`/`-shm` SQLite
/// sidecars when present, so a WAL-mode graph.db travels complete.
fn pack_list(shard_dir: &Path, include_history: bool) -> Vec<PathBuf> {
    let mut names: Vec<String> = INDEX_FILES.iter().map(ToString::to_string).collect();
    if include_history {
        names.push("history.db".to_string());
    }
    // SQLite sidecars must travel with their db or the pack loses pages.
    for extra in [
        "graph.db-wal",
        "graph.db-shm",
        "history.db-wal",
        "history.db-shm",
    ] {
        if include_history || !extra.starts_with("history") {
            names.push(extra.to_string());
        }
    }
    names
        .into_iter()
        .map(|n| shard_dir.join(&n))
        .filter(|p| p.is_file())
        .collect()
}

fn pack(root: &Path, out: &Path, include_history: bool) -> Result<Value, String> {
    let shard_dir = root.join(SHARD_DIR);
    let files = pack_list(&shard_dir, include_history);
    if files.is_empty() {
        return Err(format!(
            "index pack: nothing to pack under {} — run `pixel build-index` first",
            shard_dir.display()
        ));
    }
    let mut entries = Vec::new();
    let mut blobs: Vec<(String, Vec<u8>)> = Vec::new();
    for path in &files {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("index pack: bad name {}", path.display()))?;
        let bytes =
            std::fs::read(path).map_err(|e| format!("index pack: {}: {e}", path.display()))?;
        entries.push(ManifestFile {
            path: name.to_string(),
            size: bytes.len() as u64,
            xxh3: xxh3_of(&bytes),
        });
        blobs.push((name.to_string(), bytes));
    }
    let manifest = Manifest {
        format: PACK_FORMAT,
        pixel_version: env!("CARGO_PKG_VERSION").to_string(),
        created_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64),
        repo_head: pixel_git::GitRunner::new(root).rev_parse_head(),
        files: entries,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;

    let file = std::fs::File::create(out).map_err(|e| format!("index pack: {e}"))?;
    let mut builder = tar::Builder::new(file);
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, MANIFEST, manifest_bytes.as_slice())
        .map_err(|e| format!("index pack: {e}"))?;
    for (name, bytes) in &blobs {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("files/{name}"), bytes.as_slice())
            .map_err(|e| format!("index pack: {e}"))?;
    }
    builder.finish().map_err(|e| format!("index pack: {e}"))?;
    Ok(json!({
        "out": out.display().to_string(),
        "files": manifest.files.len(),
        "bytes": std::fs::metadata(out).map_or(0, |m| m.len()),
        "repo_head": manifest.repo_head,
    }))
}

/// Read the pack bytes from a path or an https URL.
fn fetch_source(source: &str) -> Result<Vec<u8>, String> {
    if source.starts_with("https://") || source.starts_with("http://") {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(FETCH_TIMEOUT))
            .user_agent("pixel-cli index-unpack")
            .build();
        let agent = ureq::Agent::new_with_config(agent);
        let mut response = agent
            .get(source)
            .call()
            .map_err(|e| format!("index unpack: fetch {source}: {e}"))?;
        return response
            .body_mut()
            .with_config()
            .limit(FETCH_CAP)
            .read_to_vec()
            .map_err(|e| format!("index unpack: read {source}: {e}"));
    }
    std::fs::read(source).map_err(|e| format!("index unpack: {source}: {e}"))
}

/// Verified extraction: every file lands under `files/` in the tar, is
/// hash-checked against the manifest, staged in `.unpack-tmp`, then
/// renamed into place.
fn unpack(root: &Path, source: &str, force: bool) -> Result<Value, String> {
    if crate::daemon_ping(root) && !force {
        return Err(
            "index unpack: a daemon is running for this repo — stop it or pass --force".to_string(),
        );
    }
    let bytes = fetch_source(source)?;
    let mut archive = tar::Archive::new(bytes.as_slice());
    let mut manifest: Option<Manifest> = None;
    let mut blobs: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in archive
        .entries()
        .map_err(|e| format!("index unpack: tar: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("index unpack: tar: {e}"))?;
        let name = entry
            .path()
            .map_err(|e| format!("index unpack: tar: {e}"))?
            .to_string_lossy()
            .to_string();
        let mut body = Vec::new();
        entry
            .read_to_end(&mut body)
            .map_err(|e| format!("index unpack: {name}: {e}"))?;
        if name == MANIFEST {
            manifest = Some(
                serde_json::from_slice(&body)
                    .map_err(|e| format!("index unpack: manifest: {e}"))?,
            );
        } else if let Some(file) = name.strip_prefix("files/") {
            if file.contains('/') || file.is_empty() {
                return Err(format!("index unpack: unsafe member {name}"));
            }
            blobs.push((file.to_string(), body));
        }
    }
    let manifest = manifest.ok_or_else(|| "index unpack: no manifest.json".to_string())?;
    if manifest.format != PACK_FORMAT {
        return Err(format!(
            "index unpack: pack format {} — this pixel understands {PACK_FORMAT}",
            manifest.format
        ));
    }
    // Every manifest entry must be present and hash-true before anything
    // touches .pixel.
    let mut staged = Vec::new();
    for want in &manifest.files {
        let Some((_, body)) = blobs.iter().find(|(n, _)| n == &want.path) else {
            return Err(format!("index unpack: missing member {}", want.path));
        };
        if body.len() as u64 != want.size || xxh3_of(body) != want.xxh3 {
            return Err(format!("index unpack: corrupt member {}", want.path));
        }
        staged.push((want.path.clone(), body.clone()));
    }

    let shard_dir = root.join(SHARD_DIR);
    let tmp_dir = shard_dir.join(format!(".unpack-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("index unpack: {e}"))?;
    for (name, body) in &staged {
        std::fs::write(tmp_dir.join(name), body)
            .map_err(|e| format!("index unpack: stage {name}: {e}"))?;
    }
    let mut landed = Vec::new();
    for (name, _) in &staged {
        let from = tmp_dir.join(name);
        let to = shard_dir.join(name);
        std::fs::rename(&from, &to).map_err(|e| format!("index unpack: install {name}: {e}"))?;
        landed.push(name.clone());
    }
    let _ = std::fs::remove_dir_all(&tmp_dir);

    let head_now = pixel_git::GitRunner::new(root).rev_parse_head();
    let head_matches = match (&manifest.repo_head, &head_now) {
        (Some(packed), Some(now)) => Some(packed == now),
        _ => None,
    };
    Ok(json!({
        "installed": landed,
        "repo_head_packed": manifest.repo_head,
        "repo_head_now": head_now,
        "head_matches": head_matches,
        "packed_by_pixel": manifest.pixel_version,
    }))
}

pub fn run(cmd: IndexCmd) -> Result<(), String> {
    let out = match cmd {
        IndexCmd::Pack {
            out,
            include_history,
            path,
        } => pack(&path, &out, include_history)?,
        IndexCmd::Unpack {
            source,
            force,
            path,
        } => unpack(&path, &source, force)?,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("px-idx-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(SHARD_DIR)).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn pack_then_unpack_round_trips_with_hashes_verified() {
        let src = scratch("src");
        let dst = scratch("dst");
        std::fs::write(src.join(SHARD_DIR).join("graph.db"), b"graph-bytes").unwrap();
        std::fs::write(src.join(SHARD_DIR).join("state.json"), b"{}").unwrap();
        let pack_file = src.join("index.pxpack");
        pack(&src, &pack_file, false).unwrap();

        let report = unpack(&dst, pack_file.to_str().unwrap(), true).unwrap();
        assert_eq!(report["installed"].as_array().map(Vec::len), Some(2));
        assert_eq!(
            std::fs::read(dst.join(SHARD_DIR).join("graph.db")).unwrap(),
            b"graph-bytes"
        );
        // A byte-flipped pack must be refused by the manifest hash.
        let mut bytes = std::fs::read(&pack_file).unwrap();
        // graph.db lands under "files/graph.db" — find its content block
        // and corrupt it.
        let pos = bytes
            .windows(b"graph-bytes".len())
            .position(|w| w == b"graph-bytes")
            .expect("packed content");
        bytes[pos] ^= 0xff;
        let bad = src.join("bad.pxpack");
        std::fs::write(&bad, bytes).unwrap();
        // tar checksum may fail first, or the manifest hash — either way it
        // must not install.
        let dst2 = scratch("dst2");
        let refused = unpack(&dst2, bad.to_str().unwrap(), true);
        assert!(refused.is_err());
        assert!(!dst2.join(SHARD_DIR).join("graph.db").exists());
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
        let _ = std::fs::remove_dir_all(&dst2);
    }

    #[test]
    fn pack_errors_on_an_empty_index() {
        let root = scratch("empty");
        let err = pack(&root, &root.join("x.pxpack"), false).unwrap_err();
        assert!(err.contains("nothing to pack"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
