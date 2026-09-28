use pixel::config_cmd::{set_ollaya_launch, ollaya_launch};
use serde_json::json;
use std::path::PathBuf;

fn home_env() -> Option<std::ffi::OsString> {
    std::env::var_os("HOME")
}

fn point_home(dir: &PathBuf) {
    unsafe { std::env::set_var("HOME", dir) };
}

fn restore_home(saved: Option<std::ffi::OsString>) {
    unsafe {
        match saved {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}

fn write(path: &PathBuf, doc: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, doc).unwrap();
}

#[test]
fn set_ollaya_launch_malformed_classify() {
    // Setup temporary HOME
    let home = std::env::temp_dir().join(format!("pixel-config-test-{}", std::process::id()));
    let saved = home_env();
    point_home(&home);

    // Write a malformed classify entry (not an object)
    let cfg_path = home.join(".pixel").join("config.json");
    write(&cfg_path, "{\"classify\": \"bad\"}");

    let launch = json!({
        "base": "http://127.0.0.1:11435",
        "model": "winnow:e4b",
        "argv": ["ollaya", "serve"],
    });
    set_ollaya_launch(&launch).expect("set_ollaya_launch failed");

    // Ensure launch is stored correctly
    let stored = ollama_launch().expect("ollama_launch missing");
    assert_eq!(stored, launch);

    // Verify the config file now has classify as object with ollaya field
    let text = std::fs::read_to_string(&cfg_path).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(doc.get("classify").and_then(|c| c.get("ollaya")).is_some());

    restore_home(saved);
}
