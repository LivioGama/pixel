//! End-to-end contract, private verification and native lifecycle behavior.

use std::io::Write;
use std::path::Path;
use std::process::{Output, Stdio};

use serde_json::{Value, json};

use super::support::{Scratch, git, pixel_command};

fn repo(tag: &str) -> Scratch {
    let root = Scratch::for_test("task-cli", tag);
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("source.txt"), "correct\n").unwrap();
    std::fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "initial"]);
    std::fs::create_dir(root.join(".pixel")).unwrap();
    root
}

fn contract() -> Value {
    json!({"version":1,"objective":"fix source","checks":[{"id":"content","argv":["/bin/sh","-c","test \"$(cat source.txt)\" = correct"],"timeout_ms":5000}],
        "criteria":[{"id":"task-acceptance","description":"fix source","checks":["content"]}],"conservative_checks":["content"]})
}

fn command(root: &Path, args: &[&str]) -> Output {
    pixel_command()
        .current_dir(root)
        .env("PIXEL_METRICS", "0")
        .env("PIXEL_TASK_POLICY", "gates")
        .env_remove("PIXEL_TASK_CONTRACT")
        .env_remove("PIXEL_TASK_TELEMETRY_PATH")
        .env_remove("PIXEL_TASK_ID")
        .env_remove("PIXEL_TASK_SPAN_ID")
        .env_remove("PIXEL_TASK_PARENT_SPAN_ID")
        .env_remove("PIXEL_TASK_HOST_CALL_ID")
        .args(args)
        .output()
        .unwrap()
}

