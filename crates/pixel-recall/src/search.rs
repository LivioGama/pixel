//! Lexical search over the turn corpus: trigram candidates from the
//! segments, authoritative regex verification against `turns.text`.
//!
//! Freshness: turns newer than the segments' high-water mark are always
//! candidates, so search never needs a rebuild to see new conversation.

use std::collections::HashSet;

use pixel_index::TrigramExtractor;
use pixel_index::plan::plan_pattern;
use pixel_index::posting::{GramQuery, resolve_query};
use regex::Regex;
use rusqlite::params_from_iter;

use crate::segment::SegmentSet;
use crate::store::RecallStore;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SearchFilters {
    pub agent: Option<String>,
    pub repo_prefix: Option<String>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub role: Option<String>,
    pub human_only: bool,
    pub session_id: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub turn_id: i64,
    pub session_id: i64,
    pub seq: i64,
    pub agent: String,
    pub source_session_id: String,
    pub cwd: Option<String>,
    pub role: String,
    pub ts: Option<i64>,
    pub ts_source: String,
    pub snippet: String,
    pub snippet_truncated: bool,
    pub turn_truncated: bool,
}

#[derive(Debug, Default)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub turns_considered: usize,
    /// True when the scan stopped early at the limit — more matches may
    /// exist beyond the returned page.
    pub truncated: bool,
}

const SNIPPET_RADIUS: usize = 120;
/// Above this candidate count, an ordered scan beats huge IN() fetches.
const CANDIDATE_FETCH_MAX: usize = 10_000;

pub fn search(
    store: &RecallStore,
    segments: &SegmentSet,
    pattern: &str,
    whole_word: bool,
    filters: &SearchFilters,
    offset: usize,
    limit: usize,
) -> Result<SearchResult, String> {
    let effective_pattern = if whole_word {
        format!(r"\b(?:{pattern})\b")
    } else {
        pattern.to_string()
    };
    let re = Regex::new(&effective_pattern).map_err(|e| format!("bad pattern: {e}"))?;

    // Candidate turn ids from the trigram segments. `None` = every turn is
    // a candidate (pattern had no required literals).
    let plan =
        plan_pattern(&effective_pattern, &TrigramExtractor).map_err(|e| format!("pattern: {e}"))?;
    let candidates: Option<HashSet<i64>> = match plan {
        GramQuery::All => None,
        plan => {
            let mut set = HashSet::new();
            for shard in segments.open_shards() {
                let ids = resolve_query(&plan, shard.file_count(), &|h| shard.postings(h));
                for local in ids {
                    if let Some(turn_id) = shard.path_of(local).and_then(|p| p.parse::<i64>().ok())
                    {
                        set.insert(turn_id);
                    }
                }
            }
            Some(set)
        }
    };
    let tail_floor = segments.manifest.last_turn_id;
    // Unindexed tail turns are always candidates; a huge tail (mass ingest
    // before segments catch up) must push us to the ordered-scan path, or
    // the id-fetch path degenerates into fetching the whole tail.
    let tail_count: i64 = store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM turns WHERE id > ?1",
            [tail_floor],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;

    // Small candidate sets: targeted fetch. Otherwise (or with no
    // narrowing): one ordered scan with SQL-side filters, early-stopped.
    let use_fetch = candidates
        .as_ref()
        .is_some_and(|c| c.len() + tail_count as usize <= CANDIDATE_FETCH_MAX);

    let mut result = SearchResult::default();
    let mut skipped = 0usize;

    let mut visit = |row: HitRow| -> bool {
        result.turns_considered += 1;
        let Some(m) = re.find(&row.text) else {
            return true;
        };
        if skipped < offset {
            skipped += 1;
            return true;
        }
        let (snippet, snippet_truncated) = snippet_around(&row.text, m.start(), m.end());
        result.hits.push(SearchHit {
            turn_id: row.turn_id,
            session_id: row.session_id,
            seq: row.seq,
            agent: row.agent,
            source_session_id: row.source_session_id,
            cwd: row.cwd,
            role: row.role,
            ts: row.ts,
            ts_source: row.ts_source,
            snippet,
            snippet_truncated,
            turn_truncated: row.turn_truncated,
        });
        result.hits.len() < limit
    };

    if use_fetch {
        let mut ids: Vec<i64> = candidates
            .as_ref()
            .unwrap()
            .iter()
            .copied()
            .filter(|id| *id <= tail_floor)
            .collect();
        // Tail turns (unindexed) join the candidate set unconditionally.
        ids.extend(tail_turn_ids(store, tail_floor)?);
        let mut rows = fetch_rows_by_ids(store, &ids, filters)?;
        rows.sort_by(|a, b| b.ts.cmp(&a.ts).then(b.turn_id.cmp(&a.turn_id)));
        for row in rows {
            if !visit(row) {
                result.truncated = true;
                break;
            }
        }
    } else {
        scan_ordered(store, filters, candidates.as_ref(), tail_floor, &mut |row| {
            visit(row)
        })
        .map(|stopped_early| result.truncated = stopped_early)?;
    }
    Ok(result)
}

