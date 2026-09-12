//! On-disk persistence: one directory per plan under `$RYU_DIR/blueprint`.
//!
//! ```text
//! $RYU_DIR/blueprint/plans/<plan_id>/
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
//! # Two properties to be explicit about
//!
//! **Writes are crash-atomic, not isolated.** Content goes to a temp file in the same
//! directory and is renamed over the target, so a crash mid-write leaves the previous
//! version intact rather than a half-written file that no longer parses. There is no
//! locking: two concurrent annotation posts both read the list and both write it, and
//! one is lost. That is acceptable here — the writers are one human in one companion
//! window, plus an agent that only ever appends revisions — and it is written down so
//! nobody has to rediscover it.
//!
//! **Every path goes through [`is_valid_id`].** Ids arrive over HTTP and become path
//! segments. The charset is `[a-z0-9][a-z0-9_-]{0,63}`: no dots, so `..` cannot be
//! spelled; no separators, so neither can an absolute path; no leading `-`, so an id
//! can never be mistaken for a flag by anything downstream.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::model::{Annotation, Plan, Revision, RevisionSummary, Verdict};

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

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Open (creating if needed) the store rooted at `root`.
    ///
    /// # Errors
    /// When the directory cannot be created.
    pub fn open(root: impl Into<PathBuf>) -> Result<Store> {
        let root = root.into();
        let plans = root.join("plans");
        fs::create_dir_all(&plans).with_context(|| format!("creating {}", plans.display()))?;
        Ok(Store { root })
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
            match read_json::<Plan>(&path) {
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
        read_json(&path).map(Some)
    }

    /// # Errors
    /// When the id is not usable or the write fails.
    pub fn save_plan(&self, plan: &Plan) -> Result<()> {
        let dir = self.plan_dir(&plan.id)?;
        fs::create_dir_all(dir.join("revisions"))
            .with_context(|| format!("creating {}", dir.display()))?;
        write_json(&dir.join("plan.json"), plan)
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
        write_json(&dir.join(format!("{}.json", revision.revision)), revision)
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
        read_json(&path).map(Some)
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
            match read_json::<Revision>(&path) {
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
        read_json(&path)
    }

    /// # Errors
    /// When the id is not usable or the write fails.
    pub fn save_annotations(&self, plan_id: &str, annotations: &[Annotation]) -> Result<()> {
        let dir = self.plan_dir(plan_id)?;
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        write_json(&dir.join("annotations.json"), &annotations)
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
        read_json(&path)
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
        write_json(&dir.join("verdicts.json"), &all)
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("{} is not valid", path.display()))
}

/// Write-temp-then-rename. The temp file lives in the SAME directory as the target so
/// the rename is within one filesystem and therefore atomic; a temp in `/tmp` would
/// degrade to a copy across a device boundary and lose the guarantee.
fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let body = serde_json::to_string_pretty(value)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
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
        assert!(
            !dir.join("plan.json.tmp").exists(),
            "the temp file must be renamed away, not left behind"
        );
    }
}
