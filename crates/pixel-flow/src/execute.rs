//! Flow executor — actually runs agent-browser commands from a flow.
//!
//! Unlike `replay` (which only emits text), this module shells out to
//! `agent-browser --session comet`, parses snapshot output to resolve
//! `@eN` refs from `ref_hint` descriptions, evaluates conditionals by
//! inspecting the page, and handles tab switching.

use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

use crate::types::{Flow, FlowStep};
use crate::vars::{resolve_value, substitute};

/// Result of executing a flow.
#[derive(Debug)]
pub struct ExecResult {
    pub steps_executed: usize,
    pub steps_skipped: usize,
    pub log: String,
    pub success: bool,
    pub error: Option<String>,
}

/// Execute a flow by running agent-browser commands.
///
/// `vars` is a map of `key=value` substitutions. Missing required vars
/// produce an error. A step `value_var` the caller did not pass falls back
/// to the variable's declared default, then to a placeholder `{{var}}`.
pub fn execute(flow: &Flow, vars: &HashMap<String, String>) -> ExecResult {
    execute_with(flow, vars, &mut AgentBrowser)
}

/// The process behind every step. `execute` drives the `agent-browser` on
/// PATH; tests script the answers and skip the page-load waits, so every
/// action arm is checked without a browser.
pub(crate) trait Browser {
    /// Run `agent-browser --session comet <args>` and return its stdout.
    fn run(&mut self, args: &[&str]) -> Result<String, String>;
    /// Give the page time to load or navigate.
    fn pause(&mut self, duration: Duration);
}

struct AgentBrowser;

impl Browser for AgentBrowser {
    // Spawns the real agent-browser on PATH: the trait is what tests script.
    #[cfg_attr(test, mutants::skip)]
    fn run(&mut self, args: &[&str]) -> Result<String, String> {
        run_agent_browser(args)
    }

