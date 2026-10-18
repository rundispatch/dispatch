//! Observation of what changed in a source tree since a run's baseline
//! snapshot. The current world is the tree minus whatever the source's own
//! ignore rules exclude, so build outputs are not drift.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::{self, Metadata},
    io::ErrorKind,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::{
    SourceKind,
    source::{
        bytes_to_path, checked_output, git_command, hash_sized_bytes, include_entry,
        is_excluded_name, parents_are_directories, path_bytes, run_git,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    ModeOnly,
    /// A symlink was added, retargeted, or swapped with a regular file.
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldObservation {
    /// SHA-256 over the current non-ignored tree (not over the diff).
    pub digest: String,
    /// Sorted by path.
    pub changes: Vec<Change>,
}

/// A cheap "did anything move" value; equal signals mean nothing moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signal(pub String);

/// Present regular files and symlinks by raw path.
type Present = BTreeMap<Vec<u8>, Stat>;

/// What `lstat` says of a present file: its kind, and the fields a hash kept
/// from an earlier look is trusted by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stat {
    symlink: bool,
    executable: bool,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    inode: u64,
    mode: u32,
}

/// One tree entry, keyed elsewhere by its raw path bytes.
#[derive(Clone)]
struct Entry {
    symlink: bool,
    executable: bool,
    /// Git blob hash of the file bytes, or of the symlink target text.
    hash: String,
}

/// A tree by raw path.
type Tree = BTreeMap<Vec<u8>, Entry>;

/// A file modified this close to the look that hashed it may have changed
/// again within its timestamp's granularity, so its hash is not kept.
const RACY: Duration = Duration::from_secs(2);

/// A file's hash as last seen, and when it was looked at.
struct Seen {
    stat: Stat,
    hash: String,
    looked: SystemTime,
}

/// The source as it is now, whatever baseline it is compared with.
struct Current {
    entries: Tree,
    /// Paths Git lists that are gone from disk.
    missing: BTreeSet<Vec<u8>>,
    nested: Vec<Vec<u8>>,
    digest: String,
}

/// Observations of one source that share their work, in memory only: each
/// file's hash is kept by what `lstat` says of it (size, modification and
/// change times, inode and mode), each baseline commit's tree by its id, and
/// the source as it is now until `moved` says it may have changed. The
/// project owner keeps one; `observe` starts from nothing.
#[derive(Default)]
pub struct Worlds {
    hashes: HashMap<Vec<u8>, Seen>,
    trees: HashMap<String, Arc<Tree>>,
    /// The source now, by its kind and the length of the object ids it was
    /// hashed for.
    now: Option<(SourceKind, usize, Arc<Current>)>,
}

impl Worlds {
    /// `observe`, sharing what this value already knows.
    pub fn observe(
        &mut self,
        source: &Path,
        baseline: &Path,
        baseline_commit: &str,
        kind: &SourceKind,
    ) -> Result<WorldObservation> {
        let tree = match self.trees.get(baseline_commit) {
            Some(tree) => Arc::clone(tree),
            None => {
                // A commit id names its tree, in whichever baseline it is.
                let tree = Arc::new(read_baseline_tree(baseline, baseline_commit)?);
                self.trees
                    .insert(baseline_commit.to_owned(), Arc::clone(&tree));
                tree
            }
        };
        let ids = baseline_commit.len();
        let current = match &self.now {
            Some((was, hashed, current)) if was == kind && *hashed == ids => Arc::clone(current),
            _ => {
                let current = Arc::new(self.current(source, baseline, kind, ids)?);
                self.now = Some((kind.clone(), ids, Arc::clone(&current)));
                current
            }
        };
        compare(source, kind, &tree, &current)
    }

    /// The source may have changed: the next observation looks at it again.
    pub fn moved(&mut self) {
        self.now = None;
    }

    /// List the source and hash what changed since it was last looked at.
    /// `ids` is the length of the baseline's object ids, which a kept hash
    /// must have.
    fn current(
        &mut self,
        source: &Path,
        baseline: &Path,
        kind: &SourceKind,
        ids: usize,
    ) -> Result<Current> {
        let looked = SystemTime::now();
        let (present, missing, nested) = match kind {
            SourceKind::Directory => (walk_directory(source)?, BTreeSet::new(), Vec::new()),
            SourceKind::Git | SourceKind::GitWorktree => list_git_world(source)?,
        };
        let entries = self.hash_world(source, baseline, &present, looked, ids)?;
        let mut hasher = Sha256::new();
        hasher.update(b"dispatch-world-v1\0");
        for (path, entry) in &entries {
            hash_sized_bytes(&mut hasher, path);
            hasher.update(match (entry.symlink, entry.executable) {
                (true, _) => b"l",
                (false, true) => b"x",
                (false, false) => b"f",
            });
            hash_sized_bytes(&mut hasher, entry.hash.as_bytes());
        }
        Ok(Current {
            entries,
            missing,
            nested,
            digest: hex::encode(hasher.finalize()),
        })
    }

    /// The kept hash of `path`, if `stat` still matches the look that made it,
    /// that look came well after the file was last modified or changed, and
    /// the hash has `ids` characters.
    fn kept(&self, path: &[u8], stat: &Stat, ids: usize) -> Option<&str> {
        let seen = self
            .hashes
            .get(path)
            .filter(|seen| seen.hash.len() == ids)?;
        let time = |(seconds, nanos): (i64, i64)| {
            SystemTime::UNIX_EPOCH.checked_add(Duration::new(
                u64::try_from(seconds).ok()?,
                u32::try_from(nanos).ok()?,
            ))
        };
        let last = time(stat.modified)?.max(time(stat.changed)?);
        (seen.stat == *stat
            && seen
                .looked
                .duration_since(last)
                .is_ok_and(|age| age >= RACY))
        .then_some(seen.hash.as_str())
    }
}

/// Compare the current source tree with the baseline commit.
///
/// Git sources use the source's own ignore rules; directory sources exclude
/// only `.git` and `.dispatch`. Symlinks are never followed; a file found
/// vanishing while it is being read is an error, and the caller may retry.
///
/// A nested repository (a committed gitlink or an untracked directory with its
/// own `.git`) is not comparable: the baseline holds its files, but Git will not
/// list them for the outer repository, and changes inside it are that
/// repository's business. Its files are skipped on both sides, never Deleted.
pub fn observe(
    source: &Path,
    baseline: &Path,
    baseline_commit: &str,
    kind: &SourceKind,
) -> Result<WorldObservation> {
    Worlds::default().observe(source, baseline, baseline_commit, kind)
}

/// The source now against one baseline tree.
fn compare(
    source: &Path,
    kind: &SourceKind,
    baseline_tree: &Tree,
    now: &Current,
) -> Result<WorldObservation> {
    let current = &now.entries;
    let mut changes = Vec::new();
    for (path, now) in current {
        let kind = match baseline_tree.get(path) {
            None if now.symlink => Some(ChangeKind::Symlink),
            None => Some(ChangeKind::Added),
            Some(was) if was.symlink != now.symlink => Some(ChangeKind::Symlink),
            Some(was) if was.hash != now.hash && now.symlink => Some(ChangeKind::Symlink),
            Some(was) if was.hash != now.hash => Some(ChangeKind::Modified),
            Some(was) if cfg!(unix) && was.executable != now.executable => {
                Some(ChangeKind::ModeOnly)
            }
            Some(_) => None,
        };
        if let Some(kind) = kind {
            changes.push(Change {
                path: String::from_utf8_lossy(path).into_owned(),
                kind,
            });
        }
    }

    // A baseline path missing now is Deleted, unless the source's ignore rules
    // exclude it. A path Git listed but that is gone from disk is never ignored.
    let absent: Vec<&Vec<u8>> = baseline_tree
        .keys()
        .filter(|path| !current.contains_key(*path) && !inside_nested(&now.nested, path))
        .collect();
    let to_check: Vec<&Vec<u8>> = match kind {
        SourceKind::Directory => Vec::new(),
        _ => absent
            .iter()
            .copied()
            .filter(|path| !now.missing.contains(*path))
            .collect(),
    };
    let ignored = ignored_paths(source, &to_check)?;
    for path in absent {
        if !ignored.contains(path) {
            changes.push(Change {
                path: String::from_utf8_lossy(path).into_owned(),
                kind: ChangeKind::Deleted,
            });
        }
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(WorldObservation {
        digest: now.digest.clone(),
        changes,
    })
}

/// Most files whose metadata `signal` mixes in for a Git source.
const MAX_SIGNAL_FILES: usize = 5000;

/// Cheap change doorbell that never reads file contents. For Git sources it
/// covers HEAD, `git status`, and the size and modification time of the files
/// `git status` reports (metadata only, at most `MAX_SIGNAL_FILES`), so that
/// editing an already-modified file again moves the signal.
pub fn signal(source: &Path, kind: &SourceKind) -> Result<Signal> {
    let mut hasher = Sha256::new();
    match kind {
        SourceKind::Directory => {
            for entry in WalkDir::new(source)
                .follow_links(false)
                .sort_by_file_name()
                .into_iter()
                .filter_entry(include_entry)
            {
                let entry =
                    entry.with_context(|| format!("failed to walk {}", source.display()))?;
                if entry.depth() == 0 {
                    continue;
                }
                let metadata = entry
                    .metadata()
                    .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
                let relative = entry
                    .path()
                    .strip_prefix(source)
                    .expect("walked path is under the source");
                hash_sized_bytes(&mut hasher, &path_bytes(relative));
                hasher.update(metadata.len().to_le_bytes());
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |elapsed| elapsed.as_nanos());
                hasher.update(modified.to_le_bytes());
            }
            Ok(Signal(hex::encode(hasher.finalize())))
        }
        SourceKind::Git | SourceKind::GitWorktree => {
            let mut head = git_command(source);
            head.args(["rev-parse", "--verify", "HEAD"]);
            let head = run_git(head, None, "failed to query the source repository's HEAD")?;
            let head = if head.status.success() {
                String::from_utf8_lossy(&head.stdout).trim().to_owned()
            } else {
                "unborn".to_owned()
            };

            let mut status = git_command(source);
            status.args(["status", "--porcelain=v2", "-z", "--untracked-files=all"]);
            let status = checked_output(status, "failed to read the source repository status")?;
            hasher.update(&status.stdout);
            for path in status_paths(&status.stdout)
                .into_iter()
                .take(MAX_SIGNAL_FILES)
            {
                let metadata = fs::symlink_metadata(source.join(bytes_to_path(path)));
                let (size, modified) = metadata.as_ref().map_or((0, 0), |metadata| {
                    let modified = metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(0, |elapsed| elapsed.as_nanos());
                    (metadata.len(), modified)
                });
                hasher.update(size.to_le_bytes());
                hasher.update(modified.to_le_bytes());
            }
            Ok(Signal(format!("{head}:{}", hex::encode(hasher.finalize()))))
        }
    }
}

/// The paths `git status --porcelain=v2 -z` reports as changed, untracked or
/// unmerged. A rename record is followed by its original path as a separate
/// NUL-terminated field, which is skipped. Ignored entries are not listed.
fn status_paths(status: &[u8]) -> Vec<&[u8]> {
    let mut paths = Vec::new();
    let mut records = status.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        let fields = match record.first() {
            Some(b'1') => 9,
            Some(b'2') => 10,
            Some(b'u') => 11,
            Some(b'?') => 2,
            _ => continue,
        };
        if record.first() == Some(&b'2') {
            records.next();
        }
        // The path is the last field and may itself contain spaces.
        let mut rest = record;
        for _ in 1..fields {
            match rest.iter().position(|byte| *byte == b' ') {
                Some(space) => rest = &rest[space + 1..],
                None => {
                    rest = &[];
                    break;
                }
            }
        }
        if !rest.is_empty() {
            paths.push(rest);
        }
    }
    paths
}

