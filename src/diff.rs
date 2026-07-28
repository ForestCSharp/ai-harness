//! Line-by-line diffs for the writes and edits shown in the transcript.
//!
//! What this replaces: showing every old line as a removal and every new line as
//! an addition. For a six-line span with one line changed that renders as six
//! removals and six additions, which buries the one line worth looking at. A
//! real diff keeps the unchanged lines as context and marks only what moved.
//!
//! The result is bounded on purpose. It is stored in the transcript — and so in
//! the session file — rather than recomputed per frame, so an unbounded diff of
//! a large rewrite would be paid for on every save.

use serde::{Deserialize, Serialize};

/// Largest input this will diff. The LCS table is O(n·m), so a big rewrite is
/// refused rather than allowed to stall the event loop; the caller falls back to
/// showing a bounded preview of the new contents.
pub const MAX_DIFF_LINES: usize = 400;

/// Unchanged lines kept either side of a change.
const CONTEXT: usize = 2;

/// Most lines rendered before the remainder is collapsed.
const MAX_SHOWN: usize = 40;

/// Shortest run worth collapsing. Below this the "N unchanged lines" marker is
/// longer than the lines it stands in for, and hides them for nothing.
const MIN_ELIDE: usize = 2;

/// One line of a rendered diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Change {
    /// Present in both, shown for orientation.
    Context(String),
    Removed(String),
    Added(String),
    /// A run of lines collapsed away, and how many.
    Elided(usize),
}

impl Change {
    /// How many source lines this stands for, so a summary can count them.
    fn weight(&self) -> usize {
        match self {
            Self::Elided(n) => *n,
            _ => 1,
        }
    }
}

/// Added and removed line counts, for a diff's header.
pub fn summary(changes: &[Change]) -> (usize, usize) {
    let added = changes
        .iter()
        .filter(|c| matches!(c, Change::Added(_)))
        .count();
    let removed = changes
        .iter()
        .filter(|c| matches!(c, Change::Removed(_)))
        .count();
    (added, removed)
}

/// Diff `old` into `new`, trimmed to the interesting parts.
///
/// `None` when either side exceeds [`MAX_DIFF_LINES`].
pub fn lines(old: &str, new: &str) -> Option<Vec<Change>> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    if a.len() > MAX_DIFF_LINES || b.len() > MAX_DIFF_LINES {
        return None;
    }
    Some(cap(trim(walk(&a, &b))))
}

/// The full diff, before any trimming: a longest-common-subsequence table, then
/// a walk from the front choosing whichever side keeps the most in common.
fn walk(a: &[&str], b: &[&str]) -> Vec<Change> {
    let (n, m) = (a.len(), b.len());
    // common[i][j] is the length of the longest common subsequence of a[i..]
    // and b[j..]. Filled from the back so the walk below can read it forwards.
    let mut common = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            common[i][j] = if a[i] == b[j] {
                common[i + 1][j + 1] + 1
            } else {
                common[i + 1][j].max(common[i][j + 1])
            };
        }
    }

    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(Change::Context(a[i].to_string()));
            i += 1;
            j += 1;
        } else if common[i + 1][j] >= common[i][j + 1] {
            // Removing keeps at least as much in common as adding would. The
            // tie going to removal is what puts a changed line's `-` before
            // its `+`, which is how a diff is expected to read.
            out.push(Change::Removed(a[i].to_string()));
            i += 1;
        } else {
            out.push(Change::Added(b[j].to_string()));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|l| Change::Removed(l.to_string())));
    out.extend(b[j..].iter().map(|l| Change::Added(l.to_string())));
    out
}

/// Collapse unchanged lines more than [`CONTEXT`] away from any change.
fn trim(changes: Vec<Change>) -> Vec<Change> {
    let changed: Vec<usize> = changes
        .iter()
        .enumerate()
        .filter(|(_, c)| !matches!(c, Change::Context(_)))
        .map(|(i, _)| i)
        .collect();

    // Nothing moved. Saying "12 unchanged lines" is more honest than printing
    // the file back and letting the reader hunt for a change that is not there.
    if changed.is_empty() {
        return match changes.len() {
            0 => Vec::new(),
            n => vec![Change::Elided(n)],
        };
    }

    let mut keep = vec![false; changes.len()];
    for &at in &changed {
        let lo = at.saturating_sub(CONTEXT);
        let hi = (at + CONTEXT).min(changes.len() - 1);
        keep[lo..=hi].fill(true);
    }

    // Show through any gap too short to be worth a marker.
    let mut at = 0;
    while at < keep.len() {
        if keep[at] {
            at += 1;
            continue;
        }
        let end = (at..keep.len()).find(|&i| keep[i]).unwrap_or(keep.len());
        if end - at < MIN_ELIDE {
            keep[at..end].fill(true);
        }
        at = end.max(at + 1);
    }

    let mut out = Vec::new();
    let mut dropped = 0usize;
    for (i, change) in changes.into_iter().enumerate() {
        if keep[i] {
            if dropped > 0 {
                out.push(Change::Elided(std::mem::take(&mut dropped)));
            }
            out.push(change);
        } else {
            dropped += 1;
        }
    }
    if dropped > 0 {
        out.push(Change::Elided(dropped));
    }
    out
}

