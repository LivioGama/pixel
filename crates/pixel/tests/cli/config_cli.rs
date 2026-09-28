//! User configuration contracts through the real CLI, with isolated home directories.
use crate::support::{Scratch, pixel_command};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Output, Stdio};

fn run(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    pixel_command()
        .env("HOME", home)
        .env_remove("PIXEL_METRICS")
        .env_remove("PIXEL_DAEMON_AUTO_START")
        .env_remove("PIXEL_TASK_CONTEXT")
        .env_remove("PIXEL_TASK_BOUNDARY")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn overview_should_show_effective_layers_without_creating_files_or_printing_secrets() {
    let home = Scratch::for_test("config", "overview-home");
    let repo = Scratch::for_test("config", "overview-repo");
    let empty = stdout(&run(&home, &repo, &["config"]));
    assert!(empty.contains("config.yaml"));
    assert!(empty.contains("metrics: on"));
    assert!(empty.contains("daemon_auto_start: true (default)"));
    assert!(!home.join(".pixel/config.yaml").exists());
    fs::create_dir_all(home.join(".pixel")).unwrap();
    fs::create_dir_all(repo.join(".pixel")).unwrap();
    fs::write(home.join(".pixel/config.yaml"), "metrics: 'off'\ndaemon_auto_start: false\ntask_context: false\nclassify: {engine: remote, remote_preset: deepseek}\nremote_keys: {deepseek: sk-secret}\nunknown: hidden-secret\n").unwrap();
    fs::write(
        repo.join(".pixel/config.yaml"),
        "metrics: 'on'\ntask_context: true\n",
    )
    .unwrap();
    let out = run(&home, &repo, &["config"]);
    let text = stdout(&out);
    assert!(text.contains("metrics: on (Repo)"), "{text}");
    assert!(text.contains("daemon_auto_start: false"), "{text}");
    assert!(text.contains("task_context: true"), "{text}");
    assert!(text.contains("classify.engine: remote"));
    assert!(text.contains("classify.remote_preset: deepseek"));
    assert!(text.contains("remote_keys.deepseek: set"));
    assert!(!text.contains("secret"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("secret"));
}

#[cfg(unix)]
#[test]
fn editor_should_receive_one_path_preserve_legacy_values_and_report_bad_edits() {
    let home = Scratch::for_test("config", "editor home's");
    let repo = Scratch::for_test("config", "editor repo's");
    fs::create_dir_all(home.join(".pixel")).unwrap();
    fs::create_dir_all(repo.join(".pixel")).unwrap();
    fs::write(
        home.join(".pixel/config.json"),
        "{\"metrics\":\"off\",\"future\":42}",
    )
    .unwrap();
    let script = home.join("editor script.sh");
    fs::write(&script, "test \"$1\" = '--wait' || exit 19\ntest \"$#\" = 2 || exit 20\nprintf '\\n# edited\\n' >> \"$2\"\n").unwrap();
    let editor = format!("sh {} --wait", shell_words::quote(script.to_str().unwrap()));
    let invoke = |args: &[&str]| {
        pixel_command()
            .env("HOME", &*home)
            .env("VISUAL", &editor)
            .env("EDITOR", "does-not-exist")
            .current_dir(&*repo)
            .args(args)
            .output()
            .unwrap()
    };
    stdout(&invoke(&["config", "edit"]));
    let path = home.join(".pixel/config.yaml");
    let contents = fs::read_to_string(&path).unwrap();
    assert!(contents.contains("# edited"));
    let value: serde_json::Value = serde_saphyr::from_str(&contents).unwrap();
    assert_eq!(value, serde_json::json!({"metrics":"off", "future":42}));
    stdout(&invoke(&["config", "edit", "--repo"]));
    assert!(
        fs::read_to_string(repo.join(".pixel/config.yaml"))
            .unwrap()
            .contains("# edited")
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        contents,
        "repo edit leaves global untouched"
    );
    fs::write(&script, "printf 'metrics: [sk-secret' > \"$2\"\n").unwrap();
    let out = invoke(&["config", "edit"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid configuration"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("sk-secret"));
    fs::write(&script, "exit 42\n").unwrap();
    let out = invoke(&["config", "edit"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("editor exited"));
}

#[test]
fn disabled_prompt_features_should_leave_no_handoff_or_context() {
    let home = Scratch::for_test("config", "hook-home");
    let repo = Scratch::for_test("config", "hook-repo");
    fs::create_dir_all(repo.join(".pixel")).unwrap();
    fs::write(
        repo.join(".pixel/config.yaml"),
        "task_context: false\ntask_boundary: false\n",
    )
    .unwrap();
    let mut child = pixel_command()
        .env("HOME", &*home)
        .env_remove("PIXEL_TASK_CONTEXT")
        .env_remove("PIXEL_TASK_BOUNDARY")
        .current_dir(&*repo)
        .args(["run-hook", "prompt-submit", "--provider", "claude"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let payload = serde_json::json!({"cwd": repo.to_str().unwrap(), "prompt": "Implement a configuration editor with YAML support", "session_id": "config-disabled"});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(stdout(&out), "");
    assert_eq!(out.stderr, b"");
    assert!(!repo.join(".pixel/tasks").exists());
}

#[cfg(unix)]
#[test]
fn editor_should_fall_back_from_blank_visual_to_editor_then_vi() {
    use std::os::unix::fs::PermissionsExt;
    let home = Scratch::for_test("config", "fallback-home");
    let script = home.join("vi");
    fs::write(
        &script,
        "#!/bin/sh\nprintf '\\n# fallback editor\\n' >> \"$1\"\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let editor = shell_words::quote(script.to_str().unwrap()).into_owned();
    for configured in [&editor, "  "] {
        let out = pixel_command()
            .env("HOME", &*home)
            .env("VISUAL", "  ")
            .env("EDITOR", configured)
            .env("PATH", &*home)
            .current_dir(&*home)
            .args(["config", "edit"])
            .output()
            .unwrap();
        stdout(&out);
    }
    let contents = fs::read_to_string(home.join(".pixel/config.yaml")).unwrap();
    assert_eq!(contents.matches("# fallback editor").count(), 2);
}
