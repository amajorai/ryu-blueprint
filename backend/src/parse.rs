//! Markdown → addressable blocks + a derived step DAG.
//!
//! Hand-rolled on purpose, twice over.
//!
//! **No markdown crate.** The workspace lockfile has none, and this working tree is
//! shared by several jobs, so pulling one in would mean a new lockfile entry nobody
//! asked for. More importantly a full CommonMark parser would be the wrong tool: what
//! is wanted is not a faithful AST but a *stable, coarse* segmentation — one block per
//! thing a human would want to point at. A parser that split emphasis spans out of a
//! paragraph would give a reviewer nothing to click and would make every id churn on
//! a re-wrap.
//!
//! **No LLM.** Step derivation is a convention, not an inference. `(after: setup)` and
//! `files: a.rs, b.rs` are things the author *wrote*; reading them back is parsing.
//! Guessing structure with a model would make the same markdown produce a different
//! graph on Tuesday, and a plan whose shape changes under you is worse than a plan
//! with no graph at all.
//!
//! # Block ids
//!
//! `b_<12 hex of sha256(kind + "\n" + normalized text)>_<occurrence>`.
//!
//! The occurrence suffix is **always present**, starting at 1 — the contract spells it
//! as part of the id, and a uniform shape means nothing downstream has to handle two
//! spellings of "the same block". It counts only blocks with the *same* digest, so an
//! unrelated edit anywhere else in the document cannot change it.
//!
//! The honest limit of content addressing: two byte-identical blocks are told apart
//! only by which comes first, so duplicating a line above an existing copy renumbers
//! the later ones. That is not a bug to design away — blocks with identical text are
//! interchangeable by definition, and an annotation that lands on the other copy is
//! saying the same thing about the same words. Anything positional, by contrast, moves
//! a reviewer's *blocker* onto an unrelated sentence, which is the failure this scheme
//! exists to prevent.
//!
//! Normalization differs by kind, because whitespace means different things: prose
//! collapses runs of whitespace (a re-wrapped paragraph is the same paragraph), code
//! only trims trailing whitespace per line (re-indenting Python *is* an edit).

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use crate::model::{Block, BlockKind, Step, StepStatus};

/// How many hex characters of the digest go into a block id. Six bytes is 2^48
/// values; a plan with a thousand blocks has a collision probability around 2e-9, and
/// a collision is not even a correctness bug here — identical text collides on
/// purpose, and the occurrence suffix keeps distinct blocks distinct.
const ID_HEX_LEN: usize = 12;

/// Longest slug that goes into a step id, leaving room for `s_` and a collision
/// suffix inside the 64-character id budget the store enforces.
const MAX_SLUG: usize = 48;

/// The result of parsing one revision's markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub blocks: Vec<Block>,
    pub steps: Vec<Step>,
}

/// A block plus the syntactic facts that step derivation needs but the wire model
/// does not carry (was it numbered? was it checked? how deep was it indented?).
#[derive(Debug, Clone)]
struct Raw {
    kind: BlockKind,
    text: String,
    level: Option<u8>,
    /// True for `1.` / `1)` items. Plain `-` bullets are prose, not steps.
    ordered: bool,
    /// `Some(true)` for `- [x]`, `Some(false)` for `- [ ]`, `None` for a plain item.
    checked: Option<bool>,
    indent: usize,
}

/// Parse markdown into blocks, then derive steps from it and attribute blocks to
/// them. Deterministic: the same markdown always produces the same ids in the same
/// order.
#[must_use]
pub fn parse(markdown: &str) -> Parsed {
    let raws = scan(markdown);
    let mut blocks = to_blocks(&raws);
    let steps = derive_steps(&raws, &mut blocks);
    Parsed { blocks, steps }
}

/// Parse blocks only, with no step derivation — used when the caller supplied its own
/// `steps` and the derived ones would just be a second, disagreeing opinion.
#[must_use]
pub fn parse_blocks(markdown: &str) -> Vec<Block> {
    to_blocks(&scan(markdown))
}

// ── block scanning ───────────────────────────────────────────────────────────

/// Segment the document. One pass, line-oriented, no backtracking.
fn scan(markdown: &str) -> Vec<Raw> {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut out: Vec<Raw> = Vec::new();
    let mut i = skip_front_matter(&lines);

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        // Fenced blocks first: everything inside one is literal, including lines that
        // would otherwise look like headings or list items.
        if let Some((marker, width, info)) = fence_open(trimmed) {
            let (text, next) = collect_fence(&lines, i + 1, marker, width);
            out.push(Raw {
                kind: fence_kind(&info),
                text,
                level: None,
                ordered: false,
                checked: None,
                indent: 0,
            });
            i = next;
            continue;
        }

        if is_thematic_break(trimmed) {
            i += 1;
            continue;
        }

        if let Some((level, text)) = heading(trimmed) {
            out.push(Raw {
                kind: BlockKind::Heading,
                text,
                level: Some(level),
                ordered: false,
                checked: None,
                indent: 0,
            });
            i += 1;
            continue;
        }

        if trimmed.starts_with('>') {
            let (text, next) = collect_quote(&lines, i);
            out.push(Raw {
                kind: BlockKind::Quote,
                text,
                level: None,
                ordered: false,
                checked: None,
                indent: 0,
            });
            i = next;
            continue;
        }

        if let Some(item) = list_marker(line) {
            let indent = leading_spaces(line);
            let (extra, next) = collect_indented_continuation(&lines, i + 1, indent);
            let mut text = item.text;
            if !extra.is_empty() {
                text.push('\n');
                text.push_str(&extra);
            }
            out.push(Raw {
                kind: BlockKind::ListItem,
                text,
                level: None,
                ordered: item.ordered,
                checked: item.checked,
                indent,
            });
            i = next;
            continue;
        }

        // A raw HTML block: a line that opens with a tag, up to the next blank line.
        if trimmed.starts_with('<') {
            let (text, next) = collect_paragraph(&lines, i);
            out.push(Raw {
                kind: BlockKind::Html,
                text,
                level: None,
                ordered: false,
                checked: None,
                indent: 0,
            });
            i = next;
            continue;
        }

        let (text, next) = collect_paragraph(&lines, i);
        out.push(Raw {
            kind: BlockKind::Paragraph,
            text,
            level: None,
            ordered: false,
            checked: None,
            indent: 0,
        });
        i = next;
    }

    out
}

