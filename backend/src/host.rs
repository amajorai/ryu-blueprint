//! The sidecar's one line back into Core: app events.
//!
//! Blueprint has no model callback and wants none — every derivation in this crate is
//! deterministic, and a plan whose graph changed shape because a completion came back
//! differently would be worse than no graph. So unlike the reasoning sidecar, whose
//! `host.rs` is a `/api/host/model/complete` client, this module holds the *only*
//! outbound call blueprint makes: raising the four events its manifest declares under
//! `contributes.hook_events`.
//!
//! ```text
//! POST http://127.0.0.1:$RYU_CORE_PORT/api/host/capability/events.emit
//!   authorization: Bearer $RYU_EXT_TOKEN
//!   x-ryu-plugin-id: @ryu/blueprint
//!   { "event": "@ryu/blueprint#plan.approved", "payload": { … } }
//! ```
//!
//! The dial itself is not hand-rolled here. `ryu-app-events` owns it, and that is
//! load-bearing: it reads `RYU_CORE_PORT` first and takes only the *port* half of a
//! `RYU_BIND` fallback, because a release install has no `RYU_BIND` at all and when it
//! does it is routinely `0.0.0.0:7980`, which is not a dialable address. That bug
//! shipped green once already; re-implementing the resolution here would be a second
//! chance to ship it.
//!
//! # Why this never fails a request
//!
//! A plan that was approved has been approved whether or not a workflow was listening.
//! Emitting is best-effort: [`ryu_app_events::EventEmitter::emit`] logs and swallows,
//! and `NotHosted` — the state of every `cargo test` run and every standalone dev
//! launch — is a supported no-op rather than a startup error. Nothing in this module
//! returns a `Result` for that reason.

use ryu_app_events::EventEmitter;
use serde_json::json;

/// The plugin id events are namespaced to. Core re-checks on every emit that the
/// authenticated caller *is* this plugin and that the event appears in this plugin's
/// own manifest, so an id that disagrees with `manifest.json` is a 403, not a rename.
pub const PLUGIN_ID: &str = "@ryu/blueprint";

/// The four declared events. Spelled as constants rather than inline strings because
/// each one has to match a `contributes.hook_events[].id` exactly; a typo is a silent
/// no-op with a warning in a log nobody is reading.
pub const EVENT_PLAN_PUBLISHED: &str = "@ryu/blueprint#plan.published";
pub const EVENT_PLAN_APPROVED: &str = "@ryu/blueprint#plan.approved";
pub const EVENT_PLAN_CHANGES_REQUESTED: &str = "@ryu/blueprint#plan.changes_requested";
pub const EVENT_STEP_COMPLETED: &str = "@ryu/blueprint#step.completed";

/// Raises blueprint's app events. Cheap to clone; build one at startup.
#[derive(Debug, Clone)]
pub struct Events {
    emitter: EventEmitter,
}

impl Default for Events {
    fn default() -> Self {
        Self::from_env()
    }
}

impl Events {
    /// Build from the environment Core injects at spawn. Never fails: outside Core
    /// every emit is a no-op, which is what keeps this crate's tests runnable with no
    /// host in sight.
    #[must_use]
    pub fn from_env() -> Events {
        Events {
            emitter: EventEmitter::from_env(PLUGIN_ID),
        }
    }

    /// Whether a Core is actually reachable. Only useful for diagnostics — the emit
    /// methods handle the unhosted case themselves.
    #[must_use]
    pub fn is_hosted(&self) -> bool {
        self.emitter.is_hosted()
    }

    /// A plan was published or revised. `conversation_id` is threaded through when the
    /// publisher knew one, so a consumer hook can key per-conversation state exactly
    /// as a turn hook does. Also raises a user notification: a published plan is
    /// review work waiting in the Inbox.
    pub async fn plan_published(
        &self,
        plan_id: &str,
        revision: u32,
        title: &str,
        step_count: usize,
        conversation_id: Option<&str>,
    ) {
        self.raise_with_notify(
            EVENT_PLAN_PUBLISHED,
            json!({
                "plan_id": plan_id,
                "revision": revision,
                "title": title,
                "step_count": step_count,
            }),
            conversation_id,
            Some(ryu_app_events::NotifyHint::info(
                format!("Plan “{title}” awaits review"),
                Some(format!("{step_count} steps in revision {revision}")),
            )),
        )
        .await;
    }

