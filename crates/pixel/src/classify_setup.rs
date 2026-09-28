//! The classify-engine install step and configuration.
//!
//! `pixel install` proposes the classify engine — local (the Ollaya decision
//! daemon on this Mac) or remote (a hosted LLM behind a key) — with each
//! option's measured accuracy in parentheses. Choosing local runs the
//! auto-setup: the `ollaya` binary installed into a pixel-managed prefix,
//! the recommended model pulled, and a recorded server launch that
//! `pixel classify` auto-starts on demand. Choosing remote prompts for the
//! provider's API key and stores it through the existing
//! `pixel config remote-key` flow.
//!
//! Everything interactive is gated on a TTY: a scripted install (CI, pipes)
//! prints the choice it would have asked as a suggestion and moves on —
//! `pixel config classify-engine <local|remote|auto>` records the answer
//! later. The stored preference is advisory: an explicit `--engine` flag on
//! `pixel classify` always wins.

use serde_json::{Value, json};
use std::io::BufRead;
use std::path::PathBuf;

/// Install-time proposal text, with each option's measured accuracy in
/// parentheses (Ollaya's published typed-decisions benchmark for
/// `winnow:e4b`; this repository's own 14-item public coding exam for the
/// remote preset).
pub const LOCAL_LABEL: &str = "Local — Ollaya winnow:e4b on this Mac (offline, $0 per call; 0.722 typed-decisions accuracy vs Jev's 0.738)";
pub const REMOTE_LABEL: &str = "Remote — hosted LLM behind your key (DeepSeek-v4.1-flash: 100% on the 14-item public coding exam; ~$0.0001/call, needs network)";

/// Everything Ollaya owns lives under one pixel-managed prefix (binary,
/// model store, installer, server log), never in the global PATH.
const OLLAYA_ROOT: &str = ".local/share/pixel/ollaya";

/// The stored engine preference, if any: `local`, `remote`, or `auto`.
pub fn stored_engine() -> Option<String> {
    crate::config_cmd::classify_engine()
}

/// The recorded local-daemon launch, if a local install completed.
pub fn ollaya_launch() -> Option<Value> {
    crate::config_cmd::ollaya_launch()
}

/// The engine `pixel classify` resolves to: the explicit flag wins; then
/// the stored preference; `auto` (or absent) falls back to local when the
/// server answers, remote otherwise.
pub fn resolve_engine(
    flag: Option<crate::classify::EngineChoice>,
    ollaya_url: String,
    stored: Option<String>,
    reachable: bool,
) -> ResolvedEngine {
    match flag {
        Some(crate::classify::EngineChoice::Ollaya) => {
            return ResolvedEngine::Local { base: ollaya_url };
        }
        Some(crate::classify::EngineChoice::Remote) => return ResolvedEngine::Remote,
        None => {}
    }
    match stored.as_deref() {
        Some("local") => ResolvedEngine::Local { base: local_base() },
        Some("remote") => ResolvedEngine::Remote,
        _ if reachable => ResolvedEngine::Local { base: local_base() },
        _ => ResolvedEngine::Remote,
    }
}

/// The engine a resolved classify run talks to.
pub enum ResolvedEngine {
    Remote,
    Local { base: String },
}

/// Make sure a resolved-local engine has a live daemon: auto-start the
/// recorded launch once and poll (the daemon comes up in seconds). When no
/// launch is recorded there is nothing to start, so this returns `Ok`
/// without polling — the caller's request then fails fast against the
/// closed port rather than stalling for two minutes.
pub fn ensure_local(base: &str) -> Result<(), String> {
    if server_reachable(base) {
        return Ok(());
    }
    let started = auto_start()?;
    if !started {
        return Ok(());
    }
    for _ in 0..240 {
        if server_reachable(base) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "the ollaya daemon at {base} did not come up within two minutes; check ~/.local/share/pixel/ollaya/server.log"
    ))
}

/// Whether the local Ollaya daemon answers on its base URL (TCP level —
/// enough to distinguish "daemon up" from "not started").
pub fn server_reachable(base: &str) -> bool {
    use std::net::TcpStream;
    let authority = base
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or_default();
    let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h);
    let port: u16 = authority
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or(80);
    std::net::ToSocketAddrs::to_socket_addrs(&(host, port))
        .ok()
        .and_then(|mut addrs| addrs.next())
        .is_some_and(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok())
}

/// Parse the interactive answer ("1"/"2") into an engine choice.
fn parse_choice(answer: &str) -> Option<&'static str> {
    match answer.trim() {
        "1" => Some("local"),
        "2" => Some("remote"),
        _ => None,
    }
}

