//! The wire data model.
//!
//! Every type here is serialized straight onto the HTTP surface and read verbatim by
//! the companion UI and by the manifest's route declarations, so the field names are
//! a **frozen contract** — snake_case throughout, no camelCase anywhere, and no
//! renaming without moving the UI and the manifest in the same commit.
//!
//! Two conventions worth stating because they are easy to get wrong later:
//!
//! * Timestamps are **unix epoch seconds** as `i64`, matching `ryu_activity::ActivityItem`.
//!   Not RFC3339 strings (which the reasoning sidecar uses) — the UI sorts and
//!   subtracts these, and a string sort over RFC3339 only works by accident.
//! * Optional fields are `skip_serializing_if = "Option::is_none"` rather than emitted
//!   as `null`. The contract spells them `field?`, which in TypeScript means "absent
//!   or the value" — a literal `null` would fail a `field?: string` annotation and
//!   force the UI to widen every optional to `| null`.

use serde::{Deserialize, Serialize};

/// Unix epoch seconds, the one time unit this crate speaks.
#[must_use]
pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

// ── plan ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// Published and waiting on a human. Every publish resets to this, including a
    /// revise of an already-approved plan: a new revision has not been reviewed.
    #[default]
    InReview,
    Approved,
    ChangesRequested,
}

impl PlanStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PlanStatus::InReview => "in_review",
            PlanStatus::Approved => "approved",
            PlanStatus::ChangesRequested => "changes_requested",
        }
    }
}

/// The mutable head of a plan. Everything versioned lives in [`Revision`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub title: String,
    pub status: PlanStatus,
    /// The highest revision number that exists. Revisions are 1-based; a plan always
    /// has at least one, because a plan is created by publishing one.
    pub current_revision: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Free-form provenance ("claude-code", "workflow:deploy") for the UI's list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

// ── blocks ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Heading,
    Paragraph,
    ListItem,
    Code,
    Quote,
    /// A fenced block whose info string is `mermaid`. Split out from `code` because
    /// the UI renders it as a diagram, and because a diagram edit is a plan change a
    /// reviewer should see as such.
    Mermaid,
    /// A fenced `html` block, or a raw HTML span in the markdown.
    Html,
}

impl BlockKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            BlockKind::Heading => "heading",
            BlockKind::Paragraph => "paragraph",
            BlockKind::ListItem => "list_item",
            BlockKind::Code => "code",
            BlockKind::Quote => "quote",
            BlockKind::Mermaid => "mermaid",
            BlockKind::Html => "html",
        }
    }

    /// Whether this kind's text is whitespace-significant.
    ///
    /// Prose normalizes whitespace before hashing (a re-wrapped paragraph is the same
    /// paragraph); code does not (re-indenting Python *is* an edit).
    #[must_use]
    pub fn is_verbatim(self) -> bool {
        matches!(self, BlockKind::Code | BlockKind::Mermaid | BlockKind::Html)
    }
}

/// One addressable piece of the plan.
///
/// `id` is derived from `kind` + normalized `text` — see [`crate::parse::block_id`].
/// Note what is deliberately NOT in here: a fenced block's language. The contract
/// froze the field set, and carrying the language would have meant either a new field
/// or smuggling it into `text` where it would change the hash. Code renders
/// unhighlighted; that is the known cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub id: String,
    pub kind: BlockKind,
    pub text: String,
    /// Heading depth, 1–6. `None` for every other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u8>,
    /// Position in the revision, 0-based. Present so the UI can render in order
    /// without relying on array order surviving a client-side sort; it is NOT part of
    /// the id, which is the whole point of content addressing.
    pub ordinal: u32,
    /// The step this block belongs to, when the step derivation could attribute it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
}

// ── steps ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    #[default]
    Todo,
    InProgress,
    Done,
    Blocked,
}

impl StepStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            StepStatus::Todo => "todo",
            StepStatus::InProgress => "in_progress",
            StepStatus::Done => "done",
            StepStatus::Blocked => "blocked",
        }
    }
}

/// A node of the plan's dependency graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Ids of steps that must finish first. Always present (possibly empty) so the UI
    /// never has to null-check before building edges.
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    pub status: StepStatus,
    /// Free-form risk note. Only ever set by an explicit caller; the markdown
    /// derivation does not guess at risk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<String>,
}

/// A step as a caller supplies it. Explicit steps WIN over derivation — an agent that
/// already knows its own DAG should not have its structure guessed at.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StepInput {
    #[serde(default)]
    pub id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub risk: Option<String>,
}

// ── revisions ────────────────────────────────────────────────────────────────

/// One immutable publish of a plan.
///
/// "Immutable" has exactly one exception, and it is deliberate: `steps[].status` is
/// progress, not plan content, so `step_update` rewrites it in place. The markdown,
/// the blocks and the step structure never change once written — which is what lets
/// an annotation keep meaning something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub plan_id: String,
    pub revision: u32,
    pub created_at: i64,
    pub markdown: String,
    pub blocks: Vec<Block>,
    pub steps: Vec<Step>,
    /// An optional rendered artifact (a mock, a report) the reviewer looks at beside
    /// the plan. Stored as-is; the UI is responsible for sandboxing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_html: Option<String>,
}