/// Every baseline entry keyed by raw path bytes. `git ls-tree -z` prints
/// `<mode> <type> <hash>\t<path>`.
fn read_baseline_tree(baseline: &Path, commit: &str) -> Result<BTreeMap<Vec<u8>, Entry>> {
    ensure!(!commit.starts_with('-'), "invalid baseline commit {commit}");
    let mut command = git_command(baseline);
    command.args(["ls-tree", "-r", "-z", commit]);
    let output = checked_output(command, "failed to read the baseline tree")?;
    let mut tree = BTreeMap::new();
    for record in output.stdout.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .context("malformed baseline tree entry")?;
        let meta = String::from_utf8_lossy(&record[..tab]).into_owned();
        let mut fields = meta.split(' ');
        let (Some(mode), Some(_), Some(hash)) = (fields.next(), fields.next(), fields.next())
        else {
            bail!("malformed baseline tree entry: {meta}");
        };
        tree.insert(
            record[tab + 1..].to_vec(),
            Entry {
                symlink: mode == "120000",
                executable: mode == "100755",
                hash: hash.to_owned(),
            },
        );
    }
    Ok(tree)
}

/// `(present, listed_but_missing, nested_repository_directories)`.
type GitWorld = (Present, BTreeSet<Vec<u8>>, Vec<Vec<u8>>);