    fn pause(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

pub(crate) fn execute_with(
    flow: &Flow,
    vars: &HashMap<String, String>,
    browser: &mut dyn Browser,
) -> ExecResult {
    let mut log = String::new();
    let mut steps_executed = 0usize;
    let mut steps_skipped = 0usize;

    // Validate required vars.
    for v in &flow.vars {
        if v.required && !vars.contains_key(&v.name) && v.default.is_none() {
            return ExecResult {
                steps_executed: 0,
                steps_skipped: 0,
                log: String::new(),
                success: false,
                error: Some(format!(
                    "missing required variable '{}' for flow '{}'",
                    v.name, flow.name
                )),
            };
        }
    }

    log.push_str(&format!(
        "# Executing flow: {} ({})\n# Steps: {} | Revision: {}\n\n",
        flow.name,
        flow.title,
        flow.steps.len(),
        flow.revision
    ));

    // Preconditions — print as warnings, don't block.
    if !flow.preconditions.is_empty() {
        log.push_str("# Preconditions (verify before starting):\n");
        for pre in &flow.preconditions {
            log.push_str(&format!("#   - {}\n", substitute(pre, vars)));
        }
        log.push('\n');
    }

    // Stale tab cleanup.
    if !flow.stale_tab_cleanup.is_empty() {
        log.push_str("# Stale tab cleanup:\n");
        for pattern in &flow.stale_tab_cleanup {
            let pat = substitute(pattern, vars);
            log.push_str(&format!("#   closing tabs matching '{pat}'\n"));
            if let Err(e) = close_stale_tabs(&pat, &mut log, browser) {
                log.push_str(&format!("#   WARN: stale cleanup failed: {e}\n"));
            }
        }
        log.push('\n');
    }

    // Focus the flow's default tab.
    if let Some(ref tab) = flow.tab {
        let tab = substitute(tab, vars);
        log.push_str(&format!("# Focusing flow tab: {tab}\n"));
        if let Err(e) = switch_to_tab(&tab, &mut log, browser) {
            log.push_str(&format!("#   WARN: tab focus failed: {e}\n"));
        }
        log.push('\n');
    }

    // Execute steps.
    for (i, step) in flow.steps.iter().enumerate() {
        match exec_step(step, i + 1, vars, flow, &mut log, 0, browser) {
            Ok(executed) => {
                if executed {
                    steps_executed += 1;
                } else {
                    steps_skipped += 1;
                }
            }
            Err(e) => {
                log.push_str(&format!("# ERROR at step {}: {}\n", i + 1, e));
                return ExecResult {
                    steps_executed,
                    steps_skipped,
                    log,
                    success: false,
                    error: Some(format!("step {} failed: {}", i + 1, e)),
                };
            }
        }
    }

    // Success URL check — use `get url` to check the actual page URL,
    // and also check the snapshot for success signals.
    let mut url_ok = true;
    let final_snapshot = browser.run(&["snapshot", "-i"]).unwrap_or_default();
    let final_url = browser.run(&["get", "url"]).unwrap_or_default();
    let combined = format!("{final_url}\n{final_snapshot}");

    if !flow.success_url_contains.is_empty() {
        for u in &flow.success_url_contains {
            let u = substitute(u, vars);
            if !combined.contains(&u) {
                log.push_str(&format!(
                    "# WARN: success URL check — '{u}' not found in URL or snapshot\n"
                ));
                url_ok = false;
            }
        }
    }

    // Success signal check — if the success_signal text is found in the
    // snapshot, override url_ok to true (the signal is authoritative).
    if let Some(ref signal) = flow.success_signal {
        let signal = substitute(signal, vars);
        // Check if any quoted term from the signal appears in the snapshot.
        let terms = extract_quoted_strings(&signal);
        let signal_met = if terms.is_empty() {
            // No quoted terms — check the whole signal as a keyword.
            final_snapshot
                .to_lowercase()
                .contains(&signal.to_lowercase())
        } else {
            terms
                .iter()
                .any(|t| final_snapshot.to_lowercase().contains(&t.to_lowercase()))
        };
        if signal_met {
            log.push_str(&format!("# ✓ Success signal detected: {signal}\n"));
            url_ok = true;
        }
    }

    // MFA detection.
    if !flow.mfa_keywords.is_empty()
        && let Ok(snapshot) = browser.run(&["snapshot", "-i"])
    {
        for kw in &flow.mfa_keywords {
            if snapshot.contains(kw) {
                log.push_str(&format!(
                    "# MFA DETECTED: keyword '{kw}' found in snapshot.\n"
                ));
                log.push_str("# → Hand off to user — MFA cannot be automated.\n");
                return ExecResult {
                    steps_executed,
                    steps_skipped,
                    log,
                    success: false,
                    error: Some(format!(
                        "MFA gate detected (keyword: '{kw}') — user intervention required"
                    )),
                };
            }
        }
    }

    log.push_str(&format!(
        "\n# Flow complete: {steps_executed} steps executed, {steps_skipped} skipped\n"
    ));

    if let Some(ref signal) = flow.success_signal {
        log.push_str(&format!("# Success signal: {}\n", substitute(signal, vars)));
    }

    ExecResult {
        steps_executed,
        steps_skipped,
        log,
        success: url_ok,
        error: if url_ok {
            None
        } else {
            Some("success URL check failed".into())
        },
    }
}

/// Execute a single step. Returns Ok(true) if executed, Ok(false) if skipped
/// (e.g. conditional branch not taken).
fn exec_step(
    step: &FlowStep,
    num: usize,
    vars: &HashMap<String, String>,
    flow: &Flow,
    log: &mut String,
    depth: usize,
    browser: &mut dyn Browser,
) -> Result<bool, String> {
    let indent = "  ".repeat(depth);
    if let Some(r) = &step.rationale {
        log.push_str(&format!(
            "{}# Step {}: {}\n",
            indent,
            num,
            substitute(r, vars)
        ));
    }

    // Per-step tab switching.
    let effective_tab = step.tab.as_deref().or(flow.tab.as_deref());
    if step.action != "switch_tab"
        && step.tab.is_some()
        && let Some(tab) = effective_tab
    {
        let tab = substitute(tab, vars);
        log.push_str(&format!("{indent}# Switching to tab: {tab}\n"));
        switch_to_tab(&tab, log, browser)?;
    }

    match step.action.as_str() {
        "open" => {
            if let Some(ref url) = step.url {
                let url = substitute(url, vars);
                log.push_str(&format!("{indent}agent-browser open \"{url}\"\n"));
                // Try open first; if the bound tab is gone, use `tab new`.
                match browser.run(&["open", &url]) {
                    Ok(_) => {}
                    Err(e) if e.contains("tab_gone") || e.contains("no tab") => {
                        log.push_str(&format!("{indent}# bound tab gone — opening new tab\n"));
                        browser
                            .run(&["tab", "new", &url])
                            .map_err(|e2| format!("open/tab new failed: {e2}"))?;
                    }
                    Err(e) => return Err(format!("open failed: {e}")),
                }
                // Give the page time to load (websites open in <5s).
                browser.pause(Duration::from_secs(3));
            }
            Ok(true)
        }
        "snapshot" => {
            log.push_str(&format!("{indent}agent-browser snapshot -i\n"));
            browser
                .run(&["snapshot", "-i"])
                .map_err(|e| format!("snapshot failed: {e}"))?;
            Ok(true)
        }
        "click" => {
            let target = substitute(step.ref_hint.as_deref().unwrap_or("element"), vars);
            log.push_str(&format!("{indent}# Finding ref for: {target}\n"));
            let snapshot = browser
                .run(&["snapshot", "-i"])
                .map_err(|e| format!("snapshot before click failed: {e}"))?;
            let ref_id = find_ref_in_snapshot(&snapshot, &target)
                .ok_or_else(|| format!("no element matching '{target}' found in snapshot"))?;
            log.push_str(&format!("{indent}agent-browser click @{ref_id}\n"));
            browser
                .run(&["click", &format!("@{ref_id}")])
                .map_err(|e| format!("click @{ref_id} failed: {e}"))?;
            // Wait for potential navigation.
            browser.pause(Duration::from_secs(2));

            // Check if the click actually navigated (snapshot changed).
            // If on_failure mentions JS click/eval, try that as a fallback.
            if let Some(ref fix) = step.on_failure {
                let fix = substitute(fix, vars);
                let fix_lower = fix.to_lowercase();
                if fix_lower.contains("eval")
                    || fix_lower.contains("js click")
                    || fix_lower.contains("queryselector")
                {
                    // Check if the page is still on the same URL (click didn't work).
                    let post_snapshot = browser.run(&["snapshot", "-i"]).unwrap_or_default();
                    // If the target is still visible, the click didn't navigate.
                    if post_snapshot.contains(&target) {
                        log.push_str(&format!(
                            "{indent}# click didn't navigate — trying JS click fallback\n"
                        ));
                        // Try eval with querySelector.
                        let js = "document.querySelector('button')?.click()".to_string();
                        browser
                            .run(&["eval", &js])
                            .map_err(|e| format!("JS click fallback failed: {e}"))?;
                        browser.pause(Duration::from_secs(2));
                    }
                }
            }
            Ok(true)
        }
        "fill" | "type" => {
            let target = substitute(step.ref_hint.as_deref().unwrap_or("input"), vars);
            let value = resolve_value(step, vars, &flow.vars);

            // Special case: if the ref_hint contains "Code character N of",
            // extract N and use the Nth character of user_code (without dash).
            let value = if target.contains("Code character") && value.is_empty() {
                if let Some(user_code) = vars.get("user_code") {
                    let code_clean = user_code.replace('-', "");
                    // Extract N from "Code character N of 9"
                    if let Some(n) = extract_char_number(&target) {
                        if n > 0 && n <= code_clean.len() {
                            code_clean
                                .chars()
                                .nth(n - 1)
                                .map(|c| c.to_string())
                                .unwrap_or_default()
                        } else {
                            value
                        }
                    } else {
                        value
                    }
                } else {
                    value
                }
            } else {
                value
            };

            log.push_str(&format!("{indent}# Finding ref for: {target}\n"));
            let snapshot = browser
                .run(&["snapshot", "-i"])
                .map_err(|e| format!("snapshot before fill failed: {e}"))?;
            let ref_id = find_ref_in_snapshot(&snapshot, &target)
                .ok_or_else(|| format!("no element matching '{target}' found in snapshot"))?;
            log.push_str(&format!(
                "{}agent-browser {} @{} \"{}\"\n",
                indent, step.action, ref_id, value
            ));
            browser
                .run(&[&step.action, &format!("@{ref_id}"), &value])
                .map_err(|e| format!("{} @{} failed: {e}", step.action, ref_id))?;
            Ok(true)
        }
        "select" => {
            let target = substitute(step.ref_hint.as_deref().unwrap_or("select"), vars);
            let value = resolve_value(step, vars, &flow.vars);
            let snapshot = browser
                .run(&["snapshot", "-i"])
                .map_err(|e| format!("snapshot before select failed: {e}"))?;
            let ref_id = find_ref_in_snapshot(&snapshot, &target)
                .ok_or_else(|| format!("no element matching '{target}' found in snapshot"))?;
            log.push_str(&format!(
                "{indent}agent-browser select @{ref_id} \"{value}\"\n"
            ));
            browser
                .run(&["select", &format!("@{ref_id}"), &value])
                .map_err(|e| format!("select @{ref_id} failed: {e}"))?;
            Ok(true)
        }
        "press" => {
            let key = substitute(step.key.as_deref().unwrap_or("Enter"), vars);
            log.push_str(&format!("{indent}agent-browser press {key}\n"));
            browser
                .run(&["press", &key])
                .map_err(|e| format!("press {key} failed: {e}"))?;
            Ok(true)
        }
        "wait" => {
            let wait = step.wait.as_deref().unwrap_or("load");
            let dur = parse_wait_duration(wait);
            log.push_str(&format!("{}# Waiting {}s\n", indent, dur.as_secs()));
            browser.pause(dur);
            Ok(true)
        }
        "conditional" => {
            let cond = substitute(step.condition.as_deref().unwrap_or("condition"), vars);
            log.push_str(&format!("{indent}# CONDITIONAL: if {cond}\n"));

            // Take a snapshot to evaluate the condition.
            let snapshot = browser
                .run(&["snapshot", "-i"])
                .map_err(|e| format!("snapshot for conditional failed: {e}"))?;

            // A URL condition needs the page URL; the snapshot does not
            // carry it.
            let url = if cond.to_lowercase().contains("url contains") {
                browser.run(&["get", "url"]).ok()
            } else {
                None
            };
            let condition_met = evaluate_condition(&cond, &snapshot, url.as_deref());
            log.push_str(&format!(
                "{}# Condition {} — taking {} branch\n",
                indent,
                if condition_met { "MET" } else { "NOT MET" },
                if condition_met { "THEN" } else { "ELSE" }
            ));

            let branch = if condition_met {
                &step.then
            } else {
                &step.otherwise
            };
            let mut any_executed = false;
            for (i, sub) in branch.iter().enumerate() {
                match exec_step(sub, i + 1, vars, flow, log, depth + 1, browser) {
                    Ok(true) => any_executed = true,
                    Ok(false) => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(any_executed)
        }
        "switch_tab" => {
            let tab = substitute(step.tab.as_deref().unwrap_or("tab"), vars);
            log.push_str(&format!("{indent}# Switch to tab: {tab}\n"));
            switch_to_tab(&tab, log, browser)?;
            Ok(true)
        }
        "eval" => {
            // Evaluate JS — for cases where regular click doesn't work.
            let js = substitute(step.value.as_deref().unwrap_or(""), vars);
            if !js.is_empty() {
                log.push_str(&format!("{indent}agent-browser eval \"{js}\"\n"));
                browser
                    .run(&["eval", &js])
                    .map_err(|e| format!("eval failed: {e}"))?;
            }
            Ok(true)
        }
        _ => {
            log.push_str(&format!(
                "{}# Unknown action: {} — skipping\n",
                indent, step.action
            ));
            Ok(false)
        }
    }
}

/// Run an agent-browser command and return its stdout.
fn run_agent_browser(args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("agent-browser");
    cmd.args(["--session", "comet"]);
    cmd.args(args);
    let output = cmd
        .output()
        .map_err(|e| format!("failed to spawn agent-browser: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "agent-browser {} exited with {}: stderr={stderr} stdout={stdout}",
            args.join(" "),
            output.status
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse a snapshot and find the accessibility ref (`@eN`) for an element
/// matching the `ref_hint` description.
///
/// Snapshot lines look like:
///   - button "Continue with Google" [ref=e5]
///   - link "Log in to another account" [ref=e5]
///   - textbox "Code character 1 of 9" [ref=e5]
///   - heading "Welcome back" [level=1, ref=e1]
///
/// The `ref_hint` is a natural language description like:
///   "button containing 'Continue with Google'"
///   "account matching user@example.com"
///   "input[type=email] or textbox matching 'Email'"
fn find_ref_in_snapshot(snapshot: &str, ref_hint: &str) -> Option<String> {
    // Extract quoted strings from the ref_hint — these are the search terms.
    // e.g. "button containing 'Continue with Google'" → ["Continue with Google"]
    let search_terms: Vec<&str> = extract_quoted_strings(ref_hint);

    // Detect element type preference from the hint prefix.
    // e.g. "button containing '...'" → prefer lines starting with "- button"
    let hint_lower = ref_hint.to_lowercase();
    let type_pref: Option<&str> =
        if hint_lower.contains("button containing") || hint_lower.contains("button matching") {
            Some("button")
        } else if hint_lower.contains("link containing") || hint_lower.contains("link matching") {
            Some("link")
        } else if hint_lower.contains("textbox matching") {
            Some("textbox")
        } else if hint_lower.contains("heading") {
            Some("heading")
        } else {
            None
        };

    // If no quoted strings, try to match the whole hint as a fuzzy term.
    let fallback_term = ref_hint
        .replace("button containing", "")
        .replace("link containing", "")
        .replace("account matching", "")
        .replace("textbox matching", "")
        .replace("input", "")
        .replace("or", "")
        .trim()
        .to_lowercase();
    let fallback_term = fallback_term
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_string();

    // First pass: if a type preference is set, only match lines of that type.
    // This avoids matching generic wrapper elements that contain the button's text.
    if let Some(pref) = type_pref {
        let prefix = format!("- {pref} ");
        for line in snapshot.lines() {
            if !line.trim_start().starts_with(&prefix) {
                continue;
            }
            if let Some(ref_id) = extract_ref_if_matches(line, &search_terms, &fallback_term) {
                return Some(ref_id);
            }
        }
    }

    // Second pass (or no type preference): match any line.
    for line in snapshot.lines() {
        if let Some(ref_id) = extract_ref_if_matches(line, &search_terms, &fallback_term) {
            return Some(ref_id);
        }
    }
    None
}

/// Check if a snapshot line has a ref and matches the search terms.
fn extract_ref_if_matches(
    line: &str,
    search_terms: &[&str],
    fallback_term: &str,
) -> Option<String> {
    let ref_start = line.find("ref=e")?;
    let after_e = &line[ref_start + 4..]; // skip "ref=", now at "eN...]"
    let digits_start = 1; // skip the "e"
    let rest = &after_e[digits_start..];
    let ref_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let ref_id = &rest[..ref_end];

    let line_lower = line.to_lowercase();
    let matched = if !search_terms.is_empty() {
        search_terms
            .iter()
            .all(|term| line_lower.contains(&term.to_lowercase()))
    } else if !fallback_term.is_empty() && fallback_term.len() > 2 {
        line_lower.contains(fallback_term)
    } else {
        true
    };

    if matched {
        Some(format!("e{ref_id}"))
    } else {
        None
    }
}

/// Extract the character number from a ref_hint like "Code character 3 of 9".
fn extract_char_number(hint: &str) -> Option<usize> {
    // Look for "character N of" pattern.
    let parts: Vec<&str> = hint.split_whitespace().collect();
    for i in 0..parts.len().saturating_sub(1) {
        if parts[i] == "character"
            && let Ok(n) = parts[i + 1].parse::<usize>()
        {
            return Some(n);
        }
    }
    None
}

/// Extract single-quoted strings from a hint string.
/// e.g. `button containing 'Continue'` → `["Continue"]`
fn extract_quoted_strings(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut in_quote = false;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if c == '\'' {
            if in_quote {
                result.push(&s[start..i]);
                in_quote = false;
            } else {
                start = i + 1;
                in_quote = true;
            }
        }
    }
    result
}

/// Evaluate a condition string against a snapshot.
///
/// Conditions look like:
///   "page shows 'Continue with Google' or 'Continue with Google'"
///   "page shows 'Welcome back' heading with account buttons"
///   "the desired account user@example.com is visible"
///   "page contains 'hCaptcha' or 'Drag'"
///   "page shows password input field"
///   "URL contains 'code=' parameter"
///
/// `url` is the page URL when the caller could read it (`get url`); a URL
/// condition without one is not met.
fn evaluate_condition(condition: &str, snapshot: &str, url: Option<&str>) -> bool {
    let cond_lower = condition.to_lowercase();
    let snap_lower = snapshot.to_lowercase();

    // Special case: URL check — the snapshot does not carry the URL.
    if cond_lower.contains("url contains")
        && let Some(term) = extract_quoted_strings(condition).first()
    {
        return url.is_some_and(|u| u.to_lowercase().contains(&term.to_lowercase()));
    }

    // Extract quoted terms and check if any/all appear in the snapshot.
    let terms = extract_quoted_strings(condition);

    if terms.is_empty() {
        // No quoted terms — try keyword matching.
        // Check for common condition keywords.
        let keywords: Vec<&str> = cond_lower
            .split_whitespace()
            .filter(|w| {
                w.len() > 3
                    && ![
                        "page", "shows", "contains", "visible", "button", "field", "input",
                    ]
                    .contains(w)
            })
            .collect();
        if keywords.is_empty() {
            return false;
        }
        // If any keyword matches, condition is met.
        return keywords.iter().any(|kw| snap_lower.contains(kw));
    }

    // "or" in the condition means any term matches.
    // Otherwise all terms must match.
    if cond_lower.contains(" or ") {
        terms.iter().any(|t| snap_lower.contains(&t.to_lowercase()))
    } else {
        terms.iter().all(|t| snap_lower.contains(&t.to_lowercase()))
    }
}

/// Switch to a tab matching the given pattern.
fn switch_to_tab(pattern: &str, log: &mut String, browser: &mut dyn Browser) -> Result<(), String> {
    let output = browser
        .run(&["tab", "list"])
        .map_err(|e| format!("tab list failed: {e}"))?;

    // Parse tab list output to find a tab matching the pattern.
    // Output looks like:
    //   → [t24] Welcome back - OpenAI - https://auth.openai.com/...
    //   [t12] Claude - https://claude.ai/...
    let pat_lower = pattern.to_lowercase();
    for line in output.lines() {
        if line.to_lowercase().contains(&pat_lower) {
            // Extract tab id (tNN).
            if let Some(t_start) = line.find("t") {
                let after = &line[t_start..];
                if let Some(end) = after.find(|c: char| !c.is_ascii_digit() && c != 't') {
                    let tab_id = &after[..end];
                    log.push_str(&format!("#   switching to tab {tab_id}\n"));
                    browser
                        .run(&["tab", tab_id])
                        .map_err(|e| format!("tab switch failed: {e}"))?;
                    return Ok(());
                }
            }
        }
    }
    // Tab not found — not an error, the flow may be on the right tab already.
    log.push_str(&format!("#   WARN: no tab matching '{pattern}' found\n"));
    Ok(())
}

/// Close stale tabs matching a pattern.
fn close_stale_tabs(
    pattern: &str,
    log: &mut String,
    browser: &mut dyn Browser,
) -> Result<(), String> {
    let output = browser
        .run(&["tab", "list"])
        .map_err(|e| format!("tab list failed: {e}"))?;

    let pat_lower = pattern.to_lowercase();
    for line in output.lines() {
        if line.to_lowercase().contains(&pat_lower)
            && let Some(t_start) = line.find("t")
        {
            let after = &line[t_start..];
            if let Some(end) = after.find(|c: char| !c.is_ascii_digit() && c != 't') {
                let tab_id = &after[..end];
                log.push_str(&format!("#   closing tab {tab_id}\n"));
                let _ = browser.run(&["tab", "close", tab_id]);
            }
        }
    }
    Ok(())
}

/// Parse a wait duration string like "120s", "10s", "load", "500ms".
fn parse_wait_duration(s: &str) -> Duration {
    let s = s.trim();
    if s.ends_with("ms") {
        let n: u64 = s.trim_end_matches("ms").parse().unwrap_or(500);
        return Duration::from_millis(n);
    }
    if s.ends_with('s') {
        let n: u64 = s.trim_end_matches('s').parse().unwrap_or(5);
        return Duration::from_secs(n);
    }
    if s == "load" {
        return Duration::from_secs(2);
    }
    // Try to parse as seconds.
    s.parse::<u64>()
        .map_or(Duration::from_secs(2), Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_quoted_simple() {
        let terms = extract_quoted_strings("button containing 'Continue with Google'");
        assert_eq!(terms, vec!["Continue with Google"]);
    }

    #[test]
    fn extract_quoted_multiple() {
        let terms = extract_quoted_strings("page contains 'hCaptcha' or 'Drag'");
        assert_eq!(terms, vec!["hCaptcha", "Drag"]);
    }

    #[test]
    fn extract_quoted_none() {
        let terms = extract_quoted_strings("page shows password input field");
        assert!(terms.is_empty());
    }

    #[test]
    fn find_ref_button() {
        let snapshot = "- heading \"Welcome back\" [level=1, ref=e1]\n- button \"Continue with Google\" [ref=e5]";
        let ref_id = find_ref_in_snapshot(snapshot, "button containing 'Continue with Google'");
        assert_eq!(ref_id, Some("e5".into()));
    }

    #[test]
    fn find_ref_account() {
        let snapshot = "- button \"Select account Alice alice@example.com\" [ref=e7]";
        let ref_id = find_ref_in_snapshot(snapshot, "account matching 'alice@example.com'");
        assert_eq!(ref_id, Some("e7".into()));
    }

    #[test]
    fn find_ref_not_found() {
        let snapshot = "- heading \"Welcome\" [ref=e1]";
        let ref_id = find_ref_in_snapshot(snapshot, "button containing 'Submit'");
        assert_eq!(ref_id, None);
    }

    #[test]
    fn evaluate_condition_or() {
        let snapshot = "- heading \"hCaptcha\" [ref=e1]";
        assert!(evaluate_condition(
            "page contains 'hCaptcha' or 'Drag'",
            snapshot,
            None
        ));
    }

    #[test]
    fn evaluate_condition_and() {
        let snapshot = "- button \"Continue\" [ref=e5]\n- text \"signing back in\"";
        assert!(evaluate_condition(
            "page shows 'Continue' and 'signing back in'",
            snapshot,
            None
        ));
    }

    #[test]
    fn evaluate_condition_not_met() {
        let snapshot = "- heading \"Welcome\" [ref=e1]";
        assert!(!evaluate_condition(
            "page shows 'Continue with Google'",
            snapshot,
            None
        ));
    }

    #[test]
    fn evaluate_condition_keyword() {
        let snapshot = "- button \"Continue\" [ref=e5]";
        assert!(evaluate_condition(
            "page shows Continue button",
            snapshot,
            None
        ));
    }

    // Regression: codex-auth-flow account selection. When the account email
    // is quoted in the condition/ref_hint, the evaluator and ref finder must
    // require it — not silently match any "Select account" button.
    #[test]
    fn condition_account_present_in_chooser() {
        let snapshot = "- heading \"Welcome back\" [level=1, ref=e1]\n\
            - button \"Select account Bob bob@example.com\" [ref=e3]\n\
            - button \"Select account Alice alice@example.com\" [ref=e7]";
        let cond = "the desired account 'bob@example.com' is visible as a 'Select account' button";
        assert!(evaluate_condition(cond, snapshot, None));
    }

    #[test]
    fn condition_account_absent_from_chooser() {
        // Only alice's account is shown — bob is NOT listed.
        let snapshot = "- heading \"Welcome back\" [level=1, ref=e1]\n\
            - button \"Select account Alice alice@example.com\" [ref=e7]";
        let cond = "the desired account 'bob@example.com' is visible as a 'Select account' button";
        assert!(!evaluate_condition(cond, snapshot, None));
    }

    #[test]
    fn find_ref_picks_correct_account_button() {
        let snapshot = "- button \"Select account Alice alice@example.com\" [ref=e7]\n\
            - button \"Select account Bob bob@example.com\" [ref=e3]";
        let hint = "button containing 'Select account' and matching 'bob@example.com'";
        assert_eq!(find_ref_in_snapshot(snapshot, hint), Some("e3".into()));
    }

    #[test]
    fn find_ref_account_not_present_returns_none() {
        let snapshot = "- button \"Select account Alice alice@example.com\" [ref=e7]";
        let hint = "button containing 'Select account' and matching 'bob@example.com'";
        assert_eq!(find_ref_in_snapshot(snapshot, hint), None);
    }

    #[test]
    fn parse_wait_seconds() {
        assert_eq!(parse_wait_duration("120s"), Duration::from_secs(120));
        assert_eq!(parse_wait_duration("10s"), Duration::from_secs(10));
    }

    #[test]
    fn parse_wait_millis() {
        assert_eq!(parse_wait_duration("500ms"), Duration::from_millis(500));
    }

    #[test]
    fn parse_wait_load() {
        assert_eq!(parse_wait_duration("load"), Duration::from_secs(2));
    }

    use crate::types::{Flow, FlowVar};
    use std::collections::VecDeque;

    /// Scripted stand-in for agent-browser: every call is recorded, answers
    /// come from the script in order (then `Ok("")`), waits are recorded
    /// instead of slept.
    struct Scripted {
        calls: Vec<Vec<String>>,
        answers: VecDeque<Result<String, String>>,
        paused: Vec<Duration>,
    }

    impl Scripted {
        fn new(answers: Vec<Result<&str, &str>>) -> Self {
            Scripted {
                calls: Vec::new(),
                answers: answers
                    .into_iter()
                    .map(|a| a.map(str::to_string).map_err(str::to_string))
                    .collect(),
                paused: Vec::new(),
            }
        }

        fn calls(&self) -> Vec<Vec<&str>> {
            self.calls
                .iter()
                .map(|c| c.iter().map(String::as_str).collect())
                .collect()
        }
    }

    impl Browser for Scripted {
        fn run(&mut self, args: &[&str]) -> Result<String, String> {
            self.calls
                .push(args.iter().map(ToString::to_string).collect());
            self.answers
                .pop_front()
                .unwrap_or_else(|| Ok(String::new()))
        }

        fn pause(&mut self, duration: Duration) {
            self.paused.push(duration);
        }
    }

    fn step(action: &str) -> FlowStep {
        FlowStep {
            action: action.into(),
            ..Default::default()
        }
    }

    fn run_step(step: &FlowStep, browser: &mut Scripted) -> (Result<bool, String>, String) {
        let flow = flow_with(vec![]);
        let mut log = String::new();
        let result = exec_step(step, 1, &HashMap::new(), &flow, &mut log, 0, browser);
        (result, log)
    }

    fn flow_with(steps: Vec<FlowStep>) -> Flow {
        Flow {
            name: "test".into(),
            title: "Test".into(),
            description: "Test flow".into(),
            tags: vec![],
            url: None,
            tab: None,
            success_url_contains: vec![],
            success_url_excludes: vec![],
            mfa_keywords: vec![],
            stale_tab_cleanup: vec![],
            preconditions: vec![],
            vars: vec![],
            steps,
            success_signal: None,
            created_unix: 1000,
            revised_unix: 1000,
            revision: 1,
            proven: true,
        }
    }

    #[test]
    fn open_runs_the_url_then_waits_for_the_page() {
        let mut b = Scripted::new(vec![]);
        let s = FlowStep {
            url: Some("https://example.com/{{p}}".into()),
            ..step("open")
        };
        let mut log = String::new();
        let vars = HashMap::from([("p".to_string(), "login".to_string())]);
        let r = exec_step(&s, 1, &vars, &flow_with(vec![]), &mut log, 0, &mut b);
        assert_eq!(r, Ok(true));
        assert_eq!(b.calls(), vec![vec!["open", "https://example.com/login"]]);
        assert_eq!(b.paused, vec![Duration::from_secs(3)]);
    }

    #[test]
    fn open_falls_back_to_a_new_tab_only_when_the_bound_tab_is_gone() {
        let s = FlowStep {
            url: Some("https://e.com".into()),
            ..step("open")
        };
        for gone in ["tab_gone: t3", "no tab bound"] {
            let mut b = Scripted::new(vec![Err(gone), Ok("")]);
            let (r, log) = run_step(&s, &mut b);
            assert_eq!(r, Ok(true), "{gone}");
            assert_eq!(
                b.calls(),
                vec![
                    vec!["open", "https://e.com"],
                    vec!["tab", "new", "https://e.com"]
                ]
            );
            assert!(log.contains("bound tab gone"), "{log}");
        }
        // Any other failure is the step's failure: no retry.
        let mut b = Scripted::new(vec![Err("connection refused")]);
        let (r, _) = run_step(&s, &mut b);
        assert_eq!(r, Err("open failed: connection refused".to_string()));
        assert_eq!(b.calls().len(), 1);
        // And a failed `tab new` is reported as such.
        let mut b = Scripted::new(vec![Err("tab_gone"), Err("nope")]);
        let (r, _) = run_step(&s, &mut b);
        assert_eq!(r, Err("open/tab new failed: nope".to_string()));
    }

    #[test]
    fn snapshot_takes_an_interactive_snapshot() {
        let mut b = Scripted::new(vec![]);
        let (r, log) = run_step(&step("snapshot"), &mut b);
        assert_eq!(r, Ok(true));
        assert_eq!(b.calls(), vec![vec!["snapshot", "-i"]]);
        assert!(log.contains("agent-browser snapshot -i"), "{log}");
        let mut b = Scripted::new(vec![Err("gone")]);
        let (r, _) = run_step(&step("snapshot"), &mut b);
        assert_eq!(r, Err("snapshot failed: gone".to_string()));
    }

    #[test]
    fn click_resolves_the_ref_from_a_fresh_snapshot_then_waits() {
        let mut b = Scripted::new(vec![Ok("- button \"Continue with Google\" [ref=e5]\n")]);
        let s = FlowStep {
            ref_hint: Some("button containing 'Continue with Google'".into()),
            ..step("click")
        };
        let (r, log) = run_step(&s, &mut b);
        assert_eq!(r, Ok(true));
        assert_eq!(
            b.calls(),
            vec![vec!["snapshot", "-i"], vec!["click", "@e5"]]
        );
        assert_eq!(b.paused, vec![Duration::from_secs(2)]);
        assert!(log.contains("agent-browser click @e5"), "{log}");
        // No matching element: the step fails before any click.
        let mut b = Scripted::new(vec![Ok("- heading \"Welcome\" [ref=e1]\n")]);
        let (r, _) = run_step(&s, &mut b);
        assert_eq!(
            r,
            Err(
                "no element matching 'button containing 'Continue with Google'' found in snapshot"
                    .to_string()
            )
        );
        assert_eq!(b.calls().len(), 1);
    }

    #[test]
    fn click_retries_through_javascript_when_the_page_did_not_move() {
        let snap = "- button \"Sign in\" [ref=e2]\n";
        // The fallback fires when the post-click snapshot still shows the
        // target text, so the hint is the visible label itself.
        let s = FlowStep {
            ref_hint: Some("Sign in".into()),
            on_failure: Some("use a JS click via eval".into()),
            ..step("click")
        };
        // Same snapshot after the click: the target is still there.
        let mut b = Scripted::new(vec![Ok(snap), Ok(""), Ok(snap)]);
        let (r, log) = run_step(&s, &mut b);
        assert_eq!(r, Ok(true));
        assert_eq!(
            b.calls()[2..],
            vec![
                vec!["snapshot", "-i"],
                vec!["eval", "document.querySelector('button')?.click()"]
            ]
        );
        assert!(log.contains("JS click fallback"), "{log}");
        // Page moved: no fallback.
        let mut b = Scripted::new(vec![Ok(snap), Ok(""), Ok("- heading \"Home\" [ref=e1]\n")]);
        let (r, _) = run_step(&s, &mut b);
        assert_eq!(r, Ok(true));
        assert_eq!(b.calls().len(), 3);
    }

    #[test]
    fn fill_and_type_send_the_resolved_value_to_the_matched_field() {
        for action in ["fill", "type"] {
            let mut b = Scripted::new(vec![Ok("- textbox \"Email\" [ref=e3]\n")]);
            let s = FlowStep {
                ref_hint: Some("textbox matching 'Email'".into()),
                value: Some("a@example.com".into()),
                ..step(action)
            };
            let (r, log) = run_step(&s, &mut b);
            assert_eq!(r, Ok(true), "{action}");
            assert_eq!(
                b.calls(),
                vec![vec!["snapshot", "-i"], vec![action, "@e3", "a@example.com"]]
            );
            assert!(
                log.contains(&format!("agent-browser {action} @e3 \"a@example.com\"")),
                "{log}"
            );
        }
    }

    #[test]
    fn fill_takes_the_nth_character_of_user_code_for_split_code_inputs() {
        let mut b = Scripted::new(vec![Ok("- textbox \"Code character 3 of 9\" [ref=e8]\n")]);
        let s = FlowStep {
            ref_hint: Some("textbox matching 'Code character 3 of 9'".into()),
            ..step("fill")
        };
        let vars = HashMap::from([("user_code".to_string(), "AB-CD".to_string())]);
        let mut log = String::new();
        let r = exec_step(&s, 1, &vars, &flow_with(vec![]), &mut log, 0, &mut b);
        assert_eq!(r, Ok(true));
        assert_eq!(b.calls()[1], vec!["fill", "@e8", "C"]);
    }

    #[test]
    fn select_picks_the_option_on_the_matched_select() {
        let mut b = Scripted::new(vec![Ok("- combobox \"Country\" [ref=e4]\n")]);
        let s = FlowStep {
            ref_hint: Some("combobox matching 'Country'".into()),
            value: Some("FR".into()),
            ..step("select")
        };
        let (r, log) = run_step(&s, &mut b);
        assert_eq!(r, Ok(true));
        assert_eq!(
            b.calls(),
            vec![vec!["snapshot", "-i"], vec!["select", "@e4", "FR"]]
        );
        assert!(log.contains("agent-browser select @e4 \"FR\""), "{log}");
    }

    #[test]
    fn press_sends_the_key_and_defaults_to_enter() {
        let mut b = Scripted::new(vec![]);
        let (r, _) = run_step(&step("press"), &mut b);
        assert_eq!(r, Ok(true));
        assert_eq!(b.calls(), vec![vec!["press", "Enter"]]);
        let mut b = Scripted::new(vec![]);
        let s = FlowStep {
            key: Some("Tab".into()),
            ..step("press")
        };
        run_step(&s, &mut b).0.unwrap();
        assert_eq!(b.calls(), vec![vec!["press", "Tab"]]);
    }

    #[test]
    fn wait_pauses_for_the_parsed_duration_without_touching_the_browser() {
        let mut b = Scripted::new(vec![]);
        let s = FlowStep {
            wait: Some("10s".into()),
            ..step("wait")
        };
        let (r, log) = run_step(&s, &mut b);
        assert_eq!(r, Ok(true));
        assert!(b.calls().is_empty());
        assert_eq!(b.paused, vec![Duration::from_secs(10)]);
        assert!(log.contains("# Waiting 10s"), "{log}");
    }

    #[test]
    fn conditional_runs_the_then_branch_when_met_and_otherwise_when_not() {
        let cond = FlowStep {
            condition: Some("page shows 'Welcome back'".into()),
            then: vec![FlowStep {
                key: Some("Enter".into()),
                ..step("press")
            }],
            otherwise: vec![step("snapshot")],
            ..step("conditional")
        };
        let mut b = Scripted::new(vec![Ok("- heading \"Welcome back\" [ref=e1]\n")]);
        let (r, log) = run_step(&cond, &mut b);
        assert_eq!(r, Ok(true));
        assert_eq!(
            b.calls(),
            vec![vec!["snapshot", "-i"], vec!["press", "Enter"]]
        );
        assert!(log.contains("Condition MET — taking THEN branch"), "{log}");

        let mut b = Scripted::new(vec![Ok("- heading \"Sign in\" [ref=e1]\n")]);
        let (r, log) = run_step(&cond, &mut b);
        assert_eq!(r, Ok(true));
        assert_eq!(
            b.calls(),
            vec![vec!["snapshot", "-i"], vec!["snapshot", "-i"]]
        );
        assert!(
            log.contains("Condition NOT MET — taking ELSE branch"),
            "{log}"
        );

        // An empty branch executes nothing: the step counts as skipped.
        let empty = FlowStep {
            otherwise: vec![],
            ..cond.clone()
        };
        let mut b = Scripted::new(vec![Ok("- heading \"Sign in\" [ref=e1]\n")]);
        assert_eq!(run_step(&empty, &mut b).0, Ok(false));
    }

    #[test]
    fn conditional_on_the_url_reads_it_from_the_browser() {
        let cond = FlowStep {
            condition: Some("URL contains 'code='".into()),
            then: vec![step("snapshot")],
            ..step("conditional")
        };
        let mut b = Scripted::new(vec![Ok(""), Ok("https://e.com/cb?code=abc")]);
        let (r, _) = run_step(&cond, &mut b);
        assert_eq!(r, Ok(true));
        assert_eq!(
            b.calls(),
            vec![
                vec!["snapshot", "-i"],
                vec!["get", "url"],
                vec!["snapshot", "-i"]
            ]
        );
        let mut b = Scripted::new(vec![Ok(""), Ok("https://e.com/login")]);
        assert_eq!(run_step(&cond, &mut b).0, Ok(false));
        assert!(!evaluate_condition("URL contains 'code='", "", None));
    }

    #[test]
    fn switch_tab_switches_to_the_first_matching_tab_or_warns() {
        let tabs = "→ [t24] Welcome back - OpenAI - https://auth.openai.com/\n[t12] Claude - https://claude.ai/\n";
        let mut b = Scripted::new(vec![Ok(tabs)]);
        let s = FlowStep {
            tab: Some("claude".into()),
            ..step("switch_tab")
        };
        let (r, log) = run_step(&s, &mut b);
        assert_eq!(r, Ok(true));
        assert_eq!(b.calls(), vec![vec!["tab", "list"], vec!["tab", "t12"]]);
        assert!(log.contains("switching to tab t12"), "{log}");

        let mut b = Scripted::new(vec![Ok(tabs)]);
        let s = FlowStep {
            tab: Some("github".into()),
            ..step("switch_tab")
        };
        let (r, log) = run_step(&s, &mut b);
        assert_eq!(r, Ok(true));
        assert_eq!(b.calls(), vec![vec!["tab", "list"]]);
        assert!(log.contains("WARN: no tab matching 'github'"), "{log}");
    }

    #[test]
    fn a_step_with_its_own_tab_switches_before_acting() {
        let mut b = Scripted::new(vec![Ok("[t7] GitHub - https://github.com\n")]);
        let s = FlowStep {
            tab: Some("github".into()),
            ..step("snapshot")
        };
        let (r, _) = run_step(&s, &mut b);
        assert_eq!(r, Ok(true));
        assert_eq!(
            b.calls(),
            vec![
                vec!["tab", "list"],
                vec!["tab", "t7"],
                vec!["snapshot", "-i"]
            ]
        );
    }

    #[test]
    fn eval_runs_the_script_and_skips_an_empty_one() {
        let mut b = Scripted::new(vec![]);
        let s = FlowStep {
            value: Some("document.title".into()),
            ..step("eval")
        };
        let (r, _) = run_step(&s, &mut b);
        assert_eq!(r, Ok(true));
        assert_eq!(b.calls(), vec![vec!["eval", "document.title"]]);
        let mut b = Scripted::new(vec![]);
        let (r, _) = run_step(&step("eval"), &mut b);
        assert_eq!(r, Ok(true));
        assert!(b.calls().is_empty(), "{:?}", b.calls());
    }

    #[test]
    fn unknown_action_is_skipped_not_executed() {
        let mut b = Scripted::new(vec![]);
        let (r, log) = run_step(&step("teleport"), &mut b);
        assert_eq!(r, Ok(false));
        assert!(b.calls().is_empty());
        assert!(log.contains("Unknown action: teleport"), "{log}");
    }

    #[test]
    fn stale_tab_cleanup_closes_every_matching_tab_before_the_steps() {
        let flow = Flow {
            stale_tab_cleanup: vec!["chatgpt".into()],
            ..flow_with(vec![])
        };
        let mut b = Scripted::new(vec![Ok(
            "[t3] ChatGPT - https://chatgpt.com\n[t9] Other\n[t5] chatgpt again\n",
        )]);
        let result = execute_with(&flow, &HashMap::new(), &mut b);
        assert!(result.success, "{}", result.log);
        assert_eq!(
            b.calls()[..3],
            vec![
                vec!["tab", "list"],
                vec!["tab", "close", "t3"],
                vec!["tab", "close", "t5"]
            ]
        );
        assert!(result.log.contains("closing tab t3"), "{}", result.log);
    }

    /// A `required` var carrying a `default` is absent from the map on
    /// purpose: the command must carry the default, never the literal
    /// `{{account}}` placeholder the browser would type into the form.
    #[test]
    fn execute_required_var_with_default_uses_the_default() {
        let flow = Flow {
            vars: vec![FlowVar {
                name: "account".into(),
                description: "Account to select".into(),
                required: true,
                default: Some("west".into()),
            }],
            ..flow_with(vec![FlowStep {
                ref_hint: Some("textbox matching 'Account'".into()),
                value_var: Some("account".into()),
                ..step("fill")
            }])
        };
        let mut b = Scripted::new(vec![Ok("- textbox \"Account\" [ref=e5]\n")]);
        let result = execute_with(&flow, &HashMap::new(), &mut b);
        assert!(result.success, "{}", result.log);
        assert_eq!(b.calls()[1], vec!["fill", "@e5", "west"]);
        assert!(!result.log.contains("{{account}}"), "{}", result.log);
    }

    #[test]
    fn execute_counts_executed_and_skipped_steps_and_stops_at_the_first_error() {
        let flow = flow_with(vec![step("snapshot"), step("teleport"), step("snapshot")]);
        let mut b = Scripted::new(vec![]);
        let result = execute_with(&flow, &HashMap::new(), &mut b);
        assert!(result.success);
        assert_eq!((result.steps_executed, result.steps_skipped), (2, 1));
        assert!(
            result.log.contains("2 steps executed, 1 skipped"),
            "{}",
            result.log
        );

        let mut b = Scripted::new(vec![Ok(""), Err("browser died")]);
        let result = execute_with(&flow, &HashMap::new(), &mut b);
        assert!(!result.success);
        assert_eq!(
            result.error.as_deref(),
            Some("step 3 failed: snapshot failed: browser died")
        );
        assert_eq!((result.steps_executed, result.steps_skipped), (1, 1));
    }

    #[test]
    fn execute_numbers_the_steps_from_one_and_indents_nested_ones() {
        let flow = flow_with(vec![
            FlowStep {
                rationale: Some("first".into()),
                ..step("snapshot")
            },
            FlowStep {
                rationale: Some("second".into()),
                condition: Some("page shows 'A'".into()),
                then: vec![FlowStep {
                    rationale: Some("nested".into()),
                    ..step("snapshot")
                }],
                ..step("conditional")
            },
        ]);
        let mut b = Scripted::new(vec![Ok(""), Ok("- heading \"A\" [ref=e1]\n")]);
        let result = execute_with(&flow, &HashMap::new(), &mut b);
        assert!(result.success, "{}", result.log);
        assert!(result.log.contains("\n# Step 1: first\n"), "{}", result.log);
        assert!(
            result.log.contains("\n# Step 2: second\n"),
            "{}",
            result.log
        );
        assert!(
            result.log.contains("\n  # Step 1: nested\n"),
            "{}",
            result.log
        );
    }

    #[test]
    fn execute_hands_off_when_an_mfa_keyword_shows_on_the_final_page() {
        let flow = Flow {
            mfa_keywords: vec!["Verify your identity".into()],
            ..flow_with(vec![step("snapshot")])
        };
        // step snapshot, final snapshot, get url, MFA snapshot.
        let page = "- heading \"Verify your identity\" [ref=e1]\n";
        let mut b = Scripted::new(vec![Ok(""), Ok(page), Ok("https://e.com"), Ok(page)]);
        let result = execute_with(&flow, &HashMap::new(), &mut b);
        assert!(!result.success);
        assert_eq!(
            result.error.as_deref(),
            Some(
                "MFA gate detected (keyword: 'Verify your identity') — user intervention required"
            )
        );
        assert!(result.log.contains("# MFA DETECTED"), "{}", result.log);
        // Without the keyword on the page the flow completes.
        let mut b = Scripted::new(vec![Ok(""), Ok(""), Ok("https://e.com"), Ok("")]);
        assert!(execute_with(&flow, &HashMap::new(), &mut b).success);
    }

    #[test]
    fn execute_fails_the_success_url_check_unless_the_signal_overrides_it() {
        let flow = Flow {
            success_url_contains: vec!["/dashboard".into()],
            ..flow_with(vec![])
        };
        let mut b = Scripted::new(vec![Ok(""), Ok("https://e.com/login")]);
        let result = execute_with(&flow, &HashMap::new(), &mut b);
        assert!(!result.success);
        assert_eq!(result.error.as_deref(), Some("success URL check failed"));
        let mut b = Scripted::new(vec![Ok(""), Ok("https://e.com/dashboard")]);
        assert!(execute_with(&flow, &HashMap::new(), &mut b).success);
        let signalled = Flow {
            success_signal: Some("page shows 'Signed in'".into()),
            ..flow.clone()
        };
        let mut b = Scripted::new(vec![
            Ok("- text \"Signed in\"\n"),
            Ok("https://e.com/login"),
        ]);
        let result = execute_with(&signalled, &HashMap::new(), &mut b);
        assert!(result.success, "{}", result.log);
        assert!(
            result.log.contains("Success signal detected"),
            "{}",
            result.log
        );
    }

    #[test]
    fn the_real_browser_pause_waits_wall_clock_time() {
        let start = std::time::Instant::now();
        AgentBrowser.pause(Duration::from_millis(20));
        assert!(start.elapsed() >= Duration::from_millis(20));
    }

    #[test]
    fn substitute_replaces_every_placeholder_and_keeps_unknown_ones() {
        let vars = HashMap::from([
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
        ]);
        assert_eq!(
            substitute("{{a}}+{{b}}={{a}}{{b}} {{c}}", &vars),
            "1+2=12 {{c}}"
        );
        assert_eq!(substitute("plain", &vars), "plain");
    }
}
