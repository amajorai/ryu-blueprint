//! Revision-to-revision block diff.
//!
//! # Why this is not just a set difference on ids
//!
//! Block ids are content hashes, which is what makes an annotation survive a revise
//! ([`crate::parse`] explains why). The flip side is that an *edited* block is, by
//! construction, a different id — so id-set arithmetic alone can only ever say
//! "removed, and also added", and a reviewer reading the diff of a plan whose third
//! paragraph was reworded would see the entire document as churn.
//!
//! So this runs in two passes:
//!
//! 1. **Identity.** A block whose id appears in both revisions is `same`. This is the
//!    pass that carries the meaning — it is exact, order-independent, and it is what
//!    lets a reader skip everything they have already read.
//! 2. **Pairing.** The leftovers are matched in order, by kind (and by heading level,
//!    because promoting `###` to `##` is a structural change worth seeing), and each
//!    match becomes `changed` carrying its `previous`. Unmatched new blocks are
//!    `added`; unmatched old ones are `removed`.
//!
//! The second pass is a heuristic and it is honest about it. The case worth knowing:
//! if a paragraph is *inserted* above a paragraph that was *edited*, the insertion
//! greedily takes the pairing and the diff reads "first paragraph changed, second
//! added" instead of "first added, second changed". Both describe the same document;
//! the alternative — a full Myers/LCS pass over blocks — buys a nicer story for that
//! case and costs an algorithm nobody can hand-check when it misbehaves. The behavior
//! is pinned by a test rather than left to be discovered.
//!
//! Removed blocks are placed where they used to be, not appended in a heap at the end:
//! a deletion between two surviving paragraphs reads as a deletion between those two
//! paragraphs.

use std::collections::HashMap;

use serde::Serialize;

use crate::model::{Block, BlockKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockChange {
    Added,
    Removed,
    Changed,
    Same,
}

impl BlockChange {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            BlockChange::Added => "added",
            BlockChange::Removed => "removed",
            BlockChange::Changed => "changed",
            BlockChange::Same => "same",
        }
    }
}

/// One row of the diff. For `removed`, `block` is the block that used to be there and
/// `previous` is absent — there is only one version of it, and duplicating it into
/// both fields would just invite the UI to render it twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockDiff {
    pub status: BlockChange,
    pub block: Block,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<Block>,
}

/// Classify every block of `to` against `from`, in reading order.
#[must_use]
pub fn diff_blocks(from: &[Block], to: &[Block]) -> Vec<BlockDiff> {
    // First occurrence wins: within one revision a repeated id cannot happen (the
    // occurrence suffix makes ids unique), but a hand-written revision file could
    // still carry one, and taking the first is the only stable answer.
    let mut from_by_id: HashMap<&str, usize> = HashMap::with_capacity(from.len());
    for (idx, block) in from.iter().enumerate() {
        from_by_id.entry(block.id.as_str()).or_insert(idx);
    }

    let mut consumed = vec![false; from.len()];
    let mut matched: Vec<Option<usize>> = vec![None; to.len()];
    let mut status: Vec<BlockChange> = vec![BlockChange::Added; to.len()];

    // Pass 1 — identity.
    for (ti, block) in to.iter().enumerate() {
        if let Some(&fi) = from_by_id.get(block.id.as_str()) {
            if !consumed[fi] {
                consumed[fi] = true;
                matched[ti] = Some(fi);
                status[ti] = BlockChange::Same;
            }
        }
    }

    // Pass 2 — pair the leftovers in order, by kind.
    let mut search_from = 0usize;
    for ti in 0..to.len() {
        if matched[ti].is_some() {
            continue;
        }
        let candidate =
            (search_from..from.len()).find(|fi| !consumed[*fi] && pairable(&from[*fi], &to[ti]));
        if let Some(fi) = candidate {
            consumed[fi] = true;
            matched[ti] = Some(fi);
            status[ti] = BlockChange::Changed;
            // Never pair a later `to` block with an earlier `from` block than one we
            // already used: that would report two edits as having swapped places.
            search_from = fi + 1;
        }
    }

    place(from, to, &matched, &status, &consumed)
}

