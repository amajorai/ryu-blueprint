//! Inbox, toast and push copy as localizable messages.
//!
//! Core renders a notify hint's `i18n` text per recipient (the inbox row and
//! toast in the node's `ui-language`, each push in the locale its device
//! registered with); the English rendering still goes out as the plain
//! `title`/`body`, which an older Core delivers as is.
//!
//! Each `*_MESSAGE` is `format!` with one named argument per placeholder
//! (`name = arg("name")`) so the i18n extractor catalogs the ICU source;
//! `tools/generate-core-i18n.mjs` copies the translations into Core's catalog.
//! Every message is a whole sentence. Core's renderer has no plural selector,
//! so counts are written as `label: {count}`.
#![allow(clippy::useless_format)]

use ryu_app_events::{NotifyHint, NotifyMessage, NotifyTemplate as Template};

pub(crate) const PLAN_AWAITS_REVIEW_MESSAGE: Template = |_| format!("A plan awaits review");
pub(crate) const PLAN_STEPS_MESSAGE: Template = |arg| {
    format!(
        "Steps in revision {revision}: {steps}",
        revision = arg("revision"),
        steps = arg("steps")
    )
};

/// The Inbox notification for a published plan. It names no plan content (the
/// plan title is sealed); the Companion opens the plan by id.
pub(crate) fn plan_published(revision: u32, step_count: usize) -> NotifyHint {
    NotifyHint::localized(
        NotifyMessage::new(PLAN_AWAITS_REVIEW_MESSAGE),
        vec![NotifyMessage::new(PLAN_STEPS_MESSAGE)
            .count("revision", i64::from(revision))
            .count("steps", i64::try_from(step_count).unwrap_or(i64::MAX))],
    )
}
