//! Deterministic, bounded projection of `scope-task` evidence for agents.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

const MAX_CAPS: usize = 32;
const MAX_EVIDENCE_PER_TARGET: usize = 8;
const MAX_SYMBOLS_PER_TARGET: usize = 32;
const MAX_TARGETS: usize = 100;
const MAX_TEXT_CHARS: usize = 320;
const MAX_TASK_CHARS: usize = 4096;
const MAX_WORKSTREAMS: usize = 64;

#[derive(Default)]
struct Workstream {
    area: String,
    tier: String,
    targets: Vec<Value>,
}

/// Convert the existing `Request::Targets` response into the versioned brief
/// consumed by orchestration callers. Dependency edges are deliberately not
/// inferred: this command has no complete static dependency graph.
pub fn from_scope_task(task: &str, data: &Value) -> Value {
    let mut caps = BTreeSet::new();
    collect_source_caps(data, &mut caps);
    let bounded_task = bounded_text(task, "task", MAX_TASK_CHARS, &mut caps);

    let mut targets: Vec<&Value> = match data.get("targets").and_then(Value::as_array) {
        Some(targets) => targets
            .iter()
            .filter(|target| {
                matches!(
                    target.get("tier").and_then(Value::as_str),
                    Some("P0" | "P1")
                )
            })
            .collect(),
        None => {
            caps.insert("scope-task response did not contain a targets array".to_string());
            Vec::new()
        }
    };

    if data
        .get("targets")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|item| item["tier"] == "P2"))
    {
        caps.insert("P2 targets omitted: the brief contract supports only P0 and P1".to_string());
    }

    targets.sort_by(|left, right| {
        tier_rank(left)
            .cmp(&tier_rank(right))
            .then_with(|| target_path(left).cmp(target_path(right)))
    });
    if targets.len() > MAX_TARGETS {
        targets.truncate(MAX_TARGETS);
        caps.insert(format!("execution brief targets capped at {MAX_TARGETS}"));
    }

    let mut groups: BTreeMap<(String, String), Workstream> = BTreeMap::new();
    for target in targets {
        let Some(path) = target_path_value(target) else {
            caps.insert("target without a path omitted".to_string());
            continue;
        };
        let tier = target["tier"].as_str().unwrap_or("P1").to_string();
        let area = repository_area(path);
        let key = (area.clone(), tier.clone());
        let entry = groups.entry(key).or_insert_with(|| Workstream {
            area,
            tier,
            targets: Vec::new(),
        });
        entry.targets.push(target_projection(target, &mut caps));
    }

    let mut workstreams = Vec::new();
    for (_, mut group) in groups {
        group
            .targets
            .sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
        let paths: Vec<Value> = group
            .targets
            .iter()
            .filter_map(|target| target["path"].as_str().map(Value::from))
            .collect();
        workstreams.push(json!({
            "id": format!("workstream:{}:{}", group.area, group.tier),
            "paths": paths,
            "tier": group.tier,
            "depends_on": [],
            "ownership": if group.tier == "P0" { "write" } else { "read" },
            "targets": group.targets,
        }));
    }
    // The groups come out ordered by area; order them P0 first before the
    // cap so a truncation drops read-only context, never a write workstream.
    workstreams.sort_by(|left, right| {
        tier_rank(left)
            .cmp(&tier_rank(right))
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    if workstreams.len() > MAX_WORKSTREAMS {
        workstreams.truncate(MAX_WORKSTREAMS);
        caps.insert(format!(
            "execution brief workstreams capped at {MAX_WORKSTREAMS}"
        ));
    }

    let mut caps: Vec<String> = caps.into_iter().collect();
    if caps.len() > MAX_CAPS {
        caps.truncate(MAX_CAPS);
        caps.push(format!("execution brief caps capped at {MAX_CAPS}"));
    }
    let mut validation = Vec::new();
    let has_p0 = workstreams
        .iter()
        .any(|workstream| workstream["tier"] == "P0");
    let has_p1 = workstreams
        .iter()
        .any(|workstream| workstream["tier"] == "P1");
    if has_p0 {
        validation
            .push("Run focused tests covering changed P0 behavior before integration.".to_string());
    }
    if has_p1 {
        validation.push(
            "Review P1 workstreams for callers and contracts, then run regression tests."
                .to_string(),
        );
    }
    if !has_p0 && has_p1 {
        validation
            .push("No P0 target was returned; validate the P1 context before editing.".to_string());
    }
    if workstreams.is_empty() {
        validation.push(
            "No P0/P1 targets were returned; inspect scope-task caps before acting.".to_string(),
        );
    }
    if data
        .get("targets")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|item| item["tier"] == "P2"))
    {
        validation.push(
            "P2 targets were omitted; validate any peripheral files separately if touched."
                .to_string(),
        );
    }
    if !caps.is_empty() {
        validation.push("Review uncertainty.caps before relying on this brief.".to_string());
    }

    json!({
        "version": 1,
        "task": bounded_task,
        "workstreams": workstreams,
        "uncertainty": {
            "closed_world": false,
            "lower_bound": true,
            "caps": caps,
            "unknown_dependencies": [
                "Static dependency edges are unavailable; every workstream depends_on list is intentionally empty."
            ],
        },
        "validation": validation,
    })
}

