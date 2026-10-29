//! The diff and feedback routes as a caller reaches them: through the real
//! router, over loopback HTTP, against a scratch store.

use std::sync::Arc;

use ryu_blueprint::api::{publish, routes, Ctx, PublishRequest};
use ryu_blueprint::host::Events;
use ryu_blueprint::store::Store;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn scratch(name: &str) -> Arc<Ctx> {
    let dir = std::env::temp_dir().join(format!("ryu-blueprint-http-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    Arc::new(Ctx {
        store: Store::open(dir).expect("opens"),
        events: Events::from_env(),
    })
}

fn revision(id: &str, markdown: &str) -> PublishRequest {
    PublishRequest {
        plan_id: Some(id.into()),
        title: "Moving".into(),
        markdown: markdown.into(),
        ..PublishRequest::default()
    }
}

struct Served {
    address: std::net::SocketAddr,
    server: tokio::task::JoinHandle<()>,
}

async fn serve(ctx: Arc<Ctx>) -> Served {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        axum::serve(listener, routes(ctx)).await.expect("serves");
    });
    Served { address, server }
}

async fn call(served: &Served, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let body = body.map(|value| value.to_string()).unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        served.address,
        body.len()
    );
    let mut socket = tokio::net::TcpStream::connect(served.address)
        .await
        .expect("connects");
    socket.write_all(request.as_bytes()).await.expect("writes");
    let mut raw = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        socket.read_to_end(&mut raw),
    )
    .await
    .expect("answers in time")
    .expect("reads");
    let raw = String::from_utf8(raw).expect("utf-8");
    let status = raw
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    let (_, payload) = raw.split_once("\r\n\r\n").expect("a body");
    // Small JSON answers arrive in one chunk; tolerate chunked framing too.
    let payload = payload
        .find('{')
        .and_then(|start| payload.rfind('}').map(|end| &payload[start..=end]))
        .unwrap_or("null");
    (status, serde_json::from_str(payload).expect("JSON body"))
}

fn statuses(diff: &Value) -> Vec<&str> {
    diff["blocks"]
        .as_array()
        .expect("blocks")
        .iter()
        .map(|block| block["status"].as_str().expect("status"))
        .collect()
}

#[tokio::test]
async fn the_diff_route_compares_the_requested_revisions() {
    let ctx = scratch("diff");
    publish(&ctx, revision("moving", "One.\n\nTwo.\n"))
        .await
        .expect("publishes");
    publish(
        &ctx,
        revision("moving", "One.\n\nTwo point five.\n\nThree.\n"),
    )
    .await
    .expect("revises");
    let served = serve(ctx).await;

    let (status, bare) = call(&served, "GET", "/plans/moving/diff", None).await;
    assert_eq!(status, 200, "{bare}");
    assert_eq!(
        (bare["from"].as_u64(), bare["to"].as_u64()),
        (Some(1), Some(2))
    );
    assert_eq!(statuses(&bare), ["same", "changed", "added"]);

    let (status, first) = call(&served, "GET", "/plans/moving/diff?from=0&to=1", None).await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(statuses(&first), ["added", "added"]);

    let (status, missing) = call(&served, "GET", "/plans/moving/diff?from=1&to=7", None).await;
    assert_eq!(status, 404, "{missing}");
    assert!(missing["error"]
        .as_str()
        .expect("message")
        .contains("revision 7"));

    let (status, unknown) = call(&served, "GET", "/plans/nope/diff", None).await;
    assert_eq!(status, 404, "{unknown}");
    assert!(unknown["error"].as_str().expect("message").contains("nope"));

    let (status, _) = call(&served, "GET", "/plans/Bad..Id/diff", None).await;
    assert_eq!(status, 400, "an unusable id is the caller's error");
    served.server.abort();
}

#[tokio::test]
async fn the_feedback_route_reports_only_the_current_revisions_open_annotations() {
    let ctx = scratch("feedback");
    let (_, first) = publish(&ctx, revision("decide", "1. Do it\n"))
        .await
        .expect("publishes");
    let served = serve(ctx.clone()).await;

    let (status, _) = call(
        &served,
        "POST",
        "/plans/decide/annotations",
        Some(json!({
            "target": {"type": "block", "id": first.blocks[0].id},
            "kind": "blocker",
            "body": "stale objection about revision one",
        })),
    )
    .await;
    assert_eq!(status, 200);

    let (_, second) = publish(&ctx, revision("decide", "1. Do it\n2. Check it\n"))
        .await
        .expect("revises");
    // A progress note is filed as a resolved comment: on the record, not feedback.
    let step = &second.steps.first().expect("a step").id;
    let (status, noted) = call(
        &served,
        "POST",
        &format!("/plans/decide/steps/{step}"),
        Some(json!({"status": "in_progress", "note": "progress note from the agent"})),
    )
    .await;
    assert_eq!(status, 200, "{noted}");
    for (kind, body) in [
        ("question", "is checking idempotent?"),
        ("comment", "looks fine otherwise"),
    ] {
        let (status, created) = call(
            &served,
            "POST",
            "/plans/decide/annotations",
            Some(json!({"target": {"type": "plan"}, "kind": kind, "body": body})),
        )
        .await;
        assert_eq!(status, 200, "{created}");
    }

    let (status, feedback) = call(&served, "GET", "/plans/decide/feedback", None).await;
    assert_eq!(status, 200, "{feedback}");
    assert_eq!(feedback["revision"], 2);
    assert_eq!(feedback["pending"], true);
    assert_eq!(feedback["annotation_count"], 2);
    let text = feedback["text"].as_str().expect("text");
    assert!(text.contains("is checking idempotent?"), "{text}");
    assert!(text.contains("looks fine otherwise"), "{text}");
    assert!(
        !text.contains("progress note from the agent"),
        "a resolved note is not open feedback: {text}"
    );
    assert!(
        !text.contains("stale objection"),
        "an annotation on a superseded revision is not feedback on this one: {text}"
    );

    let (status, unknown) = call(&served, "GET", "/plans/ghost/feedback", None).await;
    assert_eq!(status, 404, "{unknown}");
    served.server.abort();
}