struct HitRow {
    turn_id: i64,
    session_id: i64,
    seq: i64,
    agent: String,
    source_session_id: String,
    cwd: Option<String>,
    role: String,
    ts: Option<i64>,
    ts_source: String,
    text: String,
    turn_truncated: bool,
}

const ROW_SELECT: &str = "SELECT t.id, t.session_id, t.seq, s.agent, s.source_session_id,
    s.cwd, t.role, t.ts, s.ts_source, t.text, t.truncated
    FROM turns t JOIN sessions s ON s.id = t.session_id";

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<HitRow> {
    Ok(HitRow {
        turn_id: r.get(0)?,
        session_id: r.get(1)?,
        seq: r.get(2)?,
        agent: r.get(3)?,
        source_session_id: r.get(4)?,
        cwd: r.get(5)?,
        role: r.get(6)?,
        ts: r.get(7)?,
        ts_source: r.get(8)?,
        text: r.get(9)?,
        turn_truncated: r.get::<_, i64>(10)? != 0,
    })
}

pub(crate) fn filter_sql(
    filters: &SearchFilters,
    args: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
) -> String {
    let mut sql = String::new();
    if let Some(a) = &filters.agent {
        sql.push_str(" AND s.agent = ?");
        args.push(Box::new(a.clone()));
    }
    if let Some(r) = &filters.repo_prefix {
        sql.push_str(" AND s.cwd LIKE ? || '%'");
        args.push(Box::new(r.clone()));
    }
    if let Some(s) = filters.since_ms {
        sql.push_str(" AND t.ts >= ?");
        args.push(Box::new(s));
    }
    if let Some(u) = filters.until_ms {
        sql.push_str(" AND t.ts <= ?");
        args.push(Box::new(u));
    }
    if let Some(role) = &filters.role {
        sql.push_str(" AND t.role = ?");
        args.push(Box::new(role.clone()));
    }
    if filters.human_only {
        sql.push_str(" AND (t.role != 'user' OR t.intent_source = 'human')");
    }
    if let Some(sid) = filters.session_id {
        sql.push_str(" AND t.session_id = ?");
        args.push(Box::new(sid));
    }
    sql
}

fn tail_turn_ids(store: &RecallStore, floor: i64) -> Result<Vec<i64>, String> {
    let mut stmt = store
        .connection()
        .prepare_cached("SELECT id FROM turns WHERE id > ?1")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([floor], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<i64>, _>>().map_err(|e| e.to_string())
}

fn fetch_rows_by_ids(
    store: &RecallStore,
    ids: &[i64],
    filters: &SearchFilters,
) -> Result<Vec<HitRow>, String> {
    let mut rows = Vec::new();
    for chunk in ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut args: Vec<Box<dyn rusqlite::types::ToSql>> =
            chunk.iter().map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>).collect();
        let mut sql = format!("{ROW_SELECT} WHERE t.id IN ({placeholders})");
        sql.push_str(&filter_sql(filters, &mut args));
        let mut stmt = store
            .connection()
            .prepare(&sql)
            .map_err(|e| e.to_string())?;
        let mapped = stmt
            .query_map(params_from_iter(args.iter().map(|b| b.as_ref())), row_from)
            .map_err(|e| e.to_string())?;
        for r in mapped {
            rows.push(r.map_err(|e| e.to_string())?);
        }
    }
    Ok(rows)
}

