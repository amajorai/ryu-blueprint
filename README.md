<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="./icon-dark.png" />
    <img src="./icon-light.png" alt="Blueprint" width="144" />
  </picture>
</p>

<div align="center">

# Blueprint

</div>

Visual plan review: an agent publishes its plan over MCP, a human reads it as addressable markdown blocks plus a dependency graph derived from the steps, annotates it inline (comment, redline, question, blocker), and approves or requests changes, and the agent reads the verdict back as structured text.

> **The public home of `ryu-blueprint`.** Source, builds, and releases live here —
> binaries for every platform are attached to each release.
>
> This tree is generated from the Ryu monorepo, so commits pushed here
> directly are replaced on the next sync. **Pull requests are welcome** —
> open them here and they are ported into the monorepo, then flow back out.
> Ryu as a whole: https://github.com/amajorai/ryu

## Install

**App:** [Install](ryu://apps/@ryu/blueprint) (opens the Ryu desktop app and asks you to confirm)

**CLI:**

```bash
ryu apps add @ryu/blueprint
```

**Crate:**

```bash
cargo install ryu-blueprint
```

Prebuilt binaries for every platform are attached to [each release](https://github.com/amajorai/ryu/releases).

## License

Apache-2.0 — see [LICENSE](./LICENSE).

## The loop

1. **Publish.** The agent calls `blueprint.plan_publish` with a title, the plan
   markdown, and a `steps` array (`title`, `summary`, `depends_on`, `files`, `risk`).
   The sidecar splits the markdown into blocks and stores a **revision**. Status is
   `in_review`. `@ryu/blueprint#plan.published` fires.
2. **Read.** The companion renders the blocks, the step list, the derived graph
   (nodes coloured by step status) and, if the agent attached one, the `artifact_html`
   pane.
3. **Annotate.** Every annotation is anchored: `{ "type": "block", "id": … }`,
   `{ "type": "step", "id": … }`, or the plan as a whole. Four kinds, and the
   distinction is the point:

   | Kind | Means |
   |---|---|
   | `comment` | An observation. Does not block; survives an approval as advisory. |
   | `redline` | A concrete replacement. Carries the `suggestion` text you want instead. |
   | `question` | You need an answer before you can decide. |
   | `blocker` | This step does not start. Stops the agent on its own. |

4. **Decide.** `approved` or `changes_requested`, with an optional note.
   `@ryu/blueprint#plan.approved` / `#plan.changes_requested` fires.
5. **Read back.** The agent polls `blueprint.plan_status`, which returns the verdict
   plus `feedback`: the annotations serialized into deterministic text, each finding
   naming the step or block it attaches to and, for a redline, the substitution
   proposed. Generated in Rust and unit-tested, so the same annotations always produce
   the same bytes — an agent reading it is reading a document, not a summary.
6. **Revise.** Re-publishing under the **same `plan_id`** appends a revision and
   resets to `in_review`. Block ids are content-stable across revisions
   (`b_<hash of kind + normalized text>_<ordinal>`), so the reviewer gets a diff
   against the draft they already read instead of a second plan to read from scratch.
   Annotations stay attached to the revision they were written against.
7. **Execute.** `blueprint.step_update` moves a step through
   `todo → in_progress → done | blocked`. Entering `done` fires
   `@ryu/blueprint#step.completed`. The graph the reviewer is looking at updates
   underneath them.

## What the verdict guarantees

`approved` is a decision recorded against **one revision**, not against "the plan". If
the agent publishes again, the plan is `in_review` again — an approval cannot be
carried forward onto text nobody read. That is the property that makes it safe for an
agent to gate editing on it.

`changes_requested` guarantees the agent receives every open annotation as text, in a
stable order, anchored. It does not guarantee the agent agrees; the honest response to
a finding you think is wrong is a revision that argues the case, which comes back for
another decision.

## MCP tools

Registered by the manifest's own `mcp_servers.blueprint` (`ryu-blueprint mcp`), so
they exist for agents and for workflow `mcp` nodes:

| Tool | |
|---|---|
| `blueprint.plan_publish` | Publish or revise. `plan_id` present → new revision of that plan; absent → new plan. Returns `{ plan_id, revision, status, review_url }`. |
| `blueprint.plan_status` | `{ status, verdict?, feedback, annotation_count, revision }`. `wait_secs` clamps to 0..=60 and polls internally; **on timeout it returns `in_review`**, which is neither approval nor rejection. |
| `blueprint.plan_get` | The whole plan: current revision, annotations, verdict. |
| `blueprint.step_update` | Set a step's status as the work lands. |

`plan_status` never blocks unbounded. A tool that waits forever on a human is a hung
turn, and the caller cannot tell it apart from a crash.

## HTTP

Core proxies `/api/blueprint/*` to the sidecar on 8011 (`public_mount`). Every path is
enumerated in the manifest's `sidecars[].http.routes`, which is an **allowlist with
exact segment matching** — `/plans/:id` does not admit `/plans/:id/annotations`, so a
route that exists in the crate but not in the manifest is a 404 that reads exactly
like a router bug. The two lists move together.

## The Visual planning output style

`contributes.output_styles` ships `output-styles/visual-planning.md`. **Without it
nothing tells an agent this app exists** — the tools are registered, and no turn
would ever call them. The style is the mechanism, borrowed from superpowers: the
methodology is applied, not offered. It says investigate before proposing, publish
with real `depends_on` edges and the files each step touches, poll with a bounded
wait, treat an `in_review` timeout as "nobody answered" rather than as consent, and
revise under the same `plan_id`.

`keep-coding-instructions: true`, so it is appended after the agent's own
`system_prompt` rather than replacing it — it changes how the agent plans, not who
the agent is.

## No turn hook, no quotas, no settings tab

None of the three is an oversight.

**No `contributes.turn_hooks`.** The plugin sandbox has no HTTP. A hook body is
spliced into an async IIFE with `ctx`/`host` bound and nothing else — it cannot reach
a sidecar on loopback, which is the only place a plan lives. The reasoning app works
around this by going out through `host.runAgent`, and pays for it: that path is
conditional on a named agent *and* a live runner, so its behaviour depends on how the
node is configured. Blueprint has no reason to take that trade. The agent already
holds the MCP tools; publishing a plan is something it decides to do, not something
that should happen to it after every turn.

**No `contributes.quotas`.** The typed half never landed — there is no `quotas` field
on the Rust `Contributes` struct and none in the SDK schema, so the key is silently
dropped at parse. It is load-bearing for the three apps that declare one only because
their id is also a `PlanLimitField` in `packages/auth`, which is what actually caps
them. Blueprint owns no such field, so a declaration here would cap nothing and would
read like a limit that exists.

**No `contributes.settings_tabs`.** This one was written, then deleted. Core does not
forward settings values into a companion frame: `window.ryu.context` carries mount
context, and the prefs reader (`host.getPreference`) lives in a turn hook, which this
app deliberately does not have. Four toggles were therefore declared, rendered, saved —
and read by nothing. That is the shape of a control which reports authority it does not
have, and it is worse than no control, because a reviewer who turns off the artifact
pane for untrusted markup would still be shown it.

The switches that are real: **enabling the app at all** (it is default-off — in
`CORE_PLUGINS` for tier, never in `CORE_DEFAULT_ON`, and listed in `NOT_PRE_INSTALLED`,
so a sidecar binary a normal install does not have is never spawned uninvited), and the
companion's own pane tabs, which are in-frame state the frame actually owns. If Core
ever bakes a settings bag into the mount context, `readPaneFlags` in `ui/src/api.ts` is
the one function that has to change.

## Registration

The app is a built-in, so it is registered from Core rather than installed. Three
rows live outside this directory, and only one of them is enforced by a test:

- `BUILTIN_MANIFESTS` in `apps/core/src/plugin_manifest/mod.rs` —
  `include_str!("../../../../apps-store/blueprint/manifest.json")`. Enforced:
  `packaged_manifests_are_compiled_in_from_their_package_home`.
- `BUILTIN_OUTPUT_STYLES` in `apps/core/src/plugin_manifest/builtin_code.rs` —
  `("@ryu/blueprint", "output-styles/visual-planning.md", include_str!(…))`. A
  built-in ships only its `manifest.json`; this package directory is not on a user's
  machine, so without this row the style's `file` reference resolves to nothing.
  Enforced in both directions by
  `builtin_output_style_table_matches_package_manifests`.
- `CORE_PLUGINS` in `apps/core/src/plugins/builtins.rs` — `BLUEPRINT_PLUGIN_ID`.
  **Nothing enforces this one.** Core tier is what lets the sidecar spawn at all:
  `may_run_sidecar` permits a Community-tier sidecar only against a Gateway-approved
  `sidecar:process` grant that the Gateway denies at enable, and this manifest
  deliberately does not declare that grant — the binary is spawned by the manifest
  loader instead. `mcp_servers.blueprint` needs the same tier via
  `may_register_mcp_servers`. Drop the row and the app enables cleanly, the sidecar
  never starts, `blueprint.plan_publish` does not exist, and the output style becomes
  instructions to call a tool that is not there.

Default-OFF — `CORE_PLUGINS`, deliberately **not** `CORE_DEFAULT_ON`, because it owns
an out-of-process binary an ordinary install does not have. It is also in
`NOT_PRE_INSTALLED`: a fresh machine should not list an app nobody asked for.

`ui_entry` alone does not ship a UI to users. The companion bundle is compiled into
Core as `fixtures/blueprint.ui.html` (`BLUEPRINT_UI_HTML`), refreshed by
`scripts/sync-app-fixtures.sh blueprint`, and handed to `install_app` through the
app's `SeedSpec` — separate wiring from anything in this directory.

## Round two

Deliberately not in this round:

- **Excalidraw sketch layer.** ~7.7MB of dist for a feature that is optional on every
  plan. It needs a lazily loaded second bundle, and doing that badly taxes the
  companion's cold start for everyone.
- **Kanban board over steps.** The dependency graph already answers the ordering
  question; a board answers a scheduling question that only matters once plans are
  long-lived and shared, which they are not yet.
- **PR / diff review.** Reviewing a diff is a different surface with different
  primitives (hunks, not blocks) and a different anchor model. Bolting it onto the
  block store would make both worse.
- **Side-by-side revision diff view.** The `/plans/:id/diff` route and its
  `added|removed|changed|same` block statuses ship in round one; the two-pane UI on
  top of them does not.
- **Vim keys.** Worth having, worth doing after the annotation interactions have
  settled — keybindings written against a surface that is still moving get rewritten.
- **Mobile push on a pending verdict.** Needs the notification fan-out capability,
  which is pinned to `@ryu/monitors`.

## Layout

The crate mirrors `apps-store/reasoning/backend` — same data-dir resolution, same
write-temp-then-rename, same id validation, same fail-closed bearer middleware:

```
backend/                    the crate (also the MCP server; ZERO dependency on apps/core)
  src/model.rs              the shared types the HTTP and MCP surfaces both speak
  src/parse.rs              markdown → addressable blocks + derived steps, no model call
  src/graph.rs              step DAG: cycle rejection and the layered layout the UI draws
  src/diff.rs               revision → revision block diff, keyed on stable block ids
  src/feedback.rs           annotations → the deterministic text an agent reads
  src/store.rs              plans, revisions, annotations under $RYU_DIR/blueprint
  src/host.rs               the four app events, emitted through `ryu-app-events`
  src/api.rs                the HTTP surface
  src/mcp.rs                the same engine over MCP stdio
ui/                         the companion (vite + react + @xyflow/react, one HTML file)
output-styles/
  visual-planning.md        the style that makes an agent publish at all
```