/// YAML front matter is metadata about the document, not part of it. Dropping it
/// keeps it out of both the rendered blocks and the ids.
fn skip_front_matter(lines: &[&str]) -> usize {
    if lines.first().map(|l| l.trim()) != Some("---") {
        return 0;
    }
    for (idx, line) in lines.iter().enumerate().skip(1) {
        if line.trim() == "---" {
            return idx + 1;
        }
    }
    // An unterminated `---` is a thematic break, not front matter.
    0
}

/// `Some((marker char, run width, info string))` when this line opens a fence.
fn fence_open(trimmed: &str) -> Option<(char, usize, String)> {
    for marker in ['`', '~'] {
        let width = trimmed.chars().take_while(|c| *c == marker).count();
        if width >= 3 {
            return Some((marker, width, trimmed[width..].trim().to_owned()));
        }
    }
    None
}

/// The fence's info string decides the kind. Only the first word matters — `mermaid`
/// and ```` ```mermaid theme=dark ```` are the same thing.
fn fence_kind(info: &str) -> BlockKind {
    match info.split_whitespace().next().unwrap_or("").to_lowercase().as_str() {
        "mermaid" => BlockKind::Mermaid,
        "html" => BlockKind::Html,
        _ => BlockKind::Code,
    }
}

/// Collect a fence body. An unterminated fence runs to end of document rather than
/// being discarded — a truncated plan should still show its last code block.
fn collect_fence(lines: &[&str], start: usize, marker: char, width: usize) -> (String, usize) {
    let mut body: Vec<&str> = Vec::new();
    let mut i = start;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        let run = trimmed.chars().take_while(|c| *c == marker).count();
        if run >= width && trimmed.len() == run {
            return (body.join("\n"), i + 1);
        }
        body.push(lines[i]);
        i += 1;
    }
    (body.join("\n"), i)
}

/// `# Heading` → `(1, "Heading")`. Closing hashes are stripped; `#hashtag` is not a
/// heading (ATX requires the space).
fn heading(trimmed: &str) -> Option<(u8, String)> {
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &trimmed[level..];
    if !rest.starts_with(' ') && !rest.is_empty() {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim().to_owned();
    Some((level as u8, text))
}

fn is_thematic_break(trimmed: &str) -> bool {
    for marker in ['-', '*', '_'] {
        let stripped: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
        if stripped.len() >= 3 && stripped.chars().all(|c| c == marker) {
            return true;
        }
    }
    false
}

fn collect_quote(lines: &[&str], start: usize) -> (String, usize) {
    let mut body: Vec<String> = Vec::new();
    let mut i = start;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        let Some(rest) = trimmed.strip_prefix('>') else {
            break;
        };
        body.push(rest.strip_prefix(' ').unwrap_or(rest).to_owned());
        i += 1;
    }
    (body.join("\n"), i)
}

/// A paragraph runs to the next blank line or to the next line that starts some other
/// kind of block.
fn collect_paragraph(lines: &[&str], start: usize) -> (String, usize) {
    let mut body: Vec<&str> = Vec::new();
    let mut i = start;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if i > start
            && (fence_open(trimmed).is_some()
                || heading(trimmed).is_some()
                || trimmed.starts_with('>')
                || is_thematic_break(trimmed)
                || list_marker(line).is_some())
        {
            break;
        }
        body.push(trimmed);
        i += 1;
    }
    (body.join("\n"), i)
}

struct ListItem {
    text: String,
    ordered: bool,
    checked: Option<bool>,
}

/// Recognize `- `, `* `, `+ `, `1. `, `1) `, with an optional `[ ]` / `[x]` task box.
fn list_marker(line: &str) -> Option<ListItem> {
    let trimmed = line.trim_start();
    let (rest, ordered) = if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        (rest, false)
    } else {
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 || digits > 9 {
            return None;
        }
        let after = &trimmed[digits..];
        let rest = after.strip_prefix(". ").or_else(|| after.strip_prefix(") "))?;
        (rest, true)
    };

    let (rest, checked) = if let Some(r) = rest.strip_prefix("[ ] ") {
        (r, Some(false))
    } else if let Some(r) = rest.strip_prefix("[x] ").or_else(|| rest.strip_prefix("[X] ")) {
        (r, Some(true))
    } else {
        (rest, None)
    };

    Some(ListItem {
        text: rest.trim().to_owned(),
        ordered,
        checked,
    })
}

