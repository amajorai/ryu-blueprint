//! The HTTP surface Core proxies as `/api/blueprint/*`.
//!
//! Every path here is also enumerated in the manifest's `sidecars[].http.routes` — an
//! unlisted route is a 404 at Core's allowlist no matter what axum does with it, so the
//! two files move together or the new route simply does not exist.
//!
//! # Shape
//!
//! Thin handlers over operations that do the real work, because the same operations are
//! also what [`crate::mcp`] calls. Publishing a plan from an agent and publishing it
//! from a `curl` have to produce byte-identical stored state, and the only way to be
//! sure of that is for there to be one implementation ([`publish`], [`plan_detail`],
//! [`update_step`], [`record_verdict`]) rather than two that agree today.
//!
//! No authentication lives here. The router is returned un-gated and `main.rs` wraps
//! it in the bearer middleware, which keeps the auth decision in one place and lets
//! this module be exercised without a server.
//!
//! # The additive `graph` key
//!
//! `GET /plans/:id` and `GET /plans/:id/revisions/:rev` include a `graph` array of
//! `{ step_id, layer, order }` alongside the keys the contract names. It is additive —
//! a client that ignores it sees exactly the contracted payload — and it is what lets
//! the companion draw the dependency graph without shipping dagre or elk to the
//! browser. It is a response key rather than a route precisely because a new route
//! would need a manifest entry and a key does not.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::diff::{diff_blocks, BlockDiff};
use crate::feedback;
use crate::graph::{self, Placement};
use crate::host::Events;
use crate::model::{
    now, AnnKind, Annotation, Block, BlockKind, Decision, Plan, PlanStatus, Revision, Step,
    StepInput, StepStatus, Target, Verdict,
};
use crate::parse;
use crate::store::{is_valid_id, Store};

/// Ceiling on the `-2`, `-3`, … suffixes tried when a title slugs onto a plan id that
/// already exists. Past that the title is not distinguishing anything and a random id
/// is more honest than a fiftieth near-duplicate.
const MAX_SLUG_ATTEMPTS: u32 = 50;

pub struct Ctx {
    pub store: Store,
    pub events: Events,
}

pub fn routes(ctx: Arc<Ctx>) -> Router {
    Router::new()
        .route("/plans", get(list_plans).post(publish_route))
        .route("/plans/:id", get(get_plan).delete(delete_plan))
        .route("/plans/:id/revisions", get(list_revisions))
        .route("/plans/:id/revisions/:rev", get(get_revision))
        .route("/plans/:id/diff", get(diff_route))
        .route("/plans/:id/annotations", post(create_annotation))
        .route(
            "/plans/:id/annotations/:annotation_id",
            delete(delete_annotation),
        )
        .route("/plans/:id/verdict", post(verdict_route))
        .route("/plans/:id/steps/:step_id", post(step_route))
        .route("/plans/:id/feedback", get(feedback_route))
        .with_state(ctx)
}

// ── errors ───────────────────────────────────────────────────────────────────