/// Human-readable rendering that preserves the same bounded target details as
/// the JSON contract without making callers parse a second evidence shape.
pub fn pretty(brief: &Value) -> String {
    let mut output = String::new();
    output.push_str("execution brief v1\n");
    output.push_str(&format!(
        "task: {}\n",
        brief.get("task").and_then(Value::as_str).unwrap_or("")
    ));
    if let Some(workstreams) = brief.get("workstreams").and_then(Value::as_array) {
        for workstream in workstreams {
            output.push_str(&format!(
                "\n{} [{} / {}]\n",
                workstream.get("id").and_then(Value::as_str).unwrap_or("?"),
                workstream
                    .get("tier")
                    .and_then(Value::as_str)
                    .unwrap_or("?"),
                workstream
                    .get("ownership")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            ));
            if let Some(targets) = workstream.get("targets").and_then(Value::as_array) {
                for target in targets {
                    output.push_str(&format!(
                        "  {}\n",
                        target.get("path").and_then(Value::as_str).unwrap_or("?")
                    ));
                    if let Some(symbols) = target.get("symbols").and_then(Value::as_array) {
                        for symbol in symbols {
                            output.push_str(&format!(
                                "    symbol: {} {}\n",
                                symbol.get("kind").and_then(Value::as_str).unwrap_or("?"),
                                symbol.get("name").and_then(Value::as_str).unwrap_or("?")
                            ));
                        }
                    }
                    if let Some(evidence) = target.get("evidence").and_then(Value::as_array) {
                        for item in evidence {
                            output.push_str(&format!(
                                "    evidence [{}:{}]: {}\n",
                                item.get("keyword").and_then(Value::as_str).unwrap_or("?"),
                                item.get("line").and_then(Value::as_u64).unwrap_or(0),
                                item.get("text").and_then(Value::as_str).unwrap_or("")
                            ));
                        }
                    }
                    if let Some(reasons) = target.get("reasons").and_then(Value::as_array) {
                        for reason in reasons.iter().filter_map(Value::as_str) {
                            output.push_str(&format!("    reason: {reason}\n"));
                        }
                    }
                }
            }
        }
    }
    if let Some(uncertainty) = brief.get("uncertainty") {
        output.push_str("\nuncertainty:\n");
        output.push_str("  closed_world: false\n  lower_bound: true\n");
        if let Some(caps) = uncertainty.get("caps").and_then(Value::as_array) {
            for cap in caps.iter().filter_map(Value::as_str) {
                output.push_str(&format!("  cap: {cap}\n"));
            }
        }
        output.push_str("  dependencies: unknown (depends_on is empty)\n");
    }
    if let Some(validation) = brief.get("validation").and_then(Value::as_array) {
        output.push_str("\nvalidation:\n");
        for item in validation.iter().filter_map(Value::as_str) {
            output.push_str(&format!("  - {item}\n"));
        }
    }
    output
}

fn collect_source_caps(data: &Value, caps: &mut BTreeSet<String>) {
    if let Some(source_caps) = data["envelope"]["caps"].as_array() {
        for cap in source_caps.iter().filter_map(Value::as_str) {
            let bounded = bounded_source_cap(cap, caps);
            caps.insert(bounded);
        }
    }
    if let Some(warnings) = data["warnings"].as_array() {
        for warning in warnings {
            if let Some(message) = warning["message"].as_str() {
                let bounded = bounded_source_cap(message, caps);
                caps.insert(bounded);
            }
        }
    }
    if let Some(closed_world) = data["closed_world"].as_str() {
        let bounded = bounded_source_cap(closed_world, caps);
        caps.insert(bounded);
    }
}