fn good(root: &Path, args: &[&str]) -> Value {
    let output = command(root, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{args:?}: {error}: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn begin(root: &Path) -> String {
    std::fs::write(root.join(".pixel/contract.json"), contract().to_string()).unwrap();
    good(
        root,
        &[
            "task",
            "begin",
            "fix source",
            "--contract",
            ".pixel/contract.json",
            "--request-id",
            "begin",
            "--json",
        ],
    )["task_id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn hook(root: &Path, provider: &str, event: &str, payload: Value) -> Value {
    hook_with_policy(root, provider, event, payload, "gates")
}

fn hook_with_policy(
    root: &Path,
    provider: &str,
    event: &str,
    mut payload: Value,
    policy: &str,
) -> Value {
    payload["cwd"] = json!(root);
    let mut child = pixel_command()
        .current_dir(root)
        .env("PIXEL_METRICS", "0")
        .env("PIXEL_TASK_POLICY", policy)
        .env_remove("PIXEL_TASK_CONTRACT")
        .env_remove("PIXEL_TASK_TELEMETRY_PATH")
        .args([
            "run-hook",
            "task-event",
            "--provider",
            provider,
            "--event",
            event,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn read_only_prompts_should_stay_free_and_mutation_fallback_should_retain_the_objective() {
    let root = repo("intent");
    for (session, prompt) in [
        ("question", "How does this code implement its cache?"),
        ("request", "Please make the result deterministic"),
    ] {
        hook(
            &root,
            "pi",
            "prompt-submit",
            json!({"session_id":session,"prompt":prompt,"event_id":"prompt"}),
        );
        assert_eq!(
            hook(
                &root,
                "pi",
                "stop",
                json!({"session_id":session,"event_id":"read-only-stop"})
            )["decision"],
            "observe"
        );
        assert!(
            pixel_task::Store::open(&root)
                .unwrap()
                .find_session("pi", session)
                .unwrap()
                .is_none()
        );
    }
    let result = hook(
        &root,
        "pi",
        "pre-tool-use",
        json!({"session_id":"request","tool_name":"Write","tool_use_id":"first-edit"}),
    );
    assert_eq!(result["decision"], "deny");
    let task = pixel_task::Store::open(&root)
        .unwrap()
        .find_session("pi", "request")
        .unwrap()
        .unwrap();
    assert_eq!(
        task.contract.objective,
        "Please make the result deterministic"
    );
    assert_eq!(
        task.contract.criteria[0].description,
        task.contract.objective
    );
}

#[test]
fn enforced_tasks_should_reject_runtime_policy_downgrades() {
    let root = repo("sticky-policy");
    let payload = json!({"session_id":"s","tool_name":"Write","tool_use_id":"first-edit"});
    assert_eq!(
        hook(&root, "pi", "pre-tool-use", payload)["decision"],
        "deny"
    );
    std::fs::write(
        root.join(".pixel/config.json"),
        json!({"task":{"enforcement":"off"}}).to_string(),
    )
    .unwrap();
    assert_eq!(
        hook_with_policy(
            &root,
            "pi",
            "pre-tool-use",
            json!({"session_id":"s","tool_name":"Write","tool_use_id":"second-edit"}),
            "retrieval"
        )["decision"],
        "deny"
    );
    assert_eq!(
        hook_with_policy(
            &root,
            "pi",
            "stop",
            json!({"session_id":"s","event_id":"stop"}),
            "retrieval"
        )["decision"],
        "continue"
    );
    // Disabled policy still applies to a new task, as required for the baseline arm.
    assert_eq!(
        hook_with_policy(
            &root,
            "pi",
            "pre-tool-use",
            json!({"session_id":"baseline","tool_name":"Write","tool_use_id":"first"}),
            "retrieval"
        )["decision"],
        "observe"
    );
}

#[test]
fn repository_toolchain_pins_should_survive_every_contract_entry_path() {
    let root = repo("toolchain");
    let pins = json!({"compiler":"a".repeat(64)});
    std::fs::write(
        root.join(".pixel/config.json"),
        json!({"task":{"toolchain":pins}}).to_string(),
    )
    .unwrap();
    let initial = good(&root, &["task", "begin", "fix source", "--json"]);
    assert_eq!(initial["contract"]["toolchain"], pins);
    let from_file = begin(&root);
    assert_eq!(
        good(&root, &["task", "status", &from_file, "--json"])["task"]["contract"]["toolchain"],
        pins
    );
    let extended = json!({"compiler":"a".repeat(64), "driver":"b".repeat(64)});
    std::fs::write(
        root.join(".pixel/config.json"),
        json!({"task":{"toolchain":extended}}).to_string(),
    )
    .unwrap();
    assert_eq!(
        good(&root, &["task", "status", &from_file, "--json"])["task"]["contract"]["toolchain"],
        extended
    );
    let mut replacement = contract();
    replacement["toolchain"] = json!({"compiler":"c".repeat(64)});
    std::fs::write(root.join(".pixel/contract.json"), replacement.to_string()).unwrap();
    assert!(
        !command(
            &root,
            &[
                "task",
                "contract",
                &from_file,
                "--file",
                ".pixel/contract.json",
                "--json"
            ]
        )
        .status
        .success()
    );
}

#[test]
fn unconfigured_task_should_accept_an_inline_contract_without_authorizing_a_file_write() {
    let root = repo("inline-contract");
    let task = good(
        &root,
        &[
            "task",
            "begin",
            "fix source",
            "--provider",
            "pi",
            "--session",
            "s",
            "--json",
        ],
    );
    let id = task["task_id"].as_str().unwrap();
    let definition = contract().to_string();
    assert_eq!(
        hook(
            &root,
            "pi",
            "pre-tool-use",
            json!({"session_id":"s","tool_name":"Write","tool_use_id":"write"})
        )["decision"],
        "deny"
    );
    let declaration = format!("pixel task contract {id} --definition '{definition}' --json");
    assert_ne!(
        hook(
            &root,
            "pi",
            "pre-tool-use",
            json!({"session_id":"s","tool_name":"bash","tool_use_id":"declare","tool_input":{"command":declaration}})
        )["decision"],
        "deny"
    );
    let strengthened = good(
        &root,
        &[
            "task",
            "contract",
            id,
            "--definition",
            &definition,
            "--json",
        ],
    );
    assert_eq!(
        strengthened["contract"]["checks"],
        json!([{"id":"content","argv":["/bin/sh","-c","test \"$(cat source.txt)\" = correct"],"cwd":".","timeout_ms":5000,"required":true}])
    );
    assert!(!root.join(".pixel/contract.json").exists());
    assert!(
        !command(
            &root,
            &["task", "contract", id, "--definition", "{", "--json"]
        )
        .status
        .success()
    );
    let mut weaker = contract();
    weaker["checks"] = json!([]);
    weaker["criteria"][0]["checks"] = json!([]);
    weaker["conservative_checks"] = json!([]);
    assert!(
        !command(
            &root,
            &[
                "task",
                "contract",
                id,
                "--definition",
                &weaker.to_string(),
                "--json"
            ]
        )
        .status
        .success()
    );
}

#[test]
fn task_should_require_private_check_and_review_then_reject_same_head_edits() {
    let root = repo("lifecycle");
    let id = begin(&root);
    let task = good(&root, &["task", "status", &id, "--json"]);
    let attempt = task["task"]["attempt_id"].clone();
    let correlated = pixel_command()
        .current_dir(&root)
        .env("PIXEL_METRICS", "0")
        .env_remove("PIXEL_TASK_ID")
        .env("PIXEL_TASK_SPAN_ID", "fixture-span")
        .env("PIXEL_TASK_PARENT_SPAN_ID", "fixture-parent")
        .env("PIXEL_TASK_HOST_CALL_ID", "fixture-call")
        .args(["task", "status", &id, "--json"])
        .output()
        .unwrap();
    assert!(
        correlated.status.success(),
        "{}",
        String::from_utf8_lossy(&correlated.stderr)
    );
    let actions: Vec<Value> = std::fs::read_to_string(root.join(".pixel/actions.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let correlated = actions
        .iter()
        .find(|event| event["task"]["span_id"] == "fixture-span")
        .unwrap();
    assert_eq!(
        correlated["task"],
        json!({"task_id":id,"attempt_id":attempt,"span_id":"fixture-span","parent_span_id":"fixture-parent","host_call_id":"fixture-call"})
    );
    assert!(
        actions
            .iter()
            .any(|event| event["task"]["task_id"] == id && event["task"]["span_id"] == "root")
    );
    let duplicate = good(
        &root,
        &[
            "task",
            "begin",
            "fix source",
            "--contract",
            ".pixel/contract.json",
            "--request-id",
            "begin",
            "--json",
        ],
    );
    assert_eq!(duplicate["task_id"], id);
    assert_eq!(duplicate["revision"], 1);
    assert!(
        !command(&root, &["task", "finish", &id, "--json"])
            .status
            .success()
    );
    let prepared = good(
        &root,
        &["task", "prepare", &id, "--request-id", "prepare", "--json"],
    );
    assert_eq!(prepared["phase"], "prepared");
    let verified = good(
        &root,
        &["task", "verify", &id, "--request-id", "verify", "--json"],
    );
    assert_eq!(verified["receipts"][0]["outcome"], "passed");
    assert_ne!(
        verified["receipts"][0]["execution_root"],
        json!(root.as_ref())
    );
    assert!(
        !command(&root, &["task", "finish", &id, "--json"])
            .status
            .success()
    );
    let reviewed = good(
        &root,
        &["task", "review", &id, "--request-id", "review", "--json"],
    );
    assert_eq!(reviewed["review"]["passed"], true);
    let finished = good(
        &root,
        &["task", "finish", &id, "--request-id", "finish", "--json"],
    );
    assert_eq!(finished["phase"], "complete");
    std::fs::write(root.join("source.txt"), "regression\n").unwrap();
    let status = good(&root, &["task", "status", &id, "--json"]);
    assert_eq!(status["finish"]["allowed"], false);
    assert!(
        !command(&root, &["task", "finish", &id, "--json"])
            .status
            .success()
    );
}

#[test]
fn task_should_reject_failed_check_and_keep_live_source_unchanged() {
    let root = repo("failed");
    let id = begin(&root);
    std::fs::write(root.join("source.txt"), "wrong\n").unwrap();
    good(&root, &["task", "prepare", &id, "--json"]);
    let output = command(&root, &["task", "verify", &id, "--json"]);
    assert!(!output.status.success());
    let verified: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(verified["receipts"][0]["outcome"], "failed");
    assert_eq!(
        std::fs::read_to_string(root.join("source.txt")).unwrap(),
        "wrong\n"
    );
    assert!(
        !command(&root, &["task", "finish", &id, "--json"])
            .status
            .success()
    );
}

#[test]
fn contract_should_map_initial_criteria_preserve_repository_checks_and_reject_noninteractive_waivers()
 {
    let root = repo("contract");
    let requirement = contract()["checks"].clone();
    std::fs::write(
        root.join(".pixel/config.json"),
        json!({"task":{"checks":requirement,"conservative_checks":["content"]}}).to_string(),
    )
    .unwrap();
    let task = good(&root, &["task", "begin", "fix source", "--json"]);
    let id = task["task_id"].as_str().unwrap();
    assert_eq!(task["contract"]["criteria"][0]["checks"], json!([]));
    // Omitted default fields and their serialized forms must be equivalent requirements.
    let status = good(&root, &["task", "status", id, "--json"]);
    assert_eq!(status["task"]["contract"], task["contract"]);
    assert_eq!(
        good(&root, &["task", "status", id, "--json"])["task"]["revision"],
        status["task"]["revision"]
    );
    std::fs::write(root.join(".pixel/contract.json"), contract().to_string()).unwrap();
    let updated = good(
        &root,
        &[
            "task",
            "contract",
            id,
            "--file",
            ".pixel/contract.json",
            "--json",
        ],
    );
    assert_eq!(
        updated["contract"]["criteria"][0]["checks"],
        json!(["content"])
    );
    let mut weakened = contract();
    weakened["checks"][0]["argv"] = json!(["/usr/bin/true"]);
    std::fs::write(root.join(".pixel/contract.json"), weakened.to_string()).unwrap();
    assert!(
        !command(
            &root,
            &[
                "task",
                "contract",
                id,
                "--file",
                ".pixel/contract.json",
                "--json"
            ]
        )
        .status
        .success()
    );
    std::fs::write(root.join(".pixel/contract.json"), contract().to_string()).unwrap();
    assert!(
        !command(
            &root,
            &[
                "task",
                "contract",
                id,
                "--file",
                ".pixel/contract.json",
                "--authorize-weakening",
                "--json"
            ]
        )
        .status
        .success()
    );
}

#[test]
fn native_hosts_should_deduplicate_denied_requests_and_stop_at_the_persistent_budget() {
    for provider in ["claude", "codex", "pi"] {
        let root = repo(&format!("host-{provider}"));
        let payload = json!({"session_id":"session","tool_use_id":"request-1","tool_name":"Write","tool_input":{"file_path":"source.txt","content":"private"}});
        for _ in 0..2 {
            let result = hook(&root, provider, "pre-tool-use", payload.clone());
            if provider == "pi" {
                assert_eq!(result["decision"], "deny");
            } else {
                assert_eq!(result["hookSpecificOutput"]["permissionDecision"], "deny");
            }
        }
        let store = pixel_task::Store::open(&root).unwrap();
        let task = store.find_session(provider, "session").unwrap().unwrap();
        let id = &task.task_id;
        let events = good(&root, &["task", "events", id, "--json"]);
        assert_eq!(events["trajectory"]["observed_model_tool_requests"], 1);
        assert_eq!(events["trajectory"]["blocked_requests"], 1);
        assert_eq!(events["trajectory"]["coverage"], "partial");
        assert_eq!(events["trajectory"]["model_tool_requests"], Value::Null);
        for index in 0..3 {
            let result = hook(
                &root,
                provider,
                "stop",
                json!({"session_id":"session","event_id":format!("stop-{index}")}),
            );
            if index < 2 {
                assert_eq!(
                    result["decision"],
                    if provider == "pi" {
                        "continue"
                    } else {
                        "block"
                    }
                );
            } else if provider == "pi" {
                assert_eq!(result["decision"], "deny");
            } else {
                assert_eq!(result["continue"], false);
                assert!(
                    result["stopReason"]
                        .as_str()
                        .unwrap()
                        .contains("unverified")
                );
            }
        }
        assert_eq!(store.status(id).unwrap().budget.continuations, 2);
        let replay = good(&root, &["task", "replay", id, "--json"]);
        assert_eq!(replay["first_divergence"], Value::Null);
        assert_eq!(replay["counterfactual_outcome_established"], false);
        assert!(replay["frames_evaluated"].as_u64().unwrap() > 0);
    }
}

#[test]
fn first_mutation_should_never_authorize_itself_and_failed_writes_should_invalidate_preparation() {
    let root = repo("fallback");
    std::fs::write(
        root.join(".pixel/config.json"),
        json!({"task":{"checks":contract()["checks"],"conservative_checks":["content"]}})
            .to_string(),
    )
    .unwrap();
    let mut payload = json!({"session_id":"s","tool_use_id":"one","tool_name":"Write","tool_input":{"file_path":"source.txt"}});
    assert_eq!(
        hook(&root, "pi", "pre-tool-use", payload.clone())["decision"],
        "deny"
    );
    payload["tool_use_id"] = json!("two");
    assert_eq!(
        hook(&root, "pi", "pre-tool-use", payload.clone())["decision"],
        "allow"
    );
    std::fs::write(root.join("source.txt"), "partial write\n").unwrap();
    hook(&root, "pi", "tool-failure", payload);
    let store = pixel_task::Store::open(&root).unwrap();
    let task = store.find_session("pi", "s").unwrap().unwrap();
    assert_eq!(task.phase, pixel_task::Phase::Editing);
    assert_eq!(task.source, None);
    hook(
        &root,
        "pi",
        "interrupt",
        json!({"session_id":"s","event_id":"cancel"}),
    );
    let result = hook(
        &root,
        "pi",
        "pre-tool-use",
        json!({"session_id":"s","tool_use_id":"three","tool_name":"Write"}),
    );
    assert_eq!(result["decision"], "deny");
    assert_eq!(
        store.find_session("pi", "s").unwrap().unwrap().task_id,
        task.task_id
    );
}

#[test]
fn pi_forks_should_keep_budget_and_reject_foreign_or_unbound_tasks() {
    let root = repo("fork");
    let started = hook(
        &root,
        "pi",
        "prompt-submit",
        json!({"session_id":"origin","event_id":"prompt","prompt":"fix source"}),
    );
    let id = started["task_id"].as_str().unwrap();
    let attempt = started["attempt_id"].as_str().unwrap();
    let binding = json!({"session_id":"fork","binding_session_id":"origin","task_id":id,"attempt_id":attempt,"branch_id":"fork-branch","event_id":"stop-1"});
    assert_eq!(
        hook(&root, "pi", "stop", binding.clone())["decision"],
        "continue"
    );
    let mut second = binding;
    second["event_id"] = json!("stop-2");
    assert_eq!(
        hook(&root, "pi", "stop", second.clone())["decision"],
        "continue"
    );
    second["event_id"] = json!("stop-3");
    assert_eq!(hook(&root, "pi", "stop", second)["decision"], "deny");
    let store = pixel_task::Store::open(&root).unwrap();
    assert_eq!(store.status(id).unwrap().budget.continuations, 2);
    let rejected = hook(
        &root,
        "pi",
        "pre-tool-use",
        json!({"session_id":"foreign","task_id":id,"attempt_id":attempt,"tool_name":"Write","tool_use_id":"bad"}),
    );
    assert_eq!(rejected["decision"], "deny");
    let unbound = hook(
        &root,
        "pi",
        "pre-tool-use",
        json!({"session_id":"origin","branch_unbound":true,"tool_name":"Write","tool_use_id":"unbound"}),
    );
    assert_eq!(unbound["decision"], "deny");
    let stale = hook(
        &root,
        "pi",
        "pre-tool-use",
        json!({"session_id":"origin","task_id":id,"attempt_id":"old","tool_name":"Write","tool_use_id":"stale"}),
    );
    assert_eq!(stale["decision"], "deny");
    assert_eq!(stale["attempt_id"], attempt);
}

#[test]
fn task_model_request_and_native_hook_should_share_one_count_and_keep_available_usage() {
    let root = repo("coverage");
    hook(
        &root,
        "pi",
        "prompt-submit",
        json!({"session_id":"s","event_id":"p","prompt":"fix source"}),
    );
    hook(
        &root,
        "pi",
        "model-response",
        json!({"session_id":"s","event_id":"m","response_id":"message-1","request_ids":["call-1"],"request_tools":{"call-1":"Read"},"coverage_complete":true,"usage":{"input":10,"output":4,"cache_read":2,"cache_write":0}}),
    );
    hook(
        &root,
        "pi",
        "pre-tool-use",
        json!({"session_id":"s","tool_use_id":"call-1","tool_name":"Read"}),
    );
    hook(
        &root,
        "pi",
        "user-bash",
        json!({"session_id":"s","event_id":"private-shell","command":"private command must not persist"}),
    );
    let task = pixel_task::Store::open(&root)
        .unwrap()
        .find_session("pi", "s")
        .unwrap()
        .unwrap();
    let events = good(&root, &["task", "events", &task.task_id, "--json"]);
    assert_eq!(events["trajectory"]["observed_model_tool_requests"], 1);
    assert_eq!(events["trajectory"]["model_responses"], 1);
    assert_eq!(
        events["trajectory"]["tokens"],
        json!({"input":10,"output":4,"cache_read":2,"cache_write":0})
    );
    assert_eq!(events["trajectory"]["model_tool_requests"], Value::Null);
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["data"]["kind"] == "external_action")
    );
    assert!(
        !events
            .to_string()
            .contains("private command must not persist")
    );
}
