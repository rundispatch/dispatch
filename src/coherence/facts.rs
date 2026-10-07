//! L1 symbol layer: derive what a delta assumes about the baseline (S0), and
//! check those assumptions against the source as it is now (S1).
//!
//! Facts come from the patch alone. `Modified` facts are the S0 declarations a
//! changed line falls in (context lines never count; a class or trait counts
//! only for changes outside its members); `Referenced` facts are declarations the added lines use and
//! that bind by the unique-name rule (exactly one declaration of that base name
//! in the baseline, and not a name the work itself declares); `File` facts cover files without symbol support and files
//! the new code mentions by path. Work is proportional to the delta: only the
//! delta's files and the few files `git grep` finds for its identifiers are
//! read or parsed, all from the baseline commit through the hardened Git
//! wrappers.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    path::{Component, Path},
};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::{
    AnalysisLevel, FactKind, FactOrigin, MustHold, Reason, ReasonCode,
    coherence::{
        WorkView,
        symbols::{
            FileSymbols, Lang, SymbolDecl, SymbolKind, extract, identifiers_in, lang_for_path,
            python_call_compatible,
        },
        world::WorldObservation,
    },
    source::{git_command, run_git},
};

const MAX_FACTS: usize = 200;
const MAX_REASONS: usize = 20;
const GREP_CHUNK: usize = 50;
const MAX_GROUP_FILES: usize = 40;
const MAX_FILES_PER_NAME: usize = 8;
const MAX_DISPLAY_CHARS: usize = 100;