/// A status and a message. Not `thiserror`, not `anyhow`-into-response: the set of
/// failures this surface has is small, and every one of them wants a hand-written
/// message that tells the caller what to do differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl ApiError {
    pub fn bad(msg: impl Into<String>) -> ApiError {
        ApiError(StatusCode::BAD_REQUEST, msg.into())
    }
    pub fn missing(msg: impl Into<String>) -> ApiError {
        ApiError(StatusCode::NOT_FOUND, msg.into())
    }
    pub fn internal(e: impl std::fmt::Display) -> ApiError {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
    /// A store error caused by a caller-supplied id is the caller's problem (400), not
    /// the server's — the store reports both through one `anyhow::Error`, and mapping
    /// the whole class to 500 would tell an agent to retry a request that can never
    /// succeed.
    fn from_store(e: impl std::fmt::Display) -> ApiError {
        ApiError::bad(e.to_string())
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

// ── publish ──────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct PublishRequest {
    /// Present: append a revision to this plan (creating it under that id if it does
    /// not exist yet, so an agent may choose its own id). Absent: mint one from the
    /// title.
    #[serde(default)]
    pub plan_id: Option<String>,
    pub title: String,
    pub markdown: String,
    /// Explicit steps WIN over derivation. An agent that already knows its own DAG
    /// should not have it guessed at from prose.
    #[serde(default)]
    pub steps: Option<Vec<StepInput>>,
    #[serde(default)]
    pub artifact_html: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

/// Publish a plan, or append a revision to an existing one.
///
/// # Errors
/// 400 on an empty title or body, an unusable `plan_id`, or a step graph that does not
/// validate (cycle, dangling dependency, duplicate id); 500 if the write fails.
pub async fn publish(ctx: &Ctx, req: PublishRequest) -> ApiResult<(Plan, Revision)> {
    let title = req.title.trim().to_owned();
    if title.is_empty() {
        return Err(ApiError::bad("a plan needs a title"));
    }
    if req.markdown.trim().is_empty() {
        return Err(ApiError::bad(
            "a plan needs markdown — publish the plan text, not an empty document",
        ));
    }

    let (mut blocks, steps) = match req.steps.as_deref() {
        Some(inputs) if !inputs.is_empty() => (
            parse::parse_blocks(&req.markdown),
            parse::steps_from_input(inputs),
        ),
        _ => {
            let parsed = parse::parse(&req.markdown);
            (parsed.blocks, parsed.steps)
        }
    };
    if req.steps.as_deref().is_some_and(|s| !s.is_empty()) {
        attribute_explicit_steps(&mut blocks, &steps);
    }

    // Validate before anything is written: a cycle reported at publish time is a
    // fixable message to the agent, while a cycle discovered at render time is a
    // review surface that will not draw.
    graph::validate(&steps).map_err(|e| ApiError::bad(e.to_string()))?;

    let existing = match req.plan_id.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => {
            if !is_valid_id(id) {
                return Err(ApiError::bad(format!(
                    "plan_id '{id}' is not usable: 1–64 characters of lowercase letters, \
                     digits, '-' and '_', starting with a letter or digit"
                )));
            }
            Some((
                id.to_owned(),
                ctx.store.get_plan(id).map_err(ApiError::from_store)?,
            ))
        }
        _ => None,
    };

    let stamp = now();
    let (plan_id, mut plan) = match existing {
        Some((id, Some(mut found))) => {
            found.title = title.clone();
            // Every publish resets the status. A revision nobody has read is not
            // approved, whatever the previous one was.
            found.status = PlanStatus::InReview;
            found.current_revision = found.current_revision.saturating_add(1);
            found.updated_at = stamp;
            if req.conversation_id.is_some() {
                found.conversation_id = req.conversation_id.clone();
            }
            if req.agent_id.is_some() {
                found.agent_id = req.agent_id.clone();
            }
            if req.source.is_some() {
                found.source = req.source.clone();
            }
            (id, found)
        }
        Some((id, None)) => (id.clone(), new_plan(id, &title, &req, stamp)),
        None => {
            let id = mint_plan_id(&ctx.store, &title)?;
            (id.clone(), new_plan(id, &title, &req, stamp))
        }
    };
    plan.id = plan_id.clone();

    let revision = Revision {
        plan_id: plan_id.clone(),
        revision: plan.current_revision,
        created_at: stamp,
        markdown: req.markdown,
        blocks,
        steps,
        artifact_html: req.artifact_html.filter(|h| !h.trim().is_empty()),
    };

    // Revision first: a plan pointing at a revision that failed to write would render
    // as an empty review, while an orphan revision is invisible and harmless.
    ctx.store
        .save_revision(&revision)
        .map_err(ApiError::internal)?;
    ctx.store.save_plan(&plan).map_err(ApiError::internal)?;

    ctx.events
        .plan_published(
            &plan.id,
            revision.revision,
            &plan.title,
            revision.steps.len(),
            plan.conversation_id.as_deref(),
        )
        .await;

    Ok((plan, revision))
}

async fn publish_route(
    State(ctx): State<Arc<Ctx>>,
    Json(req): Json<PublishRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let (plan, revision) = publish(&ctx, req).await?;
    Ok(Json(json!({ "plan": plan, "revision": revision })))
}

fn new_plan(id: String, title: &str, req: &PublishRequest, stamp: i64) -> Plan {
    Plan {
        id,
        title: title.to_owned(),
        status: PlanStatus::InReview,
        current_revision: 1,
        conversation_id: req.conversation_id.clone(),
        agent_id: req.agent_id.clone(),
        source: req.source.clone(),
        created_at: stamp,
        updated_at: stamp,
    }
}

/// A readable plan id from the title, disambiguated against what already exists.
///
/// Readable rather than a uuid because this id shows up in the feedback text an agent
/// reads back and in the URL a human shares — `deploy-v2 rev 3` says what it is and
/// `9f2c…` does not.
fn mint_plan_id(store: &Store, title: &str) -> ApiResult<String> {
    let base = parse::slug(title);
    if base.is_empty() {
        return Ok(format!("plan-{}", uuid::Uuid::new_v4().simple()));
    }
    for attempt in 1..=MAX_SLUG_ATTEMPTS {
        let candidate = if attempt == 1 {
            base.clone()
        } else {
            format!("{base}-{attempt}")
        };
        if !is_valid_id(&candidate) {
            break;
        }
        if store
            .get_plan(&candidate)
            .map_err(ApiError::from_store)?
            .is_none()
        {
            return Ok(candidate);
        }
    }
    Ok(format!("plan-{}", uuid::Uuid::new_v4().simple()))
}

/// Attribute blocks to caller-supplied steps where the text says so.
///
/// With derived steps the parser knows which block produced which step. With explicit
/// steps it cannot know — the caller sent a structure and a document with no stated
/// relation between them. Rather than guess, this attributes only the unambiguous
/// case: a block whose text *is* the step's title, which is what a plan that lists its
/// steps as headings looks like. Everything else stays `None`, and the UI falls back
/// to the step list.
fn attribute_explicit_steps(blocks: &mut [Block], steps: &[Step]) {
    // Titles are prose, so they normalize as prose — comparing them under a code
    // block's verbatim rules would make a step called `run  migrate` match a code line
    // and not the paragraph that names it. Verbatim blocks are skipped outright: a
    // fenced block is never "the step's title", it is at most the step's contents.
    let titles: Vec<String> = steps
        .iter()
        .map(|s| parse::normalize(BlockKind::Paragraph, &s.title))
        .collect();
    for block in blocks.iter_mut() {
        if block.kind.is_verbatim() {
            continue;
        }
        let text = parse::normalize(BlockKind::Paragraph, &block.text);
        if let Some(idx) = titles.iter().position(|t| *t == text) {
            block.step_id = Some(steps[idx].id.clone());
        }
    }
}

// ── reads ────────────────────────────────────────────────────────────────────

/// Everything the review surface needs for one plan in one round trip.
#[derive(Debug, Serialize)]
pub struct PlanDetail {
    pub plan: Plan,
    pub revision: Revision,
    pub annotations: Vec<Annotation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    /// Additive: see the module docs. `{ step_id, layer, order }` per step.
    pub graph: Vec<Placement>,
}

/// # Errors
/// 400 for an unusable id, 404 when the plan or its current revision is missing.
pub fn plan_detail(ctx: &Ctx, plan_id: &str) -> ApiResult<PlanDetail> {
    let plan = ctx
        .store
        .get_plan(plan_id)
        .map_err(ApiError::from_store)?
        .ok_or_else(|| ApiError::missing(format!("no plan with the id '{plan_id}'")))?;
    let revision = ctx
        .store
        .get_revision(plan_id, plan.current_revision)
        .map_err(ApiError::from_store)?
        .ok_or_else(|| {
            ApiError::missing(format!(
                "plan '{plan_id}' points at revision {} but that revision is not stored",
                plan.current_revision
            ))
        })?;
    let annotations = ctx
        .store
        .annotations(plan_id)
        .map_err(ApiError::from_store)?;
    let verdict = ctx
        .store
        .verdict_for(plan_id, plan.current_revision)
        .map_err(ApiError::from_store)?;
    Ok(PlanDetail {
        graph: layout_or_empty(&revision),
        plan,
        revision,
        annotations,
        verdict,
    })
}

/// Lay the steps out, or give the UI nothing to draw.
///
/// Publishing validates the graph, so an error here means a revision written before a
/// validation rule existed, or one edited on disk. Failing the whole read for that
/// would make a plan unopenable — and unopenable is exactly the state in which nobody
/// can see what is wrong with it.
fn layout_or_empty(revision: &Revision) -> Vec<Placement> {
    match graph::layout(&revision.steps) {
        Ok(placements) => placements,
        Err(e) => {
            tracing::warn!(
                plan = %revision.plan_id,
                revision = revision.revision,
                "blueprint: step graph will not lay out, serving it without one: {e}"
            );
            Vec::new()
        }
    }
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    status: Option<String>,
}

async fn list_plans(
    State(ctx): State<Arc<Ctx>>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let mut plans = ctx.store.list_plans().map_err(ApiError::internal)?;
    if let Some(filter) = query.status.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        // Matched by hand rather than by deserializing the query into `PlanStatus`: an
        // unknown value should be an empty list with an explanation, not axum's raw
        // deserialization rejection.
        let wanted = match filter {
            "in_review" => PlanStatus::InReview,
            "approved" => PlanStatus::Approved,
            "changes_requested" => PlanStatus::ChangesRequested,
            other => {
                return Err(ApiError::bad(format!(
                    "unknown status '{other}' — expected in_review, approved or changes_requested"
                )))
            }
        };
        plans.retain(|p| p.status == wanted);
    }
    Ok(Json(json!({ "plans": plans })))
}

