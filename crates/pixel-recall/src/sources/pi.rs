//! Pi agent adapter: `~/.pi/agent/sessions/<encoded-cwd>/*.jsonl`.
//! Records are flat `{type, timestamp, ...}` lines — `session` carries the
//! meta (id, cwd); `message` wraps `{role, content:[parts]}` where parts are
//! `text` | `thinking` | `toolCall`; `custom_message` carries extension
//! injections (fanout digests) worth indexing; `custom`/`model_change`/
//! `thinking_level_change` are markers with no recall value.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;

use serde_json::Value;

use crate::intent::classify_user_text;
use crate::model::{
    IntentSource, Role, TOOL_INPUT_CAP, TOOL_RESULT_CAP, TsSource, UnifiedSession, UnifiedTurn,
    cap_text, parse_iso_ms,
};
use crate::sources::{
    Change, IngestError, ParseOutput, ParsedSession, SessionOp, SourceAdapter, SourceUnit,
    file_tail_hash,
};
use crate::store::IngestState;

pub struct Adapter {
    sessions_dir: PathBuf,
}

impl Adapter {
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        Self {
            sessions_dir: PathBuf::from(&home).join(".pi/agent/sessions"),
        }
    }
}

impl Default for Adapter {
    fn default() -> Self {
        Self::new()
    }
}

fn unit_for(path: PathBuf) -> Option<SourceUnit> {
    let meta = fs::metadata(&path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some(SourceUnit {
        unit_key: path.to_string_lossy().to_string(),
        size: meta.len(),
        mtime_ms,
        path,
    })
}

fn collect(dir: &PathBuf, units: &mut Vec<SourceUnit>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, units);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            units.extend(unit_for(path));
        }
    }
}

impl SourceAdapter for Adapter {
    fn agent(&self) -> &'static str {
        "pi"
    }

    fn discover(&self) -> Result<Vec<SourceUnit>, IngestError> {
        let mut units = Vec::new();
        collect(&self.sessions_dir, &mut units);
        Ok(units)
    }

    fn parse(
        &self,
        unit: &SourceUnit,
        change: Change,
        _state: Option<&IngestState>,
    ) -> Result<ParseOutput, IngestError> {
        let start = match change {
            Change::Appended { from } => from,
            _ => 0,
        };
        let mut file = File::open(&unit.path)?;
        if start > 0 {
            file.seek(SeekFrom::Start(start))?;
        }
        let mut reader = BufReader::new(file);

        let stem = unit
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();

        let mut session = UnifiedSession {
            agent: "pi",
            source_session_id: stem.clone(),
            source_path: unit.path.to_string_lossy().to_string(),
            cwd: None,
            git_branch: None,
            title: None,
            ts_source: TsSource::Iso,
            is_subagent: false,
            parent_source_session_id: None,
        };

        let mut turns: Vec<UnifiedTurn> = Vec::new();
        let mut meta_seen = false;
        // On an appended resume the `session` record sits before the resume
        // offset — recover identity from the file head.
        if start > 0 {
            apply_head_meta(&unit.path, &mut session, &mut meta_seen);
        }
        let mut offset = start;
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break;
            }
            if !line.ends_with('\n') {
                break;
            }
            let line_start = offset;
            offset += n as u64;
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            extract_record(
                &record,
                line_start,
                n as u64,
                &mut session,
                &mut turns,
                &mut meta_seen,
            );
        }

        let has_conversation =
            !turns.is_empty() || session.cwd.is_some() || matches!(change, Change::Appended { .. });
        let op = match change {
            Change::Appended { .. } => SessionOp::Append,
            _ => SessionOp::Replace,
        };
        Ok(ParseOutput {
            sessions: if has_conversation {
                vec![ParsedSession { op, session, turns }]
            } else {
                Vec::new()
            },
            consumed_bytes: offset,
            cursor: None,
        })
    }

    fn make_cursor(&self, unit: &SourceUnit, consumed: u64) -> Option<String> {
        file_tail_hash(&unit.path, consumed)
    }

    fn append_valid(&self, unit: &SourceUnit, state: &IngestState) -> bool {
        let Some(expected) = state.cursor.as_deref() else {
            return true;
        };
        file_tail_hash(&unit.path, state.bytes_ingested as u64).as_deref() == Some(expected)
    }
}

