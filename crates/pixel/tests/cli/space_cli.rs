//! `pixel space` contract: audit disk taken by `.pixel/` shards across a
//! tree (table and `--json`), and `--delete --yes` remove them.

use std::path::PathBuf;
use std::process::Command;

fn pixel(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_pixel"))
        .args(args)
        .env("PIXEL_DAEMON_AUTO_START", "0")
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("px-space-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `.pixel` shard of `bytes` bytes.
fn shard_of(root: &std::path::Path, bytes: usize) {
    let shard = root.join(".pixel");
    std::fs::create_dir_all(&shard).unwrap();
    std::fs::write(shard.join("base.shard"), vec![0u8; bytes]).unwrap();
}

#[test]
fn table_lists_each_project_and_the_accumulated_total() {
    let base = scratch("table");
    shard_of(&base.join("one"), 100);
    shard_of(&base.join("two"), 200);
    let (_ok, stdout, _err) = pixel(&["space", base.to_str().unwrap()]);
    assert!(stdout.contains("one"), "{stdout}");
    assert!(stdout.contains("two"), "{stdout}");
    assert!(
        stdout.contains("total across 2 projects (300 bytes)"),
        "{stdout}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn json_reports_per_project_bytes_and_the_total() {
    let base = scratch("json");
    shard_of(&base.join("one"), 1_024); // exactly 1 KiB
    let (ok, stdout, _err) = pixel(&["space", "--json", base.to_str().unwrap()]);
    assert!(ok, "{stdout}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(value["total_bytes"], 1_024);
    assert_eq!(value["entries"][0]["bytes"], 1_024);
    assert_eq!(value["entries"][0]["human"], "1.0 KiB");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn delete_with_yes_removes_the_shards_but_keeps_the_projects() {
    let base = scratch("delete");
    let proj = base.join("one");
    shard_of(&proj, 100);
    assert!(proj.join(".pixel").exists());
    let (ok, stdout, _err) = pixel(&["space", "--delete", "--yes", base.to_str().unwrap()]);
    assert!(ok, "{stdout}");
    assert!(!proj.join(".pixel").exists(), "shard removed");
    assert!(proj.exists(), "project untouched");
    assert!(stdout.contains("removed "), "{stdout}");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn empty_tree_reports_no_shards() {
    let base = scratch("empty");
    let (ok, stdout, _err) = pixel(&["space", base.to_str().unwrap()]);
    assert!(ok, "{stdout}");
    assert!(stdout.contains("no `.pixel` index shards"), "{stdout}");
    let _ = std::fs::remove_dir_all(&base);
}
