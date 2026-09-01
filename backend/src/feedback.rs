//! The verdict, rendered as text an agent reads.
//!
//! This is the return leg of the whole app, and it is plain text on purpose. The agent
//! that published the plan gets this string back from `plan_status` and has to act on
//! it without a human paraphrasing — so it has to be readable by a model with no
//! schema in front of it, and identical every time it is asked. It is generated here,
//! in Rust, from the stored annotations: no model, no template a UI could drift from,
//! no formatting decided at the call site.
//!
//! ```text
//! PLAN deploy-v2 rev 3: CHANGES REQUESTED
//! note: close the data-loss window first
//!
//! [blocker] step "Migrate schema" (s_migrate_schema)
//!   Back up the prod DB first.
//! [redline] block b_1f2e9a4c7b01_1 "run the migration in one shot"
//!   suggest: run it in two passes behind a flag
//! ```
//!
//! Ordering is severity first, then age, then id. An agent that reads only the first
//! few lines of a long review must read the blockers; and a stable tie-break means two
//! calls a second apart cannot return the same annotations in a different order, which
//! would read to the agent as new feedback.
//!
//! Resolved annotations are excluded. A reviewer who ticked something off is saying it
//! no longer needs doing, and re-serving it would have the agent redo settled work.

use crate::model::{Annotation, Decision, Plan, Revision, Target, Verdict};

/// How much of a block's text is quoted to identify it.
const EXCERPT_CHARS: usize = 60;

/// The annotations that still want something, newest last, most severe first.
///
/// Filtered to one revision: an annotation describes the text that existed when it was
/// written, so replaying a previous revision's marks against a rewritten plan would
/// have the agent chasing sentences that are gone.
#[must_use]
pub fn unresolved(annotations: &[Annotation], revision: u32) -> Vec<&Annotation> {
    let mut out: Vec<&Annotation> = annotations
        .iter()
        .filter(|a| a.revision == revision && !a.resolved)
        .collect();
    out.sort_by(|a, b| {
        a.kind
            .severity()
            .cmp(&b.kind.severity())
            .then(a.created_at.cmp(&b.created_at))
            .then(a.id.cmp(&b.id))
    });
    out
}

/// Render the feedback for one revision.
///
/// `verdict` is `None` while the plan is still waiting on a human — the text then says
/// so plainly rather than returning an empty string an agent might read as approval.
#[must_use]
pub fn render(
    plan: &Plan,
    revision: &Revision,
    annotations: &[Annotation],
    verdict: Option<&Verdict>,
) -> String {
    let marks = unresolved(annotations, revision.revision);
    let mut out = String::new();

    let headline = match verdict.map(|v| v.verdict) {
        Some(Decision::Approved) => "APPROVED",
        Some(Decision::ChangesRequested) => "CHANGES REQUESTED",
        None => "IN REVIEW",
    };
    out.push_str(&format!(
        "PLAN {} rev {}: {headline}\n",
        plan.id, revision.revision
    ));

    if let Some(note) = verdict.and_then(|v| v.note.as_deref()) {
        let note = note.trim();
        if !note.is_empty() {
            out.push_str(&format!("note: {note}\n"));
        }
    }

    // The one-line status when there is nothing to enumerate. Each of these says what
    // the agent should do next, because "no annotations" on its own is ambiguous
    // between "you are clear" and "nobody has looked yet".
    if marks.is_empty() {
        match verdict.map(|v| v.verdict) {
            Some(Decision::Approved) => {}
            Some(Decision::ChangesRequested) => out.push_str(
                "\nno annotations were left — ask the reviewer what to change before revising.\n",
            ),
            None => out.push_str("\nno verdict yet, and no annotations so far.\n"),
        }
        return out;
    }

    out.push('\n');
    match verdict.map(|v| v.verdict) {
        Some(Decision::Approved) => {
            out.push_str("advisory only — the plan was approved with these left unresolved:\n")
        }
        Some(Decision::ChangesRequested) => {}
        None => out.push_str("no verdict yet. annotations so far:\n"),
    }

    for mark in marks {
        out.push_str(&entry(mark, revision));
    }
    out
}

fn entry(mark: &Annotation, revision: &Revision) -> String {
    let mut line = format!("[{}] {}\n", mark.kind.as_str(), anchor(mark, revision));
    for body_line in mark.body.lines() {
        let trimmed = body_line.trim_end();
        if trimmed.is_empty() {
            line.push('\n');
        } else {
            line.push_str(&format!("  {trimmed}\n"));
        }
    }
    if let Some(suggestion) = mark.suggestion.as_deref() {
        let suggestion = suggestion.trim();
        if !suggestion.is_empty() {
            // Deliberately one line even for a multi-line suggestion: this is the
            // replacement text, and an agent that has to guess where the indentation
            // stops will paste the indentation too.
            line.push_str(&format!("  suggest: {}\n", suggestion.replace('\n', " ")));
        }
    }
    line
}