fn leading_spaces(line: &str) -> usize {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .count()
}

/// Continuation lines of a list item: non-blank lines indented deeper than the item's
/// own marker and not themselves a list item. Lazy (unindented) continuation is
/// deliberately NOT supported — it makes "is this a new paragraph or more of the
/// bullet?" ambiguous, and an ambiguous rule produces ids that move.
fn collect_indented_continuation(lines: &[&str], start: usize, indent: usize) -> (String, usize) {
    let mut body: Vec<String> = Vec::new();
    let mut i = start;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() || leading_spaces(line) <= indent || list_marker(line).is_some() {
            break;
        }
        body.push(line.trim().to_owned());
        i += 1;
    }
    (body.join("\n"), i)
}

// ── ids ──────────────────────────────────────────────────────────────────────

/// Collapse a block's text into the form the id hashes.
#[must_use]
pub fn normalize(kind: BlockKind, text: &str) -> String {
    if kind.is_verbatim() {
        return text
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
            .trim_matches('\n')
            .to_owned();
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The content digest a block id is built from, without the occurrence suffix.
fn digest(kind: BlockKind, text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_str().as_bytes());
    hasher.update(b"\n");
    hasher.update(normalize(kind, text).as_bytes());
    let out = hasher.finalize();
    let mut hex = String::with_capacity(ID_HEX_LEN);
    for byte in out.iter().take(ID_HEX_LEN / 2) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// The stable id of a block: content, not position.
///
/// `occurrence` is 1-based and counts only blocks with the *same* digest, so it is
/// unaffected by anything else in the document.
#[must_use]
pub fn block_id(kind: BlockKind, text: &str, occurrence: usize) -> String {
    format!("b_{}_{occurrence}", digest(kind, text))
}

fn to_blocks(raws: &[Raw]) -> Vec<Block> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::with_capacity(raws.len());
    for (ordinal, raw) in raws.iter().enumerate() {
        let hex = digest(raw.kind, &raw.text);
        let counter = seen.entry(hex.clone()).or_insert(0);
        *counter += 1;
        out.push(Block {
            id: format!("b_{hex}_{counter}"),
            kind: raw.kind,
            text: raw.text.clone(),
            level: raw.level,
            ordinal: ordinal as u32,
            step_id: None,
        });
    }
    out
}

// ── slugs ────────────────────────────────────────────────────────────────────

/// A filename- and URL-safe slug. The store's id charset is
/// `[a-z0-9][a-z0-9_-]{0,63}`, so the result is lowercased, dash-separated, and
/// trimmed of leading separators — an id that starts with `-` would be rejected at
/// the storage layer, which is a confusing place to discover a title problem.
#[must_use]
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch.to_ascii_lowercase());
            if out.len() >= MAX_SLUG {
                break;
            }
        } else {
            pending_dash = true;
        }
    }
    out
}

/// `s_<slug>`, disambiguated against ids already handed out.
///
/// Unlike block ids the suffix appears only on a collision — the contract spells the
/// two rules differently and the UI's deep links use the bare form.
fn step_id(title: &str, taken: &mut HashMap<String, usize>) -> String {
    let base = slug(title);
    let base = if base.is_empty() {
        "step".to_owned()
    } else {
        base
    };
    let id = format!("s_{base}");
    let count = taken.entry(id.clone()).or_insert(0);
    *count += 1;
    if *count == 1 {
        id
    } else {
        format!("{id}_{count}")
    }
}

// ── step derivation ──────────────────────────────────────────────────────────

/// The inline conventions, pulled out of a step's prose.
#[derive(Debug, Default, PartialEq, Eq)]
struct Conventions {
    /// `None` when the author wrote no `after:` at all — which is what selects the
    /// linear default. `Some(vec![])` means they wrote `after: none` and meant it.
    after: Option<Vec<String>>,
    files: Vec<String>,
}

/// Strip `(after: …)` / `after: …` and `(files: …)` / `files: …` out of `text`,
/// returning the cleaned text plus what was found.
fn take_conventions(text: &str) -> (String, Conventions) {
    let mut conv = Conventions::default();
    let mut kept_lines: Vec<String> = Vec::new();

    for line in text.lines() {
        let mut rest = String::new();
        let mut buf = line;
        // Parenthesized form, possibly more than one per line.
        loop {
            let Some(open) = find_paren_convention(buf) else {
                rest.push_str(buf);
                break;
            };
            let Some(close_rel) = buf[open..].find(')') else {
                rest.push_str(buf);
                break;
            };
            let close = open + close_rel;
            rest.push_str(&buf[..open]);
            absorb(&buf[open + 1..close], &mut conv);
            buf = &buf[close + 1..];
        }

        let trimmed = rest.trim();
        // Bare line form: `files: a.rs, b.rs` on its own line.
        if is_convention_line(trimmed) {
            absorb(trimmed, &mut conv);
            continue;
        }
        if !trimmed.is_empty() {
            kept_lines.push(trimmed.to_owned());
        }
    }

    (kept_lines.join("\n"), conv)
}

