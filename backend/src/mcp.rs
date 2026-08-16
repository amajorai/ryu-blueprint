//! `ryu-blueprint mcp` — the same operations, spoken as an MCP stdio server.
//!
//! This is the seam that makes the app reachable from an **agent** and from a
//! **workflow** without Core knowing anything about it. The manifest declares the
//! server under `mcp_servers`, Core spawns it like any other MCP process, and the tools
//! appear as `blueprint__plan_publish`, `blueprint__plan_status`, … — which is exactly
//! the `<server>__<tool>` id a workflow's `mcp` node takes.
//!
//! **Tool names register bare.** The `blueprint__` prefix is Core's, formed from the
//! `mcp_servers` key; a tool that names itself `blueprint__plan_publish` here would be
//! published as `blueprint__blueprint__plan_publish` and never be called by anything.
//!
//! # The one thing that must not go wrong
//!
//! `plan_status` waits for a human, and humans are slow. The obvious implementation —
//! block until someone decides — is the wrong one twice over: MCP clients time out (and
//! a timed-out call looks to the agent like a *failure*, not a "not yet"), and a request
//! that never returns pins a connection until the process dies. So `wait_secs` is
//! clamped to 0..=60, the loop always checks before it sleeps, and a timeout returns a
//! perfectly good `in_review` answer that says "ask again". Waiting is the caller's job,
//! done in slices this tool can honour.
//!
//! Framing is newline-delimited JSON-RPC 2.0 over stdin/stdout, protocol `2024-11-05` —
//! what Core's MCP client speaks.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::api::{self, Ctx, FeedbackPayload, PublishRequest, StepUpdate};
use crate::model::{StepInput, StepStatus};

const PROTOCOL_VERSION: &str = "2024-11-05";

/// The hard ceiling on `plan_status.wait_secs`. Under a typical MCP client timeout
/// with room to spare, so a caller that asks for the maximum still gets an answer
/// rather than a transport error.
pub const MAX_WAIT_SECS: u64 = 60;

/// How often the wait loop re-reads the verdict. Storage is a handful of small JSON
/// files, so this is cheap; half a second is below the threshold at which a human
/// clicking "approve" notices the agent taking a moment to react.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Where a human goes to review the plan. Handed to the agent so it can say where it
/// is waiting instead of "check the app".
fn review_url(plan_id: &str) -> String {
    format!("ryu://apps/@ryu/blueprint/plans/{plan_id}")
}

/// Tool descriptors, in the shape `tools/list` returns. Names are BARE — see the
/// module docs.
fn tool_list() -> Value {
    json!([
        {
            "name": "plan_publish",
            "description":
                "Publish a plan as markdown for a human to review, and get back a link they can \
                 open. The markdown is split into addressable blocks and, unless you pass `steps` \
                 yourself, a step graph is derived from numbered or checklist items (or `##` \
                 headings): each item becomes a step depending on the one before it, and you can \
                 override that inline with `(after: some other step)` or attach paths with \
                 `files: a.rs, b.rs`. Passing the same `plan_id` again publishes a NEW REVISION of \
                 that plan and returns it to review — use it after acting on feedback, rather than \
                 publishing a second plan. Then poll `plan_status` for the verdict.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short name for the plan, shown in the reviewer's list." },
                    "markdown": { "type": "string", "description": "The plan itself. Headings, prose, numbered steps, code and mermaid blocks all render." },
                    "plan_id": { "type": "string", "description": "Revise this existing plan instead of creating a new one." },
                    "steps": {
                        "type": "array",
                        "description": "Explicit steps, which override the ones derived from the markdown. Use when you already know the dependency structure.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "title": { "type": "string" },
                                "summary": { "type": "string" },
                                "depends_on": {
                                    "type": "array", "items": { "type": "string" },
                                    "description": "Step ids or step titles that must finish first."
                                },
                                "files": { "type": "array", "items": { "type": "string" } },
                                "risk": { "type": "string" }
                            },
                            "required": ["title"]
                        }
                    },
                    "artifact_html": { "type": "string", "description": "An optional rendered artifact shown beside the plan." },
                    "conversation_id": { "type": "string", "description": "The conversation this plan belongs to, so the review surface can link back to it." }
                },
                "required": ["title", "markdown"]
            }
        },
        {
            "name": "plan_status",
            "description":
                "Read the verdict on a published plan, as text you can act on directly: the \
                 decision, the reviewer's note, and every unresolved annotation with what it is \
                 attached to. Pass `wait_secs` (0-60) to wait for a decision instead of returning \
                 immediately; it returns as soon as someone decides, and returns \
                 status \"in_review\" if the wait runs out — that is not an error, just call it \
                 again. When the status is \"changes_requested\", revise the plan and publish it \
                 with the same plan_id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "plan_id": { "type": "string" },
                    "wait_secs": {
                        "type": "integer", "minimum": 0, "maximum": 60,
                        "description": "Seconds to wait for a human decision before answering anyway. Defaults to 0."
                    }
                },
                "required": ["plan_id"]
            }
        },
        {
            "name": "plan_get",
            "description":
                "Fetch a plan in full: its current revision's blocks and steps, every annotation \
                 on it, the verdict if there is one, and the laid-out dependency graph. Use this \
                 when you need the block ids or the step structure; `plan_status` is the shorter \
                 answer when you only want to know whether you may proceed.",
            "inputSchema": {
                "type": "object",
                "properties": { "plan_id": { "type": "string" } },
                "required": ["plan_id"]
            }
        },
        {
            "name": "step_update",
            "description":
                "Mark a step of the current revision as todo, in_progress, done or blocked, so the \
                 human watching the graph can see where you are. Completing a step raises an event \
                 other apps and workflows can react to. This changes progress only — it never \
                 edits the plan; publish a revision for that.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "plan_id": { "type": "string" },
                    "step_id": { "type": "string" },
                    "status": { "type": "string", "enum": ["todo", "in_progress", "done", "blocked"] }
                },
                "required": ["plan_id", "step_id", "status"]
            }
        }
    ])
}