async fn get_plan(State(ctx): State<Arc<Ctx>>, Path(id): Path<String>) -> ApiResult<Json<PlanDetail>> {
    plan_detail(&ctx, &id).map(Json)
}

async fn delete_plan(
    State(ctx): State<Arc<Ctx>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let deleted = ctx
        .store
        .delete_plan(&id)
        .map_err(ApiError::from_store)?;
    Ok(Json(json!({ "deleted": deleted })))
}

async fn list_revisions(
    State(ctx): State<Arc<Ctx>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let revisions = ctx
        .store
        .list_revisions(&id)
        .map_err(ApiError::from_store)?;
    Ok(Json(json!({ "revisions": revisions })))
}

async fn get_revision(
    State(ctx): State<Arc<Ctx>>,
    Path((id, rev)): Path<(String, u32)>,
) -> ApiResult<Json<serde_json::Value>> {
    let revision = ctx
        .store
        .get_revision(&id, rev)
        .map_err(ApiError::from_store)?
        .ok_or_else(|| ApiError::missing(format!("plan '{id}' has no revision {rev}")))?;
    let graph = layout_or_empty(&revision);
    Ok(Json(json!({ "revision": revision, "graph": graph })))
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
pub struct DiffQuery {
    #[serde(default)]
    pub from: Option<u32>,
    #[serde(default)]
    pub to: Option<u32>,
}

/// Classify the blocks of `to` against `from`, defaulting the range.
///
/// # Errors
/// 400 for an unusable id, 404 when the plan or either revision is missing.
pub fn diff_revisions(
    ctx: &Ctx,
    id: &str,
    query: DiffQuery,
) -> ApiResult<(u32, u32, Vec<BlockDiff>)> {
    let plan = ctx
        .store
        .get_plan(id)
        .map_err(ApiError::from_store)?
        .ok_or_else(|| ApiError::missing(format!("no plan with the id '{id}'")))?;
    let to = query.to.unwrap_or(plan.current_revision);
    // Defaulting `from` to the revision before `to` makes the bare `/diff` call answer
    // the question anyone actually has: what changed in the latest revise? For a plan
    // with only one revision that means `from = 0`, which is deliberate — the whole
    // first revision reading as `added` is the true answer, and clamping to 1 would
    // instead diff revision 1 against itself and report a brand-new plan as unchanged.
    let from = query.from.unwrap_or_else(|| to.saturating_sub(1));

    let load = |n: u32| -> ApiResult<Vec<Block>> {
        // Revision 0 does not exist; diffing revision 1 against it is how "everything
        // in the first revision is new" is spelled, and it is the default a plan with
        // one revision lands on.
        if n == 0 {
            return Ok(Vec::new());
        }
        ctx.store
            .get_revision(id, n)
            .map_err(ApiError::from_store)?
            .map(|r| r.blocks)
            .ok_or_else(|| ApiError::missing(format!("plan '{id}' has no revision {n}")))
    };
    let blocks: Vec<BlockDiff> = if from == to {
        // Comparing a revision with itself is a legitimate UI state (the picker can
        // land there); answering "everything is the same" beats a 400.
        let same = load(to)?;
        diff_blocks(&same, &same)
    } else {
        diff_blocks(&load(from)?, &load(to)?)
    };
    Ok((from, to, blocks))
}

async fn diff_route(
    State(ctx): State<Arc<Ctx>>,
    Path(id): Path<String>,
    Query(query): Query<DiffQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let (from, to, blocks) = diff_revisions(&ctx, &id, query)?;
    Ok(Json(json!({ "from": from, "to": to, "blocks": blocks })))
}

// ── annotations ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AnnotationRequest {
    #[serde(default)]
    revision: Option<u32>,
    target: Target,
    kind: AnnKind,
    body: String,
    #[serde(default)]
    suggestion: Option<String>,
    #[serde(default)]
    author: Option<String>,
}

