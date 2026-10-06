//! The Activity list: a chosen time range of the logs, read once and then
//! filtered, searched and paged in memory, newest first.
use crate::logs::Entry;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Rows per page.
pub const PAGE: usize = 500;
/// Entries kept from one read (the newest). Paging past them reads the range
/// further back.
const CAPACITY: usize = 20_000;

/// What the Activity list shows: a filter and search within `[from, to)`
/// (epoch milliseconds).
pub struct Query {
    pub from: i64,
    pub to: i64,
    pub filter: String,
    pub search_mode: String,
    /// Lowercased search text; empty matches everything.
    pub needle: String,
}

/// A position in newest-first order; the next page holds older entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Cursor {
    pub time: i64,
    pub seq: u64,
}

pub fn cursor(entry: &Entry) -> Option<Cursor> {
    entry.time.map(|t| Cursor {
        time: t.timestamp_millis(),
        seq: entry.seq,
    })
}

/// The entries of `[from, to)` (epoch milliseconds) older than `begin`,
/// newest first.
pub struct Scan {
    path: PathBuf,
    from: i64,
    to: i64,
    begin: Option<Cursor>,
    entries: Vec<Entry>,
    /// The range has older entries than the last one kept.
    truncated: bool,
}

impl Scan {
    pub fn read(path: &Path, from: i64, to: i64, begin: Option<Cursor>) -> Result<Self, String> {
        let mut entries = Vec::new();
        let mut truncated = false;
        crate::traffic::for_each_entry(path, from, |entry| {
            let Some(key) = cursor(&entry) else {
                return;
            };
            if key.time < from || key.time >= to || begin.is_some_and(|begin| key >= begin) {
                return;
            }
            entries.push(entry);
            // Files are not read in time order; trim in batches.
            if entries.len() >= CAPACITY * 2 {
                truncated |= keep_newest(&mut entries, CAPACITY);
            }
        })?;
        truncated |= keep_newest(&mut entries, CAPACITY);
        Ok(Self {
            path: path.to_owned(),
            from,
            to,
            begin,
            entries,
            truncated,
        })
    }

    /// Whether this read can list the range of `path` after `after`.
    pub fn covers(&self, path: &Path, from: i64, to: i64, after: Option<Cursor>) -> bool {
        self.path == path
            && self.from == from
            && self.to == to
            && match (self.begin, after) {
                (None, _) => true,
                (Some(begin), Some(after)) => after <= begin,
                (Some(_), None) => false,
            }
    }

    /// Up to `limit` kept entries older than `after`, newest first, and where
    /// to continue reading when this read ended before `limit` were found.
    pub fn page(
        &self,
        after: Option<Cursor>,
        limit: usize,
        keep: impl Fn(&Entry) -> bool,
    ) -> (Vec<Entry>, Option<Cursor>) {
        let start = after.map_or(0, |after| {
            self.entries
                .partition_point(|e| cursor(e).is_some_and(|key| key >= after))
        });
        let rows: Vec<Entry> = self.entries[start..]
            .iter()
            .filter(|e| keep(e))
            .take(limit)
            .cloned()
            .collect();
        let resume = (rows.len() < limit && self.truncated)
            .then(|| self.entries.last().and_then(cursor))
            .flatten();
        (rows, resume)
    }
}

/// Sorts newest first, drops lines read twice (a rotation can race a read),
/// and keeps `limit`; returns whether any were dropped for the limit.
fn keep_newest(entries: &mut Vec<Entry>, limit: usize) -> bool {
    entries.sort_by_key(|e| std::cmp::Reverse(cursor(e)));
    entries.dedup_by_key(|e| cursor(e));
    let cut = entries.len() > limit;
    entries.truncate(limit);
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Local};

    fn write(path: &Path, now: chrono::DateTime<Local>, records: &[(i64, &str)]) {
        let lines: String = records
            .iter()
            .map(|(minutes_ago, id)| {
                let time = (now - Duration::minutes(*minutes_ago)).to_rfc3339();
                format!(
                    "{}\n",
                    serde_json::json!({"event":"request_finished","timestamp":time,"request_id":id})
                )
            })
            .collect();
        std::fs::write(path, lines).unwrap();
    }

    fn ids(rows: &[Entry]) -> Vec<&str> {
        rows.iter().map(|e| e.get("request_id").unwrap()).collect()
    }

    #[test]
    fn reads_only_the_range_newest_first_across_rotated_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let now = Local::now();
        write(&path, now, &[(10, "c"), (90, "old"), (20, "b")]);
        write(
            &dir.path().join("proxy.log.1"),
            now,
            &[(5, "late"), (30, "a"), (20, "b")],
        );
        let now = now.timestamp_millis();
        let scan = Scan::read(&path, now - 35 * 60_000, now - 7 * 60_000, None).unwrap();
        let (rows, resume) = scan.page(None, 10, |_| true);
        // The same line in both files is listed once.
        assert_eq!(ids(&rows), ["c", "b", "a"]);
        assert_eq!(resume, None);
        let (rows, _) = scan.page(cursor(&rows[0]), 1, |_| true);
        assert_eq!(ids(&rows), ["b"]);
        let (rows, _) = scan.page(None, 10, |e| e.get("request_id") != Some("b"));
        assert_eq!(ids(&rows), ["c", "a"]);
        assert!(scan.covers(&path, now - 35 * 60_000, now - 7 * 60_000, None));
        assert!(!scan.covers(&path, now - 36 * 60_000, now - 7 * 60_000, None));
    }

    #[test]
    fn a_read_kept_to_capacity_resumes_before_its_oldest_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let now = Local::now();
        let lines: String = (0..CAPACITY + 3)
            .map(|i| {
                let time = (now - Duration::seconds(30_000 - i as i64)).to_rfc3339();
                format!(
                    "{}\n",
                    serde_json::json!({"event":"x","timestamp":time,"n":i})
                )
            })
            .collect();
        std::fs::write(&path, lines).unwrap();
        let (from, to) = (now.timestamp_millis() - 40_000_000, now.timestamp_millis());
        let scan = Scan::read(&path, from, to, None).unwrap();
        let (rows, resume) = scan.page(None, CAPACITY + 10, |_| true);
        assert_eq!(rows.len(), CAPACITY);
        // The newest entries are kept; older ones follow.
        assert_eq!(rows[0].fields["n"].as_u64(), Some(CAPACITY as u64 + 2));
        assert_eq!(rows.last().unwrap().fields["n"].as_u64(), Some(3));
        let resume = resume.expect("older entries follow");
        assert_eq!(Some(resume), cursor(rows.last().unwrap()));
        let rest = Scan::read(&path, from, to, Some(resume)).unwrap();
        assert!(rest.covers(&path, from, to, Some(resume)));
        assert!(!rest.covers(&path, from, to, None));
        let (rows, resume) = rest.page(Some(resume), 10, |_| true);
        let numbers: Vec<_> = rows
            .iter()
            .map(|e| e.fields["n"].as_u64().unwrap())
            .collect();
        assert_eq!(numbers, [2, 1, 0]);
        assert_eq!(resume, None);
    }
}