/// Serve MCP on stdin/stdout until the stream closes.
///
/// # Errors
/// Only on an I/O failure reading stdin or writing stdout; a bad frame or a failing
/// tool is answered, not propagated.
pub async fn serve(ctx: Arc<Ctx>) -> Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<Value>(line) else {
            continue; // A frame we cannot parse has no id to answer on.
        };
        let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
        // Notifications carry no id and take no response.
        let Some(id) = frame.get("id").cloned() else {
            continue;
        };

        let response = match method {
            "initialize" => json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "ryu-blueprint", "version": env!("CARGO_PKG_VERSION") }
            }),
            "ping" => json!({}),
            "tools/list" => json!({ "tools": tool_list() }),
            "tools/call" => {
                let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match call_tool(&ctx, name, args).await {
                    Ok(value) => tool_result(&value, false),
                    Err(e) => tool_result(&json!({ "error": e.to_string() }), true),
                }
            }
            other => {
                write_frame(
                    &mut stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": format!("unknown method '{other}'") }
                    }),
                )
                .await?;
                continue;
            }
        };
        write_frame(
            &mut stdout,
            &json!({ "jsonrpc": "2.0", "id": id, "result": response }),
        )
        .await?;
    }
    Ok(())
}

/// MCP returns tool output as content blocks; JSON goes in a text block so a client
/// that only renders text still shows something readable.
fn tool_result(value: &Value, is_error: bool) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
        }],
        "isError": is_error
    })
}

/// Clamp `wait_secs` into 0..=[`MAX_WAIT_SECS`].
///
/// Written to survive anything a model puts in the field — a float, a string, a
/// negative, a number bigger than `u64` — because the alternative to clamping is a
/// request that outlives its client's timeout, and the agent then reads a transport
/// error where the truth was "nobody has decided yet".
#[must_use]
pub fn clamp_wait_secs(raw: Option<&Value>) -> u64 {
    let Some(value) = raw else {
        return 0;
    };
    let seconds = value
        .as_i64()
        .map(|n| n as f64)
        .or_else(|| value.as_f64())
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<f64>().ok()));
    match seconds {
        Some(n) if n.is_finite() && n > 0.0 => (n as u64).min(MAX_WAIT_SECS),
        _ => 0,
    }
}

/// Read the verdict, optionally waiting up to `wait_secs` for one to appear.
///
/// **Checks before it sleeps**, so `wait_secs = 0` costs exactly one read and no delay.
/// A loop that slept first would add half a second to every poll of an already-decided
/// plan, which is the shape that turns a tight agent loop into a slow one.
///
/// # Errors
/// Propagates the read error when the plan or its revision is missing.
pub async fn wait_for_verdict(
    ctx: &Arc<Ctx>,
    plan_id: &str,
    wait_secs: u64,
) -> Result<FeedbackPayload> {
    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    loop {
        let payload = api::feedback_for(ctx, plan_id).map_err(|e| anyhow!("{}", e.1))?;
        if !payload.pending {
            return Ok(payload);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            // Not an error: "still in review" is a true and useful answer.
            return Ok(payload);
        }
        tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
    }
}