/// Byte index of a `(after:` / `(files:` opener, matched ASCII-case-insensitively.
///
/// Scanned over the ORIGINAL bytes rather than over a `to_lowercase()` copy: lowering
/// is not length-preserving for every Unicode scalar (`İ` becomes two chars), so an
/// index found in the lowered string can land mid-codepoint in the original and panic
/// the slice. A plan title with a Turkish dotted capital is not hypothetical enough to
/// risk taking the whole sidecar down.
fn find_paren_convention(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte != b'(' {
            continue;
        }
        let rest = &bytes[idx + 1..];
        for needle in [b"after:".as_slice(), b"files:".as_slice()] {
            if rest.len() >= needle.len() && rest[..needle.len()].eq_ignore_ascii_case(needle) {
                return Some(idx);
            }
        }
    }
    None
}

fn is_convention_line(trimmed: &str) -> bool {
    let lower = trimmed.to_lowercase();
    lower.starts_with("after:") || lower.starts_with("files:")
}

/// Fold one `key: a, b` fragment into the accumulator.
fn absorb(fragment: &str, conv: &mut Conventions) {
    let Some((key, values)) = fragment.split_once(':') else {
        return;
    };
    let items: Vec<String> = values
        .split(',')
        .map(|v| v.trim().trim_matches('`').trim().to_owned())
        .filter(|v| !v.is_empty())
        .collect();
    match key.trim().to_lowercase().as_str() {
        "after" => {
            // `after: none` is how an author says "this one has no prerequisite",
            // which is different from writing nothing (that means "the one above").
            let refs: Vec<String> = items
                .into_iter()
                .filter(|v| !matches!(v.to_lowercase().as_str(), "none" | "-" | "nothing"))
                .collect();
            conv.after = Some(refs);
        }
        "files" => conv.files.extend(items),
        _ => {}
    }
}

/// A step under construction, before ids are resolved.
struct Draft {
    title: String,
    summary: Option<String>,
    status: StepStatus,
    conv: Conventions,
    /// Indices into the raw/block arrays that this step owns.
    owns: Vec<usize>,
}

/// Derive steps from the document and stamp `step_id` onto the blocks they own.
///
/// Two shapes, tried in order:
/// 1. **numbered or checklist items at the top level** — `1.` / `- [ ]`. Plain `-`
///    bullets are excluded deliberately: they are how anyone writes a list of
///    considerations, and turning those into graph nodes buries the real steps.
/// 2. **`##` headings** — the usual shape of a longer plan.
///
/// If neither is present the plan has no graph, which is a fine outcome for a
/// three-paragraph plan and better than inventing one.
fn derive_steps(raws: &[Raw], blocks: &mut [Block]) -> Vec<Step> {
    let mut drafts = drafts_from_items(raws);
    if drafts.is_empty() {
        drafts = drafts_from_headings(raws);
    }
    if drafts.is_empty() {
        return Vec::new();
    }

    let mut taken: HashMap<String, usize> = HashMap::new();
    let ids: Vec<String> = drafts
        .iter()
        .map(|d| step_id(&d.title, &mut taken))
        .collect();

    let mut steps = Vec::with_capacity(drafts.len());
    for (idx, draft) in drafts.iter().enumerate() {
        let depends_on = match &draft.conv.after {
            // The author took control: resolve what they named, and silently drop
            // what does not resolve. A typo in a convention must not fail the whole
            // publish — the prose is still in the block for a human to see.
            Some(refs) => refs
                .iter()
                .filter_map(|r| resolve_ref(r, &ids, &drafts, idx))
                .collect(),
            // The default: a plan written as a numbered list is a sequence.
            None => {
                if idx == 0 {
                    Vec::new()
                } else {
                    vec![ids[idx - 1].clone()]
                }
            }
        };
        for owned in &draft.owns {
            if let Some(block) = blocks.get_mut(*owned) {
                block.step_id = Some(ids[idx].clone());
            }
        }
        steps.push(Step {
            id: ids[idx].clone(),
            title: draft.title.clone(),
            summary: draft.summary.clone(),
            depends_on,
            files: draft.conv.files.clone(),
            status: draft.status,
            risk: None,
        });
    }
    steps
}

/// Resolve one `after:` reference: an exact step id, a 1-based step number, the id a
/// title would slug to, or a case-insensitive title match.
///
/// The bare number is here because it is what actually gets written. A plan is a
/// numbered list, so an author — human or model — reaches for `(after: 2)` long before
/// they reach for the title, and an unresolved reference is dropped silently by design.
/// Without this the common case produces a step graph with no edges at all, which is
/// precisely the picture the app exists to show. Numbers index the derived step list,
/// not the markdown's own numerals, so a list that restarts at `1.` still resolves.
fn resolve_ref(reference: &str, ids: &[String], drafts: &[Draft], self_idx: usize) -> Option<String> {
    if let Ok(ordinal) = reference.parse::<usize>() {
        // A step numbered after itself is dropped rather than handed to the graph
        // layer, which would (correctly) refuse the whole publish over a one-character
        // typo. An ordinal that names no step FALLS THROUGH rather than returning
        // early, so a step whose title happens to be "2024" is still reachable by name.
        let hit = ordinal
            .checked_sub(1)
            .filter(|idx| *idx != self_idx)
            .and_then(|idx| ids.get(idx))
            .cloned();
        if hit.is_some() {
            return hit;
        }
    }
    if let Some(hit) = ids.iter().find(|id| *id == reference) {
        return Some(hit.clone());
    }
    let as_id = format!("s_{}", slug(reference));
    if let Some(hit) = ids.iter().find(|id| **id == as_id) {
        return Some(hit.clone());
    }
    let lowered = reference.to_lowercase();
    drafts
        .iter()
        .position(|d| d.title.to_lowercase() == lowered)
        .map(|idx| ids[idx].clone())
}

