//! Visual plan review: an agent publishes a plan, a human reads it, marks it up,
//! and returns a verdict the agent can act on without a human re-typing it.
//!
//! ```text
//!   agent ──plan_publish(markdown)──▶ parse ──▶ blocks (content-addressed)
//!                                       │              │
//!                                       └──▶ steps ────┼──▶ graph: layered DAG
//!                                                      ▼
//!   human ──▶ companion UI: read blocks, see the graph, annotate, decide
//!                                                      │
//!   agent ◀──plan_status──── feedback (deterministic) ◀─┘
//! ```
//!
//! # The one idea worth understanding
//!
//! A block's id is a hash of its own content, not its position. That is what makes
//! the whole loop work: when the agent revises the plan, every paragraph it did not
//! touch keeps its id, so an annotation anchored to that paragraph is still anchored
//! to it — while a paragraph that *was* edited gets a new id and its old annotation
//! stops claiming to describe text that no longer exists. Position-based anchors
//! would silently re-point a reviewer's blocker at an unrelated sentence the moment
//! the agent inserted a heading above it, which is a worse failure than losing the
//! anchor, because nobody can see it happen.
//!
//! The cost of that choice is that [`diff`] cannot report "changed" from ids alone —
//! an edited block *is* a different id. So the diff matches on ids first and pairs
//! the leftovers by kind and order; see that module for why the pairing is the part
//! worth testing.
//!
//! # Layout
//!
//! | module | role |
//! |--------|------|
//! | [`model`] | the wire data model, frozen against the UI and the manifest |
//! | [`parse`] | markdown → blocks + derived steps; deterministic, hand-rolled |
//! | [`graph`] | step DAG validation + a longest-path layered layout |
//! | [`diff`] | revision-to-revision block classification |
//! | [`feedback`] | the agent-readable verdict text |
//! | [`store`] | on-disk persistence, one directory per plan |
//! | [`host`] | the sidecar's one line back into Core: app events |
//! | [`api`] | the HTTP surface Core proxies as `/api/blueprint/*` |
//! | [`mcp`] | the same operations as an MCP stdio server, for agents |

pub mod api;
pub mod diff;
pub mod feedback;
pub mod graph;
pub mod host;
pub mod mcp;
pub mod model;
pub mod parse;
pub mod store;