/// Bound the result, so a change on every other line cannot produce a diff as
/// long as the file.
fn cap(mut changes: Vec<Change>) -> Vec<Change> {
    if changes.len() <= MAX_SHOWN {
        return changes;
    }
    let tail = changes.split_off(MAX_SHOWN);
    changes.push(Change::Elided(tail.iter().map(Change::weight).sum()));
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(old: &str, new: &str) -> Vec<Change> {
        lines(old, new).expect("input is small enough to diff")
    }

    fn context(text: &str) -> Change {
        Change::Context(text.into())
    }
    fn removed(text: &str) -> Change {
        Change::Removed(text.into())
    }
    fn added(text: &str) -> Change {
        Change::Added(text.into())
    }

    #[test]
    fn identical_input_collapses_to_nothing_worth_showing() {
        assert_eq!(diff("a\nb\nc", "a\nb\nc"), vec![Change::Elided(3)]);
    }

    #[test]
    fn an_insertion_keeps_its_neighbours_as_context() {
        assert_eq!(
            diff("a\nb", "a\nnew\nb"),
            vec![context("a"), added("new"), context("b")]
        );
    }

    #[test]
    fn a_deletion_keeps_its_neighbours_as_context() {
        assert_eq!(
            diff("a\ngone\nb", "a\nb"),
            vec![context("a"), removed("gone"), context("b")]
        );
    }

    #[test]
    fn one_changed_line_does_not_rewrite_the_whole_block() {
        // The failure this module exists to fix: five lines in, one changed.
        assert_eq!(
            diff("a\nb\nOLD\nd\ne", "a\nb\nNEW\nd\ne"),
            vec![
                context("a"),
                context("b"),
                removed("OLD"),
                added("NEW"),
                context("d"),
                context("e"),
            ]
        );
    }

    #[test]
    fn distant_unchanged_lines_are_elided() {
        let old = (0..20)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let new = old.replace("\n10\n", "\nTEN\n");

        let changes = diff(&old, &new);
        assert_eq!(
            changes.first(),
            Some(&Change::Elided(8)),
            "the run before the change collapses: {changes:?}"
        );
        assert!(changes.contains(&removed("10")));
        assert!(changes.contains(&added("TEN")));
        assert!(
            changes.contains(&context("9")),
            "context is kept either side"
        );
        assert!(changes.contains(&context("11")));
        assert_eq!(
            changes.last(),
            Some(&Change::Elided(7)),
            "and the run after — lines 13..19: {changes:?}"
        );
    }

    #[test]
    fn a_gap_too_short_to_be_worth_a_marker_is_shown_instead() {
        // "⋯ 1 unchanged line(s)" is more text than the line it hides.
        let changes = diff("OLD\na\nb\nc", "NEW\na\nb\nc");
        assert_eq!(
            changes,
            vec![
                removed("OLD"),
                added("NEW"),
                context("a"),
                context("b"),
                context("c"),
            ],
            "the single trailing line should just be shown"
        );
        assert!(!changes.iter().any(|c| matches!(c, Change::Elided(_))));
    }

    #[test]
    fn a_diff_from_nothing_is_all_additions() {
        assert_eq!(diff("", "a\nb"), vec![added("a"), added("b")]);
    }

    #[test]
    fn a_diff_to_nothing_is_all_removals() {
        assert_eq!(diff("a\nb", ""), vec![removed("a"), removed("b")]);
    }

    #[test]
    fn oversize_input_is_refused_rather_than_diffed() {
        // The caller falls back to a preview; the point is not to build an
        // O(n·m) table on the event loop.
        let big = "x\n".repeat(MAX_DIFF_LINES + 1);
        assert_eq!(lines(&big, "y"), None);
        assert_eq!(lines("y", &big), None);
        assert!(lines(&"x\n".repeat(MAX_DIFF_LINES), "y").is_some());
    }

    #[test]
    fn a_long_diff_is_capped() {
        // Every other line changed, so trimming cannot help — the cap must.
        let old: String = (0..200).map(|i| format!("{i}\n")).collect();
        let new: String = (0..200)
            .map(|i| {
                if i % 2 == 0 {
                    format!("{i}\n")
                } else {
                    format!("x{i}\n")
                }
            })
            .collect();

        let changes = diff(&old, &new);
        assert_eq!(changes.len(), MAX_SHOWN + 1, "capped, plus the marker");
        assert!(matches!(changes.last(), Some(Change::Elided(_))));
    }

    #[test]
    fn the_summary_counts_both_sides() {
        assert_eq!(summary(&diff("a\nOLD\nb", "a\nNEW\nb")), (1, 1));
        assert_eq!(summary(&diff("a", "a\nb\nc")), (2, 0));
    }

    #[test]
    fn changes_round_trip_through_json() {
        // They are stored in the session file, so this has to hold.
        let changes = diff("a\nOLD\nb", "a\nNEW\nb");
        let json = serde_json::to_string(&changes).unwrap();
        assert_eq!(serde_json::from_str::<Vec<Change>>(&json).unwrap(), changes);
    }
}
