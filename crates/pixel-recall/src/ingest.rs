//! Ingest orchestration: discover → classify → parse → store, one
//! transaction per session, resumable by construction.

use crate::sources::{Change, IngestError, SourceAdapter};
use crate::store::{IngestState, RecallStore};

#[derive(Debug, Default, Clone)]
pub struct IngestReport {
    pub agent: String,
    pub units_seen: usize,
    pub units_new: usize,
    pub units_appended: usize,
    pub units_rewritten: usize,
    pub units_unchanged: usize,
    pub sessions_written: usize,
    pub turns_written: usize,
    pub parse_errors: usize,
    pub elapsed_ms: u128,
}

pub fn ingest_source(
    store: &mut RecallStore,
    adapter: &dyn SourceAdapter,
) -> Result<IngestReport, IngestError> {
    let started = std::time::Instant::now();
    let agent = adapter.agent();
    let mut report = IngestReport {
        agent: agent.to_string(),
        ..Default::default()
    };

    let units = adapter.discover()?;
    report.units_seen = units.len();

    for unit in &units {
        let state = store.ingest_state(agent, &unit.unit_key)?;
        let mut change = adapter.classify(unit, state.as_ref());
        if let (Change::Appended { .. }, Some(st)) = (&change, state.as_ref())
            && !adapter.append_valid(unit, st)
        {
            // The file grew but its prefix changed — rewritten in place.
            change = Change::Rewritten;
        }
        match change {
            Change::Unchanged => {
                report.units_unchanged += 1;
                continue;
            }
            Change::New => report.units_new += 1,
            Change::Appended { .. } => report.units_appended += 1,
            Change::Rewritten => report.units_rewritten += 1,
        }

        match adapter.parse(unit, change, state.as_ref()) {
            Ok(parsed) => {
                let st = IngestState {
                    file_size: unit.size as i64,
                    mtime_ms: unit.mtime_ms,
                    bytes_ingested: parsed.consumed_bytes as i64,
                    cursor: parsed
                        .cursor
                        .or_else(|| adapter.make_cursor(unit, parsed.consumed_bytes)),
                };
                // The unit's resume state is committed only WITH THE LAST
                // session (or a final touch): an interrupted multi-session
                // unit must re-parse from the old cursor, never record
                // completion it didn't reach. Session writes are
                // idempotent (replace/append by source id), so re-parsing
                // is safe; skipping is not.
                let stale_st = state.clone().unwrap_or(IngestState {
                    file_size: -1,
                    mtime_ms: -1,
                    bytes_ingested: match &change {
                        Change::Appended { from } => *from as i64,
                        _ => 0,
                    },
                    cursor: None,
                });
                let last = parsed.sessions.len().saturating_sub(1);
                for (i, ps) in parsed.sessions.iter().enumerate() {
                    let st_for_this = if i == last { &st } else { &stale_st };
                    match ps.op {
                        crate::sources::SessionOp::Append => {
                            store.append_session(
                                &ps.session,
                                &ps.turns,
                                &unit.unit_key,
                                st_for_this,
                            )?;
                        }
                        crate::sources::SessionOp::Replace => {
                            store.replace_session(
                                &ps.session,
                                &ps.turns,
                                &unit.unit_key,
                                st_for_this,
                            )?;
                        }
                    }
                    report.sessions_written += 1;
                    report.turns_written += ps.turns.len();
                }
                if parsed.sessions.is_empty() {
                    store.touch_state(agent, &unit.unit_key, &st)?;
                }
            }
            Err(_) => {
                report.parse_errors += 1;
            }
        }
    }

    store.link_subagents(agent)?;
    report.elapsed_ms = started.elapsed().as_millis();
    Ok(report)
}