/// The install-time step: propose, then dispatch to the chosen setup. When
/// stdin is not a TTY the proposal is printed as a suggestion and nothing
/// interactive happens.
pub fn install_step(
    tty: bool,
    stdin: &mut dyn BufRead,
    stdout: &mut dyn std::io::Write,
) -> Result<(), String> {
    if let Some(engine) = stored_engine() {
        writeln!(stdout, "classify engine: already configured as {engine:?} (change with `pixel config classify-engine <local|remote|auto>`)")
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    writeln!(stdout, "Classify engine:").map_err(|e| e.to_string())?;
    writeln!(stdout, "  [1] {LOCAL_LABEL}").map_err(|e| e.to_string())?;
    writeln!(stdout, "  [2] {REMOTE_LABEL}").map_err(|e| e.to_string())?;
    if !tty {
        writeln!(stdout, "classify engine: not configured (non-interactive install) — run `pixel config classify-engine <local|remote>` or re-run `pixel install` in a terminal")
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    write!(stdout, "Choice> ").map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    stdin
        .read_line(&mut line)
        .map_err(|e| format!("read choice: {e}"))?;
    match parse_choice(&line) {
        Some("local") => setup_local(stdout),
        Some("remote") => propose_remote_key(stdout),
        _ => {
            writeln!(stdout, "classify engine: skipped — run `pixel config classify-engine <local|remote>` to choose later")
                .map_err(|e| e.to_string())?;
            Ok(())
        }
    }
}

fn propose_remote_key(stdout: &mut dyn std::io::Write) -> Result<(), String> {
    writeln!(
        stdout,
        "Remote providers: openrouter / ollama / deepseek / opencode-go"
    )
    .map_err(|e| e.to_string())?;
    write!(stdout, "Provider [openrouter]> ").map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("read provider: {e}"))?;
    let provider = if line.trim().is_empty() {
        "openrouter"
    } else {
        line.trim()
    };
    let Some(preset) = crate::decide_remote::Preset::parse_name(provider) else {
        return Err(format!(
            "unknown provider {provider:?} (openrouter, ollama, deepseek, opencode-go, local)"
        ));
    };
    let Some(var) = crate::decide_remote::key_env_name(preset, None) else {
        return Err(
            "the local preset needs no API key — choose it as the engine instead".to_string(),
        );
    };
    write!(
        stdout,
        "API key (stored in ~/.pixel/config.json, never printed)> "
    )
    .map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())?;
    let mut key = String::new();
    std::io::stdin()
        .read_line(&mut key)
        .map_err(|e| format!("read key: {e}"))?;
    let key = key.trim();
    if key.is_empty() {
        writeln!(stdout, "classify engine: remote selected, no key stored — set {var} or run `pixel config remote-key {} -` before classifying", preset.display())
            .map_err(|e| e.to_string())?;
    } else {
        crate::config_cmd::run_remote_key(preset, Some(key.to_string()), false)?;
        writeln!(
            stdout,
            "classify engine: remote ({}) — key stored",
            preset.display()
        )
        .map_err(|e| e.to_string())?;
    }
    crate::config_cmd::set_classify_engine("remote")
}