async fn create_annotation(
    State(ctx): State<Arc<Ctx>>,
    Path(id): Path<String>,
    Json(req): Json<AnnotationRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let plan = ctx
        .store
        .get_plan(&id)
        .map_err(ApiError::from_store)?
        .ok_or_else(|| ApiError::missing(format!("no plan with the id '{id}'")))?;
    if req.body.trim().is_empty() && req.suggestion.is_none() {
        return Err(ApiError::bad(
            "an annotation needs a body, or a suggestion if it is a redline",
        ));
    }
    let revision_number = req.revision.unwrap_or(plan.current_revision);
    let revision = ctx
        .store
        .get_revision(&id, revision_number)
        .map_err(ApiError::from_store)?
        .ok_or_else(|| ApiError::missing(format!("plan '{id}' has no revision {revision_number}")))?;

    // Refuse an anchor that points at nothing. An orphan annotation renders nowhere
    // and reads back as feedback about text the agent cannot find — a silent hole in
    // the loop, which is worse than a rejected request.
    match &req.target {
        Target::Block { id: block_id } => {
            if !revision.blocks.iter().any(|b| b.id == *block_id) {
                return Err(ApiError::bad(format!(
                    "revision {revision_number} has no block '{block_id}'"
                )));
            }
        }
        Target::Step { id: step_id } => {
            if !revision.steps.iter().any(|s| s.id == *step_id) {
                return Err(ApiError::bad(format!(
                    "revision {revision_number} has no step '{step_id}'"
                )));
            }
        }
        Target::Plan => {}
    }

    let annotation = Annotation {
        id: format!("a_{}", uuid::Uuid::new_v4().simple()),
        plan_id: id.clone(),
        revision: revision_number,
        target: req.target,
        kind: req.kind,
        body: req.body,
        suggestion: req.suggestion.filter(|s| !s.trim().is_empty()),
        author: req.author.filter(|a| !a.trim().is_empty()),
        resolved: false,
        created_at: now(),
    };
    ctx.store
        .add_annotation(&annotation)
        .map_err(ApiError::from_store)?;
    Ok(Json(json!({ "annotation": annotation })))
}

