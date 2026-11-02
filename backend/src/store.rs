//! On-disk persistence: one directory per plan under the app's plugin data dir
//! (`<data dir>/plugin-data/@ryu+blueprint`).
//!
//! ```text
//! <plugin data dir>/plans/<plan_id>/
//!     plan.json          the mutable head
//!     revisions/<n>.json  immutable, one per publish
//!     annotations.json    the whole list, rewritten on every change
//!     verdicts.json       revision -> verdict
//! ```
//!
//! JSON files rather than a database for the same reason the reasoning sidecar uses
//! them: a plan is a document someone reviews, diffs, and occasionally copies between
//! machines, there is no query surface beyond list/get/put, and plans number in the
//! dozens per node. An index would cost more than it saves.
//!
//! Verdicts are keyed by revision rather than stored on the plan. Approving revision 2
//! and then publishing revision 3 must not leave revision 3 wearing revision 2's
//! approval — and "revision 2 was approved" stays a true statement about revision 2.
//!
//! # Properties to be explicit about
//!
//! **Writes are crash-atomic; read-modify-write is the caller's to serialize.** Content
//! goes to a temp file in the same directory (a fresh name per write, so two writers
//! never share one) and is renamed over the target, so a crash mid-write leaves the
//! previous version intact rather than a half-written file that no longer parses. The
//! store methods themselves do not lock, but every read-modify-write sequence
//! (appending an annotation, recording a verdict, moving a step, publishing a
//! revision) must hold [`Store::lock_plan`] for that plan. The writers are the HTTP
//! sidecar (the reviewer's companion) AND the separate `ryu-blueprint mcp` process
//! (the agent, which also writes annotations and rewrites the current revision
//! through `step_update`), so an in-process mutex would not be enough: the lock is
//! an advisory file lock under `<root>/locks/`, held only for the synchronous
//! section and never across an `.await`.
//!
//! **Sealed at rest.** Every file is one `encb:v1:` envelope (store
//! [`STORE`], bound to its path under the data dir, so a file copied to another
//! plan or revision does not open) under this app's plugin subkey; see
//! [`Store::open_with_keys`]. Files go through an owner-only temp file and a rename.
//!
//! **Every path goes through [`is_valid_id`].** Ids arrive over HTTP and become path
//! segments. The charset is `[a-z0-9][a-z0-9_-]{0,63}`: no dots, so `..` cannot be
//! spelled; no separators, so neither can an absolute path; no leading `-`, so an id
//! can never be mistaken for a flag by anything downstream.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use ryu_crypto::ContentContext;

use crate::model::{Annotation, Plan, Revision, RevisionSummary, Verdict};
use crate::seal_keys::{PluginKeys, StoreSeal};

/// The plugin id (the sealing key's scope).
pub const PLUGIN_ID: &str = "@ryu/blueprint";
/// The registered content-store name (part of every key derivation; never rename).
pub const STORE: &str = "blueprint-plans";
/// Records the key check value of the key that sealed the plans.
pub const SEAL_RECORD_FILE: &str = "plans.seal.json";
/// The AAD purpose of a sealed plan file.
const FILE_PURPOSE: &str = "blueprint.file";

pub use crate::paths::data_dir;

/// The id charset, spelled out once so the API, the parser and the store cannot drift.
///
/// A generated id that fails this is a bug in the generator, not a user error — which
/// is why [`crate::parse::slug`] trims leading separators rather than leaving the
/// rejection to happen four layers later with no context.
#[must_use]
pub fn is_valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    if id.len() > 64 {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn check_id(kind: &str, id: &str) -> Result<()> {
    if is_valid_id(id) {
        return Ok(());
    }
    Err(anyhow!(
        "{kind} id '{id}' is not usable: ids must be 1–64 characters of lowercase \
         letters, digits, '-' and '_', starting with a letter or digit"
    ))
}

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
    seal: StoreSeal,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("root", &self.root)
            .field("seal", &self.seal)
            .finish()
    }
}

/// Holds one plan's advisory lock (see [`Store::lock_plan`]); closing the file on
/// drop releases it.
#[must_use = "the plan lock is released as soon as this guard drops"]
#[derive(Debug)]
pub struct PlanLock {
    _file: fs::File,
}