fn bounded_source_cap(text: &str, caps: &mut BTreeSet<String>) -> String {
    bounded_text(text, "source cap", MAX_TEXT_CHARS, caps)
}

fn target_projection(target: &Value, caps: &mut BTreeSet<String>) -> Value {
    let symbols = target["symbols"]
        .as_array()
        .map(|items| {
            cap_items(items, MAX_SYMBOLS_PER_TARGET, "symbols", caps);
            bounded_items(items, MAX_SYMBOLS_PER_TARGET, |symbol| {
                json!({
                    "kind": symbol["kind"].as_str().unwrap_or("?"),
                    "name": symbol["name"].as_str().unwrap_or("?"),
                    "uid": symbol["uid"].as_str().unwrap_or("?"),
                    "line": symbol["line"].as_u64().unwrap_or(0),
                })
            })
        })
        .unwrap_or_default();
    let evidence = target["evidence"]
        .as_array()
        .map(|items| {
            cap_items(items, MAX_EVIDENCE_PER_TARGET, "evidence", caps);
            bounded_items(items, MAX_EVIDENCE_PER_TARGET, |item| {
                json!({
                    "keyword": item["keyword"].as_str().unwrap_or("?"),
                    "line": item["line"].as_u64().unwrap_or(0),
                    "text": bounded_text(
                        item["text"].as_str().unwrap_or(""),
                        "evidence text",
                        MAX_TEXT_CHARS,
                        caps,
                    ),
                })
            })
        })
        .unwrap_or_default();
    let reasons = target["reasons"]
        .as_array()
        .map(|items| {
            cap_items(items, MAX_EVIDENCE_PER_TARGET, "reasons", caps);
            bounded_items(items, MAX_EVIDENCE_PER_TARGET, |reason| {
                Value::from(bounded_text(
                    reason.as_str().unwrap_or(""),
                    "reason",
                    MAX_TEXT_CHARS,
                    caps,
                ))
            })
        })
        .unwrap_or_default();
    json!({
        "path": target_path_value(target).unwrap_or("?"),
        "symbols": symbols,
        "evidence": evidence,
        "reasons": reasons,
    })
}

fn bounded_items<T>(items: &[Value], limit: usize, map: impl FnMut(&Value) -> T) -> Vec<T> {
    items.iter().take(limit).map(map).collect()
}

fn cap_items(items: &[Value], limit: usize, label: &str, caps: &mut BTreeSet<String>) {
    if items.len() > limit {
        caps.insert(format!("{label} per target capped at {limit}"));
    }
}

fn bounded_text(text: &str, label: &str, limit: usize, caps: &mut BTreeSet<String>) -> String {
    let mut chars = text.chars();
    let bounded: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        caps.insert(format!("{label} capped at {limit} characters"));
    }
    bounded
}

fn repository_area(path: &str) -> String {
    path.rsplit_once('/').map_or_else(
        || ".".to_string(),
        |(parent, _)| {
            if parent.is_empty() {
                ".".to_string()
            } else {
                parent.to_string()
            }
        },
    )
}

fn target_path(target: &Value) -> &str {
    target["path"].as_str().unwrap_or("?")
}

fn target_path_value(target: &Value) -> Option<&str> {
    target["path"].as_str().filter(|path| !path.is_empty())
}