/// The cheap shape of a revision, for the revision list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionSummary {
    pub revision: u32,
    pub created_at: i64,
    pub block_count: usize,
    pub step_count: usize,
}

// ── annotations ──────────────────────────────────────────────────────────────

/// What an annotation is attached to.
///
/// Internally tagged, so the wire form is `{"type":"block","id":"b_…"}` and the plan
/// target is the bare `{"type":"plan"}` — a discriminated union TypeScript narrows
/// for free.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Target {
    Block { id: String },
    Step { id: String },
    Plan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnKind {
    /// Ordered by how loudly the agent should hear it — see [`AnnKind::severity`].
    Blocker,
    Redline,
    Question,
    Comment,
}

impl AnnKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AnnKind::Blocker => "blocker",
            AnnKind::Redline => "redline",
            AnnKind::Question => "question",
            AnnKind::Comment => "comment",
        }
    }

    /// Sort weight for the feedback text. Lower comes first: an agent that reads only
    /// the top of a long list must read the blockers.
    #[must_use]
    pub fn severity(self) -> u8 {
        match self {
            AnnKind::Blocker => 0,
            AnnKind::Redline => 1,
            AnnKind::Question => 2,
            AnnKind::Comment => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Annotation {
    pub id: String,
    pub plan_id: String,
    /// The revision this was written against. An annotation never migrates: it
    /// describes the text that existed when it was written.
    pub revision: u32,
    pub target: Target,
    pub kind: AnnKind,
    pub body: String,
    /// The replacement text a `redline` proposes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Resolved annotations stay on the record but drop out of the feedback text and
    /// out of the `annotation_count` an agent polls on.
    #[serde(default)]
    pub resolved: bool,
    pub created_at: i64,
}

// ── verdict ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approved,
    ChangesRequested,
}

impl Decision {
    #[must_use]
    pub fn as_plan_status(self) -> PlanStatus {
        match self {
            Decision::Approved => PlanStatus::Approved,
            Decision::ChangesRequested => PlanStatus::ChangesRequested,
        }
    }
}

/// A human's decision about one revision.
///
/// Keyed by revision rather than by plan: revising an approved plan must not silently
/// inherit the approval, and the old revision's approval is still a true statement
/// about the old revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub verdict: Decision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub revision: u32,
    pub decided_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plan_target_serializes_as_a_bare_tag() {
        // The UI narrows on `type`, so the plan target must still be an object with a
        // `type` and nothing else — not a bare string, which is what an externally
        // tagged enum would have produced.
        let json = serde_json::to_value(Target::Plan).expect("serializes");
        assert_eq!(json, serde_json::json!({ "type": "plan" }));
        let json = serde_json::to_value(Target::Block { id: "b_1".into() }).expect("serializes");
        assert_eq!(json, serde_json::json!({ "type": "block", "id": "b_1" }));
    }

    #[test]
    fn absent_optionals_are_omitted_rather_than_null() {
        let plan = Plan {
            id: "p".into(),
            title: "T".into(),
            status: PlanStatus::InReview,
            current_revision: 1,
            conversation_id: None,
            agent_id: None,
            source: None,
            created_at: 1,
            updated_at: 1,
        };
        let json = serde_json::to_value(&plan).expect("serializes");
        let obj = json.as_object().expect("object");
        assert!(!obj.contains_key("conversation_id"), "got {json}");
        assert!(!obj.contains_key("agent_id"));
        assert_eq!(obj["status"], "in_review");
    }

    #[test]
    fn every_enum_renders_the_snake_case_the_contract_names() {
        assert_eq!(
            serde_json::to_value(BlockKind::ListItem).expect("ok"),
            serde_json::json!("list_item")
        );
        assert_eq!(
            serde_json::to_value(StepStatus::InProgress).expect("ok"),
            serde_json::json!("in_progress")
        );
        assert_eq!(
            serde_json::to_value(Decision::ChangesRequested).expect("ok"),
            serde_json::json!("changes_requested")
        );
        // The `as_str` helpers feed the feedback text; they must not drift from serde.
        for kind in [
            BlockKind::Heading,
            BlockKind::Paragraph,
            BlockKind::ListItem,
            BlockKind::Code,
            BlockKind::Quote,
            BlockKind::Mermaid,
            BlockKind::Html,
        ] {
            assert_eq!(
                serde_json::to_value(kind).expect("ok"),
                serde_json::json!(kind.as_str())
            );
        }
        for status in [
            StepStatus::Todo,
            StepStatus::InProgress,
            StepStatus::Done,
            StepStatus::Blocked,
        ] {
            assert_eq!(
                serde_json::to_value(status).expect("ok"),
                serde_json::json!(status.as_str())
            );
        }
        for kind in [
            AnnKind::Blocker,
            AnnKind::Redline,
            AnnKind::Question,
            AnnKind::Comment,
        ] {
            assert_eq!(
                serde_json::to_value(kind).expect("ok"),
                serde_json::json!(kind.as_str())
            );
        }
    }

    #[test]
    fn blockers_sort_ahead_of_comments() {
        let mut kinds = [AnnKind::Comment, AnnKind::Blocker, AnnKind::Question];
        kinds.sort_by_key(|k| k.severity());
        assert_eq!(kinds[0], AnnKind::Blocker);
        assert_eq!(kinds[2], AnnKind::Comment);
    }
}