/// Whether an old block can plausibly be the earlier version of a new one.
///
/// Kind must match, and for headings so must the level: a `##` that became a `###` is
/// a restructure, and calling it a text edit hides the part that matters.
fn pairable(old: &Block, new: &Block) -> bool {
    if old.kind != new.kind {
        return false;
    }
    if new.kind == BlockKind::Heading {
        return old.level == new.level;
    }
    true
}

/// Emit the rows in reading order, slotting each removed block back where it sat.
fn place(
    from: &[Block],
    to: &[Block],
    matched: &[Option<usize>],
    status: &[BlockChange],
    consumed: &[bool],
) -> Vec<BlockDiff> {
    // Where each surviving `from` block ended up, so a removal can be anchored just
    // after the last survivor that preceded it.
    let mut landed: Vec<Option<usize>> = vec![None; from.len()];
    for (ti, slot) in matched.iter().enumerate() {
        if let Some(fi) = slot {
            landed[*fi] = Some(ti);
        }
    }

    let mut removals: Vec<(usize, usize)> = Vec::new();
    let mut cursor = 0usize;
    for fi in 0..from.len() {
        match landed[fi] {
            Some(ti) => cursor = ti + 1,
            None if !consumed[fi] => removals.push((cursor, fi)),
            None => {}
        }
    }

    let mut out = Vec::with_capacity(to.len() + removals.len());
    let mut next_removal = 0usize;
    for ti in 0..to.len() {
        while next_removal < removals.len() && removals[next_removal].0 <= ti {
            out.push(BlockDiff {
                status: BlockChange::Removed,
                block: from[removals[next_removal].1].clone(),
                previous: None,
            });
            next_removal += 1;
        }
        out.push(BlockDiff {
            status: status[ti],
            block: to[ti].clone(),
            previous: matched[ti].map(|fi| from[fi].clone()).filter(|_| {
                // `same` blocks are byte-identical to their predecessor; carrying a
                // copy would double the payload of a diff whose whole point is that
                // most of it did not change.
                status[ti] == BlockChange::Changed
            }),
        });
    }
    while next_removal < removals.len() {
        out.push(BlockDiff {
            status: BlockChange::Removed,
            block: from[removals[next_removal].1].clone(),
            previous: None,
        });
        next_removal += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn rows(from: &str, to: &str) -> Vec<(BlockChange, String)> {
        let a = parse(from);
        let b = parse(to);
        diff_blocks(&a.blocks, &b.blocks)
            .into_iter()
            .map(|d| (d.status, d.block.text))
            .collect()
    }

    #[test]
    fn an_untouched_revision_is_entirely_same() {
        let md = "# Plan\n\nOne.\n\nTwo.\n";
        let all = rows(md, md);
        assert_eq!(all.len(), 3);
        assert!(all.iter().all(|(s, _)| *s == BlockChange::Same));
    }

    #[test]
    fn an_edited_paragraph_is_changed_and_carries_its_predecessor() {
        let a = parse("# Plan\n\nWe will use Postgres.\n");
        let b = parse("# Plan\n\nWe will use SQLite.\n");
        let diff = diff_blocks(&a.blocks, &b.blocks);
        assert_eq!(diff[0].status, BlockChange::Same);
        assert_eq!(diff[1].status, BlockChange::Changed);
        assert_eq!(
            diff[1].previous.as_ref().expect("previous").text,
            "We will use Postgres."
        );
        // `same` rows carry no previous: it would be a byte-for-byte copy.
        assert!(diff[0].previous.is_none());
    }

    #[test]
    fn an_appended_paragraph_is_added_and_nothing_else_moves() {
        assert_eq!(
            rows("One.\n", "One.\n\nTwo.\n"),
            vec![
                (BlockChange::Same, "One.".to_owned()),
                (BlockChange::Added, "Two.".to_owned()),
            ]
        );
    }

    #[test]
    fn a_deletion_is_reported_where_it_used_to_sit() {
        // The dropped paragraph must appear between its two surviving neighbours, not
        // in a pile at the end where a reader has to reconstruct where it was.
        assert_eq!(
            rows("One.\n\nTwo.\n\nThree.\n", "One.\n\nThree.\n"),
            vec![
                (BlockChange::Same, "One.".to_owned()),
                (BlockChange::Removed, "Two.".to_owned()),
                (BlockChange::Same, "Three.".to_owned()),
            ]
        );
    }

    #[test]
    fn a_trailing_deletion_lands_at_the_end() {
        assert_eq!(
            rows("One.\n\nTwo.\n", "One.\n"),
            vec![
                (BlockChange::Same, "One.".to_owned()),
                (BlockChange::Removed, "Two.".to_owned()),
            ]
        );
    }

    #[test]
    fn a_leading_deletion_lands_before_the_first_survivor() {
        assert_eq!(
            rows("One.\n\nTwo.\n", "Two.\n"),
            vec![
                (BlockChange::Removed, "One.".to_owned()),
                (BlockChange::Same, "Two.".to_owned()),
            ]
        );
    }

    #[test]
    fn reordering_blocks_is_all_same_because_ids_do_not_encode_position() {
        let all = rows("Alpha.\n\nBravo.\n", "Bravo.\n\nAlpha.\n");
        assert!(all.iter().all(|(s, _)| *s == BlockChange::Same), "{all:?}");
    }

    #[test]
    fn kinds_never_pair_across_each_other() {
        // A paragraph replaced by a code block is a removal plus an addition, not an
        // edit — they render differently and mean different things.
        let all = rows("Run the migration.\n", "```sh\nmake migrate\n```\n");
        assert_eq!(
            all,
            vec![
                (BlockChange::Removed, "Run the migration.".to_owned()),
                (BlockChange::Added, "make migrate".to_owned()),
            ]
        );
    }

    #[test]
    fn a_heading_that_changed_depth_is_not_a_text_edit() {
        let a = parse("## Deploy\n");
        let b = parse("### Deployment\n");
        let diff = diff_blocks(&a.blocks, &b.blocks);
        assert_eq!(diff[0].status, BlockChange::Removed);
        assert_eq!(diff[1].status, BlockChange::Added);

        // Same depth, reworded: that IS an edit.
        let c = parse("## Deployment\n");
        let diff = diff_blocks(&a.blocks, &c.blocks);
        assert_eq!(diff[0].status, BlockChange::Changed);
    }

    #[test]
    fn an_insertion_above_an_edit_greedily_takes_the_pairing() {
        // Documented heuristic, pinned so a future change to it is a deliberate one:
        // the reader sees "para 1 changed, para 2 added" rather than "para 1 added,
        // para 2 changed". Both are true descriptions of the same document.
        let all = rows("Old body.\n", "Brand new intro.\n\nNew body.\n");
        assert_eq!(
            all,
            vec![
                (BlockChange::Changed, "Brand new intro.".to_owned()),
                (BlockChange::Added, "New body.".to_owned()),
            ]
        );
    }

    #[test]
    fn pairing_never_runs_backwards() {
        // Both paragraphs were rewritten. The pairing must stay in order — reporting
        // the first new paragraph as an edit of the second old one would tell the
        // reviewer the plan's two halves swapped, which never happened.
        let a = parse("First old.\n\nSecond old.\n");
        let b = parse("First new.\n\nSecond new.\n");
        let diff = diff_blocks(&a.blocks, &b.blocks);
        assert_eq!(
            diff[0].previous.as_ref().expect("previous").text,
            "First old."
        );
        assert_eq!(
            diff[1].previous.as_ref().expect("previous").text,
            "Second old."
        );
    }

    #[test]
    fn diffing_against_nothing_is_all_added() {
        let all = rows("", "One.\n\nTwo.\n");
        assert!(all.iter().all(|(s, _)| *s == BlockChange::Added));
        let all = rows("One.\n", "");
        assert_eq!(all, vec![(BlockChange::Removed, "One.".to_owned())]);
        assert!(diff_blocks(&[], &[]).is_empty());
    }

    #[test]
    fn the_status_strings_are_the_ones_the_contract_names() {
        for status in [
            BlockChange::Added,
            BlockChange::Removed,
            BlockChange::Changed,
            BlockChange::Same,
        ] {
            assert_eq!(
                serde_json::to_value(status).expect("ok"),
                serde_json::json!(status.as_str())
            );
        }
    }
}