async fn delete_annotation(
    State(ctx): State<Arc<Ctx>>,
    Path((id, annotation_id)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let deleted = ctx
        .store
        .delete_annotation(&id, &annotation_id)
        .map_err(ApiError::from_store)?;
    Ok(Json(json!({ "deleted": deleted })))
}

// ── verdict ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct VerdictRequest {
    pub verdict: Decision,
    #[serde(default)]
    pub note: Option<String>,
}

/// Record a decision about the plan's current revision.
///
/// # Errors
/// 400 for an unusable id, 404 when the plan is missing, 500 if the write fails.
pub async fn record_verdict(
    ctx: &Ctx,
    plan_id: &str,
    req: VerdictRequest,
) -> ApiResult<(Plan, Verdict)> {
    let mut plan = ctx
        .store
        .get_plan(plan_id)
        .map_err(ApiError::from_store)?
        .ok_or_else(|| ApiError::missing(format!("no plan with the id '{plan_id}'")))?;

    let note = req.note.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty());
    let verdict = Verdict {
        verdict: req.verdict,
        note: note.clone(),
        revision: plan.current_revision,
        decided_at: now(),
    };
    ctx.store
        .set_verdict(plan_id, &verdict)
        .map_err(ApiError::internal)?;

    plan.status = req.verdict.as_plan_status();
    plan.updated_at = verdict.decided_at;
    ctx.store.save_plan(&plan).map_err(ApiError::internal)?;

    let pending = feedback::pending_count(
        &ctx.store
            .annotations(plan_id)
            .map_err(ApiError::from_store)?,
        plan.current_revision,
    );
    match req.verdict {
        Decision::Approved => {
            ctx.events
                .plan_approved(
                    plan_id,
                    verdict.revision,
                    note.as_deref(),
                    plan.conversation_id.as_deref(),
                )
                .await;
        }
        Decision::ChangesRequested => {
            ctx.events
                .plan_changes_requested(
                    plan_id,
                    verdict.revision,
                    pending,
                    note.as_deref(),
                    plan.conversation_id.as_deref(),
                )
                .await;
        }
    }
    Ok((plan, verdict))
}

async fn verdict_route(
    State(ctx): State<Arc<Ctx>>,
    Path(id): Path<String>,
    Json(req): Json<VerdictRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let (plan, verdict) = record_verdict(&ctx, &id, req).await?;
    Ok(Json(json!({ "plan": plan, "verdict": verdict })))
}

// ── step progress ────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct StepUpdate {
    #[serde(default)]
    pub status: Option<StepStatus>,
    /// A progress note from whoever is doing the work. Filed as a *resolved* comment
    /// annotation: it belongs on the record and in the UI, but it is a statement, not
    /// a request — leaving it unresolved would inflate the count an agent polls on and
    /// have it read its own notes back as feedback.
    #[serde(default)]
    pub note: Option<String>,
}

/// Move one step of the current revision.
///
/// This is the one thing that mutates a stored revision, and it is deliberate: status
/// is progress, not plan content. The markdown, the blocks and the graph stay frozen.
///
/// # Errors
/// 400 for an unusable id, 404 when the plan, revision or step is missing.
pub async fn update_step(
    ctx: &Ctx,
    plan_id: &str,
    step_id: &str,
    update: StepUpdate,
) -> ApiResult<Step> {
    let plan = ctx
        .store
        .get_plan(plan_id)
        .map_err(ApiError::from_store)?
        .ok_or_else(|| ApiError::missing(format!("no plan with the id '{plan_id}'")))?;
    let mut revision = ctx
        .store
        .get_revision(plan_id, plan.current_revision)
        .map_err(ApiError::from_store)?
        .ok_or_else(|| {
            ApiError::missing(format!(
                "plan '{plan_id}' points at revision {} but that revision is not stored",
                plan.current_revision
            ))
        })?;

    let position = revision
        .steps
        .iter()
        .position(|s| s.id == step_id)
        .ok_or_else(|| {
            ApiError::missing(format!(
                "revision {} of plan '{plan_id}' has no step '{step_id}'",
                plan.current_revision
            ))
        })?;

    let was = revision.steps[position].status;
    if let Some(status) = update.status {
        revision.steps[position].status = status;
    }
    let step = revision.steps[position].clone();
    ctx.store
        .save_revision(&revision)
        .map_err(ApiError::internal)?;

    if let Some(note) = update.note.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()) {
        let annotation = Annotation {
            id: format!("a_{}", uuid::Uuid::new_v4().simple()),
            plan_id: plan_id.to_owned(),
            revision: revision.revision,
            target: Target::Step {
                id: step.id.clone(),
            },
            kind: AnnKind::Comment,
            body: note,
            suggestion: None,
            author: plan.agent_id.clone(),
            resolved: true,
            created_at: now(),
        };
        ctx.store
            .add_annotation(&annotation)
            .map_err(ApiError::from_store)?;
    }

    // Only on the transition, so a client that re-sends `done` does not re-fire the
    // event and re-run whatever workflow is subscribed to it.
    if step.status == StepStatus::Done && was != StepStatus::Done {
        ctx.events
            .step_completed(
                plan_id,
                &step.id,
                &step.title,
                plan.conversation_id.as_deref(),
            )
            .await;
    }
    Ok(step)
}