/// Describe what the annotation is pointing at, in terms the agent can act on: a step
/// by title *and* id, a block by id *and* the words it contains.
fn anchor(mark: &Annotation, revision: &Revision) -> String {
    match &mark.target {
        Target::Step { id } => match revision.steps.iter().find(|s| s.id == *id) {
            Some(step) => format!("step \"{}\" ({id})", step.title),
            // The step is gone from this revision. Say so instead of quoting a title
            // that no longer exists.
            None => format!("step ({id}, no longer in this revision)"),
        },
        Target::Block { id } => match revision.blocks.iter().find(|b| b.id == *id) {
            Some(block) => format!("block {id} \"{}\"", excerpt(&block.text)),
            None => format!("block ({id}, no longer in this revision)"),
        },
        Target::Plan => "plan".to_owned(),
    }
}

/// A one-line quotation of a block, short enough to stay on one line.
fn excerpt(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= EXCERPT_CHARS {
        return flat;
    }
    let head: String = flat.chars().take(EXCERPT_CHARS).collect();
    format!("{}…", head.trim_end())
}

/// How many annotations still want something — the number `plan_status` reports.
///
/// Unresolved only, and only for this revision. Counting everything would inflate the
/// number with the agent's own progress notes and with marks a reviewer already ticked
/// off, so an agent polling for "0 things to fix" would never see it reach zero.
#[must_use]
pub fn pending_count(annotations: &[Annotation], revision: u32) -> usize {
    unresolved(annotations, revision).len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AnnKind, Block, BlockKind, PlanStatus, Step, StepStatus};

    fn plan() -> Plan {
        Plan {
            id: "deploy-v2".into(),
            title: "Deploy v2".into(),
            status: PlanStatus::ChangesRequested,
            current_revision: 3,
            conversation_id: None,
            agent_id: None,
            source: None,
            created_at: 100,
            updated_at: 200,
        }
    }

    fn revision() -> Revision {
        Revision {
            plan_id: "deploy-v2".into(),
            revision: 3,
            created_at: 100,
            markdown: String::new(),
            blocks: vec![Block {
                id: "b_1f2e9a4c7b01_1".into(),
                kind: BlockKind::Paragraph,
                text: "run the migration in one shot".into(),
                level: None,
                ordinal: 0,
                step_id: None,
            }],
            steps: vec![Step {
                id: "s_migrate_schema".into(),
                title: "Migrate schema".into(),
                summary: None,
                depends_on: Vec::new(),
                files: Vec::new(),
                status: StepStatus::Todo,
                risk: None,
            }],
            artifact_html: None,
        }
    }

    fn mark(id: &str, kind: AnnKind, target: Target, body: &str, at: i64) -> Annotation {
        Annotation {
            id: id.into(),
            plan_id: "deploy-v2".into(),
            revision: 3,
            target,
            kind,
            body: body.into(),
            suggestion: None,
            author: None,
            resolved: false,
            created_at: at,
        }
    }

    #[test]
    fn a_changes_requested_verdict_renders_the_documented_shape() {
        let verdict = Verdict {
            verdict: Decision::ChangesRequested,
            note: Some("close the data-loss window first".into()),
            revision: 3,
            decided_at: 300,
        };
        let mut redline = mark(
            "a_2",
            AnnKind::Redline,
            Target::Block {
                id: "b_1f2e9a4c7b01_1".into(),
            },
            "this is not reversible",
            2,
        );
        redline.body = String::new();
        redline.suggestion = Some("run it in two passes behind a flag".into());
        let marks = vec![
            mark(
                "a_1",
                AnnKind::Blocker,
                Target::Step {
                    id: "s_migrate_schema".into(),
                },
                "Back up the prod DB first.",
                1,
            ),
            redline,
        ];

        let text = render(&plan(), &revision(), &marks, Some(&verdict));
        assert_eq!(
            text,
            "PLAN deploy-v2 rev 3: CHANGES REQUESTED\n\
             note: close the data-loss window first\n\
             \n\
             [blocker] step \"Migrate schema\" (s_migrate_schema)\n\
             \x20 Back up the prod DB first.\n\
             [redline] block b_1f2e9a4c7b01_1 \"run the migration in one shot\"\n\
             \x20 suggest: run it in two passes behind a flag\n"
        );
    }

    #[test]
    fn blockers_come_first_regardless_of_when_they_were_written() {
        let marks = vec![
            mark("a_1", AnnKind::Comment, Target::Plan, "nit", 1),
            mark("a_2", AnnKind::Question, Target::Plan, "why?", 2),
            mark("a_3", AnnKind::Blocker, Target::Plan, "no", 3),
        ];
        let ordered: Vec<&str> = unresolved(&marks, 3)
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(ordered, vec!["a_3", "a_2", "a_1"]);
    }

    #[test]
    fn annotations_written_at_the_same_instant_still_have_one_order() {
        // Two reviewers clicking in the same second must not make the feedback text
        // flip between calls — an agent re-reading it would think the review changed.
        let marks = vec![
            mark("a_zzz", AnnKind::Comment, Target::Plan, "second", 7),
            mark("a_aaa", AnnKind::Comment, Target::Plan, "first", 7),
        ];
        let ordered: Vec<&str> = unresolved(&marks, 3)
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(ordered, vec!["a_aaa", "a_zzz"]);
    }

    #[test]
    fn resolved_marks_and_other_revisions_are_excluded() {
        let mut resolved = mark("a_1", AnnKind::Blocker, Target::Plan, "done with", 1);
        resolved.resolved = true;
        let mut older = mark("a_2", AnnKind::Blocker, Target::Plan, "old news", 1);
        older.revision = 2;
        let live = mark("a_3", AnnKind::Comment, Target::Plan, "still open", 1);
        let marks = vec![resolved, older, live];

        assert_eq!(pending_count(&marks, 3), 1);
        let text = render(&plan(), &revision(), &marks, None);
        assert!(text.contains("still open"), "{text}");
        assert!(!text.contains("done with"), "{text}");
        assert!(!text.contains("old news"), "{text}");
    }

    #[test]
    fn an_approval_with_nothing_left_open_is_two_lines() {
        let verdict = Verdict {
            verdict: Decision::Approved,
            note: Some("ship it".into()),
            revision: 3,
            decided_at: 300,
        };
        assert_eq!(
            render(&plan(), &revision(), &[], Some(&verdict)),
            "PLAN deploy-v2 rev 3: APPROVED\nnote: ship it\n"
        );
    }

    #[test]
    fn an_approval_with_open_comments_marks_them_advisory() {
        let verdict = Verdict {
            verdict: Decision::Approved,
            note: None,
            revision: 3,
            decided_at: 300,
        };
        let marks = vec![mark(
            "a_1",
            AnnKind::Comment,
            Target::Plan,
            "watch the index size",
            1,
        )];
        assert_eq!(
            render(&plan(), &revision(), &marks, Some(&verdict)),
            "PLAN deploy-v2 rev 3: APPROVED\n\
             \n\
             advisory only — the plan was approved with these left unresolved:\n\
             [comment] plan\n\
             \x20 watch the index size\n"
        );
    }

    #[test]
    fn a_pending_plan_says_so_instead_of_returning_something_that_reads_as_a_pass() {
        let text = render(&plan(), &revision(), &[], None);
        assert_eq!(
            text,
            "PLAN deploy-v2 rev 3: IN REVIEW\n\nno verdict yet, and no annotations so far.\n"
        );

        let marks = vec![mark(
            "a_1",
            AnnKind::Question,
            Target::Plan,
            "which region?",
            1,
        )];
        assert_eq!(
            render(&plan(), &revision(), &marks, None),
            "PLAN deploy-v2 rev 3: IN REVIEW\n\
             \n\
             no verdict yet. annotations so far:\n\
             [question] plan\n\
             \x20 which region?\n"
        );
    }

    #[test]
    fn changes_requested_with_no_annotations_tells_the_agent_what_to_do() {
        let verdict = Verdict {
            verdict: Decision::ChangesRequested,
            note: None,
            revision: 3,
            decided_at: 300,
        };
        assert_eq!(
            render(&plan(), &revision(), &[], Some(&verdict)),
            "PLAN deploy-v2 rev 3: CHANGES REQUESTED\n\
             \n\
             no annotations were left — ask the reviewer what to change before revising.\n"
        );
    }

    #[test]
    fn a_target_that_no_longer_exists_is_named_as_such() {
        let marks = vec![
            mark(
                "a_1",
                AnnKind::Blocker,
                Target::Block {
                    id: "b_deadbeef0000_1".into(),
                },
                "gone",
                1,
            ),
            mark(
                "a_2",
                AnnKind::Blocker,
                Target::Step {
                    id: "s_vanished".into(),
                },
                "also gone",
                2,
            ),
        ];
        let text = render(&plan(), &revision(), &marks, None);
        assert!(
            text.contains("block (b_deadbeef0000_1, no longer in this revision)"),
            "{text}"
        );
        assert!(
            text.contains("step (s_vanished, no longer in this revision)"),
            "{text}"
        );
    }

    #[test]
    fn a_long_block_is_quoted_short_enough_to_stay_on_one_line() {
        let mut rev = revision();
        rev.blocks[0].text = "word ".repeat(40);
        let marks = vec![mark(
            "a_1",
            AnnKind::Redline,
            Target::Block {
                id: "b_1f2e9a4c7b01_1".into(),
            },
            "too long",
            1,
        )];
        let text = render(&plan(), &rev, &marks, None);
        let quoted = text
            .lines()
            .find(|l| l.starts_with("[redline]"))
            .expect("redline line");
        assert!(quoted.ends_with("…\""), "{quoted}");
        assert!(quoted.chars().count() < 120, "{quoted}");
    }

    #[test]
    fn a_multiline_body_is_indented_and_a_multiline_suggestion_is_flattened() {
        let mut ann = mark("a_1", AnnKind::Redline, Target::Plan, "first\nsecond", 1);
        ann.suggestion = Some("do this\nthen that".into());
        let text = render(&plan(), &revision(), &[ann], None);
        assert!(text.contains("  first\n  second\n"), "{text}");
        assert!(text.contains("  suggest: do this then that\n"), "{text}");
    }
}