fn drafts_from_items(raws: &[Raw]) -> Vec<Draft> {
    let base_indent = raws
        .iter()
        .filter(|r| r.kind == BlockKind::ListItem && (r.ordered || r.checked.is_some()))
        .map(|r| r.indent)
        .min();
    let Some(base_indent) = base_indent else {
        return Vec::new();
    };

    let mut drafts = Vec::new();
    for (idx, raw) in raws.iter().enumerate() {
        if raw.kind != BlockKind::ListItem
            || raw.indent != base_indent
            || !(raw.ordered || raw.checked.is_some())
        {
            continue;
        }
        let (clean, conv) = take_conventions(&raw.text);
        let mut lines = clean.lines();
        let title = lines.next().unwrap_or_default().trim().to_owned();
        let summary = non_empty(lines.collect::<Vec<_>>().join("\n"));
        drafts.push(Draft {
            title,
            summary,
            status: if raw.checked == Some(true) {
                StepStatus::Done
            } else {
                StepStatus::Todo
            },
            conv,
            owns: vec![idx],
        });
    }
    drafts.retain(|d| !d.title.is_empty());
    drafts
}

fn drafts_from_headings(raws: &[Raw]) -> Vec<Draft> {
    let mut drafts: Vec<Draft> = Vec::new();
    for (idx, raw) in raws.iter().enumerate() {
        if raw.kind == BlockKind::Heading && raw.level == Some(2) {
            let (clean, conv) = take_conventions(&raw.text);
            drafts.push(Draft {
                title: clean.trim().to_owned(),
                summary: None,
                status: StepStatus::Todo,
                conv,
                owns: vec![idx],
            });
            continue;
        }
        // A deeper heading closes nothing: everything until the next `##` belongs to
        // the section, so a reviewer clicking a node highlights the whole section.
        if let Some(current) = drafts.last_mut() {
            if raw.kind == BlockKind::Heading && raw.level.is_some_and(|l| l < 2) {
                continue;
            }
            current.owns.push(idx);
            // Conventions written in the body of a section still count — but only in
            // prose. A code block that happens to contain a line starting `files:`
            // (a YAML fragment, a config sample) is showing you a file, not declaring
            // one, and absorbing it would put fiction in the step's file list.
            if !matches!(raw.kind, BlockKind::Paragraph | BlockKind::ListItem) {
                continue;
            }
            let (clean, conv) = take_conventions(&raw.text);
            if current.conv.after.is_none() {
                current.conv.after = conv.after;
            }
            current.conv.files.extend(conv.files);
            if current.summary.is_none() && raw.kind == BlockKind::Paragraph {
                current.summary = non_empty(clean);
            }
        }
    }
    drafts.retain(|d| !d.title.is_empty());
    drafts
}