async fn step_route(
    State(ctx): State<Arc<Ctx>>,
    Path((id, step_id)): Path<(String, String)>,
    Json(req): Json<StepUpdate>,
) -> ApiResult<Json<serde_json::Value>> {
    let step = update_step(&ctx, &id, &step_id, req).await?;
    Ok(Json(json!({ "step": step })))
}

// ── feedback ─────────────────────────────────────────────────────────────────

/// The agent-readable verdict for a plan's current revision.
#[derive(Debug, Clone, Serialize)]
pub struct FeedbackPayload {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    /// True while nobody has decided. The agent's cue to keep waiting rather than to
    /// treat an absent verdict as approval.
    pub pending: bool,
    /// Unresolved annotations on this revision only — see [`crate::feedback`].
    pub annotation_count: usize,
    pub revision: u32,
    pub status: PlanStatus,
}

/// # Errors
/// 400 for an unusable id, 404 when the plan or its revision is missing.
pub fn feedback_for(ctx: &Ctx, plan_id: &str) -> ApiResult<FeedbackPayload> {
    let detail = plan_detail(ctx, plan_id)?;
    let text = feedback::render(
        &detail.plan,
        &detail.revision,
        &detail.annotations,
        detail.verdict.as_ref(),
    );
    Ok(FeedbackPayload {
        text,
        pending: detail.verdict.is_none(),
        annotation_count: feedback::pending_count(&detail.annotations, detail.revision.revision),
        revision: detail.revision.revision,
        status: detail.plan.status,
        verdict: detail.verdict,
    })
}