/// Present regular files and symlinks as `(is_symlink, is_executable)`, the
/// paths Git lists that no longer exist as files, and the nested repositories
/// (directory paths without a trailing slash). `ls-files -co` lists tracked and
/// untracked files that the source's ignore rules do not exclude; a gitlink
/// shows as a bare directory and an untracked nested repository as `dir/`.
fn list_git_world(source: &Path) -> Result<GitWorld> {
    let mut command = git_command(source);
    command.args(["ls-files", "-co", "--exclude-standard", "-z"]);
    let output = checked_output(command, "failed to list the current source files")?;

    let mut nested = Vec::new();
    let mut staged = git_command(source);
    staged.args(["ls-files", "-s", "-z"]);
    let staged = checked_output(staged, "failed to list the current source index")?;
    for record in staged.stdout.split(|byte| *byte == 0) {
        // `<mode> <hash> <stage>\t<path>`; mode 160000 is a gitlink.
        if let Some(path) = record.strip_prefix(b"160000 ").and_then(|rest| {
            rest.iter()
                .position(|byte| *byte == b'\t')
                .map(|i| &rest[i + 1..])
        }) {
            nested.push(path.to_vec());
        }
    }

    let mut present = BTreeMap::new();
    let mut missing = BTreeSet::new();
    let mut real_directories = HashSet::new();
    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        if let Some(directory) = raw.strip_suffix(b"/") {
            nested.push(directory.to_vec());
            continue;
        }
        let relative = bytes_to_path(raw);
        if relative
            .components()
            .any(|part| is_excluded_name(part.as_os_str()))
        {
            continue;
        }
        // Reading through a parent that became a symlink would leave the tree.
        let metadata = if parents_are_directories(source, &relative, &mut real_directories) {
            match fs::symlink_metadata(source.join(&relative)) {
                Ok(metadata) => Some(metadata),
                Err(error) if error.kind() == ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to inspect {}", source.join(&relative).display())
                    });
                }
            }
        } else {
            None
        };
        match metadata {
            Some(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                present.insert(raw.to_vec(), entry_kind(&metadata));
            }
            Some(_) => {}
            None => {
                missing.insert(raw.to_vec());
            }
        }
    }
    // Files under a nested repository are not part of this world.
    present.retain(|path, _| !inside_nested(&nested, path));
    Ok((present, missing, nested))
}

/// True when `path` lies strictly below one of the nested repository directories.
fn inside_nested(nested: &[Vec<u8>], path: &[u8]) -> bool {
    nested.iter().any(|directory| {
        path.strip_prefix(directory.as_slice())
            .is_some_and(|rest| rest.first() == Some(&b'/'))
    })
}