fn tier_rank(target: &Value) -> u8 {
    match target["tier"].as_str() {
        Some("P0") => 0,
        Some("P1") => 1,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(tier: &str, path: &str) -> Value {
        json!({"tier": tier, "path": path})
    }

    fn brief_of(targets: Vec<Value>) -> Value {
        from_scope_task("task", &json!({"targets": targets}))
    }

    fn caps_of(brief: &Value) -> Vec<String> {
        brief["uncertainty"]["caps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|cap| cap.as_str().unwrap().to_string())
            .collect()
    }

    fn target_count(brief: &Value) -> usize {
        brief["workstreams"]
            .as_array()
            .unwrap()
            .iter()
            .map(|workstream| workstream["targets"].as_array().unwrap().len())
            .sum()
    }

    #[test]
    fn targets_are_capped_at_one_hundred_and_the_cap_is_disclosed() {
        let make = |n: usize| {
            (0..n)
                .map(|i| target("P1", &format!("src/f{i:03}.rs")))
                .collect()
        };
        let exact = brief_of(make(MAX_TARGETS));
        assert_eq!(target_count(&exact), 100);
        assert!(caps_of(&exact).is_empty(), "{:?}", caps_of(&exact));
        let over = brief_of(make(MAX_TARGETS + 1));
        assert_eq!(target_count(&over), 100);
        assert_eq!(caps_of(&over), ["execution brief targets capped at 100"]);
    }

    #[test]
    fn a_workstream_cap_drops_read_context_before_any_write_workstream() {
        // 64 P1 areas named before the one P0 area: an area-ordered cap
        // would keep them all and lose the only write workstream.
        let mut targets: Vec<Value> = (0..MAX_WORKSTREAMS)
            .map(|i| target("P1", &format!("a{i:02}/x.rs")))
            .collect();
        targets.push(target("P0", "zz/edit.rs"));
        let brief = brief_of(targets);
        let workstreams = brief["workstreams"].as_array().unwrap();
        assert_eq!(workstreams.len(), 64);
        assert_eq!(workstreams[0]["id"], "workstream:zz:P0");
        assert_eq!(workstreams[0]["ownership"], "write");
        assert_eq!(
            caps_of(&brief),
            ["execution brief workstreams capped at 64"]
        );
        assert!(
            !brief["validation"].to_string().contains("No P0 target"),
            "{}",
            brief["validation"]
        );

        let exact: Vec<Value> = (0..MAX_WORKSTREAMS)
            .map(|i| target("P1", &format!("a{i:02}/x.rs")))
            .collect();
        let brief = brief_of(exact);
        assert_eq!(brief["workstreams"].as_array().unwrap().len(), 64);
        assert!(caps_of(&brief).is_empty());
    }

    #[test]
    fn per_target_lists_and_texts_are_cut_exactly_at_their_limits() {
        let symbols = |n: usize| -> Vec<Value> {
            (0..n)
                .map(|i| json!({"kind": "fn", "name": format!("s{i}")}))
                .collect()
        };
        let at_limit = brief_of(vec![json!({
            "tier": "P0", "path": "a.rs",
            "symbols": symbols(MAX_SYMBOLS_PER_TARGET),
            "evidence": [{"keyword": "k", "line": 1, "text": "x".repeat(MAX_TEXT_CHARS)}],
        })]);
        let projected = &at_limit["workstreams"][0]["targets"][0];
        assert_eq!(projected["symbols"].as_array().unwrap().len(), 32);
        assert_eq!(
            projected["evidence"][0]["text"].as_str().unwrap().len(),
            320
        );
        assert!(caps_of(&at_limit).is_empty(), "{:?}", caps_of(&at_limit));

        let over = brief_of(vec![json!({
            "tier": "P0", "path": "a.rs",
            "symbols": symbols(MAX_SYMBOLS_PER_TARGET + 1),
            "evidence": [{"keyword": "k", "line": 1, "text": "x".repeat(MAX_TEXT_CHARS + 1)}],
        })]);
        let projected = &over["workstreams"][0]["targets"][0];
        assert_eq!(projected["symbols"].as_array().unwrap().len(), 32);
        assert_eq!(
            projected["evidence"][0]["text"].as_str().unwrap().len(),
            320
        );
        assert_eq!(
            caps_of(&over),
            [
                "evidence text capped at 320 characters",
                "symbols per target capped at 32",
            ]
        );
    }

    #[test]
    fn more_than_thirty_two_caps_keep_thirty_two_plus_a_marker() {
        let source = |n: usize| -> Value {
            json!({
                "targets": [],
                "envelope": {"caps": (0..n).map(|i| format!("cap {i:02}")).collect::<Vec<_>>()},
            })
        };
        // The empty targets array adds no cap of its own.
        let exact = from_scope_task("t", &source(MAX_CAPS));
        assert_eq!(caps_of(&exact).len(), 32);
        let over = from_scope_task("t", &source(MAX_CAPS + 1));
        let caps = caps_of(&over);
        assert_eq!(caps.len(), 33);
        assert_eq!(caps[32], "execution brief caps capped at 32");
    }

    #[test]
    fn the_pretty_brief_carries_each_targets_reasons() {
        let brief = brief_of(vec![json!({
            "tier": "P0", "path": "src/login.rs", "reasons": ["defines login_user"],
        })]);
        let text = pretty(&brief);
        assert!(
            text.contains("  src/login.rs\n    reason: defines login_user\n"),
            "{text}"
        );
    }
}