/// Names too common to say anything about a specific declaration.
const DENYLIST: &[&str] = &[
    "new", "get", "set", "len", "main", "self", "Self", "default", "clone", "from", "into", "map",
    "iter", "push", "pop", "insert", "remove", "contains", "is_empty", "unwrap", "expect",
    "format", "println", "String", "Vec", "Option", "Result", "Some", "None", "Ok", "Err", "str",
    "i32", "u32", "u64", "usize", "bool", "print", "range", "list", "dict", "int", "float", "open",
    "super", "__init__",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeltaStatus {
    Added,
    Modified,
    Deleted,
}

/// One text file of the delta and the lines it actually changes; context lines
/// are never included. Ranges are 1-based and inclusive.
#[derive(Clone, Debug)]
pub struct DeltaFile {
    pub path: String,
    pub status: DeltaStatus,
    /// S0 lines the delta removes or replaces.
    pub old_ranges: Vec<(u32, u32)>,
    /// Lines the delta adds, in post-image coordinates.
    pub new_ranges: Vec<(u32, u32)>,
    /// Pure insertions (added lines with nothing removed beside them), as the
    /// S0 line after which the text goes; 0 means before line 1.
    pub insertions: Vec<u32>,
    /// The S0 blob the delta was taken against (`index <old>..`), when given.
    pub old_blob: Option<String>,
    hunks: Vec<Hunk>,
}

impl DeltaFile {
    /// The S0 lines each hunk covers, context included: what must still be
    /// there, unchanged and together, for the hunk to apply as written.
    pub fn spans(&self) -> Vec<(u32, u32)> {
        self.hunks
            .iter()
            .map(|hunk| {
                let end = hunk.old_start + hunk.old_len.max(1) - 1;
                (hunk.old_start, end)
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
struct Hunk {
    old_start: u32,
    old_len: u32,
    new_start: u32,
    new_len: u32,
    lines: Vec<HunkLine>,
}

#[derive(Clone, Debug)]
struct HunkLine {
    /// b' ', b'-' or b'+'.
    tag: u8,
    text: Vec<u8>,
    /// False when Git marked the line "\ No newline at end of file".
    newline: bool,
}

#[derive(Debug, Default)]
pub struct Derived {
    pub facts: Vec<MustHold>,
    /// Referenced names left unbound because they were ambiguous or too common.
    pub unbound: u32,
    /// Delta files whose S0 or post-image could not be analyzed.
    pub uncertain: Vec<String>,
    /// Supported delta files whose post-image does not parse cleanly, so what
    /// the work declares there is not known.
    pub unparsed: Vec<String>,
    /// Declarations the work adds, as (path, qualified name).
    pub introduced: Vec<(String, String)>,
    /// For each Modified declaration whose post-image parsed cleanly, whether
    /// what its users rely on still holds, keyed by (path, qualified name).
    pub contracts: BTreeMap<(String, String), Contract>,
}

/// What a change does to a declaration's contract: the part a `Referenced`
/// fact on it relies on (its signature, or for Python a compatible call).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Contract {
    Kept,
    Changed,
    Removed,
}

// ---------------------------------------------------------------------------
// Delta parsing

/// Parse a `git diff --full-index --no-renames --binary` patch. Binary files,
/// symlinks and files without hunks (mode-only) are skipped.
pub fn parse_patch(patch: &[u8]) -> Vec<DeltaFile> {
    let lines: Vec<&[u8]> = patch.split(|byte| *byte == b'\n').collect();
    let mut files = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if !lines[index].starts_with(b"diff --git ") {
            index += 1;
            continue;
        }
        index += 1;
        let (mut old_path, mut new_path) = (None, None);
        let (mut added, mut skip) = (false, false);
        let mut old_blob = None;
        while index < lines.len()
            && !lines[index].starts_with(b"diff --git ")
            && !lines[index].starts_with(b"@@ ")
        {
            let line = lines[index];
            if let Some(rest) = line.strip_prefix(b"--- ") {
                old_path = header_path(rest, b"a/");
            } else if let Some(rest) = line.strip_prefix(b"+++ ") {
                new_path = header_path(rest, b"b/");
            } else if line.starts_with(b"new file mode") {
                added = true;
            } else if let Some(rest) = line.strip_prefix(b"index ") {
                old_blob = rest
                    .split(|byte| *byte == b'.')
                    .next()
                    .map(|blob| String::from_utf8_lossy(blob).into_owned());
            } else if line.starts_with(b"GIT binary patch")
                || line.starts_with(b"Binary files")
                || line.ends_with(b"mode 120000")
            {
                skip = true;
            }
            index += 1;
        }
        let mut hunks = Vec::new();
        let mut malformed = false;
        while index < lines.len() && lines[index].starts_with(b"@@ ") {
            match parse_hunk(&lines, &mut index) {
                Some(hunk) => hunks.push(hunk),
                None => {
                    malformed = true;
                    break;
                }
            }
        }
        let status = match (&old_path, &new_path) {
            (None, Some(_)) => DeltaStatus::Added,
            (Some(_), None) => DeltaStatus::Deleted,
            _ if added => DeltaStatus::Added,
            _ => DeltaStatus::Modified,
        };
        let path = if status == DeltaStatus::Deleted {
            old_path
        } else {
            new_path
        };
        let Some(path) = path else { continue };
        if skip || malformed || hunks.is_empty() {
            continue;
        }
        let (old_ranges, new_ranges, insertions) = changed_lines(&hunks);
        files.push(DeltaFile {
            path,
            status,
            old_ranges,
            new_ranges,
            insertions,
            old_blob,
            hunks,
        });
    }
    files
}

/// Every path the patch changes, with its status, including the binary,
/// symlink and mode-only files `parse_patch` skips. With `--no-renames` both
/// sides of a `diff --git` line name the same path.
pub fn changed_paths(patch: &[u8]) -> Vec<(String, DeltaStatus)> {
    let mut paths: Vec<(String, DeltaStatus)> = Vec::new();
    for line in patch.split(|byte| *byte == b'\n') {
        if let Some(rest) = line.strip_prefix(b"diff --git ") {
            let path = if rest.starts_with(b"\"") {
                let end = (1..rest.len())
                    .find(|&at| rest[at] == b'"' && rest[at - 1] != b'\\')
                    .unwrap_or(rest.len() - 1);
                unquote(&rest[..=end])
            } else {
                rest[..rest.len().saturating_sub(1) / 2].to_vec()
            };
            let path = path.strip_prefix(b"a/").unwrap_or(&path);
            paths.push((
                String::from_utf8_lossy(path).into_owned(),
                DeltaStatus::Modified,
            ));
        } else if let Some((_, status)) = paths.last_mut() {
            if line.starts_with(b"new file mode") {
                *status = DeltaStatus::Added;
            } else if line.starts_with(b"deleted file mode") {
                *status = DeltaStatus::Deleted;
            }
        }
    }
    paths
}

type Ranges = Vec<(u32, u32)>;

/// The removed lines (S0), added lines (post-image) and pure-insertion anchors
/// of `hunks`, with adjacent lines merged into ranges.
fn changed_lines(hunks: &[Hunk]) -> (Ranges, Ranges, Vec<u32>) {
    fn push(ranges: &mut Vec<(u32, u32)>, line: u32) {
        match ranges.last_mut() {
            Some((_, end)) if *end + 1 == line => *end = line,
            _ => ranges.push((line, line)),
        }
    }
    let (mut removed, mut added, mut insertions) = (Vec::new(), Vec::new(), Vec::new());
    for hunk in hunks {
        // A zero length makes the header's start the line *before* the hunk.
        let mut old_line = hunk.old_start + u32::from(hunk.old_len == 0);
        let mut new_line = hunk.new_start + u32::from(hunk.new_len == 0);
        // The S0 line before the run of changed lines in progress, and whether
        // the run removes anything (a run that only adds is an insertion).
        let mut run: Option<(u32, bool)> = None;
        for line in &hunk.lines {
            if line.tag == b' ' {
                if let Some((anchor, false)) = run.take() {
                    insertions.push(anchor);
                }
                old_line += 1;
                new_line += 1;
                continue;
            }
            let entry = run.get_or_insert((old_line - 1, false));
            if line.tag == b'-' {
                entry.1 = true;
                push(&mut removed, old_line);
                old_line += 1;
            } else {
                push(&mut added, new_line);
                new_line += 1;
            }
        }
        if let Some((anchor, false)) = run {
            insertions.push(anchor);
        }
    }
    (removed, added, insertions)
}

/// The path of a `---`/`+++` line, or `None` for `/dev/null`.
fn header_path(rest: &[u8], prefix: &[u8]) -> Option<String> {
    let rest = rest.split(|byte| *byte == b'\t').next().unwrap_or(rest);
    if rest == b"/dev/null" {
        return None;
    }
    let bytes = if rest.starts_with(b"\"") {
        unquote(rest)
    } else {
        rest.to_vec()
    };
    let bytes = bytes.strip_prefix(prefix).unwrap_or(&bytes);
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Undo Git's C-style path quoting.
fn unquote(quoted: &[u8]) -> Vec<u8> {
    let inner = quoted
        .strip_prefix(b"\"")
        .and_then(|rest| rest.strip_suffix(b"\""))
        .unwrap_or(quoted);
    let mut out = Vec::new();
    let mut index = 0;
    while index < inner.len() {
        if inner[index] != b'\\' || index + 1 == inner.len() {
            out.push(inner[index]);
            index += 1;
            continue;
        }
        index += 1;
        match inner[index] {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            digit @ b'0'..=b'7' => {
                let mut value = u32::from(digit - b'0');
                for _ in 0..2 {
                    match inner.get(index + 1) {
                        Some(next @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(next - b'0');
                            index += 1;
                        }
                        _ => break,
                    }
                }
                out.push(value as u8);
            }
            other => out.push(other),
        }
        index += 1;
    }
    out
}

/// Parse the hunk at `lines[*index]`, leaving `*index` after its last line.
fn parse_hunk(lines: &[&[u8]], index: &mut usize) -> Option<Hunk> {
    let header = std::str::from_utf8(lines[*index]).ok()?;
    let mut parts = header.strip_prefix("@@ ")?.split(' ');
    let (old_start, old_len) = parse_span(parts.next()?.strip_prefix('-')?)?;
    let (new_start, new_len) = parse_span(parts.next()?.strip_prefix('+')?)?;
    *index += 1;
    let (mut old_left, mut new_left) = (old_len, new_len);
    let mut hunk_lines: Vec<HunkLine> = Vec::new();
    while old_left > 0 || new_left > 0 {
        let line = *lines.get(*index)?;
        let tag = *line.first()?;
        match tag {
            b' ' => (old_left, new_left) = (old_left.checked_sub(1)?, new_left.checked_sub(1)?),
            b'-' => old_left = old_left.checked_sub(1)?,
            b'+' => new_left = new_left.checked_sub(1)?,
            _ => return None,
        }
        *index += 1;
        // "\ No newline at end of file" follows the line it describes.
        let newline = !lines
            .get(*index)
            .is_some_and(|next| next.starts_with(b"\\"));
        if !newline {
            *index += 1;
        }
        hunk_lines.push(HunkLine {
            tag,
            text: line[1..].to_vec(),
            newline,
        });
    }
    Some(Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
        lines: hunk_lines,
    })
}

/// `start[,len]` of a hunk header; a missing length is 1.
fn parse_span(span: &str) -> Option<(u32, u32)> {
    match span.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
        None => Some((span.parse().ok()?, 1)),
    }
}

/// `base` with the hunks applied: context and `+` lines are kept, `-` lines
/// dropped. Hunks apply exactly to the baseline by construction; any mismatch
/// (or an out-of-order hunk) returns `None` and the file is unanalyzable.
fn post_image(base: &[u8], hunks: &[Hunk]) -> Option<Vec<u8>> {
    let base_lines: Vec<&[u8]> = base.split_inclusive(|byte| *byte == b'\n').collect();
    let mut out = Vec::with_capacity(base.len());
    let mut next = 0_usize;
    for hunk in hunks {
        let start = if hunk.old_len == 0 {
            hunk.old_start
        } else {
            hunk.old_start.checked_sub(1)?
        } as usize;
        if start < next || start > base_lines.len() {
            return None;
        }
        for line in &base_lines[next..start] {
            out.extend_from_slice(line);
        }
        next = start;
        for line in &hunk.lines {
            let mut expected = line.text.clone();
            if line.newline {
                expected.push(b'\n');
            }
            if line.tag != b'+' {
                if *base_lines.get(next)? != expected.as_slice() {
                    return None;
                }
                next += 1;
            }
            if line.tag != b'-' {
                out.extend_from_slice(&expected);
            }
        }
    }
    for line in base_lines.get(next..)? {
        out.extend_from_slice(line);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Baseline (S0) access

/// The S0 blob at `path`, or `None` when it cannot be read.
fn read_s0(work: &WorkView, path: &str) -> Option<Vec<u8>> {
    read_s0_many(work, &[path.to_owned()]).ok()?.pop()?
}

/// S0 blobs for `paths` through one `git cat-file --batch`, in order.
fn read_s0_many(work: &WorkView, paths: &[String]) -> Result<Vec<Option<Vec<u8>>>> {
    let mut input = Vec::new();
    for path in paths {
        // A newline would split the request; such a path is reported missing.
        let spec = if path.contains('\n') {
            String::new()
        } else {
            format!("{}:{path}", work.baseline_commit)
        };
        input.extend_from_slice(spec.as_bytes());
        input.push(b'\n');
    }
    let mut command = git_command(work.baseline);
    command.args(["cat-file", "--batch"]);
    let output = run_git(command, Some(&input), "failed to read baseline files")?;
    let out = output.stdout;
    let mut blobs = Vec::with_capacity(paths.len());
    let mut at = 0_usize;
    for _ in paths {
        let end = out
            .get(at..)
            .and_then(|rest| rest.iter().position(|byte| *byte == b'\n'))
            .map(|offset| at + offset);
        let Some(end) = end else {
            blobs.push(None);
            continue;
        };
        let header = String::from_utf8_lossy(&out[at..end]).into_owned();
        at = end + 1;
        let fields: Vec<&str> = header.split(' ').collect();
        let size = match fields.as_slice() {
            [_, kind, size] => size.parse::<usize>().ok().map(|size| (*kind, size)),
            _ => None,
        };
        match size {
            Some((kind, size)) if at + size <= out.len() => {
                blobs.push((kind == "blob").then(|| out[at..at + size].to_vec()));
                at += size + 1;
            }
            _ => blobs.push(None),
        }
    }
    Ok(blobs)
}

/// Whether the baseline repository can be read at its commit.
fn baseline_readable(work: &WorkView) -> bool {
    let mut command = git_command(work.baseline);
    command
        .args(["cat-file", "-e"])
        .arg(format!("{}^{{commit}}", work.baseline_commit));
    run_git(command, None, "failed to inspect the baseline")
        .is_ok_and(|output| output.status.success())
}

/// Every tracked path at the baseline commit.
fn s0_paths(work: &WorkView) -> Result<Vec<String>> {
    let mut command = git_command(work.baseline);
    command
        .args(["ls-tree", "-r", "--name-only", "-z"])
        .arg(work.baseline_commit);
    let output = run_git(command, None, "failed to list baseline files")?;
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
}

/// Baseline `*.rs`/`*.py` files that contain any of `names` as a whole word.
fn grep_files(work: &WorkView, names: &[String]) -> Result<Vec<String>> {
    let mut command = git_command(work.baseline);
    command.args(["grep", "-l", "-z", "-I", "-w", "-F"]);
    for name in names {
        command.arg("-e").arg(name);
    }
    command
        .arg(work.baseline_commit)
        .args(["--", "*.rs", "*.py"]);
    // Exit status 1 means no match, which is an empty list.
    let output = run_git(command, None, "failed to search the baseline")?;
    let prefix = format!("{}:", work.baseline_commit);
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            let entry = String::from_utf8_lossy(entry).into_owned();
            entry.strip_prefix(&prefix).map(str::to_owned)
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Fact derivation

fn make_fact(
    kind: FactKind,
    path: &str,
    subject: &str,
    origin: FactOrigin,
    sig_fp: String,
    full_fp: Option<String>,
    display: String,
) -> MustHold {
    let tag = match kind {
        FactKind::Signature => "signature",
        FactKind::File => "file",
    };
    let digest = Sha256::digest(format!("{tag}|{path}|{subject}").as_bytes());
    MustHold {
        id: hex::encode(digest)[..16].to_owned(),
        kind,
        path: path.to_owned(),
        subject: subject.to_owned(),
        origin,
        sig_fp,
        full_fp,
        display,
    }
}

fn file_fact(path: &str, bytes: &[u8]) -> MustHold {
    make_fact(
        FactKind::File,
        path,
        "",
        FactOrigin::FileFallback,
        hex::encode(Sha256::digest(bytes)),
        None,
        path.to_owned(),
    )
}

/// What `delta` assumes about the baseline. Modified facts come first, then
/// Referenced, then File; ids are unique and the set is capped.
pub fn derive_facts(work: &WorkView, delta: &[DeltaFile]) -> Result<Derived> {
    let mut derived = Derived::default();
    let mut modified = Vec::new();
    let mut file_facts = Vec::new();
    let mut names = BTreeSet::new();
    let mut introduced = BTreeSet::new();
    for file in delta {
        let lang = lang_for_path(&file.path);
        let s0 = match file.status {
            DeltaStatus::Added => Some(Vec::new()),
            _ => read_s0(work, &file.path),
        };
        let Some(s0) = s0 else {
            derived.uncertain.push(file.path.clone());
            continue;
        };
        let Some(lang) = lang else {
            if file.status != DeltaStatus::Added {
                file_facts.push(file_fact(&file.path, &s0));
            }
            continue;
        };
        let before = if file.status == DeltaStatus::Added {
            Some(FileSymbols::default())
        } else {
            extract(lang, &s0).ok()
        };
        let mut after: Option<FileSymbols> = None;
        if file.status != DeltaStatus::Deleted {
            let post = post_image(&s0, &file.hunks);
            // What the new code uses: identifiers on the lines it adds.
            match post
                .as_ref()
                .and_then(|post| identifiers_in(lang, post, &file.new_ranges).ok())
            {
                Some(found) => names.extend(found.into_iter().filter(|name| bindable(name))),
                None => derived.uncertain.push(file.path.clone()),
            }
            // Names the work declares itself are not the baseline's to bind.
            if let (Some(post), Some(before)) = (&post, &before)
                && let Ok(parsed) = extract(lang, post)
            {
                let known: HashSet<&str> =
                    before.decls.iter().map(|d| base_name(&d.name)).collect();
                introduced.extend(
                    parsed
                        .decls
                        .iter()
                        .map(|decl| base_name(&decl.name))
                        .filter(|name| !known.contains(name))
                        .map(str::to_owned),
                );
                if !parsed.has_error {
                    let existing: HashSet<&str> =
                        before.decls.iter().map(|d| d.name.as_str()).collect();
                    derived.introduced.extend(
                        parsed
                            .decls
                            .iter()
                            .filter(|decl| !existing.contains(decl.name.as_str()))
                            .map(|decl| (file.path.clone(), decl.name.clone())),
                    );
                    after = Some(parsed);
                }
            }
            // Only the post-image can be at fault here: a baseline that does
            // not parse makes the file file-level below, whatever the work does.
            if after.is_none() && before.as_ref().is_some_and(|b| !b.has_error) {
                derived.unparsed.push(file.path.clone());
            }
        }
        if file.status != DeltaStatus::Modified {
            continue;
        }
        match before {
            Some(symbols) if !symbols.has_error => {
                for decl in &symbols.decls {
                    if touches(file, decl, &symbols.decls) {
                        if let Some(after) = &after {
                            let contract = match after.decls.iter().find(|d| d.name == decl.name) {
                                None => Contract::Removed,
                                Some(now)
                                    if now.sig_fp == decl.sig_fp
                                        || (lang == Lang::Python
                                            && python_call_compatible(
                                                &decl.display,
                                                &now.display,
                                            )) =>
                                {
                                    Contract::Kept
                                }
                                Some(_) => Contract::Changed,
                            };
                            derived
                                .contracts
                                .insert((file.path.clone(), decl.name.clone()), contract);
                        }
                        // A container is checked by its header alone.
                        let full_fp = (!is_container(decl)).then(|| decl.full_fp.clone());
                        modified.push(make_fact(
                            FactKind::Signature,
                            &file.path,
                            &decl.name,
                            FactOrigin::Modified,
                            decl.sig_fp.clone(),
                            full_fp,
                            decl.display.clone(),
                        ));
                    }
                }
            }
            _ => {
                derived.uncertain.push(file.path.clone());
                file_facts.push(file_fact(&file.path, &s0));
            }
        }
    }
    names.retain(|name| !introduced.contains(name));
    let skip: HashSet<(String, String)> = modified
        .iter()
        .map(|fact| (fact.path.clone(), fact.subject.clone()))
        .collect();
    let (referenced, unbound) = bind_names(work, &names, &skip, None)?;
    derived.unbound = unbound;
    file_facts.extend(mentioned_files(work, delta)?);

    let mut seen = HashSet::new();
    derived.facts = modified
        .into_iter()
        .chain(referenced)
        .chain(file_facts)
        .filter(|fact| seen.insert(fact.id.clone()))
        .take(MAX_FACTS)
        .collect();
    Ok(derived)
}

fn is_container(decl: &SymbolDecl) -> bool {
    matches!(decl.kind, SymbolKind::Class | SymbolKind::Trait)
}

/// Whether the delta changes `decl`: it removes one of its lines, or inserts
/// text strictly inside it. A container's members are symbols of their own, so
/// only changes outside every member count for the container.
fn touches(file: &DeltaFile, decl: &SymbolDecl, decls: &[SymbolDecl]) -> bool {
    let members: Vec<&SymbolDecl> = if is_container(decl) {
        decls
            .iter()
            .filter(|member| {
                member.start_line >= decl.start_line
                    && member.end_line <= decl.end_line
                    && (member.start_line, member.end_line) != (decl.start_line, decl.end_line)
            })
            .collect()
    } else {
        Vec::new()
    };
    let removed = file.old_ranges.iter().any(|&(low, high)| {
        (low.max(decl.start_line)..=high.min(decl.end_line)).any(|line| {
            !members
                .iter()
                .any(|member| member.start_line <= line && line <= member.end_line)
        })
    });
    let inserted = file.insertions.iter().any(|&anchor| {
        decl.start_line <= anchor
            && anchor < decl.end_line
            && !members
                .iter()
                .any(|member| member.start_line <= anchor && anchor < member.end_line)
    });
    removed || inserted
}

fn bindable(name: &str) -> bool {
    name.chars().count() >= 3 && !DENYLIST.contains(&name)
}

/// The last segment of a qualified declaration name: `Point::new` -> `new`,
/// `Point.norm` -> `norm`, `<Shape as Area>::area` -> `area`.
fn base_name(name: &str) -> &str {
    let name = match (name.starts_with('<'), name.rfind(">::")) {
        (true, Some(at)) => &name[at + 3..],
        _ => name,
    };
    name.rsplit([':', '.']).next().unwrap_or(name)
}

fn contains_word(haystack: &[u8], name: &str) -> bool {
    let word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80;
    let needle = name.as_bytes();
    let mut from = 0;
    while let Some(offset) = haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
    {
        let (start, end) = (from + offset, from + offset + needle.len());
        if (start == 0 || !word(haystack[start - 1]))
            && (end == haystack.len() || !word(haystack[end]))
        {
            return true;
        }
        from = start + 1;
    }
    false
}

/// A baseline file read and parsed for name binding.
struct Candidate {
    bytes: Vec<u8>,
    symbols: FileSymbols,
}

/// Bind each name to the one declaration that bears it in the baseline, if
/// there is exactly one. Names are looked up in groups with one `git grep`;
/// a group that matches too many files is split until each is small enough to
/// read completely, so uniqueness is judged over every file that could declare
/// the name. Returns the facts and the number of names left unbound; `causes`,
/// when given, receives each name's outcome for `audit_names`.
fn bind_names(
    work: &WorkView,
    names: &BTreeSet<String>,
    skip: &HashSet<(String, String)>,
    mut causes: Option<&mut BTreeMap<String, NameCause>>,
) -> Result<(Vec<MustHold>, u32)> {
    let mut record = |name: &str, cause| {
        if let Some(causes) = causes.as_deref_mut() {
            causes.insert(name.to_owned(), cause);
        }
    };
    let names: Vec<String> = names.iter().cloned().collect();
    let mut pending: Vec<Vec<String>> = names.chunks(GREP_CHUNK).map(<[String]>::to_vec).collect();
    let mut cache: HashMap<String, Option<Candidate>> = HashMap::new();
    let mut bound: BTreeMap<(String, String), MustHold> = BTreeMap::new();
    let mut unbound = 0_u32;
    while let Some(group) = pending.pop() {
        let files = grep_files(work, &group)?;
        if files.len() > MAX_GROUP_FILES {
            if group.len() == 1 {
                unbound += 1;
                record(&group[0], NameCause::Widespread);
            } else {
                let (left, right) = group.split_at(group.len() / 2);
                pending.push(left.to_vec());
                pending.push(right.to_vec());
            }
            continue;
        }
        let unread: Vec<String> = files
            .iter()
            .filter(|path| !cache.contains_key(*path))
            .cloned()
            .collect();
        for (path, blob) in unread.iter().zip(read_s0_many(work, &unread)?) {
            let candidate = blob.and_then(|bytes| {
                let symbols = extract(lang_for_path(path)?, &bytes).ok()?;
                Some(Candidate { bytes, symbols })
            });
            cache.insert(path.clone(), candidate);
        }
        for name in &group {
            let holding: Vec<(&String, &Candidate)> = files
                .iter()
                .filter_map(|path| Some((path, cache.get(path)?.as_ref()?)))
                .filter(|(_, candidate)| contains_word(&candidate.bytes, name))
                .collect();
            if holding.len() > MAX_FILES_PER_NAME {
                unbound += 1;
                record(name, NameCause::Widespread);
                continue;
            }
            let decls: Vec<_> = holding
                .iter()
                .flat_map(|(path, candidate)| {
                    candidate
                        .symbols
                        .decls
                        .iter()
                        .filter(|decl| base_name(&decl.name) == name)
                        .map(move |decl| (*path, decl))
                })
                .collect();
            match decls.as_slice() {
                [] => record(name, NameCause::NotFound),
                [(path, decl)] => {
                    record(name, NameCause::Bound);
                    let key = ((*path).clone(), decl.name.clone());
                    if !skip.contains(&key) {
                        let fact = make_fact(
                            FactKind::Signature,
                            path,
                            &decl.name,
                            FactOrigin::Referenced,
                            decl.sig_fp.clone(),
                            None,
                            decl.display.clone(),
                        );
                        bound.insert(key, fact);
                    }
                }
                _ => {
                    unbound += 1;
                    record(name, NameCause::Ambiguous);
                }
            }
        }
    }
    Ok((bound.into_values().collect(), unbound))
}

/// Why a name the added lines use did or did not bind (see `audit_names`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(not(test), allow(dead_code))]
enum NameCause {
    /// Exactly one declaration of its base name in the baseline.
    Bound,
    /// Several declarations of its base name in the baseline.
    Ambiguous,
    /// In more baseline files than binding reads, so never looked up.
    Widespread,
    /// Too short, or on the denylist.
    Denied,
    /// Declared by the work itself.
    Introduced,
    /// No declaration of its base name in the baseline.
    NotFound,
    /// Only on added lines of a file whose post-image could not be analyzed.
    Unsupported,
}

/// Measurement only: the cause for each name the delta's added lines use, by
/// the rules `derive_facts` binds with. `Ambiguous` plus `Widespread` names
/// are its `unbound`. Nothing here feeds a fact or a verdict.
#[cfg(test)]
fn audit_names(work: &WorkView, delta: &[DeltaFile]) -> Result<BTreeMap<String, NameCause>> {
    let (mut used, mut unsupported, mut introduced) =
        (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    for file in delta {
        let Some(lang) = lang_for_path(&file.path) else {
            continue;
        };
        if file.status == DeltaStatus::Deleted {
            continue;
        }
        let s0 = match file.status {
            DeltaStatus::Added => Some(Vec::new()),
            _ => read_s0(work, &file.path),
        };
        let post = s0.as_ref().and_then(|s0| post_image(s0, &file.hunks));
        match post
            .as_ref()
            .and_then(|post| identifiers_in(lang, post, &file.new_ranges).ok())
        {
            Some(found) => used.extend(found),
            None => {
                // `derive_facts` reads no names here; the added lines alone
                // still say which ones went unexamined.
                let mut added = Vec::new();
                for line in file.hunks.iter().flat_map(|hunk| &hunk.lines) {
                    if line.tag == b'+' {
                        added.extend_from_slice(&line.text);
                        added.push(b'\n');
                    }
                }
                let count = added.iter().filter(|byte| **byte == b'\n').count() as u32;
                unsupported.extend(identifiers_in(lang, &added, &[(1, count)]).unwrap_or_default());
            }
        }
        let before = match file.status {
            DeltaStatus::Added => Some(FileSymbols::default()),
            _ => s0.as_ref().and_then(|s0| extract(lang, s0).ok()),
        };
        if let (Some(post), Some(before)) = (&post, &before)
            && let Ok(parsed) = extract(lang, post)
        {
            let known: HashSet<&str> = before.decls.iter().map(|d| base_name(&d.name)).collect();
            introduced.extend(
                parsed
                    .decls
                    .iter()
                    .map(|decl| base_name(&decl.name))
                    .filter(|name| !known.contains(name))
                    .map(str::to_owned),
            );
        }
    }
    let mut causes = BTreeMap::new();
    let mut names = BTreeSet::new();
    for name in used.iter().chain(&unsupported) {
        let cause = if !bindable(name) {
            NameCause::Denied
        } else if introduced.contains(name) {
            NameCause::Introduced
        } else if used.contains(name) {
            names.insert(name.clone());
            continue;
        } else {
            NameCause::Unsupported
        };
        causes.insert(name.clone(), cause);
    }
    bind_names(work, &names, &HashSet::new(), Some(&mut causes))?;
    Ok(causes)
}

/// Path-like tokens (`name.ext`) in `line`, without trailing punctuation.
fn path_tokens(line: &str) -> impl Iterator<Item = &str> {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-')))
        .map(|token| token.trim_start_matches('-').trim_end_matches(['.', '-']))
        .filter(|token| {
            token.rsplit_once('.').is_some_and(|(stem, ext)| {
                !stem.is_empty()
                    && (1..=8).contains(&ext.len())
                    && ext.chars().all(|c| c.is_ascii_alphanumeric())
            })
        })
}

/// File facts for baseline files the added lines mention by path: an exact
/// tracked path, or a bare or partial path matching exactly one tracked file.
fn mentioned_files(work: &WorkView, delta: &[DeltaFile]) -> Result<Vec<MustHold>> {
    let mut tokens = BTreeSet::new();
    for file in delta {
        for hunk in &file.hunks {
            for line in hunk.lines.iter().filter(|line| line.tag == b'+') {
                let text = String::from_utf8_lossy(&line.text);
                for mut token in path_tokens(&text) {
                    while let Some(rest) = token
                        .strip_prefix("./")
                        .or_else(|| token.strip_prefix("../"))
                    {
                        token = rest;
                    }
                    tokens.insert(token.to_owned());
                }
            }
        }
    }
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    let paths = s0_paths(work)?;
    let mut by_name: HashMap<&str, Vec<&str>> = HashMap::new();
    for path in &paths {
        let name = path.rsplit('/').next().unwrap_or(path);
        by_name.entry(name).or_default().push(path);
    }
    let known: HashSet<&str> = paths.iter().map(String::as_str).collect();
    let delta_paths: HashSet<&str> = delta.iter().map(|file| file.path.as_str()).collect();
    let mut matched = BTreeSet::new();
    for token in &tokens {
        let name = token.rsplit('/').next().unwrap_or(token);
        let candidates = by_name.get(name).map(Vec::as_slice).unwrap_or_default();
        let hit = if known.contains(token.as_str()) {
            Some(token.as_str())
        } else if !token.contains('/') {
            (candidates.len() == 1).then(|| candidates[0])
        } else {
            let suffix = format!("/{token}");
            let ends: Vec<&str> = candidates
                .iter()
                .copied()
                .filter(|path| path.ends_with(&suffix))
                .collect();
            (ends.len() == 1).then(|| ends[0])
        };
        if let Some(path) = hit.filter(|path| !delta_paths.contains(path)) {
            matched.insert(path.to_owned());
        }
    }
    let matched: Vec<String> = matched.into_iter().collect();
    Ok(matched
        .iter()
        .zip(read_s0_many(work, &matched)?)
        .filter_map(|(path, blob)| Some(file_fact(path, &blob?)))
        .collect())
}

// ---------------------------------------------------------------------------
// Evaluation against the current source (S1)

/// Reasons why `facts` no longer hold in the source, Modified facts first.
/// Only facts whose file the world changed are checked, and always from what is
/// on disk now: the world's kind is a hint, not a fact (a nested repository is
/// reported Deleted while its files are still there).
pub fn evaluate_facts(
    work: &WorkView,
    world: &WorldObservation,
    facts: &[MustHold],
) -> Result<Vec<Reason>> {
    let changed: HashSet<&str> = world
        .changes
        .iter()
        .map(|change| change.path.as_str())
        .collect();
    let mut ordered: Vec<&MustHold> = facts
        .iter()
        .filter(|fact| changed.contains(fact.path.as_str()))
        .collect();
    ordered.sort_by_key(|fact| match fact.origin {
        FactOrigin::Modified => 0,
        FactOrigin::Referenced => 1,
        FactOrigin::FileFallback => 2,
    });
    let mut reasons = Vec::new();
    let mut parsed: HashMap<&str, Option<FileSymbols>> = HashMap::new();
    for fact in ordered {
        if reasons.len() >= MAX_REASONS {
            break;
        }
        let bytes = read_s1(work.source, &fact.path)?;
        let reason = |code, detail: String| Reason {
            code,
            fact_id: Some(fact.id.clone()),
            path: Some(fact.path.clone()),
            detail,
        };
        if fact.kind == FactKind::File {
            match &bytes {
                None => reasons.push(reason(
                    ReasonCode::FactMissing,
                    format!("{} no longer exists", fact.path),
                )),
                Some(bytes) if hex::encode(Sha256::digest(bytes)) != fact.sig_fp => {
                    reasons.push(reason(
                        ReasonCode::FactBroken,
                        format!("{} changed since the work started", fact.path),
                    ));
                }
                Some(_) => {}
            }
            continue;
        }
        let Some(bytes) = bytes else {
            reasons.push(reason(
                ReasonCode::FactMissing,
                format!(
                    "{} was in {}, which no longer exists",
                    fact.subject, fact.path
                ),
            ));
            continue;
        };
        let Some(lang) = lang_for_path(&fact.path) else {
            continue;
        };
        let symbols = parsed
            .entry(fact.path.as_str())
            .or_insert_with(|| extract(lang, &bytes).ok());
        let Some(symbols) = symbols.as_ref().filter(|symbols| !symbols.has_error) else {
            reasons.push(reason(
                ReasonCode::AnalysisUncertain,
                format!(
                    "{} does not parse; cannot check {}",
                    fact.path, fact.subject
                ),
            ));
            continue;
        };
        let decls: Vec<_> = symbols
            .decls
            .iter()
            .filter(|decl| decl.name == fact.subject)
            .collect();
        if decls.is_empty() {
            reasons.push(reason(
                ReasonCode::FactMissing,
                format!("{} no longer declared in {}", fact.subject, fact.path),
            ));
        } else if fact.origin == FactOrigin::Modified {
            // A container has no full fingerprint: only its header is compared.
            let holds = |decl: &&SymbolDecl| match &fact.full_fp {
                Some(full) => &decl.full_fp == full,
                None => decl.sig_fp == fact.sig_fp,
            };
            if !decls.iter().any(holds) {
                reasons.push(reason(
                    ReasonCode::SameSymbolEdited,
                    format!("{} was also edited in {}", fact.subject, fact.path),
                ));
            }
        } else if !decls.iter().any(|decl| {
            decl.sig_fp == fact.sig_fp
                // An added optional parameter still accepts the work's calls.
                || (lang == Lang::Python && python_call_compatible(&fact.display, &decl.display))
        }) {
            reasons.push(reason(
                ReasonCode::FactBroken,
                format!(
                    "{} => {}",
                    truncate(&fact.display),
                    truncate(&decls[0].display)
                ),
            ));
        }
    }
    Ok(reasons)
}

fn truncate(text: &str) -> String {
    text.chars().take(MAX_DISPLAY_CHARS).collect()
}

/// The bytes of the regular file at `path` in the source, `None` when there is
/// none there (missing, a directory, a symlink or an unsafe path).
fn read_s1(source: &Path, path: &str) -> Result<Option<Vec<u8>>> {
    let relative = Path::new(path);
    if !relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        return Ok(None);
    }
    let full = source.join(relative);
    match fs::symlink_metadata(&full) {
        Ok(metadata) if metadata.is_file() => {
            Ok(Some(fs::read(&full).with_context(|| {
                format!("failed to read {}", full.display())
            })?))
        }
        _ => Ok(None),
    }
}

/// The L1 verdict inputs for `work`: the reasons and how deep the analysis
/// went. An unreadable baseline gives no reasons and `FilesOnly`.
pub fn check(work: &WorkView, world: &WorldObservation) -> Result<(Vec<Reason>, AnalysisLevel)> {
    if !baseline_readable(work) {
        return Ok((Vec::new(), AnalysisLevel::FilesOnly));
    }
    let patch = fs::read(work.delta_patch)
        .with_context(|| format!("failed to read delta {}", work.delta_patch.display()))?;
    let derived = derive_facts(work, &parse_patch(&patch))?;
    let reasons = evaluate_facts(work, world, &derived.facts)?;
    let changed: HashSet<&str> = world
        .changes
        .iter()
        .map(|change| change.path.as_str())
        .collect();
    let symbols = derived
        .facts
        .iter()
        .any(|fact| fact.kind == FactKind::Signature && changed.contains(fact.path.as_str()));
    let analysis = if symbols {
        AnalysisLevel::Symbols
    } else {
        AnalysisLevel::FilesOnly
    };
    Ok((reasons, analysis))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;
    use crate::{
        Decision, SourceKind,
        coherence::{
            evaluate,
            world::{Change, ChangeKind, observe},
        },
        source::{
            SourceSnapshot, apply_patch_in_workspace, collect_diff, create_candidate_workspace,
            create_snapshot,
        },
    };

    /// A directory source snapshotted like a run, plus helpers to author deltas
    /// (through the real `collect_diff`) and to move the source afterwards.
    struct Repo {
        root: TempDir,
        source: PathBuf,
        snapshot: SourceSnapshot,
        patch: PathBuf,
        deltas: std::cell::Cell<u32>,
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    impl Repo {
        fn new(files: &[(&str, &str)]) -> Self {
            let root = TempDir::new().unwrap();
            let source = fs::canonicalize(root.path()).unwrap().join("source");
            for (path, contents) in files {
                write(&source.join(path), contents);
            }
            let snapshot = create_snapshot(&source, &root.path().join("run")).unwrap();
            let patch = root.path().join("delta.patch");
            Self {
                root,
                source,
                snapshot,
                patch,
                deltas: std::cell::Cell::new(0),
            }
        }

        /// Write the delta (`Some` = new contents, `None` = delete) into a
        /// candidate workspace and collect its patch.
        fn delta(&self, edits: &[(&str, Option<&str>)]) -> Vec<u8> {
            self.deltas.set(self.deltas.get() + 1);
            let workspace = create_candidate_workspace(
                &self.snapshot.baseline_path,
                &self
                    .root
                    .path()
                    .join(format!("workspace{}", self.deltas.get())),
            )
            .unwrap();
            for (path, contents) in edits {
                match contents {
                    Some(contents) => write(&workspace.join(path), contents),
                    None => fs::remove_file(workspace.join(path)).unwrap(),
                }
            }
            collect_diff(&self.snapshot.baseline_path, &workspace, &self.patch).unwrap();
            fs::read(&self.patch).unwrap()
        }

        fn work(&self) -> WorkView<'_> {
            WorkView {
                source: &self.source,
                delta_patch: &self.patch,
                baseline: &self.snapshot.baseline_path,
                baseline_commit: &self.snapshot.baseline_commit,
            }
        }

        fn derive(&self, edits: &[(&str, Option<&str>)]) -> Derived {
            let patch = self.delta(edits);
            derive_facts(&self.work(), &parse_patch(&patch)).unwrap()
        }

        /// Move the source, then observe it.
        fn world(&self, edits: &[(&str, Option<&str>)]) -> WorldObservation {
            for (path, contents) in edits {
                match contents {
                    Some(contents) => write(&self.source.join(path), contents),
                    None => fs::remove_file(self.source.join(path)).unwrap(),
                }
            }
            observe(
                &self.source,
                &self.snapshot.baseline_path,
                &self.snapshot.baseline_commit,
                &SourceKind::Directory,
            )
            .unwrap()
        }
    }

    fn find<'a>(facts: &'a [MustHold], path: &str, subject: &str) -> Option<&'a MustHold> {
        facts
            .iter()
            .find(|fact| fact.path == path && fact.subject == subject)
    }

    fn subjects(facts: &[MustHold], origin: FactOrigin) -> Vec<String> {
        facts
            .iter()
            .filter(|fact| fact.origin == origin)
            .map(|fact| format!("{}:{}", fact.path, fact.subject))
            .collect()
    }

    fn codes(reasons: &[Reason]) -> Vec<ReasonCode> {
        reasons.iter().map(|reason| reason.code).collect()
    }

    // ---- parse_patch -------------------------------------------------------

    fn numbered(count: usize, edited: &[usize]) -> String {
        (1..=count)
            .map(|n| {
                if edited.contains(&n) {
                    format!("line {n} edited\n")
                } else {
                    format!("line {n}\n")
                }
            })
            .collect()
    }

    #[test]
    fn changed_paths_names_every_file_including_those_without_hunks() {
        let repo = Repo::new(&[
            ("a b.txt", "one\n"),
            ("logo.bin", "\0\x01"),
            ("gone.txt", "bye\n"),
        ]);
        let patch = repo.delta(&[
            ("a b.txt", Some("two\n")),
            ("logo.bin", Some("\0\x02")),
            ("gone.txt", None),
            ("new.txt", Some("hello\n")),
        ]);
        let mut paths = changed_paths(&patch);
        paths.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            paths,
            vec![
                ("a b.txt".to_owned(), DeltaStatus::Modified),
                ("gone.txt".to_owned(), DeltaStatus::Deleted),
                ("logo.bin".to_owned(), DeltaStatus::Modified),
                ("new.txt".to_owned(), DeltaStatus::Added),
            ]
        );
        let parsed: Vec<String> = parse_patch(&patch).into_iter().map(|f| f.path).collect();
        assert!(!parsed.contains(&"logo.bin".to_owned()), "{parsed:?}");
        let quoted =
            b"diff --git \"a/sp\\303\\251c.txt\" \"b/sp\\303\\251c.txt\"\nindex 1..2 100644\n";
        assert_eq!(
            changed_paths(quoted),
            vec![("sp\u{e9}c.txt".to_owned(), DeltaStatus::Modified)]
        );
    }

    #[test]
    fn parse_patch_reads_modified_added_deleted_and_multi_hunk_files() {
        let repo = Repo::new(&[
            ("a.txt", &numbered(30, &[])),
            ("gone.txt", "bye\nnow\n"),
            ("keep.txt", "same\n"),
        ]);
        let edited = numbered(30, &[3, 27]);
        let patch = repo.delta(&[
            ("a.txt", Some(&edited)),
            ("new.rs", Some("fn a() {}\nfn b() {}\n")),
            ("gone.txt", None),
        ]);
        let files = parse_patch(&patch);
        let by_path: HashMap<&str, &DeltaFile> = files
            .iter()
            .map(|file| (file.path.as_str(), file))
            .collect();
        assert_eq!(files.len(), 3);

        let modified = by_path["a.txt"];
        assert_eq!(modified.status, DeltaStatus::Modified);
        // Only the edited lines: context is never reported.
        assert_eq!(modified.old_ranges, vec![(3, 3), (27, 27)]);
        assert_eq!(modified.new_ranges, vec![(3, 3), (27, 27)]);
        assert!(modified.insertions.is_empty());

        let added = by_path["new.rs"];
        assert_eq!(added.status, DeltaStatus::Added);
        assert_eq!(added.new_ranges, vec![(1, 2)]);
        assert!(added.old_ranges.is_empty());
        assert_eq!(added.insertions, vec![0]);

        let deleted = by_path["gone.txt"];
        assert_eq!(deleted.status, DeltaStatus::Deleted);
        assert_eq!(deleted.old_ranges, vec![(1, 2)]);
        assert!(deleted.new_ranges.is_empty());
        assert!(deleted.insertions.is_empty());
    }

    #[test]
    fn parse_patch_handles_pure_insertions_deletions_and_missing_newlines() {
        let patch = b"diff --git a/x.rs b/x.rs\nindex 1..2 100644\n--- a/x.rs\n+++ b/x.rs\n\
@@ -3,0 +4,2 @@\n+one\n+two\n@@ -9,2 +10,0 @@\n-gone\n-too\n\
diff --git a/y.rs b/y.rs\nindex 1..2 100644\n--- a/y.rs\n+++ b/y.rs\n\
@@ -0,0 +1 @@\n+only\n\\ No newline at end of file\n";
        let files = parse_patch(patch);
        assert_eq!(files.len(), 2);
        // The insertion goes after S0 line 3; the deletion removes lines 9-10.
        assert_eq!(files[0].insertions, vec![3]);
        assert_eq!(files[0].old_ranges, vec![(9, 10)]);
        assert_eq!(files[0].new_ranges, vec![(4, 5)]);
        assert_eq!(files[1].insertions, vec![0]);
        assert!(files[1].old_ranges.is_empty());
        assert_eq!(files[1].new_ranges, vec![(1, 1)]);
        assert!(!files[1].hunks[0].lines[0].newline);
    }

    #[test]
    fn parse_patch_skips_binary_and_mode_only_files() {
        let repo = Repo::new(&[
            ("logo.bin", "\u{0}PNG-old\u{1}\u{2}"),
            ("run.sh", "echo\n"),
            ("a.rs", "fn a() {}\n"),
        ]);
        let patch = repo.delta(&[
            ("logo.bin", Some("\u{0}PNG-new\u{1}\u{3}\u{4}")),
            ("a.rs", Some("fn a() {}\nfn b() {}\n")),
        ]);
        assert!(String::from_utf8_lossy(&patch).contains("GIT binary patch"));
        let files = parse_patch(&patch);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "a.rs");
        assert!(parse_patch(b"").is_empty());
        let mode_only = b"diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n";
        assert!(parse_patch(mode_only).is_empty());
    }

    #[test]
    fn parse_patch_unquotes_paths_with_special_characters() {
        let repo = Repo::new(&[("dir one/a b.rs", "fn a() {}\n")]);
        let patch = repo.delta(&[("dir one/a b.rs", Some("fn a() {}\nfn c() {}\n"))]);
        assert_eq!(parse_patch(&patch)[0].path, "dir one/a b.rs");
        assert_eq!(unquote(br#""caf\303\251/x\ty""#), "café/x\ty".as_bytes());
    }

    // ---- post-image applier ------------------------------------------------

    #[test]
    fn post_image_matches_git_apply_on_real_patches() {
        let long = numbered(40, &[]);
        let cases: Vec<(&str, &str, String)> = vec![
            ("edit", "a.rs", numbered(40, &[2, 20, 39])),
            ("grow", "a.rs", format!("{long}tail 1\ntail 2\n")),
            ("shrink", "a.rs", numbered(10, &[])),
            ("head", "a.rs", format!("head\n{long}")),
            ("no final newline", "a.rs", long.trim_end().to_owned()),
            ("empty", "a.rs", String::new()),
        ];
        for (name, path, edited) in cases {
            let repo = Repo::new(&[(path, &long)]);
            let patch = repo.delta(&[(path, Some(&edited))]);
            let files = parse_patch(&patch);
            let base = read_s0(&repo.work(), path).unwrap();
            assert_eq!(base, long.as_bytes());
            let post = post_image(&base, &files[0].hunks).unwrap();
            assert_eq!(post, edited.as_bytes(), "{name}: matches the candidate");

            let workspace = create_candidate_workspace(
                &repo.snapshot.baseline_path,
                &repo.root.path().join("applied"),
            )
            .unwrap();
            apply_patch_in_workspace(&workspace, &repo.patch).unwrap();
            assert_eq!(
                post,
                fs::read(workspace.join(path)).unwrap_or_default(),
                "{name}: matches git apply"
            );
        }
    }

    #[test]
    fn post_image_handles_added_files_and_a_missing_final_newline_in_the_base() {
        let repo = Repo::new(&[("a.rs", "fn a() {}\nfn last() {}")]);
        let patch = repo.delta(&[
            ("a.rs", Some("fn a() {}\nfn last() {}\nfn more() {}\n")),
            ("b.rs", Some("fn b() {}\n")),
        ]);
        let files = parse_patch(&patch);
        let post = post_image(b"fn a() {}\nfn last() {}", &files[0].hunks).unwrap();
        assert_eq!(post, b"fn a() {}\nfn last() {}\nfn more() {}\n");
        assert_eq!(post_image(b"", &files[1].hunks).unwrap(), b"fn b() {}\n");
    }

    #[test]
    fn post_image_rejects_a_patch_that_does_not_match_its_base() {
        let repo = Repo::new(&[("a.rs", "fn a() {}\nfn b() {}\n")]);
        let patch = repo.delta(&[("a.rs", Some("fn a() {}\nfn c() {}\n"))]);
        let files = parse_patch(&patch);
        assert!(post_image(b"fn a() {}\nfn other() {}\n", &files[0].hunks).is_none());
        assert!(post_image(b"fn a() {}\n", &files[0].hunks).is_none());
    }

    // ---- derivation --------------------------------------------------------

    const AUTH: &str = "pub fn validate(token: &str) -> bool {\n    !token.is_empty()\n}\n\npub fn refresh(token: &str) -> String {\n    token.to_owned()\n}\n";
    const HANDLER: &str = "pub fn handle(request: &str) -> bool {\n    let ok = true;\n    ok\n}\n";
    const HANDLER_DELTA: &str =
        "pub fn handle(request: &str) -> bool {\n    let ok = validate(request);\n    ok\n}\n";

    #[test]
    fn rust_delta_yields_modified_and_uniquely_bound_referenced_facts() {
        let repo = Repo::new(&[("src/auth.rs", AUTH), ("src/handler.rs", HANDLER)]);
        let derived = repo.derive(&[("src/handler.rs", Some(HANDLER_DELTA))]);
        assert_eq!(
            subjects(&derived.facts, FactOrigin::Modified),
            ["src/handler.rs:handle"]
        );
        assert_eq!(
            subjects(&derived.facts, FactOrigin::Referenced),
            ["src/auth.rs:validate"]
        );
        let modified = find(&derived.facts, "src/handler.rs", "handle").unwrap();
        assert_eq!(modified.kind, FactKind::Signature);
        assert!(modified.full_fp.is_some());
        assert_eq!(modified.display, "pub fn handle(request: &str) -> bool");
        let referenced = find(&derived.facts, "src/auth.rs", "validate").unwrap();
        assert!(referenced.full_fp.is_none());
        assert_eq!(referenced.display, "pub fn validate(token: &str) -> bool");
        assert_eq!(derived.unbound, 0);
        assert!(derived.uncertain.is_empty());
        // Deterministic ids and order.
        let again = repo.derive(&[("src/handler.rs", Some(HANDLER_DELTA))]);
        assert_eq!(derived.facts, again.facts);
        assert_eq!(modified.id.len(), 16);
    }

    #[test]
    fn an_ambiguous_name_is_not_bound_and_is_counted() {
        let repo = Repo::new(&[
            ("src/auth.rs", AUTH),
            ("src/other.rs", "pub fn validate(x: u8) -> u8 {\n    x\n}\n"),
            ("src/handler.rs", HANDLER),
        ]);
        let derived = repo.derive(&[("src/handler.rs", Some(HANDLER_DELTA))]);
        assert!(subjects(&derived.facts, FactOrigin::Referenced).is_empty());
        assert_eq!(derived.unbound, 1);
    }

    #[test]
    fn denylisted_and_short_names_are_never_bound() {
        let auth =
            "pub fn get(key: &str) -> u8 {\n    1\n}\n\npub fn ab(x: u8) -> u8 {\n    x\n}\n";
        let repo = Repo::new(&[("src/auth.rs", auth), ("src/handler.rs", HANDLER)]);
        let delta = "pub fn handle(request: &str) -> bool {\n    let ok = get(request) + ab(1);\n    ok\n}\n";
        let derived = repo.derive(&[("src/handler.rs", Some(delta))]);
        assert!(subjects(&derived.facts, FactOrigin::Referenced).is_empty());
        assert_eq!(derived.unbound, 0);
    }

    #[test]
    fn a_name_found_in_too_many_files_is_left_unbound() {
        let mut files: Vec<(String, String)> = (0..10)
            .map(|n| {
                (
                    format!("src/m{n}.rs"),
                    format!("pub fn user_{n}() {{\n    common_helper();\n}}\n"),
                )
            })
            .collect();
        files.push((
            "src/lib.rs".to_owned(),
            "pub fn common_helper() {}\n".to_owned(),
        ));
        files.push(("src/handler.rs".to_owned(), HANDLER.to_owned()));
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        let repo = Repo::new(&refs);
        let delta = "pub fn handle(request: &str) -> bool {\n    common_helper();\n    true\n}\n";
        let derived = repo.derive(&[("src/handler.rs", Some(delta))]);
        assert!(subjects(&derived.facts, FactOrigin::Referenced).is_empty());
        assert_eq!(derived.unbound, 1);
    }

    #[test]
    fn python_delta_yields_modified_and_referenced_facts_with_qualified_names() {
        let models = "class Account:\n    def __init__(self, owner):\n        self.owner = owner\n\n    def deposit(self, amount):\n        self.balance = amount\n";
        let billing =
            "def charge(owner, amount):\n    return None\n\n\ndef unrelated():\n    return 1\n";
        let delta = "def charge(owner, amount):\n    account = Account(owner)\n    account.deposit(amount)\n    return account\n\n\ndef unrelated():\n    return 1\n";
        let repo = Repo::new(&[("models.py", models), ("billing.py", billing)]);
        let derived = repo.derive(&[("billing.py", Some(delta))]);
        // The neighbouring `unrelated` (three lines below) is context, not edited.
        assert_eq!(
            subjects(&derived.facts, FactOrigin::Modified),
            ["billing.py:charge"]
        );
        assert_eq!(
            subjects(&derived.facts, FactOrigin::Referenced),
            ["models.py:Account", "models.py:Account.deposit"]
        );
    }

    /// The multi-agent trial's case: new code calls `format_user(user)` while
    /// the world adds an optional parameter. The call still holds; a required
    /// parameter would not.
    #[test]
    fn an_added_optional_python_parameter_keeps_callers_valid() {
        let textutil = "def format_user(user):\n    return user['name']\n";
        let repo = Repo::new(&[("textutil.py", textutil)]);
        let labels = "from textutil import format_user\n\n\ndef user_label(user):\n    return format_user(user)\n";
        let derived = repo.derive(&[("labels.py", Some(labels))]);
        assert_eq!(
            subjects(&derived.facts, FactOrigin::Referenced),
            ["textutil.py:format_user"]
        );
        let optional = repo.world(&[(
            "textutil.py",
            Some("def format_user(user, brackets=\"()\"):\n    return user['name']\n"),
        )]);
        assert!(
            evaluate_facts(&repo.work(), &optional, &derived.facts)
                .unwrap()
                .is_empty()
        );
        let required = repo.world(&[(
            "textutil.py",
            Some("def format_user(user, brackets):\n    return user['name']\n"),
        )]);
        let reasons = evaluate_facts(&repo.work(), &required, &derived.facts).unwrap();
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].code, ReasonCode::FactBroken);
    }

    #[test]
    fn a_file_without_symbol_support_gets_a_file_fact() {
        let repo = Repo::new(&[
            ("config.toml", "retries = 3\n"),
            ("src/a.rs", "fn a() {}\n"),
        ]);
        let derived = repo.derive(&[("config.toml", Some("retries = 5\n"))]);
        assert_eq!(derived.facts.len(), 1);
        let fact = &derived.facts[0];
        assert_eq!(fact.kind, FactKind::File);
        assert_eq!(fact.origin, FactOrigin::FileFallback);
        assert_eq!(fact.path, "config.toml");
        assert_eq!(fact.subject, "");
        assert_eq!(fact.sig_fp, hex::encode(Sha256::digest(b"retries = 3\n")));
        // A deleted unsupported file is as coarse; an added one has no baseline.
        let derived = repo.derive(&[("config.toml", None), ("new.toml", Some("a = 1\n"))]);
        assert_eq!(derived.facts.len(), 1);
        assert_eq!(derived.facts[0].path, "config.toml");
    }

    #[test]
    fn a_file_the_new_code_mentions_becomes_a_file_fact() {
        let repo = Repo::new(&[
            ("schema/user.json", "{}\n"),
            ("a/schema.json", "{}\n"),
            ("b/schema.json", "{}\n"),
            ("docs/limits.md", "text\n"),
            ("src/loader.rs", "pub fn load() -> u8 {\n    1\n}\n"),
        ]);
        let delta = "pub fn load() -> u8 {\n    let _s = include_str!(\"../schema/user.json\");\n    // see limits.md and schema.json and missing.json\n    1\n}\n";
        let derived = repo.derive(&[("src/loader.rs", Some(delta))]);
        let files: Vec<&str> = derived
            .facts
            .iter()
            .filter(|fact| fact.kind == FactKind::File)
            .map(|fact| fact.path.as_str())
            .collect();
        // Exact path (after dropping ../), unique basename; the ambiguous
        // schema.json and the unknown missing.json are not files we can bind.
        assert_eq!(files, ["docs/limits.md", "schema/user.json"]);
        for fact in derived.facts.iter().filter(|f| f.kind == FactKind::File) {
            assert_eq!(fact.origin, FactOrigin::FileFallback);
        }
        // The delta's own files are never facts about the baseline.
        let own = "pub fn load() -> u8 {\n    // src/loader.rs\n    1\n}\n";
        let derived = repo.derive(&[("src/loader.rs", Some(own))]);
        assert!(derived.facts.iter().all(|f| f.kind != FactKind::File));
    }

    #[test]
    fn an_unparsable_baseline_file_becomes_a_file_fact_and_is_reported() {
        let broken = "pub fn a( {\n    1\n}\n\npub fn b() {\n    2\n}\n";
        let repo = Repo::new(&[("src/a.rs", broken)]);
        let derived = repo.derive(&[(
            "src/a.rs",
            Some("pub fn a( {\n    1\n}\n\npub fn b() {\n    3\n}\n"),
        )]);
        assert_eq!(derived.uncertain, ["src/a.rs"]);
        assert_eq!(derived.facts.len(), 1);
        assert_eq!(derived.facts[0].kind, FactKind::File);
    }

    #[test]
    fn facts_are_capped_and_ordered_modified_then_referenced_then_file() {
        let mut source = String::new();
        let mut delta = String::new();
        for n in 0..250 {
            source.push_str(&format!("pub fn item_{n}() {{\n    1\n}}\n\n"));
            delta.push_str(&format!("pub fn item_{n}() {{\n    2\n}}\n\n"));
        }
        let repo = Repo::new(&[("src/big.rs", &source)]);
        let derived = repo.derive(&[("src/big.rs", Some(&delta))]);
        assert_eq!(derived.facts.len(), MAX_FACTS);
        assert!(
            derived
                .facts
                .iter()
                .all(|fact| fact.origin == FactOrigin::Modified)
        );
        let ids: HashSet<_> = derived.facts.iter().map(|fact| &fact.id).collect();
        assert_eq!(ids.len(), MAX_FACTS);
    }

    #[test]
    fn base_names_strip_qualifiers_and_wrappers() {
        assert_eq!(base_name("validate"), "validate");
        assert_eq!(base_name("Point::new"), "new");
        assert_eq!(base_name("Point.norm"), "norm");
        assert_eq!(base_name("<Shape as Area>::area"), "area");
        assert_eq!(base_name("<a::B as c::D>::area"), "area");
    }

    // ---- evaluation --------------------------------------------------------

    /// Facts for the handler/auth pair, and the repo they came from.
    fn handler_repo() -> (Repo, Vec<MustHold>) {
        let repo = Repo::new(&[("src/auth.rs", AUTH), ("src/handler.rs", HANDLER)]);
        let derived = repo.derive(&[("src/handler.rs", Some(HANDLER_DELTA))]);
        (repo, derived.facts)
    }

    fn reasons_after(edits: &[(&str, Option<&str>)]) -> Vec<Reason> {
        let (repo, facts) = handler_repo();
        let world = repo.world(edits);
        evaluate_facts(&repo.work(), &world, &facts).unwrap()
    }

    #[test]
    fn a_changed_callee_signature_breaks_the_referenced_fact() {
        let world = AUTH.replace("validate(token: &str)", "validate(token: &str, ctx: &Ctx)");
        let reasons = reasons_after(&[("src/auth.rs", Some(&world))]);
        assert_eq!(codes(&reasons), [ReasonCode::FactBroken]);
        assert_eq!(
            reasons[0].detail,
            "pub fn validate(token: &str) -> bool => pub fn validate(token: &str, ctx: &Ctx) -> bool"
        );
        assert_eq!(reasons[0].path.as_deref(), Some("src/auth.rs"));
        assert!(reasons[0].fact_id.is_some());
    }

    #[test]
    fn a_callee_body_or_comment_change_leaves_the_fact_holding() {
        let world = AUTH.replace("!token.is_empty()", "token.len() > 3 // stricter");
        assert!(reasons_after(&[("src/auth.rs", Some(&world))]).is_empty());
        // A sibling symbol changing signature is irrelevant to this delta.
        let world = AUTH.replace("refresh(token: &str)", "refresh(token: &str, ttl: u64)");
        assert!(reasons_after(&[("src/auth.rs", Some(&world))]).is_empty());
    }

    #[test]
    fn a_symbol_edited_by_both_sides_is_reported() {
        let world = HANDLER.replace("let ok = true;", "let ok = false;");
        let reasons = reasons_after(&[("src/handler.rs", Some(&world))]);
        assert_eq!(codes(&reasons), [ReasonCode::SameSymbolEdited]);
        assert_eq!(
            reasons[0].detail,
            "handle was also edited in src/handler.rs"
        );
    }

    #[test]
    fn a_missing_symbol_or_file_is_reported() {
        let world = "pub fn refresh(token: &str) -> String {\n    token.to_owned()\n}\n";
        let reasons = reasons_after(&[("src/auth.rs", Some(world))]);
        assert_eq!(codes(&reasons), [ReasonCode::FactMissing]);
        assert_eq!(
            reasons[0].detail,
            "validate no longer declared in src/auth.rs"
        );
        let reasons = reasons_after(&[("src/auth.rs", None)]);
        assert_eq!(codes(&reasons), [ReasonCode::FactMissing]);
        assert_eq!(
            reasons[0].detail,
            "validate was in src/auth.rs, which no longer exists"
        );
    }

    #[test]
    fn a_syntax_error_in_a_world_file_is_uncertain_not_ignored() {
        let world = "pub fn validate(token: &str) -> bool {\n    !token.is_empty(\n}\n";
        let reasons = reasons_after(&[("src/auth.rs", Some(world))]);
        assert_eq!(codes(&reasons), [ReasonCode::AnalysisUncertain]);
        assert_eq!(
            reasons[0].detail,
            "src/auth.rs does not parse; cannot check validate"
        );
    }

    #[test]
    fn modified_facts_come_before_referenced_ones() {
        let auth = AUTH.replace("validate(token: &str)", "validate(token: &str, ctx: &Ctx)");
        let handler = HANDLER.replace("let ok = true;", "let ok = false;");
        let reasons = reasons_after(&[
            ("src/auth.rs", Some(&auth)),
            ("src/handler.rs", Some(&handler)),
        ]);
        assert_eq!(
            codes(&reasons),
            [ReasonCode::SameSymbolEdited, ReasonCode::FactBroken]
        );
    }

    #[test]
    fn a_deleted_verdict_is_never_trusted_over_the_file_on_disk() {
        let (repo, facts) = handler_repo();
        // The world says Deleted, but the file is right there (as for files
        // inside a nested repository) and unchanged: the fact holds.
        let world = WorldObservation {
            digest: String::new(),
            changes: vec![Change {
                path: "src/auth.rs".into(),
                kind: ChangeKind::Deleted,
            }],
        };
        assert!(
            evaluate_facts(&repo.work(), &world, &facts)
                .unwrap()
                .is_empty()
        );
        // And a file that really is gone is missing even when the world says
        // Modified.
        fs::remove_file(repo.source.join("src/auth.rs")).unwrap();
        let world = WorldObservation {
            digest: String::new(),
            changes: vec![Change {
                path: "src/auth.rs".into(),
                kind: ChangeKind::Modified,
            }],
        };
        let reasons = evaluate_facts(&repo.work(), &world, &facts).unwrap();
        assert_eq!(codes(&reasons), [ReasonCode::FactMissing]);
    }

    #[test]
    fn a_file_fact_breaks_when_the_file_changes_or_vanishes() {
        let repo = Repo::new(&[("config.toml", "retries = 3\n")]);
        let derived = repo.derive(&[("config.toml", Some("retries = 5\n"))]);
        let world = repo.world(&[("config.toml", Some("retries = 4\n"))]);
        let reasons = evaluate_facts(&repo.work(), &world, &derived.facts).unwrap();
        assert_eq!(codes(&reasons), [ReasonCode::FactBroken]);
        assert_eq!(
            reasons[0].detail,
            "config.toml changed since the work started"
        );
        let world = repo.world(&[("config.toml", None)]);
        let reasons = evaluate_facts(&repo.work(), &world, &derived.facts).unwrap();
        assert_eq!(codes(&reasons), [ReasonCode::FactMissing]);
        // Rewritten with identical bytes is not a change.
        let world = repo.world(&[("config.toml", Some("retries = 3\n"))]);
        assert!(world.changes.is_empty());
        assert!(
            evaluate_facts(&repo.work(), &world, &derived.facts)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn evaluate_runs_l1_after_l0_and_reports_the_analysis_level() {
        let (repo, _) = handler_repo();
        repo.delta(&[("src/handler.rs", Some(HANDLER_DELTA))]);
        let auth = AUTH.replace("validate(token: &str)", "validate(token: &str, ctx: &Ctx)");
        let world = repo.world(&[("src/auth.rs", Some(&auth))]);
        let validity = evaluate(&world, &repo.work()).unwrap();
        assert_eq!(validity.decision, Decision::Refresh);
        assert_eq!(validity.analysis, AnalysisLevel::Symbols);
        assert_eq!(codes(&validity.reasons), [ReasonCode::FactBroken]);

        // An unrelated file changing: Continue, and nothing symbol-level ran.
        let (repo, _) = handler_repo();
        repo.delta(&[("src/handler.rs", Some(HANDLER_DELTA))]);
        let world = repo.world(&[("notes.txt", Some("hello\n"))]);
        let validity = evaluate(&world, &repo.work()).unwrap();
        assert_eq!(validity.decision, Decision::Continue);
        assert_eq!(validity.analysis, AnalysisLevel::FilesOnly);
        assert!(validity.reasons.is_empty());
    }

    #[test]
    fn an_unreadable_baseline_bails_out_of_l1_without_error() {
        let (repo, _) = handler_repo();
        repo.delta(&[("src/handler.rs", Some(HANDLER_DELTA))]);
        let auth = AUTH.replace("validate(token: &str)", "validate(token: &str, ctx: &Ctx)");
        let world = repo.world(&[("src/auth.rs", Some(&auth))]);
        let mut work = repo.work();
        let nowhere = repo.root.path().join("nowhere");
        work.baseline = &nowhere;
        let (reasons, analysis) = check(&work, &world).unwrap();
        assert!(reasons.is_empty());
        assert_eq!(analysis, AnalysisLevel::FilesOnly);
    }

    // ---- precise Modified facts ---------------------------------------------

    #[test]
    fn parse_patch_separates_replacements_from_pure_insertions() {
        let repo = Repo::new(&[("a.txt", &numbered(20, &[]))]);
        let mut edited = numbered(20, &[5]);
        edited = edited.replace("line 12\n", "line 12\nextra\n");
        let files = parse_patch(&repo.delta(&[("a.txt", Some(&edited))]));
        assert_eq!(files[0].old_ranges, vec![(5, 5)]);
        assert_eq!(files[0].insertions, vec![12]);
        assert_eq!(files[0].new_ranges, vec![(5, 5), (13, 13)]);
    }

    const TWO_FNS: &str =
        "pub fn a() -> u8 {\n    let x = 1;\n    x\n}\npub fn b() -> u8 {\n    2\n}\n";

    fn modified_after(s0: &str, delta: &str) -> Vec<String> {
        let repo = Repo::new(&[("src/x.rs", s0)]);
        subjects(
            &repo.derive(&[("src/x.rs", Some(delta))]).facts,
            FactOrigin::Modified,
        )
    }

    #[test]
    fn only_the_declaration_a_changed_line_is_in_is_modified() {
        // The last line of `a` changes and `b` starts right below: context
        // does not make `b` modified.
        let delta = TWO_FNS.replace("    x\n}", "    x + 1\n}");
        assert_eq!(modified_after(TWO_FNS, &delta), ["src/x.rs:a"]);
        // The closing brace line is part of the declaration too.
        let delta = TWO_FNS.replace("    x\n}\npub", "    x\n} // done\npub");
        assert_eq!(modified_after(TWO_FNS, &delta), ["src/x.rs:a"]);
    }

    #[test]
    fn an_insertion_modifies_a_declaration_only_strictly_inside_it() {
        // Inside `a`, after its first line.
        let delta = TWO_FNS.replace("    let x = 1;\n", "    let x = 1;\n    let y = 2;\n");
        assert_eq!(modified_after(TWO_FNS, &delta), ["src/x.rs:a"]);
        // Between `a` and `b`: neither is modified.
        let delta = TWO_FNS.replace("}\npub fn b", "}\npub fn c() {}\npub fn b");
        assert!(modified_after(TWO_FNS, &delta).is_empty());
        // Before the first line of the file.
        let delta = format!("pub fn z() {{}}\n{TWO_FNS}");
        assert!(modified_after(TWO_FNS, &delta).is_empty());
    }

    const STORE_CLASS: &str = "class Store:\n    LIMIT = 1\n\n    def save(self, key):\n        self.a = key\n\n    def drop(self, key):\n        return None\n";

    #[test]
    fn a_member_edit_does_not_make_its_container_modified() {
        let repo = Repo::new(&[("store.py", STORE_CLASS)]);
        let delta = STORE_CLASS.replace("self.a = key", "self.a = key + 1");
        let derived = repo.derive(&[("store.py", Some(&delta))]);
        assert_eq!(
            subjects(&derived.facts, FactOrigin::Modified),
            ["store.py:Store.save"]
        );
        // Another member changing underneath is irrelevant.
        let world = STORE_CLASS.replace("return None", "return 5");
        let world = repo.world(&[("store.py", Some(&world))]);
        assert!(
            evaluate_facts(&repo.work(), &world, &derived.facts)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_container_is_modified_by_class_level_changes_and_checked_by_its_header() {
        let repo = Repo::new(&[("store.py", STORE_CLASS)]);
        let delta = STORE_CLASS.replace("LIMIT = 1", "LIMIT = 2");
        let derived = repo.derive(&[("store.py", Some(&delta))]);
        assert_eq!(
            subjects(&derived.facts, FactOrigin::Modified),
            ["store.py:Store"]
        );
        assert!(derived.facts[0].full_fp.is_none());
        // A member edit leaves the class fact holding.
        let member = STORE_CLASS.replace("return None", "return 5");
        let world = repo.world(&[("store.py", Some(&member))]);
        assert!(
            evaluate_facts(&repo.work(), &world, &derived.facts)
                .unwrap()
                .is_empty()
        );
        // A class-level edit does not.
        let level = STORE_CLASS.replace("LIMIT = 1", "LIMIT = 3");
        let world = repo.world(&[("store.py", Some(&level))]);
        let reasons = evaluate_facts(&repo.work(), &world, &derived.facts).unwrap();
        assert_eq!(codes(&reasons), [ReasonCode::SameSymbolEdited]);
        assert_eq!(reasons[0].detail, "Store was also edited in store.py");
    }

    #[test]
    fn a_rust_trait_default_method_edit_modifies_the_method_not_the_trait() {
        let shape = "pub trait Shape {\n    fn area(&self) -> f64;\n\n    fn describe(&self) -> String {\n        String::new()\n    }\n}\n";
        let delta = shape.replace("String::new()", "String::from(\"shape\")");
        assert_eq!(modified_after(shape, &delta), ["src/x.rs:Shape::describe"]);
        let delta = shape.replace("pub trait Shape {", "pub trait Shape: Sized {");
        assert_eq!(modified_after(shape, &delta), ["src/x.rs:Shape"]);
    }

    #[test]
    fn names_the_work_declares_itself_are_not_bound() {
        let repo = Repo::new(&[("src/auth.rs", AUTH), ("src/handler.rs", HANDLER)]);
        let delta = format!(
            "{HANDLER_DELTA}\nfn validate(request: &str) -> bool {{\n    request.len() > 1\n}}\n"
        );
        let derived = repo.derive(&[("src/handler.rs", Some(&delta))]);
        assert!(subjects(&derived.facts, FactOrigin::Referenced).is_empty());
        assert_eq!(derived.unbound, 0);
        // A brand-new file's declarations count as the work's own as well.
        let derived = repo.derive(&[(
            "src/extra.rs",
            Some(
                "pub fn validate() -> bool {\n    true\n}\n\npub fn go() -> bool {\n    validate()\n}\n",
            ),
        )]);
        assert!(subjects(&derived.facts, FactOrigin::Referenced).is_empty());
    }

    // ---- name audit --------------------------------------------------------

    /// The audit and the derived facts for one patch, after checking that the
    /// audit's unbound causes are exactly `derive_facts`'s `unbound`.
    fn audit(repo: &Repo, patch: &[u8]) -> (BTreeMap<String, NameCause>, Derived) {
        let delta = parse_patch(patch);
        let causes = audit_names(&repo.work(), &delta).unwrap();
        let derived = derive_facts(&repo.work(), &delta).unwrap();
        let unbound = causes
            .values()
            .filter(|cause| matches!(cause, NameCause::Ambiguous | NameCause::Widespread))
            .count();
        assert_eq!(unbound, derived.unbound as usize, "{causes:?}");
        (causes, derived)
    }

    #[test]
    fn the_audit_gives_each_used_name_its_binding_cause() {
        let mut files: Vec<(String, String)> = (0..10)
            .map(|n| {
                (
                    format!("src/m{n}.rs"),
                    format!("pub fn user_{n}() {{\n    common_helper();\n}}\n"),
                )
            })
            .collect();
        files.extend(
            [
                ("src/lib.rs", "pub fn common_helper() {}\n"),
                ("src/auth.rs", AUTH),
                ("src/other.rs", "pub fn check(x: u8) -> u8 {\n    x\n}\n"),
                ("src/more.rs", "pub fn check() {}\n"),
                ("src/handler.rs", HANDLER),
            ]
            .map(|(path, text)| (path.to_owned(), text.to_owned())),
        );
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        let repo = Repo::new(&refs);
        let delta = "pub fn handle(request: &str) -> bool {\n    \
            let ok = validate(request) && check(1) == 1 && get(ab(1));\n    \
            common_helper();\n    let table = HashMap::new();\n    fresh(ok)\n}\n\n\
            fn fresh(ok: bool) -> bool {\n    ok\n}\n";
        let patch = repo.delta(&[("src/handler.rs", Some(delta))]);
        let (causes, derived) = audit(&repo, &patch);
        for (name, cause) in [
            ("validate", NameCause::Bound),
            ("check", NameCause::Ambiguous),
            ("common_helper", NameCause::Widespread),
            ("get", NameCause::Denied),
            ("ab", NameCause::Denied),
            ("new", NameCause::Denied),
            ("fresh", NameCause::Introduced),
            ("HashMap", NameCause::NotFound),
            ("table", NameCause::NotFound),
        ] {
            assert_eq!(causes.get(name), Some(&cause), "{name}: {causes:?}");
        }
        assert_eq!(derived.unbound, 2);
        // Binding itself is unchanged by the audit.
        assert_eq!(
            subjects(&derived.facts, FactOrigin::Referenced),
            ["src/auth.rs:validate"]
        );
    }

    #[test]
    fn the_audit_counts_names_in_a_file_it_could_not_analyze_as_unsupported() {
        let repo = Repo::new(&[("src/a.rs", "fn a() {}\n"), ("src/b.rs", AUTH)]);
        // The context does not match the baseline, so there is no post-image.
        let patch = b"diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n\
--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1,2 @@\n fn zzz() {}\n\
+fn b() { lookup_table(); validate(\"x\"); }\n";
        let (causes, derived) = audit(&repo, patch);
        assert_eq!(derived.uncertain, ["src/a.rs"]);
        assert_eq!(causes.get("lookup_table"), Some(&NameCause::Unsupported));
        // Not bound by `derive_facts`, so unsupported too, however unique.
        assert_eq!(causes.get("validate"), Some(&NameCause::Unsupported));
        assert_eq!(causes.get("b"), Some(&NameCause::Denied));
        assert!(subjects(&derived.facts, FactOrigin::Referenced).is_empty());
    }

    #[test]
    fn the_audit_matches_unbound_on_existing_binding_fixtures() {
        let repo = Repo::new(&[
            ("src/auth.rs", AUTH),
            ("src/other.rs", "pub fn validate(x: u8) -> u8 {\n    x\n}\n"),
            ("src/handler.rs", HANDLER),
        ]);
        let patch = repo.delta(&[("src/handler.rs", Some(HANDLER_DELTA))]);
        let (causes, derived) = audit(&repo, &patch);
        assert_eq!(causes.get("validate"), Some(&NameCause::Ambiguous));
        assert_eq!(derived.unbound, 1);
        // A deleted file uses no names; an added one's own declarations are introduced.
        let patch = repo.delta(&[
            ("src/other.rs", None),
            (
                "src/extra.rs",
                Some("pub fn run_extra() -> bool {\n    refresh(\"\") == \"\"\n}\n"),
            ),
        ]);
        let (causes, _) = audit(&repo, &patch);
        assert_eq!(causes.get("run_extra"), Some(&NameCause::Introduced));
        assert_eq!(causes.get("refresh"), Some(&NameCause::Bound));
        assert!(!causes.contains_key("x"), "{causes:?}");
    }

    /// Unresolved-name audit over real runs (measurement only, ignored by default).
    ///
    /// `DISPATCH_AUDIT_RUNS` is a colon-separated list of run directories, each
    /// holding `baseline/` (a Git repository) and `delta.patch`. The baseline commit
    /// is `metadata.json`'s `baseline_commit` when present, else the baseline's HEAD.
    ///
    /// ```text
    /// DISPATCH_AUDIT_RUNS=runA:runB cargo test --lib coherence::facts::tests::real_runs -- --ignored --nocapture
    /// ```
    mod real_runs {
        use std::{
            collections::{BTreeMap, BTreeSet},
            fs,
            path::{Path, PathBuf},
            process::Command,
        };

        use super::super::{NameCause, audit_names, derive_facts, parse_patch};
        use crate::{FactOrigin, coherence::WorkView};

        const EXAMPLES: usize = 20;

        struct Run {
            label: String,
            causes: BTreeMap<String, NameCause>,
            unbound: u32,
            /// Base names of the S0 declarations the patch changes, with their
            /// qualified names.
            changed: BTreeMap<String, BTreeSet<String>>,
        }

        fn baseline_commit(dir: &Path) -> String {
            let recorded = fs::read(dir.join("metadata.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|meta| meta["baseline_commit"].as_str().map(str::to_owned));
            recorded.unwrap_or_else(|| {
                let output = Command::new("git")
                    .arg("-C")
                    .arg(dir.join("baseline"))
                    .args(["rev-parse", "HEAD"])
                    .output()
                    .unwrap();
                assert!(output.status.success(), "no baseline commit in {dir:?}");
                String::from_utf8(output.stdout).unwrap().trim().to_owned()
            })
        }

        /// `facts::base_name`: `Point::new` -> `new`, `<S as T>::f` -> `f`.
        fn base_name(name: &str) -> &str {
            let name = match (name.starts_with('<'), name.rfind(">::")) {
                (true, Some(at)) => &name[at + 3..],
                _ => name,
            };
            name.rsplit([':', '.']).next().unwrap_or(name)
        }

        fn audit(dir: &Path) -> Run {
            let baseline = dir.join("baseline");
            let patch_path = dir.join("delta.patch");
            let commit = baseline_commit(dir);
            let work = WorkView {
                source: &baseline,
                delta_patch: &patch_path,
                baseline: &baseline,
                baseline_commit: &commit,
            };
            let delta = parse_patch(&fs::read(&patch_path).unwrap());
            let causes = audit_names(&work, &delta).unwrap();
            let derived = derive_facts(&work, &delta).unwrap();
            let unbound = causes
                .values()
                .filter(|cause| matches!(cause, NameCause::Ambiguous | NameCause::Widespread))
                .count();
            assert_eq!(unbound, derived.unbound as usize, "{dir:?}");
            let mut changed: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for fact in derived
                .facts
                .iter()
                .filter(|fact| fact.origin == FactOrigin::Modified)
            {
                changed
                    .entry(base_name(&fact.subject).to_owned())
                    .or_default()
                    .insert(format!("{}:{}", fact.path, fact.subject));
            }
            Run {
                label: dir.file_name().unwrap().to_string_lossy().into_owned(),
                causes,
                unbound: derived.unbound,
                changed,
            }
        }

        #[test]
        #[ignore = "reads real runs named by DISPATCH_AUDIT_RUNS"]
        fn audit_unresolved_names_in_real_runs() {
            let list = std::env::var("DISPATCH_AUDIT_RUNS").expect("set DISPATCH_AUDIT_RUNS");
            let runs: Vec<Run> = list
                .split(':')
                .filter(|dir| !dir.is_empty())
                .map(|dir| audit(&PathBuf::from(dir)))
                .collect();
            for run in &runs {
                let mut by_cause: BTreeMap<NameCause, Vec<&str>> = BTreeMap::new();
                for (name, cause) in &run.causes {
                    by_cause.entry(*cause).or_default().push(name);
                }
                let counts: Vec<String> = by_cause
                    .iter()
                    .map(|(cause, names)| format!("{cause:?}={}", names.len()))
                    .collect();
                println!("== {}", run.label);
                println!(
                    "names={} unbound={} {}",
                    run.causes.len(),
                    run.unbound,
                    counts.join(" ")
                );
                for (cause, names) in &by_cause {
                    let shown: Vec<&str> = names.iter().take(EXAMPLES).copied().collect();
                    println!("  {cause:?}: {}", shown.join(" "));
                }
                let mut checked = 0;
                for (name, cause) in &run.causes {
                    if !matches!(
                        cause,
                        NameCause::Ambiguous | NameCause::Widespread | NameCause::NotFound
                    ) {
                        continue;
                    }
                    checked += 1;
                    for other in runs.iter().filter(|other| other.label != run.label) {
                        if let Some(decls) = other.changed.get(name) {
                            let decls: Vec<&str> = decls.iter().map(String::as_str).collect();
                            println!(
                                "  possibly hid an interaction: {name} ({cause:?}) changed by {}: {}",
                                other.label,
                                decls.join(" ")
                            );
                        }
                    }
                }
                println!("  unresolved names checked against the other runs: {checked}");
            }
        }
    }
}