fn apply_head_meta(path: &std::path::Path, session: &mut UnifiedSession, meta_seen: &mut bool) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let mut reader = BufReader::new(file);
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() {
        return;
    }
    let Ok(record) = serde_json::from_str::<Value>(&first) else {
        return;
    };
    let mut no_turns = Vec::new();
    extract_record(&record, 0, 0, session, &mut no_turns, meta_seen);
}

fn extract_record(
    record: &Value,
    byte_start: u64,
    byte_len: u64,
    session: &mut UnifiedSession,
    turns: &mut Vec<UnifiedTurn>,
    meta_seen: &mut bool,
) {
    let rtype = record.get("type").and_then(Value::as_str).unwrap_or("");
    let ts = record
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_iso_ms);

    let mut push = |role: Role, text: String, truncated: bool, intent: Option<IntentSource>| {
        if text.trim().is_empty() {
            return;
        }
        turns.push(UnifiedTurn {
            role,
            intent_source: intent,
            ts,
            text,
            truncated,
            source_byte_start: Some(byte_start),
            source_byte_len: Some(byte_len),
        });
    };

    match rtype {
        "session" => {
            if *meta_seen {
                return;
            }
            *meta_seen = true;
            if let Some(id) = record.get("id").and_then(Value::as_str) {
                session.source_session_id = id.to_string();
            }
            if session.cwd.is_none() {
                session.cwd = record
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }
        "message" => {
            let msg = record.get("message").unwrap_or(&Value::Null);
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
            match role {
                "user" => {
                    let text = join_parts(msg.get("content"), "text", "text");
                    if text.trim().is_empty() {
                        return;
                    }
                    let intent = classify_user_text(&text);
                    push(Role::User, text, false, Some(intent));
                }
                "assistant" => {
                    let text = join_parts(msg.get("content"), "text", "text");
                    if !text.trim().is_empty() {
                        push(Role::Assistant, text, false, None);
                    }
                    // thinking parts carry ANSI styling, no recall value.
                    if let Some(parts) = msg.get("content").and_then(Value::as_array) {
                        for part in parts {
                            if part.get("type").and_then(Value::as_str) == Some("toolCall") {
                                let name = part
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown");
                                let args = part
                                    .get("arguments")
                                    .map(|a| {
                                        if a.is_string() {
                                            a.as_str().unwrap_or_default().to_string()
                                        } else {
                                            a.to_string()
                                        }
                                    })
                                    .unwrap_or_default();
                                let (capped, truncated) = cap_text(&args, TOOL_INPUT_CAP);
                                push(
                                    Role::Assistant,
                                    format!("\u{22ee}tool {name} {capped}"),
                                    truncated,
                                    None,
                                );
                            }
                        }
                    }
                }
                "toolResult" => {
                    let text = join_parts(msg.get("content"), "text", "text");
                    if text.trim().is_empty() {
                        return;
                    }
                    let (capped, truncated) = cap_text(&text, TOOL_RESULT_CAP);
                    push(Role::Tool, capped, truncated, None);
                }
                _ => {}
            }
        }
        // Extension-injected messages (fanout digests, wake events) — harness
        // text, classified as orchestrator so recall can filter it.
        "custom_message" => {
            let text = record
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if text.trim().is_empty() {
                return;
            }
            let (capped, truncated) = cap_text(text, TOOL_RESULT_CAP);
            push(
                Role::User,
                capped,
                truncated,
                Some(IntentSource::Orchestrator),
            );
        }
        // custom (steering markers), model_change, thinking_level_change,
        // compactions — no recall value.
        _ => {}
    }
}

/// Join `content` parts where `part.type == want`, reading `field` from each.
fn join_parts(content: Option<&Value>, want: &str, field: &str) -> String {
    let mut out = String::new();
    if let Some(Value::Array(parts)) = content {
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some(want)
                && let Some(t) = part.get(field).and_then(Value::as_str)
            {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
    }
    out
}