    pub async fn plan_approved(
        &self,
        plan_id: &str,
        revision: u32,
        note: Option<&str>,
        conversation_id: Option<&str>,
    ) {
        self.raise(
            EVENT_PLAN_APPROVED,
            json!({ "plan_id": plan_id, "revision": revision, "note": note }),
            conversation_id,
        )
        .await;
    }

    pub async fn plan_changes_requested(
        &self,
        plan_id: &str,
        revision: u32,
        annotation_count: usize,
        note: Option<&str>,
        conversation_id: Option<&str>,
    ) {
        self.raise(
            EVENT_PLAN_CHANGES_REQUESTED,
            json!({
                "plan_id": plan_id,
                "revision": revision,
                "annotation_count": annotation_count,
                "note": note,
            }),
            conversation_id,
        )
        .await;
    }

    pub async fn step_completed(
        &self,
        plan_id: &str,
        step_id: &str,
        title: &str,
        conversation_id: Option<&str>,
    ) {
        self.raise(
            EVENT_STEP_COMPLETED,
            json!({ "plan_id": plan_id, "step_id": step_id, "title": title }),
            conversation_id,
        )
        .await;
    }

    /// One emit. `try_emit` is used rather than `emit` only so the conversation id can
    /// be carried; the outcome is deliberately discarded for the reason in the module
    /// docs, and `NotHosted` is not even worth a log line.
    async fn raise(&self, event: &str, payload: serde_json::Value, conversation_id: Option<&str>) {
        self.raise_with_notify(event, payload, conversation_id, None)
            .await;
    }

    /// [`Self::raise`] with an optional user-facing notification raised alongside the
    /// event fan-out (the Inbox shows the plan row with the Blueprint icon).
    async fn raise_with_notify(
        &self,
        event: &str,
        payload: serde_json::Value,
        conversation_id: Option<&str>,
        notify: Option<ryu_app_events::NotifyHint>,
    ) {
        match self
            .emitter
            .try_emit_with_notify(event, payload, conversation_id, notify)
            .await
        {
            Ok(_) | Err(ryu_app_events::EmitError::NotHosted) => {}
            Err(e) => tracing::warn!(event, "blueprint: emitting the app event failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_id_is_namespaced_to_this_plugin() {
        // Core rejects an emit whose event is namespaced to another plugin, and the
        // `#` separator is what tells an app event apart from a Core turn phase. Both
        // are silent-403 material if a constant is edited carelessly.
        for event in [
            EVENT_PLAN_PUBLISHED,
            EVENT_PLAN_APPROVED,
            EVENT_PLAN_CHANGES_REQUESTED,
            EVENT_STEP_COMPLETED,
        ] {
            let (owner, name) = event.split_once('#').expect("every event id has a '#'");
            assert_eq!(
                owner, PLUGIN_ID,
                "{event} is namespaced to the wrong plugin"
            );
            assert!(!name.is_empty(), "{event} has no event name");
        }
    }

    #[tokio::test]
    async fn emitting_without_a_host_is_a_no_op_rather_than_a_failure() {
        // The state of every test run and every standalone launch. If this ever
        // started returning or panicking, the whole suite would fail for a reason that
        // has nothing to do with the code under test.
        let events = Events::from_env();
        events
            .plan_published("p", 1, "Plan", 3, Some("conv-1"))
            .await;
        events.plan_approved("p", 1, None, None).await;
        events
            .plan_changes_requested("p", 1, 2, Some("fix it"), None)
            .await;
        events.step_completed("p", "s_one", "One", None).await;
    }
}