/// Ordered ts-desc scan with SQL filters; `visit` returns false to stop.
/// Returns whether the scan stopped early.
fn scan_ordered(
    store: &RecallStore,
    filters: &SearchFilters,
    candidates: Option<&HashSet<i64>>,
    tail_floor: i64,
    visit: &mut dyn FnMut(HitRow) -> bool,
) -> Result<bool, String> {
    let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut sql = format!("{ROW_SELECT} WHERE 1=1");
    sql.push_str(&filter_sql(filters, &mut args));
    sql.push_str(" ORDER BY t.ts DESC NULLS LAST, t.id DESC");
    let mut stmt = store
        .connection()
        .prepare(&sql)
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params_from_iter(args.iter().map(|b| b.as_ref())), row_from)
        .map_err(|e| e.to_string())?;
    for r in rows {
        let row = r.map_err(|e| e.to_string())?;
        if let Some(set) = candidates
            && row.turn_id <= tail_floor
            && !set.contains(&row.turn_id)
        {
            continue;
        }
        if !visit(row) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Trigram-candidate count for a pattern — `None` when the pattern has no
/// required literals (every turn matches). Cheap: postings only, no SQL,
/// no verification. Used to skip uselessly common words in ask's lexical
/// channel.
pub fn candidate_count(segments: &SegmentSet, pattern: &str) -> Option<usize> {
    let plan = plan_pattern(pattern, &TrigramExtractor).ok()?;
    if matches!(plan, GramQuery::All) {
        return None;
    }
    let mut total = 0usize;
    for shard in segments.open_shards() {
        total += resolve_query(&plan, shard.file_count(), &|h| shard.postings(h)).len();
    }
    Some(total)
}

/// Compact one-line rendering of a hit, shared by CLI and daemon.
pub fn format_hit(h: &SearchHit) -> String {
    let ts = h
        .ts
        .map(crate::model::format_ms)
        .unwrap_or_else(|| "?".to_string());
    let cwd = h.cwd.as_deref().unwrap_or("-");
    format!(
        "{}:{} #{} t{} {} {} {} \"{}\"",
        h.agent,
        &h.source_session_id[..h.source_session_id.len().min(8)],
        h.session_id,
        h.seq,
        ts,
        cwd,
        h.role,
        h.snippet
    )
}

/// Count matching turns and the distinct sessions containing them —
/// unbounded (no limit/offset), used by the MAX TEST.
pub fn count_matches(
    store: &RecallStore,
    segments: &SegmentSet,
    pattern: &str,
    whole_word: bool,
    filters: &SearchFilters,
) -> Result<(usize, HashSet<i64>), String> {
    let mut turns = 0usize;
    let mut sessions: HashSet<i64> = HashSet::new();
    // usize::MAX limit: visit never stops early.
    let result = search(store, segments, pattern, whole_word, filters, 0, usize::MAX)?;
    for hit in result.hits {
        turns += 1;
        sessions.insert(hit.session_id);
    }
    debug_assert!(!result.truncated);
    Ok((turns, sessions))
}

/// ±`SNIPPET_RADIUS` chars around the first match, on char boundaries,
/// newlines flattened.
pub(crate) fn snippet_around(text: &str, m_start: usize, m_end: usize) -> (String, bool) {
    let mut start = m_start.saturating_sub(SNIPPET_RADIUS);
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (m_end + SNIPPET_RADIUS).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    let mut snippet = text[start..end].replace(['\n', '\r'], " ");
    let truncated = start > 0 || end < text.len();
    if start > 0 {
        snippet = format!("…{snippet}");
    }
    if end < text.len() {
        snippet.push('…');
    }
    (snippet, truncated)
}