fn walk_directory(source: &Path) -> Result<Present> {
    let mut present = BTreeMap::new();
    for entry in WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(include_entry)
    {
        let entry = entry.with_context(|| format!("failed to walk {}", source.display()))?;
        let file_type = entry.file_type();
        if !file_type.is_file() && !file_type.is_symlink() {
            continue;
        }
        let metadata = entry
            .metadata()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .expect("walked path is under the source");
        present.insert(path_bytes(relative), entry_kind(&metadata));
    }
    Ok(present)
}

#[cfg(unix)]
fn entry_kind(metadata: &Metadata) -> Stat {
    use std::os::unix::fs::MetadataExt;
    let symlink = metadata.file_type().is_symlink();
    Stat {
        symlink,
        // A symlink's own permission bits are not versioned by Git.
        executable: !symlink && metadata.mode() & 0o100 != 0,
        size: metadata.size(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
        inode: metadata.ino(),
        mode: metadata.mode(),
    }
}

#[cfg(not(unix))]
fn entry_kind(metadata: &Metadata) -> Stat {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or((0, 0), |elapsed| {
            (elapsed.as_secs() as i64, i64::from(elapsed.subsec_nanos()))
        });
    Stat {
        symlink: metadata.file_type().is_symlink(),
        executable: false,
        size: metadata.len(),
        modified,
        changed: modified,
        inode: 0,
        mode: 0,
    }
}

impl Worlds {
    /// Blob-hash the present files with the trusted baseline repository,
    /// reusing every kept hash `kept` trusts; what was not present is
    /// forgotten. `--no-filters` matches how the baseline was frozen: literal
    /// bytes.
    fn hash_world(
        &mut self,
        source: &Path,
        baseline: &Path,
        present: &Present,
        looked: SystemTime,
        ids: usize,
    ) -> Result<Tree> {
        let mut regular = Vec::new();
        let mut input = Vec::new();
        for (path, stat) in present {
            if !stat.symlink && self.kept(path, stat, ids).is_none() {
                input.extend(c_quote(&path_bytes(&source.join(bytes_to_path(path)))));
                input.push(b'\n');
                regular.push(path);
            }
        }
        let mut hashes = if regular.is_empty() {
            Vec::new()
        } else {
            blob_hashes(baseline, "--stdin-paths", &input)?
        };
        ensure!(
            hashes.len() == regular.len(),
            "Git hashed {} files but {} were requested",
            hashes.len(),
            regular.len()
        );
        hashes.reverse();

        let mut world = BTreeMap::new();
        for (path, stat) in present {
            let hash = match self.kept(path, stat, ids) {
                Some(hash) => hash.to_owned(),
                None if stat.symlink => {
                    let full = source.join(bytes_to_path(path));
                    let target = fs::read_link(&full)
                        .with_context(|| format!("failed to read symlink {}", full.display()))?;
                    blob_hashes(baseline, "--stdin", &path_bytes(&target))?.remove(0)
                }
                None => hashes.pop().expect("one hash per regular file"),
            };
            world.insert(
                path.clone(),
                Entry {
                    symlink: stat.symlink,
                    executable: stat.executable,
                    hash,
                },
            );
        }
        self.hashes.retain(|path, _| present.contains_key(path));
        for (path, entry) in &world {
            let stat = present[path];
            if self.kept(path, &stat, ids).is_none() {
                self.hashes.insert(
                    path.clone(),
                    Seen {
                        stat,
                        hash: entry.hash.clone(),
                        looked,
                    },
                );
            }
        }
        Ok(world)
    }
}

fn blob_hashes(baseline: &Path, mode: &str, input: &[u8]) -> Result<Vec<String>> {
    let mut command = git_command(baseline);
    command.args(["hash-object", "--no-filters", mode]);
    let output = run_git(command, Some(input), "failed to hash source files")?;
    ensure!(
        output.status.success(),
        "failed to hash source files: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// `hash-object --stdin-paths` unquotes C-style quoted lines, so quote every
/// path to make leading quotes and newlines in file names harmless.
fn c_quote(path: &[u8]) -> Vec<u8> {
    let mut quoted = vec![b'"'];
    for byte in path {
        match byte {
            b'"' | b'\\' => quoted.extend([b'\\', *byte]),
            0..=0x1f | 0x7f => quoted.extend(format!("\\{byte:03o}").bytes()),
            _ => quoted.push(*byte),
        }
    }
    quoted.push(b'"');
    quoted
}

/// Which of `paths` the source's ignore rules exclude, even when tracked.
fn ignored_paths(source: &Path, paths: &[&Vec<u8>]) -> Result<HashSet<Vec<u8>>> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
    let mut input = Vec::new();
    for path in paths {
        input.extend(path.iter());
        input.push(0);
    }
    let mut command = git_command(source);
    command.args(["check-ignore", "--no-index", "-z", "--stdin"]);
    let output = run_git(command, Some(&input), "failed to check ignored files")?;
    // Exit status 1 means that none of the paths is ignored.
    ensure!(
        matches!(output.status.code(), Some(0 | 1)),
        "failed to check ignored files: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, process::Command};

    use tempfile::TempDir;

    use super::*;
    use crate::source::{SourceSnapshot, create_snapshot};

    struct Fixture {
        _root: TempDir,
        source: PathBuf,
        snapshot: SourceSnapshot,
    }

    impl Fixture {
        fn new(git: bool, files: &[(&str, &str)]) -> Self {
            let root = TempDir::new().unwrap();
            let source = root.path().join("source");
            fs::create_dir_all(&source).unwrap();
            let source = fs::canonicalize(source).unwrap();
            for (path, contents) in files {
                write(&source.join(path), contents);
            }
            if git {
                git_in(&source, &["init", "--quiet"]);
                git_in(&source, &["add", "-A"]);
                git_in(&source, &["commit", "--quiet", "-m", "initial"]);
            }
            let snapshot = create_snapshot(&source, &root.path().join("run")).unwrap();
            Self {
                _root: root,
                source,
                snapshot,
            }
        }

        fn observe(&self) -> WorldObservation {
            observe(
                &self.source,
                &self.snapshot.baseline_path,
                &self.snapshot.baseline_commit,
                &self.snapshot.kind,
            )
            .unwrap()
        }

        fn signal(&self) -> Signal {
            signal(&self.source, &self.snapshot.kind).unwrap()
        }

        fn write(&self, path: &str, contents: &str) {
            write(&self.source.join(path), contents);
        }
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn git_in(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
            ])
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn change(path: &str, kind: ChangeKind) -> Change {
        Change {
            path: path.to_owned(),
            kind,
        }
    }

    #[test]
    fn git_unchanged_tree_has_no_changes_and_stable_digest() {
        let fixture = Fixture::new(true, &[("a.txt", "one\n"), ("src/b.txt", "two\n")]);
        let first = fixture.observe();
        assert!(first.changes.is_empty());
        assert_eq!(first, fixture.observe());
    }

    #[test]
    fn git_classifies_edit_add_delete() {
        let fixture = Fixture::new(
            true,
            &[("a.txt", "one\n"), ("gone.txt", "x\n"), ("k.txt", "k\n")],
        );
        let before = fixture.observe().digest;
        fixture.write("a.txt", "edited\n");
        fixture.write("new/file.txt", "new\n");
        fs::remove_file(fixture.source.join("gone.txt")).unwrap();
        let after = fixture.observe();
        assert_eq!(
            after.changes,
            [
                change("a.txt", ChangeKind::Modified),
                change("gone.txt", ChangeKind::Deleted),
                change("new/file.txt", ChangeKind::Added),
            ]
        );
        assert_ne!(before, after.digest);
    }

    #[test]
    fn git_untracked_non_ignored_file_is_added() {
        let fixture = Fixture::new(true, &[("a.txt", "one\n")]);
        fixture.write("untracked.txt", "u\n");
        assert_eq!(
            fixture.observe().changes,
            [change("untracked.txt", ChangeKind::Added)]
        );
    }

    #[test]
    fn git_ignored_build_output_is_not_drift() {
        let fixture = Fixture::new(
            true,
            &[
                (".gitignore", "target/\n"),
                ("a.txt", "one\n"),
                ("target/out.bin", "v1\n"),
            ],
        );
        let before = fixture.observe();
        assert!(before.changes.is_empty());
        fixture.write("target/out.bin", "v2 longer\n");
        fixture.write("target/extra/new.bin", "n\n");
        let after = fixture.observe();
        assert!(after.changes.is_empty());
        assert_eq!(before.digest, after.digest);
        fs::remove_dir_all(fixture.source.join("target")).unwrap();
        assert!(fixture.observe().changes.is_empty());
    }

    #[test]
    fn git_deleted_file_is_distinguished_from_ignored_file() {
        let fixture = Fixture::new(
            true,
            &[
                (".gitignore", "*.log\n"),
                ("del.txt", "d\n"),
                ("later.tmp", "t\n"),
                ("build.log", "b\n"),
            ],
        );
        // build.log is untracked but ignored, so the baseline still holds it.
        assert!(fixture.observe().changes.is_empty());
        fs::remove_file(fixture.source.join("del.txt")).unwrap();
        fs::remove_file(fixture.source.join("build.log")).unwrap();
        git_in(&fixture.source, &["rm", "--cached", "--quiet", "later.tmp"]);
        fixture.write(".gitignore", "*.log\n*.tmp\n");
        assert_eq!(
            fixture.observe().changes,
            [
                change(".gitignore", ChangeKind::Modified),
                change("del.txt", ChangeKind::Deleted),
            ]
        );
    }

    #[test]
    fn git_crlf_file_is_unchanged() {
        let fixture = Fixture::new(
            false,
            &[
                (".gitattributes", "*.txt text eol=lf\n"),
                ("crlf.txt", "a\r\nb\r\n"),
            ],
        );
        git_in(&fixture.source, &["init", "--quiet"]);
        git_in(&fixture.source, &["config", "core.autocrlf", "true"]);
        git_in(&fixture.source, &["add", "-A"]);
        git_in(&fixture.source, &["commit", "--quiet", "-m", "initial"]);
        // The baseline predates the repository, so it has literal Directory bytes;
        // observe as a Git source to exercise ls-files and the byte-exact hashes.
        let world = observe(
            &fixture.source,
            &fixture.snapshot.baseline_path,
            &fixture.snapshot.baseline_commit,
            &SourceKind::Git,
        )
        .unwrap();
        assert!(world.changes.is_empty(), "{:?}", world.changes);
    }

    /// A Git source whose baseline was frozen as a plain directory, with
    /// `vendor/dep` its own repository; `commit_nested` tracks it as a gitlink,
    /// otherwise the outer repository leaves it untracked.
    fn nested_repo_fixture(commit_nested: bool) -> Fixture {
        let fixture = Fixture::new(
            false,
            &[
                ("a.txt", "one\n"),
                ("vendor/dep/lib.rs", "pub fn dep() {}\n"),
            ],
        );
        let nested = fixture.source.join("vendor/dep");
        git_in(&nested, &["init", "--quiet"]);
        git_in(&nested, &["add", "-A"]);
        git_in(&nested, &["commit", "--quiet", "-m", "nested"]);
        git_in(&fixture.source, &["init", "--quiet"]);
        git_in(
            &fixture.source,
            &["add", if commit_nested { "-A" } else { "a.txt" }],
        );
        git_in(&fixture.source, &["commit", "--quiet", "-m", "initial"]);
        fixture
    }

    fn observe_as_git(fixture: &Fixture) -> WorldObservation {
        observe(
            &fixture.source,
            &fixture.snapshot.baseline_path,
            &fixture.snapshot.baseline_commit,
            &SourceKind::Git,
        )
        .unwrap()
    }

    #[test]
    fn git_committed_nested_repo_is_not_comparable() {
        let fixture = nested_repo_fixture(true);
        let unchanged = observe_as_git(&fixture);
        assert!(unchanged.changes.is_empty(), "{:?}", unchanged.changes);
        assert_eq!(unchanged, observe_as_git(&fixture));
        // Edits, additions, and removal inside the nested repo are its own business.
        fixture.write("vendor/dep/lib.rs", "pub fn dep() { edited }\n");
        fixture.write("vendor/dep/new.rs", "new\n");
        assert_eq!(observe_as_git(&fixture), unchanged);
        fs::remove_dir_all(fixture.source.join("vendor/dep")).unwrap();
        assert_eq!(observe_as_git(&fixture), unchanged);
        fixture.write("a.txt", "edited\n");
        assert_eq!(
            observe_as_git(&fixture).changes,
            [change("a.txt", ChangeKind::Modified)]
        );
    }

    #[test]
    fn git_untracked_nested_repo_is_not_comparable() {
        let fixture = nested_repo_fixture(false);
        let unchanged = observe_as_git(&fixture);
        assert!(unchanged.changes.is_empty(), "{:?}", unchanged.changes);
        assert_eq!(unchanged, observe_as_git(&fixture));
        fixture.write("vendor/dep/lib.rs", "pub fn dep() { edited }\n");
        fixture.write("vendor/dep/new.rs", "new\n");
        assert_eq!(observe_as_git(&fixture), unchanged);
        fixture.write("b.txt", "b\n");
        assert_eq!(
            observe_as_git(&fixture).changes,
            [change("b.txt", ChangeKind::Added)]
        );
    }

    /// A file whose last modification is `age` before now.
    fn age(path: &Path, age: Duration) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
    }

    #[test]
    fn a_kept_hash_is_trusted_only_while_every_stat_field_matches_and_outside_the_racy_window() {
        let stat = Stat {
            symlink: false,
            executable: false,
            size: 4,
            modified: (1_000_000, 5),
            changed: (1_000_000, 7),
            inode: 42,
            mode: 0o100644,
        };
        let modified = SystemTime::UNIX_EPOCH + Duration::new(1_000_000, 7);
        let hash = "a".repeat(40);
        let mut worlds = Worlds::default();
        let mut keep = |looked: SystemTime| {
            worlds.hashes.insert(
                b"a.txt".to_vec(),
                Seen {
                    stat,
                    hash: hash.clone(),
                    looked,
                },
            );
        };
        keep(modified + Duration::from_secs(60));
        assert_eq!(worlds.kept(b"a.txt", &stat, 40), Some(hash.as_str()));
        assert_eq!(
            worlds.kept(b"a.txt", &stat, 64),
            None,
            "another hash length"
        );
        assert_eq!(worlds.kept(b"b.txt", &stat, 40), None);
        let changed = [
            Stat { size: 5, ..stat },
            Stat {
                modified: (1_000_000, 6),
                ..stat
            },
            Stat {
                changed: (1_000_001, 7),
                ..stat
            },
            Stat { inode: 43, ..stat },
            Stat {
                mode: 0o100755,
                ..stat
            },
        ];
        for other in changed {
            assert_eq!(worlds.kept(b"a.txt", &other, 40), None, "{other:?}");
        }
        let mut worlds = Worlds::default();
        let mut keep = |looked: SystemTime| {
            worlds.hashes.insert(
                b"a.txt".to_vec(),
                Seen {
                    stat,
                    hash: hash.clone(),
                    looked,
                },
            );
            worlds.kept(b"a.txt", &stat, 40).is_some()
        };
        // Looked at within 2 s of the modification: it may have changed
        // again within its timestamp's granularity.
        assert!(!keep(modified + Duration::from_millis(1999)));
        assert!(!keep(modified));
        assert!(
            !keep(modified - Duration::from_secs(1)),
            "modified after the look"
        );
        assert!(keep(modified + RACY));
        // The later of the two times counts: a file whose modification time
        // was set back changed when it was set.
        let set_back = Stat {
            changed: (1_000_100, 0),
            ..stat
        };
        let mut worlds = Worlds::default();
        worlds.hashes.insert(
            b"a.txt".to_vec(),
            Seen {
                stat: set_back,
                hash: hash.clone(),
                looked: modified + Duration::from_secs(60),
            },
        );
        assert_eq!(worlds.kept(b"a.txt", &set_back, 40), None);
    }

    #[test]
    fn kept_hashes_are_reused_and_an_edit_that_keeps_size_and_time_is_still_seen() {
        let fixture = Fixture::new(true, &[("a.txt", "one\n"), ("b.txt", "two\n")]);
        let file = fixture.source.join("a.txt");
        age(&file, Duration::from_secs(3600));
        age(&fixture.source.join("b.txt"), Duration::from_secs(3600));
        // Setting a time is a change, which must be older than the racy
        // window before a look can keep the hash.
        std::thread::sleep(RACY + Duration::from_millis(100));
        let mut worlds = Worlds::default();
        let observe = |worlds: &mut Worlds| {
            worlds
                .observe(
                    &fixture.source,
                    &fixture.snapshot.baseline_path,
                    &fixture.snapshot.baseline_commit,
                    &fixture.snapshot.kind,
                )
                .unwrap()
        };
        let first = observe(&mut worlds);
        assert!(first.changes.is_empty());
        let looked = worlds.hashes[b"a.txt".as_slice()].looked;
        worlds.moved();
        assert_eq!(observe(&mut worlds), first);
        assert_eq!(
            worlds.hashes[b"a.txt".as_slice()].looked,
            looked,
            "an unchanged old file is not hashed again"
        );

        // Same size and the same modification time: the change time moves.
        let modified = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, "eno\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        worlds.moved();
        let edited = observe(&mut worlds);
        assert_eq!(edited.changes, [change("a.txt", ChangeKind::Modified)]);
        assert_eq!(edited, fixture.observe());
        assert_ne!(edited.digest, first.digest);

        // Until `moved`, the source now is the one already looked at.
        fixture.write("c.txt", "three\n");
        assert_eq!(observe(&mut worlds), edited);
        worlds.moved();
        assert_eq!(observe(&mut worlds), fixture.observe());
        assert!(!worlds.hashes.contains_key(b"zzz".as_slice()));
        fs::remove_file(fixture.source.join("c.txt")).unwrap();
        worlds.moved();
        observe(&mut worlds);
        assert!(
            !worlds.hashes.contains_key(b"c.txt".as_slice()),
            "a file gone is forgotten"
        );
    }

    #[test]
    fn one_look_at_the_source_serves_every_baseline_as_a_fresh_observation_would() {
        let fixture = Fixture::new(true, &[("a.txt", "one\n"), ("gone.txt", "g\n")]);
        fixture.write("a.txt", "edited\n");
        fs::remove_file(fixture.source.join("gone.txt")).unwrap();
        let later = create_snapshot(&fixture.source, &fixture._root.path().join("run2")).unwrap();
        fixture.write("new.txt", "new\n");
        let mut worlds = Worlds::default();
        for snapshot in [&fixture.snapshot, &later, &fixture.snapshot] {
            let shared = worlds
                .observe(
                    &fixture.source,
                    &snapshot.baseline_path,
                    &snapshot.baseline_commit,
                    &snapshot.kind,
                )
                .unwrap();
            let fresh = observe(
                &fixture.source,
                &snapshot.baseline_path,
                &snapshot.baseline_commit,
                &snapshot.kind,
            )
            .unwrap();
            assert_eq!(shared, fresh);
        }
        assert_eq!(worlds.trees.len(), 2);
    }

    #[test]
    fn git_odd_path_names_are_handled() {
        let name = "we\"ird\nname.txt";
        let fixture = Fixture::new(true, &[(name, "one\n"), ("\"quoted.txt", "q\n")]);
        assert!(fixture.observe().changes.is_empty());
        fixture.write(name, "two\n");
        assert_eq!(
            fixture.observe().changes,
            [change(name, ChangeKind::Modified)]
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_classifies_mode_and_symlink_changes() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let fixture = Fixture::new(
            true,
            &[("run.sh", "#!/bin/sh\n"), ("t1", "1\n"), ("t2", "2\n")],
        );
        symlink("t1", fixture.source.join("link")).unwrap();
        git_in(&fixture.source, &["add", "-A"]);
        git_in(&fixture.source, &["commit", "--quiet", "-m", "link"]);
        let fixture = Fixture {
            snapshot: create_snapshot(&fixture.source, &fixture._root.path().join("run2")).unwrap(),
            ..fixture
        };
        assert!(fixture.observe().changes.is_empty());

        let script = fixture.source.join("run.sh");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_file(fixture.source.join("link")).unwrap();
        symlink("t2", fixture.source.join("link")).unwrap();
        symlink("t1", fixture.source.join("newlink")).unwrap();
        assert_eq!(
            fixture.observe().changes,
            [
                change("link", ChangeKind::Symlink),
                change("newlink", ChangeKind::Symlink),
                change("run.sh", ChangeKind::ModeOnly),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_parent_replaced_by_symlink_is_not_followed() {
        let fixture = Fixture::new(true, &[("d/file.txt", "inside\n")]);
        let outside = TempDir::new().unwrap();
        write(&outside.path().join("file.txt"), "outside\n");
        fs::remove_dir_all(fixture.source.join("d")).unwrap();
        std::os::unix::fs::symlink(outside.path(), fixture.source.join("d")).unwrap();
        assert_eq!(
            fixture.observe().changes,
            [
                change("d", ChangeKind::Symlink),
                change("d/file.txt", ChangeKind::Deleted),
            ]
        );
    }

    #[test]
    fn directory_unchanged_tree_has_no_changes_and_stable_digest() {
        let fixture = Fixture::new(false, &[("a.txt", "one\n"), ("src/b.txt", "two\n")]);
        let first = fixture.observe();
        assert!(first.changes.is_empty());
        assert_eq!(first, fixture.observe());
    }

    #[test]
    fn directory_classifies_edit_add_delete_and_ignores_state_dirs() {
        let fixture = Fixture::new(
            false,
            &[("a.txt", "one\n"), ("gone.txt", "x\n"), ("k.txt", "k\n")],
        );
        let before = fixture.observe().digest;
        fixture.write("a.txt", "edited\n");
        fixture.write("new.txt", "new\n");
        fixture.write(".dispatch/state.json", "{}\n");
        fixture.write(".git/config", "[core]\n");
        fs::remove_file(fixture.source.join("gone.txt")).unwrap();
        let after = fixture.observe();
        assert_eq!(
            after.changes,
            [
                change("a.txt", ChangeKind::Modified),
                change("gone.txt", ChangeKind::Deleted),
                change("new.txt", ChangeKind::Added),
            ]
        );
        assert_ne!(before, after.digest);
    }

    #[cfg(unix)]
    #[test]
    fn directory_classifies_mode_and_symlink_changes() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = TempDir::new().unwrap();
        let source = fs::canonicalize(root.path()).unwrap().join("source");
        fs::create_dir_all(&source).unwrap();
        write(&source.join("run.sh"), "#!/bin/sh\n");
        write(&source.join("t1"), "1\n");
        write(&source.join("t2"), "2\n");
        symlink("t1", source.join("link")).unwrap();
        let snapshot = create_snapshot(&source, &root.path().join("run")).unwrap();
        let observe = || {
            observe(
                &source,
                &snapshot.baseline_path,
                &snapshot.baseline_commit,
                &snapshot.kind,
            )
            .unwrap()
        };
        assert_eq!(observe().changes, []);

        fs::set_permissions(source.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_file(source.join("link")).unwrap();
        symlink("t2", source.join("link")).unwrap();
        symlink("t1", source.join("newlink")).unwrap();
        assert_eq!(
            observe().changes,
            [
                change("link", ChangeKind::Symlink),
                change("newlink", ChangeKind::Symlink),
                change("run.sh", ChangeKind::ModeOnly),
            ]
        );
    }

    #[test]
    fn git_signal_moves_on_edit_or_add_only() {
        let fixture = Fixture::new(true, &[(".gitignore", "target/\n"), ("a.txt", "one\n")]);
        let quiet = fixture.signal();
        assert_eq!(quiet, fixture.signal());
        fixture.write("target/out.bin", "ignored\n");
        assert_eq!(quiet, fixture.signal());
        fixture.write("a.txt", "edited\n");
        let edited = fixture.signal();
        assert_ne!(quiet, edited);
        fixture.write("new.txt", "n\n");
        assert_ne!(edited, fixture.signal());
    }

    #[test]
    fn git_signal_moves_when_an_already_dirty_file_is_edited_again() {
        let fixture = Fixture::new(true, &[("a.txt", "one\n"), ("b.txt", "same\n")]);
        fixture.write("a.txt", "edited once\n");
        fixture.write("new.txt", "n\n");
        let dirty = fixture.signal();
        assert_eq!(dirty, fixture.signal());
        // A longer edit of a modified tracked file.
        fixture.write("a.txt", "edited twice, longer\n");
        let longer = fixture.signal();
        assert_ne!(dirty, longer);
        // The same length with a new modification time.
        fixture.write("a.txt", "edited twice, LONGER\n");
        let file = fs::File::options()
            .write(true)
            .open(fixture.source.join("a.txt"))
            .unwrap();
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        let touched = fixture.signal();
        assert_ne!(longer, touched);
        // An already-untracked file counts as well.
        fixture.write("new.txt", "much longer than before\n");
        let grown = fixture.signal();
        assert_ne!(touched, grown);
        assert_eq!(grown, fixture.signal());
    }

    #[test]
    fn status_paths_reads_every_record_kind() {
        let status = b"# branch.oid abc\0\
            1 .M N... 100644 100644 100644 aaa bbb dir/a file.txt\0\
            2 R. N... 100644 100644 100644 aaa bbb R100 new.txt\0old.txt\0\
            u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.txt\0\
            ? untracked.txt\0\
            ! ignored.txt\0";
        let paths: Vec<&[u8]> = status_paths(status);
        assert_eq!(
            paths,
            [
                &b"dir/a file.txt"[..],
                b"new.txt",
                b"conflict.txt",
                b"untracked.txt"
            ]
        );
    }

    #[test]
    fn directory_signal_moves_on_edit_or_add_only() {
        let fixture = Fixture::new(false, &[("a.txt", "one\n")]);
        let quiet = fixture.signal();
        assert_eq!(quiet, fixture.signal());
        fixture.write(".dispatch/state.json", "{}\n");
        assert_eq!(quiet, fixture.signal());
        fixture.write("a.txt", "edited longer\n");
        let edited = fixture.signal();
        assert_ne!(quiet, edited);
        fixture.write("new.txt", "n\n");
        assert_ne!(edited, fixture.signal());
    }
}