fn non_empty(text: String) -> Option<String> {
    let trimmed = text.trim().to_owned();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Turn caller-supplied steps into stored steps: fill in missing ids, disambiguate
/// collisions, and resolve `depends_on` entries that name a title instead of an id.
///
/// That last part is not politeness. An LLM writing `depends_on: ["Migrate schema"]`
/// is the single most likely shape of a real `plan_publish` call, and without the
/// title fallback the most likely outcome of the app's headline tool would be a 400.
/// A reference that resolves to nothing is left ALONE, so [`crate::graph`] can report
/// it by name — for an explicit caller a dangling edge is a bug worth surfacing.
#[must_use]
pub fn steps_from_input(inputs: &[crate::model::StepInput]) -> Vec<Step> {
    let mut taken: HashMap<String, usize> = HashMap::new();
    let ids: Vec<String> = inputs
        .iter()
        .map(|input| match input.id.as_deref().map(str::trim) {
            Some(id) if !id.is_empty() => {
                let slugged = slug(id);
                let candidate = if slugged.is_empty() {
                    step_id(&input.title, &mut taken)
                } else if id.starts_with("s_") {
                    // Already in our shape; keep it so a caller can round-trip ids.
                    let count = taken.entry(id.to_owned()).or_insert(0);
                    *count += 1;
                    if *count == 1 {
                        id.to_owned()
                    } else {
                        format!("{id}_{count}")
                    }
                } else {
                    step_id(id, &mut taken)
                };
                candidate
            }
            _ => step_id(&input.title, &mut taken),
        })
        .collect();

    let titles: Vec<String> = inputs.iter().map(|i| i.title.to_lowercase()).collect();

    inputs
        .iter()
        .enumerate()
        .map(|(idx, input)| Step {
            id: ids[idx].clone(),
            title: input.title.clone(),
            summary: input.summary.clone(),
            depends_on: input
                .depends_on
                .iter()
                .map(|reference| resolve_input_ref(reference, &ids, &titles))
                .collect(),
            files: input.files.clone(),
            status: StepStatus::Todo,
            risk: input.risk.clone(),
        })
        .collect()
}

fn resolve_input_ref(reference: &str, ids: &[String], titles: &[String]) -> String {
    if ids.iter().any(|id| id == reference) {
        return reference.to_owned();
    }
    let as_id = format!("s_{}", slug(reference));
    if ids.contains(&as_id) {
        return as_id;
    }
    let lowered = reference.to_lowercase();
    if let Some(idx) = titles.iter().position(|t| *t == lowered) {
        return ids[idx].clone();
    }
    reference.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::StepInput;

    fn kinds(blocks: &[Block]) -> Vec<BlockKind> {
        blocks.iter().map(|b| b.kind).collect()
    }

    fn titles(steps: &[Step]) -> Vec<&str> {
        steps.iter().map(|s| s.title.as_str()).collect()
    }

    #[test]
    fn each_markdown_construct_becomes_its_own_block() {
        let md = "\
# Plan

Some prose that
wraps two lines.

- [ ] first thing
- [ ] second thing

> a caution

```rust
fn main() {}
```

```mermaid
graph TD; a-->b;
```
";
        let parsed = parse(md);
        assert_eq!(
            kinds(&parsed.blocks),
            vec![
                BlockKind::Heading,
                BlockKind::Paragraph,
                BlockKind::ListItem,
                BlockKind::ListItem,
                BlockKind::Quote,
                BlockKind::Code,
                BlockKind::Mermaid,
            ]
        );
        assert_eq!(parsed.blocks[0].level, Some(1));
        assert_eq!(parsed.blocks[5].text, "fn main() {}");
        assert_eq!(parsed.blocks[6].text, "graph TD; a-->b;");
        // Ordinals are positions, ids are not.
        assert_eq!(parsed.blocks[3].ordinal, 3);
    }

    #[test]
    fn front_matter_and_rules_are_not_blocks() {
        let parsed = parse("---\ntitle: x\n---\n\nreal content\n\n---\n\nmore\n");
        assert_eq!(kinds(&parsed.blocks), vec![BlockKind::Paragraph; 2]);
        assert_eq!(parsed.blocks[0].text, "real content");
    }

    #[test]
    fn a_block_id_survives_an_edit_to_a_different_block() {
        // This is the property the whole app rests on: revising one paragraph must
        // not move the anchor of any other.
        let first = parse("# Deploy\n\nStep one stays.\n\nStep two changes.\n");
        let second = parse("# Deploy\n\nStep one stays.\n\nStep two is completely rewritten.\n");
        assert_eq!(first.blocks[0].id, second.blocks[0].id, "heading moved");
        assert_eq!(first.blocks[1].id, second.blocks[1].id, "untouched para moved");
        assert_ne!(
            first.blocks[2].id, second.blocks[2].id,
            "an edited block must NOT keep its id — a stale annotation would then \
             claim to describe text that no longer exists"
        );
    }

    #[test]
    fn a_block_id_survives_being_moved_and_having_neighbours_inserted() {
        let before = parse("Alpha para.\n\nBravo para.\n");
        let after = parse("Inserted first.\n\nBravo para.\n\nAlpha para.\n");
        let alpha_before = &before.blocks[0].id;
        let alpha_after = &after.blocks[2].id;
        assert_eq!(alpha_before, alpha_after, "content addressing must ignore position");
        assert_eq!(before.blocks[1].id, after.blocks[1].id);
    }

    #[test]
    fn rewrapping_prose_does_not_move_its_id_but_reindenting_code_does() {
        let wrapped = parse("one two\nthree four\n");
        let joined = parse("one   two three    four\n");
        assert_eq!(wrapped.blocks[0].id, joined.blocks[0].id);

        let flat = parse("```python\nif x:\n    go()\n```\n");
        let indented = parse("```python\nif x:\n        go()\n```\n");
        assert_ne!(
            flat.blocks[0].id, indented.blocks[0].id,
            "whitespace in code is semantic; normalizing it away would hide an edit"
        );
    }

    #[test]
    fn identical_blocks_are_told_apart_only_by_occurrence() {
        let one = parse("same line\n\nfiller\n\nsame line\n");
        assert!(one.blocks[0].id.ends_with("_1"));
        assert!(one.blocks[2].id.ends_with("_2"));
        assert_eq!(
            one.blocks[0].id.rsplit_once('_').expect("suffix").0,
            one.blocks[2].id.rsplit_once('_').expect("suffix").0,
            "identical text must share a digest"
        );

        // Insert a second copy near the top. The occurrence numbers renumber, which is
        // the documented limit of content addressing — but the digest, and therefore
        // what the annotation is *about*, is unchanged.
        let two = parse("same line\n\nsame line\n\nfiller\n\nsame line\n");
        assert_eq!(one.blocks[0].id, two.blocks[0].id);
        assert_eq!(one.blocks[2].id, two.blocks[1].id);
        assert!(two.blocks[3].id.ends_with("_3"));
        // The filler paragraph, which nobody touched, keeps its id across the insert.
        assert_eq!(one.blocks[1].id, two.blocks[2].id);
    }

    #[test]
    fn numbered_items_become_steps_with_linear_dependencies() {
        let parsed = parse("1. Set up the database\n2. Migrate schema\n3. Backfill rows\n");
        assert_eq!(
            titles(&parsed.steps),
            vec!["Set up the database", "Migrate schema", "Backfill rows"]
        );
        assert_eq!(parsed.steps[0].id, "s_set-up-the-database");
        assert!(parsed.steps[0].depends_on.is_empty());
        assert_eq!(parsed.steps[1].depends_on, vec!["s_set-up-the-database"]);
        assert_eq!(parsed.steps[2].depends_on, vec!["s_migrate-schema"]);
        // The item block is attributed to the step it produced.
        assert_eq!(parsed.blocks[1].step_id.as_deref(), Some("s_migrate-schema"));
    }

    #[test]
    fn a_checked_item_is_already_done_and_plain_bullets_are_not_steps() {
        let parsed = parse("- [x] Ship it\n- [ ] Tell someone\n");
        assert_eq!(parsed.steps[0].status, StepStatus::Done);
        assert_eq!(parsed.steps[1].status, StepStatus::Todo);

        let prose = parse("- consideration one\n- consideration two\n");
        assert!(
            prose.steps.is_empty(),
            "plain bullets are prose; turning them into graph nodes buries the plan"
        );
    }

    #[test]
    fn the_after_convention_overrides_the_linear_default() {
        let parsed = parse(
            "1. Set up\n2. Write tests\n3. Migrate schema (after: Set up)\n4. Deploy (after: Migrate schema, Write tests)\n",
        );
        assert_eq!(parsed.steps[2].depends_on, vec!["s_set-up"]);
        assert_eq!(
            parsed.steps[3].depends_on,
            vec!["s_migrate-schema", "s_write-tests"]
        );
        // The convention is stripped out of the title, not left in it.
        assert_eq!(parsed.steps[2].title, "Migrate schema");
        assert_eq!(parsed.steps[3].title, "Deploy");
    }

    #[test]
    fn a_bare_number_names_the_nth_step() {
        // The form an author writing a numbered list actually reaches for. Before this
        // resolved, `(after: 1)` was silently dropped and a three-step plan rendered as
        // three disconnected nodes — the app's headline view showing nothing.
        let parsed = parse(
            "1. Add the session table\n2. Swap the cookie writer (after: 1)\n3. Backfill (after: 1, 2)\n",
        );
        assert_eq!(parsed.steps[1].depends_on, vec!["s_add-the-session-table"]);
        assert_eq!(
            parsed.steps[2].depends_on,
            vec!["s_add-the-session-table", "s_swap-the-cookie-writer"]
        );
    }

    #[test]
    fn a_number_that_points_nowhere_or_at_itself_is_dropped() {
        // Out of range and self-reference are both typos. Handing the self-edge to the
        // graph layer would be a cycle, and a cycle refuses the whole publish.
        let parsed = parse("1. Set up\n2. Cut over (after: 9)\n3. Verify (after: 3)\n");
        assert!(parsed.steps[1].depends_on.is_empty(), "9 names no step");
        assert!(parsed.steps[2].depends_on.is_empty(), "3 is itself");
    }

    #[test]
    fn a_step_titled_with_a_number_still_wins_over_the_ordinal_reading() {
        // `after: 2024` should mean the step called "2024" when one exists. Ids are
        // checked for an exact match first only for id-shaped refs, so this pins the
        // one case where the two readings collide.
        let parsed = parse("1. 2024\n2. Later (after: 2024)\n");
        assert_eq!(
            parsed.steps[1].depends_on,
            vec!["s_2024"],
            "a numeric ref out of ordinal range falls through to the title match"
        );
    }

    #[test]
    fn after_none_makes_a_root_and_an_unresolvable_ref_is_dropped_not_fatal() {
        let parsed = parse("1. Set up\n2. Independent thing (after: none)\n3. Typo (after: nonexistent step)\n");
        assert!(parsed.steps[1].depends_on.is_empty(), "`after: none` means root");
        assert!(
            parsed.steps[2].depends_on.is_empty(),
            "a typo in a convention must not fail the publish — the prose is still \
             in the block for a human to read"
        );
    }

    #[test]
    fn a_forward_reference_resolves_because_ids_are_assigned_before_edges() {
        let parsed = parse("1. Deploy (after: Build)\n2. Build\n");
        assert_eq!(parsed.steps[0].depends_on, vec!["s_build"]);
    }

    #[test]
    fn the_files_convention_is_collected_from_both_forms() {
        let parsed = parse(
            "1. Migrate schema (files: db/schema.sql, db/up.rs)\n2. Wire the route\n   files: src/api.rs\n",
        );
        assert_eq!(parsed.steps[0].files, vec!["db/schema.sql", "db/up.rs"]);
        assert_eq!(parsed.steps[0].title, "Migrate schema");
        assert_eq!(parsed.steps[1].files, vec!["src/api.rs"]);
        assert_eq!(parsed.steps[1].title, "Wire the route");
        assert_eq!(parsed.steps[1].summary, None, "a files: line is not a summary");
    }

    #[test]
    fn both_conventions_can_share_one_item() {
        let parsed = parse("1. Set up\n2. Cut over (after: Set up) (files: a.rs, b.rs)\n");
        assert_eq!(parsed.steps[1].title, "Cut over");
        assert_eq!(parsed.steps[1].depends_on, vec!["s_set-up"]);
        assert_eq!(parsed.steps[1].files, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn headings_are_the_fallback_and_carry_their_section() {
        let md = "\
# Plan

## Set up

We create the tables first.

More detail here.

## Migrate

files: db/up.rs

Then we move the rows.
";
        let parsed = parse(md);
        assert_eq!(titles(&parsed.steps), vec!["Set up", "Migrate"]);
        assert_eq!(parsed.steps[0].summary.as_deref(), Some("We create the tables first."));
        assert_eq!(parsed.steps[1].depends_on, vec!["s_set-up"]);
        assert_eq!(parsed.steps[1].files, vec!["db/up.rs"]);
        // Every block under `## Set up` is attributed to it; the `#` title is not.
        assert_eq!(parsed.blocks[0].step_id, None);
        assert_eq!(parsed.blocks[1].step_id.as_deref(), Some("s_set-up"));
        assert_eq!(parsed.blocks[3].step_id.as_deref(), Some("s_set-up"));
        assert_eq!(parsed.blocks[4].step_id.as_deref(), Some("s_migrate"));
    }

    #[test]
    fn numbered_items_win_over_headings_when_both_are_present() {
        let parsed = parse("## Section\n\n1. Real step\n2. Other step\n");
        assert_eq!(titles(&parsed.steps), vec!["Real step", "Other step"]);
    }

    #[test]
    fn nested_items_do_not_become_steps() {
        let parsed = parse("1. Top level\n    1. Sub detail\n2. Second\n");
        assert_eq!(titles(&parsed.steps), vec!["Top level", "Second"]);
    }

    #[test]
    fn duplicate_titles_get_a_numeric_suffix_only_on_collision() {
        let parsed = parse("1. Deploy\n2. Deploy\n3. Deploy\n");
        let ids: Vec<&str> = parsed.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["s_deploy", "s_deploy_2", "s_deploy_3"]);
    }

    #[test]
    fn slugs_cannot_produce_an_id_the_store_would_reject() {
        assert_eq!(slug("Refund Policy 2026"), "refund-policy-2026");
        assert_eq!(slug("../../etc/passwd"), "etc-passwd");
        assert_eq!(slug("!!!"), "");
        assert!(slug("---leading").starts_with('l'));
        assert!(slug(&"x".repeat(200)).len() <= MAX_SLUG);
        // Every derived id must match `[a-z0-9][a-z0-9_-]{0,63}`.
        let parsed = parse("1. ¡Hola! ¿Qué tal?\n2. ¡Hola! ¿Qué tal?\n");
        for step in &parsed.steps {
            assert!(crate::store::is_valid_id(&step.id), "bad id {}", step.id);
        }
        for block in &parsed.blocks {
            assert!(crate::store::is_valid_id(&block.id), "bad id {}", block.id);
        }
    }

    #[test]
    fn explicit_steps_resolve_dependencies_named_by_title() {
        let inputs = vec![
            StepInput {
                title: "Migrate schema".into(),
                ..StepInput::default()
            },
            StepInput {
                title: "Backfill".into(),
                // The shape an LLM actually emits: a human-readable title, not an id.
                depends_on: vec!["Migrate schema".into()],
                ..StepInput::default()
            },
            StepInput {
                title: "Cut over".into(),
                depends_on: vec!["s_backfill".into(), "ghost step".into()],
                ..StepInput::default()
            },
        ];
        let steps = steps_from_input(&inputs);
        assert_eq!(steps[1].depends_on, vec!["s_migrate-schema"]);
        assert_eq!(
            steps[2].depends_on,
            vec!["s_backfill", "ghost step"],
            "an unresolvable reference from an explicit caller is left alone so the \
             graph layer can name it in a 400"
        );
    }

    #[test]
    fn explicit_ids_are_kept_and_deduplicated() {
        let inputs = vec![
            StepInput {
                id: Some("s_custom".into()),
                title: "One".into(),
                ..StepInput::default()
            },
            StepInput {
                id: Some("s_custom".into()),
                title: "Two".into(),
                ..StepInput::default()
            },
            StepInput {
                id: Some("Not An Id".into()),
                title: "Three".into(),
                ..StepInput::default()
            },
        ];
        let steps = steps_from_input(&inputs);
        assert_eq!(steps[0].id, "s_custom");
        assert_eq!(steps[1].id, "s_custom_2");
        assert_eq!(steps[2].id, "s_not-an-id");
    }

    #[test]
    fn an_unterminated_fence_still_yields_its_block() {
        let parsed = parse("intro\n\n```sh\nmake build\n");
        assert_eq!(kinds(&parsed.blocks), vec![BlockKind::Paragraph, BlockKind::Code]);
        assert_eq!(parsed.blocks[1].text, "make build");
    }

    #[test]
    fn a_heading_needs_its_space_and_a_hashtag_is_prose() {
        let parsed = parse("#nothashtag\n");
        assert_eq!(kinds(&parsed.blocks), vec![BlockKind::Paragraph]);
    }

    #[test]
    fn empty_markdown_parses_to_nothing_rather_than_a_phantom_block() {
        let parsed = parse("   \n\n\n");
        assert!(parsed.blocks.is_empty());
        assert!(parsed.steps.is_empty());
    }
}