impl Store {
    /// Open (creating if needed) the store rooted at `root`, resolving this
    /// process's sealing key with `root` as the plugin data dir.
    ///
    /// # Errors
    /// When the directory cannot be created or the key cannot be resolved.
    pub fn open(root: impl Into<PathBuf>) -> Result<Store> {
        let root = root.into();
        let keys = PluginKeys::resolve(PLUGIN_ID, &root)?;
        Self::open_with_keys(root, &keys)
    }

    /// [`Self::open`] with explicit keys.
    ///
    /// With a key, the first open records its key check value in
    /// [`SEAL_RECORD_FILE`] and rewrites every legacy plaintext file sealed
    /// (temp + rename; idempotent). Without a key, a never-sealed store keeps
    /// working in plaintext (compat mode, an older Ryu); a store that holds
    /// sealed files or a recorded key refuses writes and keeps its data.
    ///
    /// # Errors
    /// When the directory cannot be created or a legacy file cannot be resealed.
    pub fn open_with_keys(root: impl Into<PathBuf>, keys: &PluginKeys) -> Result<Store> {
        let root = root.into();
        let plans = root.join("plans");
        fs::create_dir_all(&plans).with_context(|| format!("creating {}", plans.display()))?;
        let record_path = root.join(SEAL_RECORD_FILE);
        let recorded = read_seal_record(&record_path)?;
        let files = plan_files(&plans);
        let holds_sealed = files.iter().any(|path| is_sealed_file(path));
        let seal = keys.store_state(STORE, recorded.as_deref(), holds_sealed);
        let store = Store { root, seal };
        if let StoreSeal::Sealing(cipher) = &store.seal {
            if recorded.is_none() {
                let body = serde_json::json!({ "v": 1, "kcv": cipher.kcv() });
                crate::seal_keys::write_private_file(&record_path, format!("{body}\n").as_bytes())?;
            }
            let mut sealed = 0usize;
            for path in files {
                if path.extension().and_then(|e| e.to_str()) == Some("tmp") {
                    // A temp file an earlier version left behind mid-write.
                    let _ = fs::remove_file(&path);
                    continue;
                }
                if is_sealed_file(&path) {
                    continue;
                }
                let raw = fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                store.write_body(&path, &raw)?;
                sealed += 1;
            }
            if sealed > 0 {
                tracing::info!(count = sealed, "blueprint: sealed legacy plan files");
            }
        }
        Ok(store)
    }

    /// The sealing health item: `{state, detail?}`.
    #[must_use]
    pub fn seal_health(&self) -> serde_json::Value {
        self.seal.health()
    }

    /// The AAD of one file: its path under the data dir.
    fn context(&self, path: &Path) -> ContentContext {
        let relative = path.strip_prefix(&self.root).unwrap_or(path);
        let id = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        ContentContext::for_id(FILE_PURPOSE, id)
    }

    fn read_json<T: serde::de::DeserializeOwned>(&self, path: &Path) -> Result<T> {
        let raw =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let json = self
            .seal
            .open_text(raw.trim_end(), &self.context(path))
            .with_context(|| format!("{} does not open", path.display()))?;
        serde_json::from_str(&json).with_context(|| format!("{} is not valid", path.display()))
    }