async fn call_tool(ctx: &Arc<Ctx>, name: &str, args: Value) -> Result<Value> {
    let required = |key: &str| -> Result<String> {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| anyhow!("{key} is required"))
    };
    let optional = |key: &str| -> Option<String> {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|v| !v.trim().is_empty())
    };

    match name {
        "plan_publish" => {
            let steps: Option<Vec<StepInput>> = match args.get("steps") {
                Some(Value::Null) | None => None,
                Some(value) => Some(serde_json::from_value(value.clone()).map_err(|e| {
                    anyhow!("`steps` is not in the expected shape ({e}) — each entry needs a title")
                })?),
            };
            let request = PublishRequest {
                plan_id: optional("plan_id"),
                title: required("title")?,
                markdown: required("markdown")?,
                steps,
                artifact_html: optional("artifact_html"),
                conversation_id: optional("conversation_id"),
                agent_id: optional("agent_id"),
                // Stamped so a reviewer can see a plan arrived over MCP rather than
                // from the companion's own editor.
                source: optional("source").or_else(|| Some("mcp".to_owned())),
            };
            let (plan, revision) = api::publish(ctx, request)
                .await
                .map_err(|e| anyhow!("{}", e.1))?;
            Ok(json!({
                "plan_id": plan.id,
                "revision": revision.revision,
                "status": plan.status.as_str(),
                "review_url": review_url(&plan.id),
            }))
        }
        "plan_status" => {
            let plan_id = required("plan_id")?;
            let wait = clamp_wait_secs(args.get("wait_secs"));
            let payload = wait_for_verdict(ctx, &plan_id, wait).await?;
            Ok(json!({
                "status": payload.status.as_str(),
                "verdict": payload.verdict,
                "feedback": payload.text,
                "annotation_count": payload.annotation_count,
                "revision": payload.revision,
            }))
        }
        "plan_get" => {
            let plan_id = required("plan_id")?;
            let detail = api::plan_detail(ctx, &plan_id).map_err(|e| anyhow!("{}", e.1))?;
            Ok(serde_json::to_value(detail)?)
        }
        "step_update" => {
            let plan_id = required("plan_id")?;
            let step_id = required("step_id")?;
            let status: StepStatus = serde_json::from_value(json!(required("status")?))
                .map_err(|_| anyhow!("status must be one of todo, in_progress, done, blocked"))?;
            let step = api::update_step(
                ctx,
                &plan_id,
                &step_id,
                StepUpdate {
                    status: Some(status),
                    note: optional("note"),
                },
            )
            .await
            .map_err(|e| anyhow!("{}", e.1))?;
            Ok(json!({ "step": step }))
        }
        other => Err(anyhow!(
            "unknown tool '{other}' — this server offers plan_publish, plan_status, \
             plan_get and step_update"
        )),
    }
}