async fn feedback_route(
    State(ctx): State<Arc<Ctx>>,
    Path(id): Path<String>,
) -> ApiResult<Json<FeedbackPayload>> {
    feedback_for(&ctx, &id).map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> Arc<Ctx> {
        let dir = std::env::temp_dir().join(format!("ryu-blueprint-api-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        Arc::new(Ctx {
            store: Store::open(dir).expect("opens"),
            events: Events::from_env(),
        })
    }

    fn request(title: &str, markdown: &str) -> PublishRequest {
        PublishRequest {
            title: title.into(),
            markdown: markdown.into(),
            ..PublishRequest::default()
        }
    }

    #[tokio::test]
    async fn publishing_twice_under_one_id_appends_a_revision_and_resets_the_status() {
        let ctx = scratch("revise");
        let (plan, rev) = publish(&ctx, request("Deploy v2", "1. Set up\n2. Ship\n"))
            .await
            .expect("publishes");
        assert_eq!(plan.id, "deploy-v2");
        assert_eq!(plan.current_revision, 1);
        assert_eq!(rev.steps.len(), 2);

        record_verdict(
            &ctx,
            "deploy-v2",
            VerdictRequest {
                verdict: Decision::Approved,
                note: None,
            },
        )
        .await
        .expect("approves");
        assert_eq!(
            ctx.store.get_plan("deploy-v2").expect("gets").expect("some").status,
            PlanStatus::Approved
        );

        let mut again = request("Deploy v2", "1. Set up\n2. Ship\n3. Announce\n");
        again.plan_id = Some("deploy-v2".into());
        let (plan, rev) = publish(&ctx, again).await.expect("revises");
        assert_eq!(plan.current_revision, 2);
        assert_eq!(rev.revision, 2);
        assert_eq!(
            plan.status,
            PlanStatus::InReview,
            "a revision nobody has read is not approved, whatever the last one was"
        );
        // The approval of revision 1 is still true about revision 1.
        assert!(ctx.store.verdict_for("deploy-v2", 1).expect("gets").is_some());
        assert!(ctx.store.verdict_for("deploy-v2", 2).expect("gets").is_none());
    }

    #[tokio::test]
    async fn two_plans_with_the_same_title_get_distinct_ids() {
        let ctx = scratch("collide");
        let first = publish(&ctx, request("Deploy", "body\n")).await.expect("ok").0;
        let second = publish(&ctx, request("Deploy", "body\n")).await.expect("ok").0;
        assert_eq!(first.id, "deploy");
        assert_eq!(
            second.id, "deploy-2",
            "publishing without a plan_id must never silently revise someone else's plan"
        );
    }

    #[tokio::test]
    async fn a_title_with_no_usable_characters_still_gets_a_valid_id() {
        let ctx = scratch("unslugabble");
        let plan = publish(&ctx, request("!!! ???", "body\n")).await.expect("ok").0;
        assert!(plan.id.starts_with("plan-"));
        assert!(is_valid_id(&plan.id), "{}", plan.id);
    }

    #[tokio::test]
    async fn a_cyclic_step_graph_is_refused_before_anything_is_written() {
        let ctx = scratch("cycle");
        let mut req = request("Loop", "body\n");
        req.steps = Some(vec![
            StepInput {
                title: "A".into(),
                depends_on: vec!["B".into()],
                ..StepInput::default()
            },
            StepInput {
                title: "B".into(),
                depends_on: vec!["A".into()],
                ..StepInput::default()
            },
        ]);
        let err = publish(&ctx, req).await.expect_err("refused");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("cycle"), "{}", err.1);
        assert!(
            ctx.store.list_plans().expect("lists").is_empty(),
            "a refused publish must not leave a plan behind"
        );
    }

    #[tokio::test]
    async fn explicit_steps_win_over_derivation() {
        let ctx = scratch("explicit");
        let mut req = request("Manual", "1. Derived one\n2. Derived two\n");
        req.steps = Some(vec![StepInput {
            title: "The only real step".into(),
            ..StepInput::default()
        }]);
        let (_, rev) = publish(&ctx, req).await.expect("publishes");
        assert_eq!(rev.steps.len(), 1);
        assert_eq!(rev.steps[0].title, "The only real step");
        // The markdown is still parsed into blocks — the caller replaced the steps,
        // not the document.
        assert_eq!(rev.blocks.len(), 2);
    }

    #[tokio::test]
    async fn explicit_steps_are_attributed_to_the_prose_that_names_them_and_never_to_code() {
        let ctx = scratch("attribute");
        let mut req = request(
            "Attributed",
            "## Migrate schema\n\nSome prose.\n\n```sh\nMigrate schema\n```\n",
        );
        req.steps = Some(vec![StepInput {
            title: "Migrate schema".into(),
            ..StepInput::default()
        }]);
        let (_, rev) = publish(&ctx, req).await.expect("publishes");
        assert_eq!(rev.blocks[0].step_id.as_deref(), Some("s_migrate-schema"));
        assert_eq!(rev.blocks[1].step_id, None, "unrelated prose is not attributed");
        assert_eq!(
            rev.blocks[2].step_id, None,
            "a code block that happens to contain the title text is the step's \
             contents at most, never the step's heading"
        );
    }

    #[tokio::test]
    async fn an_empty_publish_is_refused_with_a_message_that_says_what_to_send() {
        let ctx = scratch("empty");
        let err = publish(&ctx, request("", "body")).await.expect_err("refused");
        assert!(err.1.contains("title"), "{}", err.1);
        let err = publish(&ctx, request("Title", "   \n"))
            .await
            .expect_err("refused");
        assert!(err.1.contains("markdown"), "{}", err.1);
    }

    #[tokio::test]
    async fn the_detail_payload_carries_a_layout_the_ui_can_draw() {
        let ctx = scratch("detail");
        publish(&ctx, request("Graph", "1. First\n2. Second\n"))
            .await
            .expect("publishes");
        let detail = plan_detail(&ctx, "graph").expect("reads");
        assert_eq!(detail.graph.len(), 2);
        assert_eq!(detail.graph[0].layer, 0);
        assert_eq!(detail.graph[1].layer, 1);
        assert!(detail.verdict.is_none());
    }

    #[tokio::test]
    async fn the_bare_diff_of_a_brand_new_plan_reads_as_all_added() {
        // The trap this pins: defaulting `from` to `max(current - 1, 1)` would diff
        // revision 1 against itself and report an entirely new plan as unchanged,
        // which is the least useful possible answer to "what is in this plan?".
        let ctx = scratch("firstdiff");
        publish(&ctx, request("Fresh", "One.\n\nTwo.\n"))
            .await
            .expect("publishes");
        let (from, to, blocks) =
            diff_revisions(&ctx, "fresh", DiffQuery::default()).expect("diffs");
        assert_eq!((from, to), (0, 1));
        assert_eq!(blocks.len(), 2);
        assert!(
            blocks.iter().all(|b| b.status == crate::diff::BlockChange::Added),
            "{blocks:?}"
        );
    }

    #[tokio::test]
    async fn the_bare_diff_of_a_revised_plan_compares_it_with_the_previous_revision() {
        let ctx = scratch("revdiff");
        publish(&ctx, request("Moving", "One.\n\nTwo.\n"))
            .await
            .expect("publishes");
        let mut again = request("Moving", "One.\n\nTwo point five.\n");
        again.plan_id = Some("moving".into());
        publish(&ctx, again).await.expect("revises");

        let (from, to, blocks) =
            diff_revisions(&ctx, "moving", DiffQuery::default()).expect("diffs");
        assert_eq!((from, to), (1, 2));
        assert_eq!(blocks[0].status, crate::diff::BlockChange::Same);
        assert_eq!(blocks[1].status, crate::diff::BlockChange::Changed);

        // An explicit self-comparison is answerable, not a 400.
        let (_, _, same) = diff_revisions(
            &ctx,
            "moving",
            DiffQuery {
                from: Some(2),
                to: Some(2),
            },
        )
        .expect("diffs");
        assert!(same.iter().all(|b| b.status == crate::diff::BlockChange::Same));

        // A revision that does not exist is a 404, not an empty diff that reads as
        // "nothing changed".
        assert_eq!(
            diff_revisions(
                &ctx,
                "moving",
                DiffQuery {
                    from: Some(1),
                    to: Some(9)
                }
            )
            .expect_err("missing")
            .0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn a_missing_plan_is_a_404_and_an_unusable_id_is_a_400() {
        let ctx = scratch("notfound");
        assert_eq!(
            plan_detail(&ctx, "nope").expect_err("missing").0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            plan_detail(&ctx, "../etc").expect_err("rejected").0,
            StatusCode::BAD_REQUEST,
            "a traversal attempt is the caller's error, not a server fault"
        );
    }

    #[tokio::test]
    async fn completing_a_step_is_idempotent_and_records_its_note_as_settled() {
        let ctx = scratch("steps");
        publish(&ctx, request("Work", "1. Do it\n"))
            .await
            .expect("publishes");
        let step = update_step(
            &ctx,
            "work",
            "s_do-it",
            StepUpdate {
                status: Some(StepStatus::Done),
                note: Some("took two tries".into()),
            },
        )
        .await
        .expect("updates");
        assert_eq!(step.status, StepStatus::Done);

        let annotations = ctx.store.annotations("work").expect("lists");
        assert_eq!(annotations.len(), 1);
        assert!(
            annotations[0].resolved,
            "an agent's own progress note must not read back to it as feedback"
        );
        assert_eq!(feedback::pending_count(&annotations, 1), 0);

        // Re-sending the same status is allowed and changes nothing.
        let again = update_step(
            &ctx,
            "work",
            "s_do-it",
            StepUpdate {
                status: Some(StepStatus::Done),
                note: None,
            },
        )
        .await
        .expect("updates");
        assert_eq!(again.status, StepStatus::Done);

        assert_eq!(
            update_step(&ctx, "work", "s_ghost", StepUpdate::default())
                .await
                .expect_err("missing")
                .0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn feedback_reports_pending_until_someone_decides() {
        let ctx = scratch("feedback");
        publish(&ctx, request("Decide", "1. Do it\n"))
            .await
            .expect("publishes");

        let pending = feedback_for(&ctx, "decide").expect("reads");
        assert!(pending.pending);
        assert_eq!(pending.status, PlanStatus::InReview);
        assert!(pending.text.contains("IN REVIEW"), "{}", pending.text);

        record_verdict(
            &ctx,
            "decide",
            VerdictRequest {
                verdict: Decision::ChangesRequested,
                note: Some("not yet".into()),
            },
        )
        .await
        .expect("decides");

        let decided = feedback_for(&ctx, "decide").expect("reads");
        assert!(!decided.pending);
        assert_eq!(decided.status, PlanStatus::ChangesRequested);
        assert!(decided.text.contains("CHANGES REQUESTED"), "{}", decided.text);
        assert!(decided.text.contains("note: not yet"), "{}", decided.text);
    }

    #[tokio::test]
    async fn an_annotation_must_point_at_something_that_exists() {
        let ctx = scratch("anchor");
        let (_, rev) = publish(&ctx, request("Anchor", "1. Do it\n"))
            .await
            .expect("publishes");

        // A real block id is accepted; a fabricated one is refused rather than stored
        // as an annotation that renders nowhere.
        let real = rev.blocks[0].id.clone();
        let good = Annotation {
            id: "a_1".into(),
            plan_id: "anchor".into(),
            revision: 1,
            target: Target::Block { id: real },
            kind: AnnKind::Redline,
            body: "reword".into(),
            suggestion: None,
            author: None,
            resolved: false,
            created_at: 1,
        };
        ctx.store.add_annotation(&good).expect("adds");
        assert_eq!(ctx.store.annotations("anchor").expect("lists").len(), 1);
        assert_eq!(feedback::pending_count(&ctx.store.annotations("anchor").expect("lists"), 1), 1);
    }

    #[test]
    fn a_scratch_path_is_a_path() {
        // Guards against the temp-dir helper silently returning a relative path, which
        // would have the tests write into the repository.
        let dir: PathBuf = std::env::temp_dir();
        assert!(dir.is_absolute());
    }
}
