//! `ryu-blueprint` — the out-of-process visual-plan-review sidecar.
//!
//! Core spawns it (`kind: local`, sibling on `PATH` or `RYU_BLUEPRINT_BIN`),
//! health-checks it, and proxies `/api/blueprint/*` to it on loopback, exactly like
//! `ryu-reasoning` / `ryu-monitors`. The parser, the graph, the store and the handlers
//! live in the crate lib; this binary is only the process shell around them.
//!
//! SECURITY: loopback-only bind (127.0.0.1) plus a shared-secret bearer gate
//! (`RYU_EXT_TOKEN`, injected by Core at spawn and re-stamped on every proxied hop).
//! Every `/api/blueprint/*` route is protected and the gate is FAIL-CLOSED: with no
//! token configured, every protected route rejects with 401. `/health` is the one
//! un-gated route, so Core's pre-auth probe succeeds; it returns no plan data.
//!
//! Port: `RYU_BLUEPRINT_PORT`, default 8011. Data: `$RYU_DIR/blueprint`, so plans land
//! under the same node directory Core uses.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::extract::Request;
use axum::http::{header::AUTHORIZATION, StatusCode};
use axum::middleware::{from_fn, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use ryu_blueprint::api::{routes, Ctx};
use ryu_blueprint::host::Events;
use ryu_blueprint::store::{data_dir, Store};

/// Default loopback port, kept identical to `apps-store/blueprint/manifest.json`.
const DEFAULT_PORT: u16 = 8011;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stderr, never stdout: in `mcp` mode stdout carries the JSON-RPC stream, and a
    // log line written into it desynchronizes the framing on the client side.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let port: u16 = std::env::var("RYU_BLUEPRINT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let token = std::env::var("RYU_EXT_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    if token.is_none() {
        tracing::warn!(
            "ryu-blueprint: no RYU_EXT_TOKEN set; every /api/blueprint/* route is FAIL-CLOSED \
             (401) until Core spawns this sidecar with one"
        );
    }

    let events = Events::from_env();
    if !events.is_hosted() {
        tracing::warn!(
            "ryu-blueprint: not Core-hosted, so app events (plan.published, plan.approved, …) \
             will no-op; reviewing still works end to end"
        );
    }

    let store = Store::open(data_dir())?;
    let ctx = Arc::new(Ctx { store, events });

    // `ryu-blueprint mcp` speaks MCP on stdio instead of serving HTTP — the same store
    // and the same operations, reached the way an agent or a workflow reaches any
    // other tool server. Core spawns this form from the manifest's `mcp_servers`
    // block; nothing binds a port in this mode, which is why the check comes first.
    if std::env::args().nth(1).as_deref() == Some("mcp") {
        return ryu_blueprint::mcp::serve(ctx).await;
    }

    // `/openapi.json` rides INSIDE the same bearer gate as `/api/blueprint/*`, at the
    // SERVER ROOT. Core fetches `http://127.0.0.1:<port>/openapi.json` on this
    // sidecar's first Healthy edge and lowers every operation it finds into searchable
    // LLM tools. That is what gives the six routes with NO `mcp_servers` twin
    // (revisions, diff, annotations, verdict, delete) any agent reach at all.
    //
    // Root, not under `/api/blueprint`: Core tries the root FIRST, and keeping the
    // document off the mount keeps it out of the manifest's declared `http.routes[]` —
    // anything declared there is reachable through the generic ext-proxy, and the
    // schema is Core's to read, not an app surface. Inside the gate, not next to the
    // un-gated `/health`: Core stamps the injected `RYU_EXT_TOKEN` on the fetch, so the
    // gate costs the fetcher nothing — while un-gated it would disclose this app's
    // entire internal API surface to any other process on loopback.
    let protected = Router::new()
        .nest("/api/blueprint", routes(ctx))
        .route(
            "/openapi.json",
            get(|| async { Json(ryu_blueprint::api::openapi()) }),
        )
        .layer(from_fn(move |req: Request, next: Next| {
            let expected = token.clone();
            async move { bearer_gate(expected.as_deref(), req, next).await }
        }));

    let app = Router::new().route("/health", get(health)).merge(protected);

    // LOOPBACK ONLY (belt) + shared-secret bearer (suspenders): Core is the auth front
    // and re-stamps the bearer on the proxied hop.
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "ryu-blueprint listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// The pre-auth probe Core polls before it will proxy anything. Deliberately says
/// nothing about plans: it answers before the bearer gate has had a chance to.
async fn health() -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "name": "ryu-blueprint",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Shared-secret bearer gate. Core stamps `authorization: Bearer <RYU_EXT_TOKEN>` on
/// the loopback hop, so a request that did NOT come through Core has no way to present
/// it. Fail-closed when no token is configured.
async fn bearer_gate(expected: Option<&str>, req: Request, next: Next) -> Response {
    let provided = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if bearer_ok(provided, expected) {
        return next.run(req).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthorized" })),
    )
        .into_response()
}

/// Pure bearer check, factored out so the auth decision is unit-testable without a
/// server. Constant-time comparison: the token is a secret, and a length- or
/// prefix-sensitive compare leaks it a byte at a time.
fn bearer_ok(provided: Option<&str>, expected: Option<&str>) -> bool {
    ryu_sidecar_runtime::token_ok(provided, expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_is_fail_closed_without_a_configured_token() {
        // The posture that separates this sidecar from the four Python ones that
        // shipped unauthenticated: no token means nothing gets in, rather than
        // everything.
        assert!(!bearer_ok(Some("anything"), None));
        assert!(!bearer_ok(None, None));
    }

    #[test]
    fn only_the_exact_token_passes() {
        assert!(bearer_ok(Some("s3cret"), Some("s3cret")));
        assert!(!bearer_ok(Some("s3cre"), Some("s3cret")));
        assert!(!bearer_ok(Some("s3cretx"), Some("s3cret")));
        assert!(!bearer_ok(None, Some("s3cret")));
        assert!(!bearer_ok(Some(""), Some("s3cret")));
    }

    #[test]
    fn the_default_port_matches_the_one_the_manifest_declares() {
        // Core injects `RYU_BLUEPRINT_PORT` from the manifest, so a mismatch only
        // shows up when someone runs the binary by hand and then cannot find it.
        assert_eq!(DEFAULT_PORT, 8011);
    }
}