/// The local auto-setup: download the installer, install the `ollaya`
/// binary into the pixel-managed prefix, pull the recommended model, and
/// record a launch that `pixel classify` auto-starts. Long-running and
/// network-bound; every step is echoed as it starts.
pub fn setup_local(stdout: &mut dyn std::io::Write) -> Result<(), String> {
    let root = local_root()?;
    let root_str = root.to_string_lossy().into_owned();
    let bin = root.join("bin").join("ollaya");
    let models = root.join("models");
    let models_str = models.to_string_lossy().into_owned();

    writeln!(
        stdout,
        "ollaya setup [1/2] install the ollaya binary (https://ollaya.dev/install.sh) → {}",
        bin.display()
    )
    .map_err(|e| e.to_string())?;
    if !bin.exists() {
        let installer = root.join("install.sh");
        let installer_str = installer.to_string_lossy().into_owned();
        run(
            "curl",
            &[
                "-fsSL",
                "https://ollaya.dev/install.sh",
                "-o",
                &installer_str,
            ],
        )?;
        run_env(
            "sh",
            &[installer_str.as_str()],
            &[
                ("OLLAYA_INSTALL_DIR", root_str.as_str()),
                ("OLLAYA_NO_SERVICE", "1"),
            ],
        )?;
        if !bin.exists() {
            return Err(format!(
                "the ollaya installer finished but {} is missing",
                bin.display()
            ));
        }
    }

    writeln!(
        stdout,
        "ollaya setup [2/2] pull {} (model weights; runs on Apple-silicon Metal)",
        crate::decide_ollaya::DEFAULT_MODEL
    )
    .map_err(|e| e.to_string())?;
    let bin_str = bin.to_string_lossy().into_owned();
    run_env(
        &bin_str,
        &["pull", crate::decide_ollaya::DEFAULT_MODEL],
        &[("OLLAYA_MODELS", models_str.as_str())],
    )?;

    let launch = json!({
        "base": crate::decide_ollaya::DEFAULT_BASE,
        "model_name": crate::decide_ollaya::DEFAULT_MODEL,
        "argv": [bin_str, "serve".to_string()],
        "env": {
            "OLLAYA_MODELS": models_str,
            "OLLAYA_HOST": "127.0.0.1:11435",
        },
    });
    crate::config_cmd::set_ollaya_launch(&launch)?;
    crate::config_cmd::set_classify_engine("local")?;
    writeln!(
        stdout,
        "classify engine: local — `pixel classify` will auto-start the daemon on demand"
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Spawn the recorded local daemon if it is not already answering. Returns
/// whether a spawn happened (the caller polls for reachability itself).
pub fn auto_start() -> Result<bool, String> {
    let base = local_base();
    if server_reachable(&base) {
        return Ok(false);
    }
    let Some(launch) = ollaya_launch() else {
        return Ok(false);
    };
    let argv: Vec<String> = launch
        .get("argv")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if argv.is_empty() {
        return Ok(false);
    }
    let env: Vec<(String, String)> = launch
        .get("env")
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(local_root()?.join("server.log"))
        .map_err(|e| format!("open server log: {e}"))?;
    let mut command = std::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log);
    command
        .spawn()
        .map_err(|e| format!("start ollaya daemon: {e}"))?;
    Ok(true)
}

/// The local daemon base: the recorded one, else the documented default.
pub fn local_base() -> String {
    ollaya_launch()
        .and_then(|l| l.get("base").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| crate::decide_ollaya::DEFAULT_BASE.to_string())
}

fn local_root() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("no HOME")?;
    let root = PathBuf::from(home).join(OLLAYA_ROOT);
    std::fs::create_dir_all(&root).map_err(|e| format!("create {}: {e}", root.display()))?;
    Ok(root)
}

fn run(program: &str, args: &[&str]) -> Result<(), String> {
    run_env(program, args, &[])
}

fn run_env(program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<(), String> {
    let mut command = std::process::Command::new(program);
    command.args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    let status = command
        .status()
        .map_err(|e| format!("run {program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed: {status}"))
    }
}

use std::time::Duration;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_choice_parser_accepts_exactly_one_and_two() {
        assert_eq!(parse_choice("1"), Some("local"));
        assert_eq!(parse_choice(" 2\n"), Some("remote"));
        assert_eq!(parse_choice(""), None);
        assert_eq!(parse_choice("yes"), None);
        assert_eq!(parse_choice("0"), None);
    }

    #[test]
    fn resolution_prefers_the_flag_then_the_stored_setting_then_reachability() {
        use crate::classify::EngineChoice;
        // The explicit flag wins over everything, no probing needed.
        assert!(matches!(
            resolve_engine(
                Some(EngineChoice::Ollaya),
                "http://127.0.0.1:9999".to_string(),
                Some("remote".to_string()),
                false
            ),
            ResolvedEngine::Local { .. }
        ));
        assert!(matches!(
            resolve_engine(Some(EngineChoice::Remote), String::new(), None, true),
            ResolvedEngine::Remote
        ));
        // Stored remote wins over a reachable local server.
        assert!(matches!(
            resolve_engine(None, String::new(), Some("remote".to_string()), true),
            ResolvedEngine::Remote
        ));
        // Stored local wins even when the server is not yet reachable (the
        // caller auto-starts it after resolution).
        assert!(matches!(
            resolve_engine(None, String::new(), Some("local".to_string()), false),
            ResolvedEngine::Local { .. }
        ));
        // auto/absent: reachability decides.
        assert!(matches!(
            resolve_engine(None, String::new(), Some("auto".to_string()), true),
            ResolvedEngine::Local { .. }
        ));
        assert!(matches!(
            resolve_engine(None, String::new(), None, false),
            ResolvedEngine::Remote
        ));
    }

    #[test]
    fn reachability_probe_rejects_malformed_and_closed_bases() {
        assert!(!server_reachable("http://127.0.0.1:1"));
        assert!(!server_reachable("not a url"));
    }
}