async fn write_frame(out: &mut tokio::io::Stdout, frame: &Value) -> Result<()> {
    let mut line = serde_json::to_string(frame)?;
    line.push('\n');
    out.write_all(line.as_bytes()).await?;
    out.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Events;
    use crate::store::Store;

    fn scratch(name: &str) -> Arc<Ctx> {
        let dir = std::env::temp_dir().join(format!("ryu-blueprint-mcp-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        Arc::new(Ctx {
            store: Store::open(dir).expect("opens"),
            events: Events::from_env(),
        })
    }

    #[test]
    fn the_four_contracted_tools_are_declared_bare_with_usable_schemas() {
        let tools = tool_list();
        let tools = tools.as_array().expect("array");
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(
            names,
            vec!["plan_publish", "plan_status", "plan_get", "step_update"]
        );
        for tool in tools {
            let name = tool["name"].as_str().expect("name");
            assert!(
                !name.starts_with("blueprint__"),
                "{name} self-prefixes — Core adds `blueprint__`, so this would register \
                 as blueprint__blueprint__{name} and never be called"
            );
            assert!(
                tool["description"].as_str().is_some_and(|d| d.len() > 40),
                "{name} needs a description that says when to use it"
            );
            assert_eq!(
                tool["inputSchema"]["type"], "object",
                "{name} needs an object input schema"
            );
        }
    }

    #[test]
    fn wait_secs_is_clamped_to_the_documented_window() {
        assert_eq!(clamp_wait_secs(None), 0);
        assert_eq!(clamp_wait_secs(Some(&json!(0))), 0);
        assert_eq!(clamp_wait_secs(Some(&json!(10))), 10);
        assert_eq!(clamp_wait_secs(Some(&json!(60))), 60);
        // The one that matters: an agent asking to wait "until it is done" must not be
        // able to hold the connection past its own client's timeout.
        assert_eq!(clamp_wait_secs(Some(&json!(3600))), MAX_WAIT_SECS);
        assert_eq!(clamp_wait_secs(Some(&json!(u64::MAX))), MAX_WAIT_SECS);
        assert_eq!(clamp_wait_secs(Some(&json!(-5))), 0);
        assert_eq!(clamp_wait_secs(Some(&json!(2.9))), 2);
        assert_eq!(clamp_wait_secs(Some(&json!(f64::INFINITY))), 0);
        // Models put strings in integer fields.
        assert_eq!(clamp_wait_secs(Some(&json!("30"))), 30);
        assert_eq!(clamp_wait_secs(Some(&json!("soon"))), 0);
        assert_eq!(clamp_wait_secs(Some(&json!(null))), 0);
    }

    #[tokio::test]
    async fn a_zero_wait_returns_immediately_and_says_it_is_still_in_review() {
        let ctx = scratch("nowait");
        api::publish(
            &ctx,
            PublishRequest {
                title: "Waiting".into(),
                markdown: "1. Do it\n".into(),
                ..PublishRequest::default()
            },
        )
        .await
        .expect("publishes");

        let started = Instant::now();
        let payload = wait_for_verdict(&ctx, "waiting", 0).await.expect("answers");
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "a zero wait must check once and return — a loop that sleeps first adds \
             latency to every poll of an already-decided plan"
        );
        assert!(payload.pending);
        assert_eq!(payload.status.as_str(), "in_review");
        assert!(payload.text.contains("IN REVIEW"), "{}", payload.text);
    }

    #[tokio::test]
    async fn a_wait_that_runs_out_answers_rather_than_failing() {
        let ctx = scratch("timeout");
        api::publish(
            &ctx,
            PublishRequest {
                title: "Slow".into(),
                markdown: "1. Do it\n".into(),
                ..PublishRequest::default()
            },
        )
        .await
        .expect("publishes");

        let started = Instant::now();
        let payload = wait_for_verdict(&ctx, "slow", 1).await.expect("answers");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(900),
            "it should have waited about a second, waited {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "it must not overshoot the requested window, waited {elapsed:?}"
        );
        assert!(payload.pending, "a timeout is 'not yet', not a verdict");
    }

    #[tokio::test]
    async fn a_decided_plan_returns_before_the_wait_elapses() {
        let ctx = scratch("decided");
        api::publish(
            &ctx,
            PublishRequest {
                title: "Fast".into(),
                markdown: "1. Do it\n".into(),
                ..PublishRequest::default()
            },
        )
        .await
        .expect("publishes");
        api::record_verdict(
            &ctx,
            "fast",
            api::VerdictRequest {
                verdict: crate::model::Decision::Approved,
                note: Some("go".into()),
            },
        )
        .await
        .expect("approves");

        let started = Instant::now();
        let payload = wait_for_verdict(&ctx, "fast", 60).await.expect("answers");
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "an already-decided plan must not wait out the window"
        );
        assert!(!payload.pending);
        assert!(payload.text.contains("APPROVED"), "{}", payload.text);
    }

    #[tokio::test]
    async fn publishing_over_mcp_returns_a_link_and_an_unknown_tool_says_what_exists() {
        let ctx = scratch("dispatch");
        let out = call_tool(
            &ctx,
            "plan_publish",
            json!({ "title": "Ship it", "markdown": "1. Do it\n" }),
        )
        .await
        .expect("publishes");
        assert_eq!(out["plan_id"], "ship-it");
        assert_eq!(out["revision"], 1);
        assert_eq!(out["status"], "in_review");
        assert_eq!(out["review_url"], "ryu://apps/@ryu/blueprint/plans/ship-it");

        let err = call_tool(&ctx, "plan_delete", json!({}))
            .await
            .expect_err("unknown tool");
        assert!(err.to_string().contains("plan_publish"), "{err}");

        let err = call_tool(&ctx, "plan_status", json!({}))
            .await
            .expect_err("missing arg");
        assert!(err.to_string().contains("plan_id"), "{err}");
    }

    #[tokio::test]
    async fn step_update_takes_the_contracted_status_strings_and_rejects_others() {
        let ctx = scratch("stepstatus");
        call_tool(
            &ctx,
            "plan_publish",
            json!({ "title": "Track", "markdown": "1. Do it\n" }),
        )
        .await
        .expect("publishes");

        let out = call_tool(
            &ctx,
            "step_update",
            json!({ "plan_id": "track", "step_id": "s_do-it", "status": "in_progress" }),
        )
        .await
        .expect("updates");
        assert_eq!(out["step"]["status"], "in_progress");

        let err = call_tool(
            &ctx,
            "step_update",
            json!({ "plan_id": "track", "step_id": "s_do-it", "status": "In Progress" }),
        )
        .await
        .expect_err("bad status");
        assert!(err.to_string().contains("in_progress"), "{err}");
    }

    #[test]
    fn a_tool_error_is_marked_as_one() {
        let result = tool_result(&json!({ "error": "boom" }), true);
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("boom"));
    }
}
