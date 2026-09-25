//! `lifecycle.rs` — lifecycle of a path or token: first-seen, last-changed,
//! removed-in, present-at-HEAD.
//!
//! Path lifecycle reads `file_changes` directly. Token lifecycle reads verified
//! diff hunks (added/removed) via the trigram index (`text_index`).

use serde::{Deserialize, Serialize};

use crate::search::relevance_of;
use crate::store::{CommitRef, FactsStore, Result, subject_of};
use crate::text_index::matching_hunks;

/// A lifecycle summary for a path or token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lifecycle {
    pub what: String,
    pub first_seen: Option<CommitRef>,
    pub last_changed: Option<CommitRef>,
    pub removed_in: Option<CommitRef>,
    pub present_at_head: bool,
    pub total_touches: u64,
}

fn reference(oid: &str, at: &str, message: &str) -> CommitRef {
    CommitRef {
        oid: crate::store::short_oid(oid),
        at: at.to_string(),
        subject: subject_of(message).to_string(),
    }
}

impl FactsStore {
    /// Lifecycle of a path: first-seen / last-changed / removed-in / present.
    pub fn path_lifecycle(&self, path: &str) -> Result<Option<Lifecycle>> {
        let mut stmt = self.conn().prepare(
            "SELECT c.oid, c.committed_at, c.message, f.status
             FROM file_changes f
             JOIN commits c ON c.id = f.commit_id
             WHERE f.path = ?1 OR f.old_path = ?1
             ORDER BY c.committed_at, c.id",
        )?;
        let rows = stmt.query_map([path], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut touches: Vec<(String, String, String, String)> = Vec::new();
        for row in rows {
            touches.push(row?);
        }
        if touches.is_empty() {
            return Ok(None);
        }
        let first = touches.first().cloned();
        let last = touches.last().cloned();
        // removed-in = the newest touch whose status is D AND it is also the
        // last touch overall.
        let mut removed_in: Option<(String, String, String)> = None;
        if let Some(last) = &last
            && last.3 == "D"
        {
            removed_in = Some((last.0.clone(), last.1.clone(), last.2.clone()));
        }
        // present-at-head: check whether the blob exists at HEAD.
        let present = self
            .runner()
            .run(&["cat-file", "-e", &format!("HEAD:{path}")])
            .is_ok();
        Ok(Some(Lifecycle {
            what: path.to_string(),
            first_seen: first.map(|(o, a, m, _)| reference(&o, &a, &m)),
            last_changed: last.map(|(o, a, m, _)| reference(&o, &a, &m)),
            removed_in: removed_in.map(|(o, a, m)| reference(&o, &a, &m)),
            present_at_head: present,
            total_touches: touches.len() as u64,
        }))
    }

    /// Lifecycle of a token (substring): uses verified diff hunks.
    pub fn token_lifecycle(&self, token: &str) -> Result<Option<Lifecycle>> {
        let units = vec![token.to_string()];
        // Candidate hunks via trigrams, verified against text. Every
        // candidate: first-seen needs the oldest touch, not the newest few.
        let Some(ids) = matching_hunks(self.conn(), &units, usize::MAX)? else {
            // Token too short for trigram; fall back to a direct scan.
            return self.token_lifecycle_scan(token);
        };
        let mut rows: Vec<(String, String, String)> = Vec::new();
        for id in ids {
            if let Ok((oid, at, msg, added, removed)) = self.conn.query_row(
                "SELECT c.oid, c.committed_at, c.message, h.added, h.removed
                     FROM hunks h JOIN commits c ON c.id = h.commit_id
                     WHERE h.id = ?1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                },
            ) {
                let text = format!("{added}\n{removed}");
                let rel = relevance_of(&text, &units);
                if rel == 0 {
                    continue;
                }
                rows.push((oid, at, msg));
            }
        }
        if rows.is_empty() {
            return Ok(None);
        }
        rows.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        let present = self
            .runner()
            .run(&[
                "grep",
                "-I",
                "-l",
                "--fixed-strings",
                "--end-of-options",
                token,
                "HEAD",
            ])
            .is_ok();
        let total = rows.len() as u64;
        let first = rows.first().cloned();
        let last = rows.last().cloned();
        Ok(Some(Lifecycle {
            what: token.to_string(),
            first_seen: first.map(|(o, a, m)| reference(&o, &a, &m)),
            last_changed: last.map(|(o, a, m)| reference(&o, &a, &m)),
            removed_in: None,
            present_at_head: present,
            total_touches: total,
        }))
    }

    fn token_lifecycle_scan(&self, token: &str) -> Result<Option<Lifecycle>> {
        let mut stmt = self.conn().prepare(
            "SELECT c.oid, c.committed_at, c.message, h.added, h.removed
             FROM hunks h JOIN commits c ON c.id = h.commit_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut found: Vec<(String, String, String)> = Vec::new();
        for row in rows {
            let (oid, at, msg, added, removed) = row?;
            if added.contains(token) || removed.contains(token) {
                found.push((oid, at, msg));
            }
        }
        if found.is_empty() {
            return Ok(None);
        }
        let present = self
            .runner()
            .run(&[
                "grep",
                "-I",
                "-l",
                "--fixed-strings",
                "--end-of-options",
                token,
                "HEAD",
            ])
            .is_ok();
        let total = found.len() as u64;
        let first = found.first().cloned();
        let last = found.last().cloned();
        Ok(Some(Lifecycle {
            what: token.to_string(),
            first_seen: first.map(|(o, a, m)| reference(&o, &a, &m)),
            last_changed: last.map(|(o, a, m)| reference(&o, &a, &m)),
            removed_in: None,
            present_at_head: present,
            total_touches: total,
        }))
    }
}

#[cfg(test)]
mod tests {
    use crate::FactsStore;
    use crate::ingest::tests::ingest_within;
    use crate::store::short_oid;
    use crate::testutil::{commit_at, days_ago, init_repo};

    /// Token lifecycle runs through the trigram index (its SQL once named a
    /// table alias that did not exist, so every token of three characters
    /// or more failed): first-seen is the oldest touch, last-changed the
    /// newest, and a hunk holding the trigrams apart is no touch.
    #[test]
    fn token_lifecycle_spans_the_oldest_and_newest_verified_touches() {
        let dir = init_repo();
        let root = dir.path();
        let added = commit_at(
            root,
            &[("a.rs", b"let retry_budget = 3;\n")],
            "add",
            days_ago(3),
        );
        commit_at(root, &[("b.rs", b"retry\nbudget\n")], "apart", days_ago(2));
        let removed = commit_at(root, &[("a.rs", b"let x = 3;\n")], "drop", days_ago(1));
        let mut store = FactsStore::open(root).unwrap();
        ingest_within(&mut store);
        let life = store
            .token_lifecycle("retry_budget")
            .unwrap()
            .expect("touched");
        assert_eq!(life.what, "retry_budget");
        assert_eq!(life.total_touches, 2, "{life:?}");
        assert_eq!(life.first_seen.unwrap().oid, short_oid(&added));
        assert_eq!(life.last_changed.unwrap().oid, short_oid(&removed));
        assert!(!life.present_at_head);
        assert_eq!(store.token_lifecycle("never_written").unwrap(), None);
    }
}