    fn write_json<T: serde::Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        let body = serde_json::to_string_pretty(value)?;
        self.write_body(path, &body)
    }

    /// Seal `body` and replace `path` with it (owner-only temp file in the
    /// same directory, then a rename, so a crash never leaves a half-written
    /// file and the rename is atomic).
    fn write_body(&self, path: &Path, body: &str) -> Result<()> {
        if matches!(self.seal, StoreSeal::Compat) && self.sealed_since_open() {
            // The sidecar sealed the store after this keyless process opened
            // it: never write plaintext next to sealed files.
            bail!(
                "the plans were sealed by the Blueprint app after this process opened them; \
                 writes from here are refused (use the running app)"
            );
        }
        let sealed = self.seal.seal_text(body, &self.context(path))?;
        crate::seal_keys::write_private_file(path, sealed.as_bytes())
            .with_context(|| format!("replacing {}", path.display()))
    }

    /// Whether this process cannot use the plans: they are sealed and it holds
    /// no key (or another key), including a store the sidecar sealed after a
    /// keyless process opened it.
    #[must_use]
    pub fn locked_out(&self) -> bool {
        match self.seal {
            StoreSeal::Unhealthy(_) => true,
            StoreSeal::Compat => self.sealed_since_open(),
            StoreSeal::Sealing(_) => false,
        }
    }

    /// Whether a sealing key was recorded after this process opened the store.
    fn sealed_since_open(&self) -> bool {
        self.root.join(SEAL_RECORD_FILE).exists()
    }

    /// Take the exclusive cross-process lock for one plan, blocking until it is free.
    /// Released when the returned guard drops.
    ///
    /// The lock file lives under `<root>/locks/`, not in the plan's own directory, so
    /// locking a plan that does not exist yet (a first publish, or a request for a
    /// missing id) never leaves a phantom plan directory behind. Not re-entrant: never
    /// take it twice for the same plan in one call path.
    ///
    /// # Errors
    /// When the id is not usable or the lock file cannot be opened or locked.
    pub fn lock_plan(&self, plan_id: &str) -> Result<PlanLock> {
        check_id("plan", plan_id)?;
        let dir = self.root.join("locks");
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{plan_id}.lock"));
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(PlanLock { _file: file })
    }

    fn plans_dir(&self) -> PathBuf {
        self.root.join("plans")
    }

    /// The directory for one plan. **The traversal choke point** — every path in this
    /// module is built from here or from a sibling that validated the same way.
    fn plan_dir(&self, plan_id: &str) -> Result<PathBuf> {
        check_id("plan", plan_id)?;
        Ok(self.plans_dir().join(plan_id))
    }

    // ── plans ────────────────────────────────────────────────────────────────

    /// Every plan, newest activity first.
    ///
    /// # Errors
    /// Only when the plans directory exists but cannot be read.
    pub fn list_plans(&self) -> Result<Vec<Plan>> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(self.plans_dir()) else {
            return Ok(out);
        };
        for entry in entries.flatten() {
            let path = entry.path().join("plan.json");
            if !path.exists() {
                continue;
            }
            match self.read_json::<Plan>(&path) {
                Ok(plan) => out.push(plan),
                // One unreadable plan must not hide every other plan.
                Err(e) => tracing::warn!(path = %path.display(), error = %e, "skipping plan"),
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    /// # Errors
    /// When the id is not usable, or the file exists but does not parse.
    pub fn get_plan(&self, plan_id: &str) -> Result<Option<Plan>> {
        let path = self.plan_dir(plan_id)?.join("plan.json");
        if !path.exists() {
            return Ok(None);
        }
        self.read_json(&path).map(Some)
    }

    /// # Errors
    /// When the id is not usable or the write fails.
    pub fn save_plan(&self, plan: &Plan) -> Result<()> {
        let dir = self.plan_dir(&plan.id)?;
        fs::create_dir_all(dir.join("revisions"))
            .with_context(|| format!("creating {}", dir.display()))?;
        self.write_json(&dir.join("plan.json"), plan)
    }

    /// Remove a plan and everything under it. Returns whether anything was there.
    ///
    /// # Errors
    /// When the id is not usable or the removal fails.
    pub fn delete_plan(&self, plan_id: &str) -> Result<bool> {
        let dir = self.plan_dir(plan_id)?;
        if !dir.exists() {
            return Ok(false);
        }
        fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        Ok(true)
    }

    // ── revisions ────────────────────────────────────────────────────────────

    /// # Errors
    /// When the id is not usable or the write fails.
    pub fn save_revision(&self, revision: &Revision) -> Result<()> {
        let dir = self.plan_dir(&revision.plan_id)?.join("revisions");
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        self.write_json(&dir.join(format!("{}.json", revision.revision)), revision)
    }

    /// # Errors
    /// When the id is not usable, or the file exists but does not parse.
    pub fn get_revision(&self, plan_id: &str, revision: u32) -> Result<Option<Revision>> {
        let path = self
            .plan_dir(plan_id)?
            .join("revisions")
            .join(format!("{revision}.json"));
        if !path.exists() {
            return Ok(None);
        }
        self.read_json(&path).map(Some)
    }

    /// Cheap summaries, oldest first.
    ///
    /// # Errors
    /// When the id is not usable.
    pub fn list_revisions(&self, plan_id: &str) -> Result<Vec<RevisionSummary>> {
        let dir = self.plan_dir(plan_id)?.join("revisions");
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(out);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match self.read_json::<Revision>(&path) {
                Ok(rev) => out.push(RevisionSummary {
                    revision: rev.revision,
                    created_at: rev.created_at,
                    block_count: rev.blocks.len(),
                    step_count: rev.steps.len(),
                }),
                Err(e) => tracing::warn!(path = %path.display(), error = %e, "skipping revision"),
            }
        }
        out.sort_by_key(|r| r.revision);
        Ok(out)
    }

    // ── annotations ──────────────────────────────────────────────────────────

    /// # Errors
    /// When the id is not usable, or the file exists but does not parse.
    pub fn annotations(&self, plan_id: &str) -> Result<Vec<Annotation>> {
        let path = self.plan_dir(plan_id)?.join("annotations.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        self.read_json(&path)
    }

    /// # Errors
    /// When the id is not usable or the write fails.
    pub fn save_annotations(&self, plan_id: &str, annotations: &[Annotation]) -> Result<()> {
        let dir = self.plan_dir(plan_id)?;
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        self.write_json(&dir.join("annotations.json"), &annotations)
    }

    /// # Errors
    /// When either id is not usable or the write fails.
    pub fn add_annotation(&self, annotation: &Annotation) -> Result<()> {
        check_id("annotation", &annotation.id)?;
        let mut all = self.annotations(&annotation.plan_id)?;
        all.push(annotation.clone());
        self.save_annotations(&annotation.plan_id, &all)
    }

    /// Returns whether anything was removed.
    ///
    /// # Errors
    /// When either id is not usable or the write fails.
    pub fn delete_annotation(&self, plan_id: &str, annotation_id: &str) -> Result<bool> {
        check_id("annotation", annotation_id)?;
        let mut all = self.annotations(plan_id)?;
        let before = all.len();
        all.retain(|a| a.id != annotation_id);
        if all.len() == before {
            return Ok(false);
        }
        self.save_annotations(plan_id, &all)?;
        Ok(true)
    }

    // ── verdicts ─────────────────────────────────────────────────────────────

    /// # Errors
    /// When the id is not usable, or the file exists but does not parse.
    pub fn verdicts(&self, plan_id: &str) -> Result<BTreeMap<u32, Verdict>> {
        let path = self.plan_dir(plan_id)?.join("verdicts.json");
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        self.read_json(&path)
    }

    /// # Errors
    /// When the id is not usable, or the file exists but does not parse.
    pub fn verdict_for(&self, plan_id: &str, revision: u32) -> Result<Option<Verdict>> {
        let mut all = self.verdicts(plan_id)?;
        Ok(all.remove(&revision))
    }

    /// # Errors
    /// When the id is not usable or the write fails.
    pub fn set_verdict(&self, plan_id: &str, verdict: &Verdict) -> Result<()> {
        let dir = self.plan_dir(plan_id)?;
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut all = self.verdicts(plan_id)?;
        all.insert(verdict.revision, verdict.clone());
        self.write_json(&dir.join("verdicts.json"), &all)
    }
}

fn read_seal_record(path: &Path) -> Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => {
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", path.display()))?;
            match value.get("kcv").and_then(serde_json::Value::as_str) {
                Some(kcv) if !kcv.is_empty() => Ok(Some(kcv.to_owned())),
                _ => bail!("{} is malformed", path.display()),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Every file this store writes (plus leftover `.tmp` files), under `plans/`.
fn plan_files(plans: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(plans) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        for sub in [dir.clone(), dir.join("revisions")] {
            let Ok(files) = fs::read_dir(&sub) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                // `<name>.json.tmp` (pre-sealing) and `<name>.json.<pid>.<n>.tmp`
                // (pre-sealing per-write temps) may hold plaintext; this store's
                // own sealed temps are dot-prefixed and never collected.
                let legacy_tmp = name.ends_with(".tmp") && !name.starts_with('.');
                if path.is_file() && (name.ends_with(".json") || legacy_tmp) {
                    out.push(path);
                }
            }
        }
    }
    out
}

fn is_sealed_file(path: &Path) -> bool {
    let mut prefix = [0u8; 5];
    fs::File::open(path)
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut prefix))
        .is_ok_and(|()| &prefix == b"encb:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AnnKind, Block, BlockKind, Decision, PlanStatus, Step, StepStatus, Target};

    /// A scratch directory per test. No `tempfile` dependency: this crate deliberately
    /// adds nothing to the workspace lockfile, and a uniquely-named directory under
    /// the OS temp dir is all the isolation these tests need.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ryu-blueprint-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn plan(id: &str) -> Plan {
        Plan {
            id: id.into(),
            title: format!("Plan {id}"),
            status: PlanStatus::InReview,
            current_revision: 1,
            conversation_id: None,
            agent_id: None,
            source: None,
            created_at: 10,
            updated_at: 10,
        }
    }

    fn revision(plan_id: &str, n: u32) -> Revision {
        Revision {
            plan_id: plan_id.into(),
            revision: n,
            created_at: 10,
            markdown: "# hi\n".into(),
            blocks: vec![Block {
                id: "b_aaaaaaaaaaaa_1".into(),
                kind: BlockKind::Heading,
                text: "hi".into(),
                level: Some(1),
                ordinal: 0,
                step_id: None,
            }],
            steps: vec![Step {
                id: "s_hi".into(),
                title: "hi".into(),
                summary: None,
                depends_on: Vec::new(),
                files: Vec::new(),
                status: StepStatus::Todo,
                risk: None,
            }],
            artifact_html: None,
        }
    }

    fn annotation(id: &str, plan_id: &str) -> Annotation {
        Annotation {
            id: id.into(),
            plan_id: plan_id.into(),
            revision: 1,
            target: Target::Plan,
            kind: AnnKind::Comment,
            body: "hm".into(),
            suggestion: None,
            author: None,
            resolved: false,
            created_at: 11,
        }
    }

    #[test]
    fn a_plan_round_trips_with_its_revisions() {
        let store = Store::open(scratch("roundtrip")).expect("opens");
        store.save_plan(&plan("deploy")).expect("saves");
        store.save_revision(&revision("deploy", 1)).expect("saves");
        store.save_revision(&revision("deploy", 2)).expect("saves");

        assert_eq!(
            store.get_plan("deploy").expect("gets").expect("some").title,
            "Plan deploy"
        );
        assert_eq!(
            store
                .get_revision("deploy", 2)
                .expect("gets")
                .expect("some")
                .revision,
            2
        );
        assert!(store.get_revision("deploy", 7).expect("gets").is_none());

        let summaries = store.list_revisions("deploy").expect("lists");
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].revision, 1, "revisions list oldest first");
        assert_eq!(summaries[0].block_count, 1);
        assert_eq!(summaries[0].step_count, 1);
    }

    #[test]
    fn every_entry_point_refuses_an_id_that_could_escape_the_store() {
        let store = Store::open(scratch("traversal")).expect("opens");
        for bad in [
            "../escape",
            "a/b",
            "..",
            "with.dot",
            "",
            "UPPER",
            "-leading-dash",
            "_leading_underscore",
            "a\\b",
            "plan\0null",
            &"x".repeat(65),
        ] {
            assert!(
                store.get_plan(bad).is_err(),
                "get_plan accepted '{bad}' — it must never reach the filesystem"
            );
            assert!(
                store.delete_plan(bad).is_err(),
                "delete_plan accepted '{bad}'"
            );
            assert!(
                store.list_revisions(bad).is_err(),
                "list_revisions accepted '{bad}'"
            );
            assert!(
                store.annotations(bad).is_err(),
                "annotations accepted '{bad}'"
            );
            assert!(store.verdicts(bad).is_err(), "verdicts accepted '{bad}'");
            assert!(
                store.get_revision(bad, 1).is_err(),
                "get_revision accepted '{bad}'"
            );
        }
        // And the annotation id is validated too — it is a value, but it is one a
        // caller supplies and one the delete route routes on.
        assert!(store.delete_annotation("ok", "../x").is_err());
    }

    #[test]
    fn the_id_charset_is_exactly_what_the_contract_names() {
        for good in ["a", "0", "plan-1", "plan_1", "a-b_c-9", &"a".repeat(64)] {
            assert!(is_valid_id(good), "'{good}' should be valid");
        }
        for bad in ["", "-a", "_a", "A", "a.b", "a/b", "a b", &"a".repeat(65)] {
            assert!(!is_valid_id(bad), "'{bad}' should be rejected");
        }
    }

    #[test]
    fn annotations_append_and_delete_by_id() {
        let store = Store::open(scratch("annotations")).expect("opens");
        store.save_plan(&plan("p")).expect("saves");
        assert!(store.annotations("p").expect("empty").is_empty());

        store.add_annotation(&annotation("a_1", "p")).expect("adds");
        store.add_annotation(&annotation("a_2", "p")).expect("adds");
        assert_eq!(store.annotations("p").expect("lists").len(), 2);

        assert!(store.delete_annotation("p", "a_1").expect("deletes"));
        assert!(
            !store.delete_annotation("p", "a_1").expect("deletes"),
            "deleting twice must report that the second one did nothing"
        );
        assert_eq!(store.annotations("p").expect("lists")[0].id, "a_2");
    }

    #[test]
    fn a_verdict_is_stored_per_revision_so_a_revise_does_not_inherit_an_approval() {
        let store = Store::open(scratch("verdicts")).expect("opens");
        store.save_plan(&plan("p")).expect("saves");
        store
            .set_verdict(
                "p",
                &Verdict {
                    verdict: Decision::Approved,
                    note: Some("ok".into()),
                    revision: 1,
                    decided_at: 20,
                },
            )
            .expect("sets");

        assert_eq!(
            store
                .verdict_for("p", 1)
                .expect("gets")
                .expect("some")
                .verdict,
            Decision::Approved
        );
        assert!(
            store.verdict_for("p", 2).expect("gets").is_none(),
            "a new revision starts with no verdict, or approval would be inherited"
        );
    }

    #[test]
    fn deleting_a_plan_takes_its_revisions_with_it() {
        let store = Store::open(scratch("delete")).expect("opens");
        store.save_plan(&plan("p")).expect("saves");
        store.save_revision(&revision("p", 1)).expect("saves");
        assert!(store.delete_plan("p").expect("deletes"));
        assert!(!store.delete_plan("p").expect("deletes"));
        assert!(store.get_plan("p").expect("gets").is_none());
        assert!(store.list_revisions("p").expect("lists").is_empty());
    }

    #[test]
    fn listing_skips_an_unreadable_plan_instead_of_failing() {
        let root = scratch("corrupt");
        let store = Store::open(&root).expect("opens");
        store.save_plan(&plan("good")).expect("saves");
        let broken = root.join("plans").join("broken");
        fs::create_dir_all(&broken).expect("mkdir");
        fs::write(broken.join("plan.json"), "{ not json").expect("writes");

        let all = store.list_plans().expect("lists");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "good");
    }

    #[test]
    fn plans_list_newest_activity_first() {
        let store = Store::open(scratch("ordering")).expect("opens");
        let mut old = plan("old");
        old.updated_at = 1;
        let mut new = plan("new");
        new.updated_at = 99;
        store.save_plan(&old).expect("saves");
        store.save_plan(&new).expect("saves");
        let ids: Vec<String> = store
            .list_plans()
            .expect("lists")
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, vec!["new", "old"]);
    }

    #[test]
    fn a_half_written_file_never_replaces_a_good_one() {
        // The temp file must sit beside the target (same filesystem) and must not be
        // mistaken for a plan directory by the lister.
        let root = scratch("atomic");
        let store = Store::open(&root).expect("opens");
        store.save_plan(&plan("p")).expect("saves");
        let dir = root.join("plans").join("p");
        assert!(dir.join("plan.json").exists());
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .expect("reads")
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "the temp file must be renamed away, not left behind: {leftovers:?}"
        );
    }

    #[test]
    fn concurrent_annotation_appends_under_the_plan_lock_lose_nothing() {
        let root = scratch("locked-appends");
        let store = Store::open(&root).expect("opens");
        store.save_plan(&plan("p")).expect("saves");
        let writers: Vec<_> = (0..8)
            .map(|n| {
                let store = store.clone();
                std::thread::spawn(move || {
                    let _lock = store.lock_plan("p").expect("locks");
                    store
                        .add_annotation(&annotation(&format!("a{n}"), "p"))
                        .expect("adds");
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("joins");
        }
        assert_eq!(store.annotations("p").expect("lists").len(), 8);
        assert!(
            !root.join("plans").join("p").join(".lock").exists(),
            "the lock must not live in the plan's own directory"
        );
        assert!(store.lock_plan("../escape").is_err());
    }

    mod at_rest {
        use super::*;
        use crate::seal_keys::KeySource;
        use base64::Engine as _;
        use ryu_crypto::PluginCipher;

        const MARKER: &str = "quokka-plan-marker";

        fn keys(n: u8) -> PluginKeys {
            PluginKeys::sealing(
                PluginCipher::from_subkey_b64(
                    PLUGIN_ID,
                    &base64::engine::general_purpose::STANDARD.encode([n; 32]),
                )
                .unwrap(),
                KeySource::Delivered,
            )
        }

        fn seed(store: &Store) {
            let mut first = plan("alpha");
            first.title = MARKER.into();
            store.save_plan(&first).unwrap();
            store.save_plan(&plan("beta")).unwrap();
            let mut rev = revision("alpha", 1);
            rev.markdown = format!("# {MARKER}\n");
            store.save_revision(&rev).unwrap();
            let mut note = annotation("n1", "alpha");
            note.body = MARKER.into();
            store.add_annotation(&note).unwrap();
        }

        fn files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
            let mut out: Vec<_> = plan_files(&root.join("plans"))
                .into_iter()
                .map(|path| {
                    let bytes = fs::read(&path).unwrap();
                    (path, bytes)
                })
                .collect();
            out.sort();
            out
        }

        fn anywhere(root: &Path, needle: &str) -> bool {
            files(root)
                .iter()
                .any(|(_, bytes)| bytes.windows(needle.len()).any(|w| w == needle.as_bytes()))
        }

        #[test]
        fn every_file_is_sealed_on_disk_and_reads_back() {
            let root = scratch("at-rest-sealed");
            let store = Store::open_with_keys(&root, &keys(1)).unwrap();
            seed(&store);
            assert_eq!(store.get_plan("alpha").unwrap().unwrap().title, MARKER);
            assert_eq!(store.annotations("alpha").unwrap()[0].body, MARKER);
            assert!(!anywhere(&root, MARKER));
            let all = files(&root);
            assert_eq!(all.len(), 4);
            assert!(all.iter().all(|(_, bytes)| bytes.starts_with(b"encb:v1:")));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                for (path, _) in &all {
                    let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "{}", path.display());
                }
            }
        }

        #[test]
        fn legacy_files_are_sealed_once_and_still_read() {
            let root = scratch("at-rest-legacy");
            seed(&Store::open_with_keys(&root, &PluginKeys::compat()).unwrap());
            assert!(anywhere(&root, MARKER));
            fs::write(root.join("plans/alpha/plan.json.tmp"), MARKER).unwrap();
            let store = Store::open_with_keys(&root, &keys(2)).unwrap();
            assert_eq!(store.get_plan("alpha").unwrap().unwrap().title, MARKER);
            assert!(!anywhere(&root, MARKER));
            assert!(!root.join("plans/alpha/plan.json.tmp").exists());
            let first = files(&root);
            drop(Store::open_with_keys(&root, &keys(2)).unwrap());
            assert_eq!(files(&root), first, "a second open rewrites nothing");
        }

        #[test]
        fn a_copied_file_does_not_open_and_lists_skip_it() {
            let root = scratch("at-rest-swap");
            let store = Store::open_with_keys(&root, &keys(3)).unwrap();
            seed(&store);
            fs::copy(
                root.join("plans/alpha/plan.json"),
                root.join("plans/beta/plan.json"),
            )
            .unwrap();
            assert!(store.get_plan("beta").is_err());
            let listed = store.list_plans().unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].id, "alpha");
        }

        #[test]
        fn another_key_or_no_key_refuses_writes_and_keeps_the_data() {
            let root = scratch("at-rest-wrong");
            seed(&Store::open_with_keys(&root, &keys(4)).unwrap());
            let before = files(&root);
            for other in [keys(5), PluginKeys::compat()] {
                let wrong = Store::open_with_keys(&root, &other).unwrap();
                assert_eq!(wrong.seal_health()["state"], "unhealthy");
                assert!(wrong.locked_out());
                assert!(wrong.save_plan(&plan("gamma")).is_err());
                assert!(wrong.list_plans().unwrap().is_empty());
            }
            assert_eq!(files(&root), before);
        }

        #[test]
        fn a_keyless_process_stops_writing_once_the_sidecar_sealed_the_store() {
            let root = scratch("at-rest-race");
            let keyless = Store::open_with_keys(&root, &PluginKeys::compat()).unwrap();
            assert_eq!(keyless.seal_health()["state"], "compat");
            keyless.save_plan(&plan("early")).unwrap();
            let sidecar = Store::open_with_keys(&root, &keys(6)).unwrap();
            assert!(keyless.locked_out());
            assert!(keyless.save_plan(&plan("late")).is_err());
            assert!(!root.join("plans/late/plan.json").exists());
            assert!(files(&root)
                .iter()
                .all(|(_, bytes)| bytes.starts_with(b"encb:")));
            assert_eq!(sidecar.get_plan("early").unwrap().unwrap().id, "early");
        }
    }
}
