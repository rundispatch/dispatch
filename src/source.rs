use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    fs::{self, File, Metadata},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::Builder;
use walkdir::{DirEntry, WalkDir};

use crate::{
    DiffStats, RunRecord, SourceKind,
    executor::{trusted_host_executable, trusted_host_path},
    state::write_atomically,
};

const BASELINE_DIRECTORY: &str = "baseline";
const EXCLUDED_DIRECTORIES: [&str; 2] = [".git", ".dispatch"];
const MAX_CANDIDATE_FILES: u64 = 200_000;
const MAX_CANDIDATE_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CANDIDATE_TREE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_GIT_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// The immutable, Dispatch-managed representation of a source tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSnapshot {
    pub kind: SourceKind,
    pub git_head: Option<String>,
    pub fingerprint: String,
    pub baseline_path: PathBuf,
    pub baseline_commit: String,
}

/// A summary of an explicitly applied candidate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplyReport {
    pub candidate_label: String,
    pub files_changed: u64,
    /// What landed instead of Δ, merged 3-way from S0 (`LANDED_PATCH`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed_patch: Option<PathBuf>,
    /// The paths it changed that did not hold the merge right after it
    /// landed (`unlanded`): the landing is not confirmed there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unconfirmed: Vec<PathBuf>,
}

/// Resolve an explicit source directory, or the current directory when omitted.
pub fn resolve_source(input: Option<&Path>) -> Result<PathBuf> {
    let input = input.unwrap_or_else(|| Path::new("."));
    let source = fs::canonicalize(input)
        .with_context(|| format!("failed to resolve source {}", input.display()))?;
    ensure!(
        fs::metadata(&source)
            .with_context(|| format!("failed to inspect source {}", source.display()))?
            .is_dir(),
        "source is not a directory: {}",
        source.display()
    );
    Ok(source)
}

/// Identify whether a source is an ordinary directory, a Git repository, or a
/// linked Git worktree. The returned commit is `None` for an unborn repository.
pub fn inspect_source(source: &Path) -> Result<(SourceKind, Option<String>)> {
    let source = resolve_source(Some(source))?;

    let mut inside = git_command(&source);
    inside.args(["rev-parse", "--is-inside-work-tree"]);
    let output = run_git(
        inside,
        None,
        "failed to run Git while inspecting the source",
    )?;
    // Only Git's own "not a git repository" makes a plain directory: any
    // other failure (a broken configuration, an unsafe owner) is not one.
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.starts_with("fatal: not a git repository") {
            return Ok((SourceKind::Directory, None));
        }
        bail!(
            "failed to inspect the source {}: {}",
            source.display(),
            stderr.trim()
        );
    }
    if trim_ascii(&output.stdout) != b"true" {
        return Ok((SourceKind::Directory, None));
    }

    let mut git_dir_command = git_command(&source);
    git_dir_command.args(["rev-parse", "--absolute-git-dir"]);
    let git_dir_output = checked_output(
        git_dir_command,
        "failed to locate the source repository's Git directory",
    )?;
    let git_dir = bytes_to_path(trim_ascii(&git_dir_output.stdout));
    let kind = if git_dir.join("commondir").is_file() {
        SourceKind::GitWorktree
    } else {
        SourceKind::Git
    };

    let mut head_command = git_command(&source);
    head_command.args(["rev-parse", "--verify", "HEAD"]);
    let head_output = head_command
        .output()
        .with_context(|| "failed to query the source repository's HEAD")?;
    let head = if head_output.status.success() {
        Some(
            String::from_utf8(head_output.stdout)
                .context("Git returned a non-UTF-8 HEAD commit")?
                .trim()
                .to_owned(),
        )
    } else {
        None
    };

    Ok((kind, head))
}

/// The identity of a Git repository, shared by its main worktree and every
/// linked worktree: the common Git directory (canonical), the main worktree's
/// path (canonical), and a key derived from the common directory so two paths
/// of the same repository always compare equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoIdentity {
    pub common_dir: PathBuf,
    pub main_worktree: PathBuf,
    pub key: String,
}

/// Identify the repository a path belongs to. `None` for a plain directory
/// (whatever `inspect_source` reports as `SourceKind::Directory`).
pub fn repo_identity(path: &Path) -> Result<Option<RepoIdentity>> {
    let path = resolve_source(Some(path))?;
    let (kind, _) = inspect_source(&path)?;
    if matches!(kind, SourceKind::Directory) {
        return Ok(None);
    }

    let mut common_dir_command = git_command(&path);
    common_dir_command.args(["rev-parse", "--git-common-dir"]);
    let common_dir_output = checked_output(
        common_dir_command,
        "failed to locate the repository's common Git directory",
    )?;
    // A linked worktree already prints an absolute path here; the main
    // worktree prints one relative to itself (typically ".git").
    let raw_common_dir = bytes_to_path(trim_ascii(&common_dir_output.stdout));
    let common_dir = if raw_common_dir.is_absolute() {
        raw_common_dir
    } else {
        path.join(raw_common_dir)
    };
    let common_dir = fs::canonicalize(&common_dir).with_context(|| {
        format!(
            "failed to resolve the repository's common Git directory {}",
            common_dir.display()
        )
    })?;

    let mut worktree_list_command = git_command(&path);
    worktree_list_command.args(["worktree", "list", "--porcelain"]);
    let worktree_list_output = checked_output(
        worktree_list_command,
        "failed to list the repository's worktrees",
    )?;
    let main_worktree_line = worktree_list_output
        .stdout
        .split(|byte| *byte == b'\n')
        .find(|line| line.starts_with(b"worktree "))
        .context("git worktree list produced no worktree entry")?;
    let raw_main_worktree = bytes_to_path(trim_ascii(&main_worktree_line[b"worktree ".len()..]));
    let main_worktree = fs::canonicalize(&raw_main_worktree).with_context(|| {
        format!(
            "failed to resolve the repository's main worktree {}",
            raw_main_worktree.display()
        )
    })?;

    let mut hasher = Sha256::new();
    hasher.update(path_bytes(&common_dir));
    let key = hex::encode(hasher.finalize());

    Ok(Some(RepoIdentity {
        common_dir,
        main_worktree,
        key,
    }))
}

/// The top of the Git checkout `path` lies in (a subdirectory's worktree
/// root), or `None` outside any Git checkout.
pub fn checkout_top(path: &Path) -> Result<Option<PathBuf>> {
    let path = resolve_source(Some(path))?;
    if repo_identity(&path)?.is_none() {
        return Ok(None);
    }
    let mut top = git_command(&path);
    top.args(["rev-parse", "--show-toplevel"]);
    let output = checked_output(top, "failed to find the checkout's top")?;
    let top = bytes_to_path(trim_ascii(&output.stdout));
    Ok(Some(fs::canonicalize(&top).with_context(|| {
        format!("failed to resolve {}", top.display())
    })?))
}

/// The merge base of `root`'s `HEAD` and `workspace`'s `HEAD`, or `None` when
/// they share no history. `root` and `workspace` must name the same
/// repository (equal `repo_identity` keys); a workspace from an unrelated
/// repository is refused rather than compared, since Git's answer there would
/// be either an error or a misleading coincidence.
pub fn merge_base(root: &Path, workspace: &Path) -> Result<Option<String>> {
    let root = resolve_source(Some(root))?;
    let workspace = resolve_source(Some(workspace))?;

    let root_identity = repo_identity(&root)?
        .with_context(|| format!("{} is not a Git repository", root.display()))?;
    let workspace_identity = repo_identity(&workspace)?
        .with_context(|| format!("{} is not a Git repository", workspace.display()))?;
    ensure!(
        root_identity.key == workspace_identity.key,
        "workspace {} belongs to a different repository than {}",
        workspace.display(),
        root.display()
    );

    let mut head_command = git_command(&workspace);
    head_command.args(["rev-parse", "--verify", "HEAD"]);
    let head_output = checked_output(head_command, "failed to resolve the workspace HEAD")?;
    let workspace_head = String::from_utf8(trim_ascii(&head_output.stdout).to_vec())
        .context("Git returned a non-UTF-8 workspace HEAD commit")?;

    let mut merge_base_command = git_command(&root);
    merge_base_command.args(["merge-base", "HEAD", &workspace_head]);
    let output = run_git(merge_base_command, None, "failed to compute the merge base")?;
    if output.status.success() {
        let commit = String::from_utf8(trim_ascii(&output.stdout).to_vec())
            .context("Git returned a non-UTF-8 merge base commit")?;
        Ok(Some(commit))
    } else if output.status.code() == Some(1) {
        // No common ancestor: the documented "no merge base" exit status.
        Ok(None)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            bail!(
                "failed to compute the merge base (Git exited with {})",
                output.status
            );
        }
        bail!("failed to compute the merge base: {stderr}");
    }
}

/// Freeze the exact current source contents in a private Git repository.
///
/// This deliberately snapshots the working tree rather than merely cloning
/// `HEAD`, so dirty and untracked source files are part of every candidate's
/// common baseline. Git and Dispatch state directories are never copied.
pub fn create_snapshot(source: &Path, run_dir: &Path) -> Result<SourceSnapshot> {
    create_snapshot_with_mode(source, run_dir, false)
}

pub(crate) fn create_snapshot_with_mode(
    source: &Path,
    run_dir: &Path,
    strict: bool,
) -> Result<SourceSnapshot> {
    let source = resolve_source(Some(source))?;
    let projected_run_dir = canonicalize_allow_missing(run_dir)?;
    ensure!(
        !projected_run_dir.starts_with(&source),
        "Dispatch run directory must be outside the source tree: {}",
        run_dir.display()
    );

    let (kind, git_head) = inspect_source(&source)?;
    let fingerprint = if strict {
        fingerprint_tree_strict
    } else {
        fingerprint_tree
    };
    let fingerprint_before = fingerprint(&source)?;

    fs::create_dir_all(run_dir)
        .with_context(|| format!("failed to create run directory {}", run_dir.display()))?;
    let baseline_path = run_dir.join(BASELINE_DIRECTORY);
    ensure!(
        fs::symlink_metadata(&baseline_path).is_err(),
        "baseline already exists: {}",
        baseline_path.display()
    );

    let staging = Builder::new()
        .prefix(".dispatch-baseline-")
        .tempdir_in(run_dir)
        .with_context(|| format!("failed to create a snapshot in {}", run_dir.display()))?;
    copy_tree_contents(&source, staging.path())?;

    // A source changed while it was being copied is not a trustworthy baseline.
    // Check both ends so a mixed snapshot cannot be accepted silently.
    let fingerprint_after = fingerprint(&source)?;
    let copied_fingerprint = if !strict && matches!(kind, SourceKind::Git | SourceKind::GitWorktree)
    {
        fingerprint_git_tree(staging.path(), &source)?
    } else {
        fingerprint(staging.path())?
    };
    ensure!(
        fingerprint_before == fingerprint_after && fingerprint_before == copied_fingerprint,
        "source changed while Dispatch was creating its baseline; retry the run"
    );

    // A Git source's world (`world::observe`) is the tree minus its own ignore
    // rules; the baseline commit must track exactly that, or a build artifact
    // that happens to exist at snapshot time gets force-tracked, regenerated by
    // a check, and mistaken for part of the candidate's Δ. A plain directory has
    // no ignore rules for its world, so it keeps tracking everything.
    let honor_ignore = matches!(kind, SourceKind::Git | SourceKind::GitWorktree);
    initialize_internal_repository(staging.path(), &source, honor_ignore)?;
    let baseline_commit = git_head_at(staging.path())?
        .context("internal baseline repository did not produce a commit")?;

    fs::rename(staging.path(), &baseline_path)
        .with_context(|| format!("failed to finalize baseline {}", baseline_path.display()))?;

    Ok(SourceSnapshot {
        kind,
        git_head,
        fingerprint: fingerprint_before,
        baseline_path,
        baseline_commit,
    })
}

/// Materialize a commit's tree (not the live working tree) as the baseline
/// for attached work: `S0` is `merge_base(root, workspace)`, a real commit
/// that predates the agent's edits, rather than a snapshot of anything
/// currently checked out. Follows the same staging/fingerprint/rename
/// discipline as `create_snapshot`, and the same reasoning for honoring the
/// source's ignore rules: the baseline must track exactly what
/// `world::observe` considers part of the world, or a build artifact
/// materialized at the commit gets force-tracked and mistaken for Δ. `repo`
/// supplies both the commit's objects and, via `initialize_internal_repository`,
/// its `info/exclude`.
pub fn materialize_baseline_from_commit(
    repo: &Path,
    commit: &str,
    run_dir: &Path,
) -> Result<SourceSnapshot> {
    let repo = resolve_source(Some(repo))?;
    fs::create_dir_all(run_dir)
        .with_context(|| format!("failed to create run directory {}", run_dir.display()))?;
    let baseline_path = run_dir.join(BASELINE_DIRECTORY);
    ensure!(
        fs::symlink_metadata(&baseline_path).is_err(),
        "baseline already exists: {}",
        baseline_path.display()
    );

    let staging = Builder::new()
        .prefix(".dispatch-baseline-")
        .tempdir_in(run_dir)
        .with_context(|| format!("failed to create a baseline in {}", run_dir.display()))?;
    export_commit_tree(&repo, commit, staging.path())?;

    initialize_internal_repository(staging.path(), &repo, true)?;
    let baseline_commit = git_head_at(staging.path())?
        .context("internal baseline repository did not produce a commit")?;
    let fingerprint = fingerprint_tree(staging.path())?;

    fs::rename(staging.path(), &baseline_path)
        .with_context(|| format!("failed to finalize baseline {}", baseline_path.display()))?;

    Ok(SourceSnapshot {
        kind: SourceKind::Git,
        git_head: Some(commit.to_owned()),
        fingerprint,
        baseline_path,
        baseline_commit,
    })
}

/// The exact world of a Git checkout as a commit, without copying files: its
/// tracked files as they are now, dirty or deleted, and its untracked files
/// its ignore rules do not exclude (what `world::observe` calls the world),
/// with `HEAD` as parent. Built like `git stash create`: staged into a copy of
/// the checkout's own index (so Git's stat cache spares unchanged files),
/// never its real index. Untracked nested repositories are left out, as the
/// world leaves them out. The commit's objects live in the checkout's own
/// repository, written durably; `materialize_baseline_from_commit` turns it
/// into a baseline.
///
/// A capture during which the checkout moved is never accepted, since it may
/// mix files from before and after a change: the checkout's change signal
/// (`world::signal`) is taken before and after it. If it moved, the capture is
/// made once more; if that one moves too, this is an error.
pub fn world_commit(checkout: &Path) -> Result<String> {
    let checkout = resolve_source(Some(checkout))?;
    capture_settled(&checkout, || capture_world(&checkout))
}

/// The tree of `world_commit`, captured the same way but with no commit
/// object written: for looking at a world rather than recording it.
pub(crate) fn world_tree(checkout: &Path) -> Result<String> {
    let checkout = resolve_source(Some(checkout))?;
    capture_settled(&checkout, || Ok(capture_tree(&checkout)?.0))
}

/// The ref naming the source as it was after its latest landing.
pub const SOURCE_REF: &str = "refs/dispatch/source";

/// Record `root`'s world now as a commit (`world_commit`), landed changes
/// included though accept never commits them, and point `SOURCE_REF` at it
/// with `message` in its reflog, so an agent can merge exactly what landed.
/// Only that ref moves: the checkout's branches, HEAD, index and files don't.
pub fn record_source(root: &Path, message: &str) -> Result<String> {
    let root = resolve_source(Some(root))?;
    let commit = world_commit(&root)?;
    let mut update = git_command(&root);
    update
        .env("GIT_COMMITTER_NAME", "Dispatch")
        .env("GIT_COMMITTER_EMAIL", "dispatch@localhost")
        .args(["update-ref", "--create-reflog", "-m", message, SOURCE_REF])
        .arg(&commit);
    checked_output(update, "failed to point refs/dispatch/source at the source")?;
    Ok(commit)
}

fn capture_settled(checkout: &Path, mut capture: impl FnMut() -> Result<String>) -> Result<String> {
    let signal = || crate::coherence::world::signal(checkout, &SourceKind::Git);
    let mut before = signal()?;
    for _ in 0..2 {
        let commit = capture()?;
        let after = signal()?;
        if after == before {
            return Ok(commit);
        }
        before = after;
    }
    bail!(
        "the workspace changed while its starting state was being captured ({}); \
         try again once it is quiet",
        checkout.display()
    )
}

/// Objects S0 is made of are fsynced as they are written.
const FSYNC_OBJECTS: [&str; 2] = ["-c", "core.fsync=loose-object"];

fn capture_world(checkout: &Path) -> Result<String> {
    let (tree, head) = capture_tree(checkout)?;
    let mut commit = git_command(checkout);
    commit
        .env("GIT_AUTHOR_NAME", "Dispatch")
        .env("GIT_AUTHOR_EMAIL", "dispatch@localhost")
        .env("GIT_COMMITTER_NAME", "Dispatch")
        .env("GIT_COMMITTER_EMAIL", "dispatch@localhost")
        .args(FSYNC_OBJECTS)
        .args(["commit-tree", &tree, "-m", "Dispatch S0"]);
    if let Some(head) = &head {
        commit.args(["-p", head]);
    }
    let commit = checked_output(commit, "failed to record the checkout's world")?.stdout;
    Ok(String::from_utf8_lossy(&commit).trim().to_owned())
}

/// `capture_world`'s tree, and the checkout's HEAD.
fn capture_tree(checkout: &Path) -> Result<(String, Option<String>)> {
    let scratch = Builder::new()
        .prefix("dispatch-world-")
        .tempdir()
        .context("failed to create a temporary Git index")?;
    let index = scratch.path().join("index");

    let mut index_path = git_command(checkout);
    index_path.args(["rev-parse", "--path-format=absolute", "--git-path", "index"]);
    let real_index = PathBuf::from(
        String::from_utf8_lossy(
            &checked_output(index_path, "failed to locate the Git index")?.stdout,
        )
        .trim(),
    );
    if real_index.is_file() {
        // The copy keeps the index's mtime, read before copying: Git trusts
        // an entry's stat data only when the file is older than the index
        // file, and re-reads "racily clean" entries written in the same
        // second. A fresh mtime would hide a same-size edit made in the
        // second the index was written.
        let written = fs::metadata(&real_index)
            .and_then(|metadata| metadata.modified())
            .with_context(|| format!("failed to read {}", real_index.display()))?;
        fs::copy(&real_index, &index)
            .with_context(|| format!("failed to copy {}", real_index.display()))?;
        fs::File::options()
            .write(true)
            .open(&index)
            .and_then(|copy| copy.set_modified(written))
            .context("failed to keep the Git index's time on its copy")?;
    }
    let head = git_head_at(checkout)?;
    if !real_index.is_file() && head.is_some() {
        let mut read_tree = git_command(checkout);
        read_tree
            .env("GIT_INDEX_FILE", &index)
            .args(["read-tree", "HEAD"]);
        checked_output(read_tree, "failed to initialize a temporary Git index")?;
    }

    // Untracked nested repositories are listed as `dir/`; the world skips them.
    let mut list = git_command(checkout);
    list.env("GIT_INDEX_FILE", &index)
        .args(["ls-files", "--others", "--exclude-standard", "-z"]);
    let listed = checked_output(list, "failed to list the checkout's untracked files")?;
    let nested: Vec<String> = listed
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|raw| raw.strip_suffix(b"/"))
        .map(|directory| format!(":(exclude,literal){}", String::from_utf8_lossy(directory)))
        .collect();

    let mut add = git_command(checkout);
    add.env("GIT_INDEX_FILE", &index)
        .args(FSYNC_OBJECTS)
        .args(["add", "-A", "--", "."])
        .args(dispatch_exclusion_pathspecs())
        .args(&nested);
    checked_output(add, "failed to stage the checkout's world")?;

    let mut write_tree = git_command(checkout);
    write_tree
        .env("GIT_INDEX_FILE", &index)
        .args(FSYNC_OBJECTS)
        .arg("write-tree");
    let tree = checked_output(write_tree, "failed to write the checkout's world")?.stdout;
    Ok((String::from_utf8_lossy(&tree).trim().to_owned(), head))
}

/// A private workspace at `dir` that is the baseline plus `patch`: the Work
/// as it was when its own workspace was removed, for verifying it.
pub fn rebuild_workspace(baseline_path: &Path, patch: &Path, dir: &Path) -> Result<PathBuf> {
    let workspace = create_candidate_workspace(baseline_path, dir)?;
    if fs::metadata(patch)?.len() > 0 {
        let mut apply = git_command(&workspace);
        apply
            .args(["apply", "--binary", "--whitespace=nowarn", "--"])
            .arg(patch);
        checked_output(apply, "failed to rebuild the work from its kept changes")?;
    }
    Ok(workspace)
}

/// A linked worktree of `checkout`'s repository at `path`, on a new `branch`
/// at `commit`: a workspace whose files are exactly that commit. `path` must
/// lie outside the checkout.
pub fn create_linked_workspace(
    checkout: &Path,
    commit: &str,
    path: &Path,
    branch: &str,
) -> Result<()> {
    let checkout = resolve_source(Some(checkout))?;
    let projected = canonicalize_allow_missing(path)?;
    ensure!(
        !projected.starts_with(&checkout),
        "a workspace must be outside the checkout: {}",
        path.display()
    );
    ensure!(
        fs::symlink_metadata(path).is_err(),
        "workspace already exists: {}",
        path.display()
    );
    let mut add = git_command(&checkout);
    add.args(["worktree", "add", "--quiet", "-b", branch, "--"])
        .arg(path)
        .arg(commit);
    checked_output(add, "failed to create the workspace")?;
    Ok(())
}

/// Remove a linked worktree `create_linked_workspace` made, and its branch.
pub fn remove_linked_workspace(checkout: &Path, path: &Path, branch: &str) -> Result<()> {
    let mut remove = git_command(checkout);
    remove
        .args(["worktree", "remove", "--force", "--"])
        .arg(path);
    checked_output(remove, "failed to remove the workspace")?;
    let mut delete = git_command(checkout);
    delete.args(["branch", "-D", "--", branch]);
    checked_output(delete, "failed to delete the workspace's branch")?;
    Ok(())
}

/// Create a complete, independent workspace at the frozen baseline commit.
pub fn create_candidate_workspace(baseline_path: &Path, candidate_dir: &Path) -> Result<PathBuf> {
    let baseline_path = resolve_source(Some(baseline_path))?;
    ensure_internal_repository(&baseline_path)?;
    ensure_repository_clean(&baseline_path, "baseline")?;

    ensure!(
        fs::symlink_metadata(candidate_dir).is_err(),
        "candidate workspace already exists: {}",
        candidate_dir.display()
    );
    let candidate_dir = canonicalize_allow_missing(candidate_dir)?;
    ensure!(
        !candidate_dir.starts_with(&baseline_path),
        "candidate workspace must not be inside the baseline repository"
    );
    if let Some(parent) = candidate_dir.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create candidate parent {}", parent.display()))?;
    }

    let result = (|| {
        let mut clone = plain_git_command();
        clone
            .args(["clone", "--quiet", "--no-hardlinks", "--no-checkout", "--"])
            .arg(&baseline_path)
            .arg(&candidate_dir);
        checked_output(clone, "failed to clone the frozen baseline")?;

        configure_candidate_repository(&candidate_dir)?;
        let mut read_tree = git_command(&candidate_dir);
        read_tree.args(["read-tree", "HEAD"]);
        checked_output(read_tree, "failed to prepare the candidate Git index")?;

        // The clone intentionally has no checkout. Copying the pristine
        // worktree creates the exact source bytes, empty directories, symlink
        // targets, and permissions instead of asking Git to reconstruct the
        // subset of metadata its object model represents.
        copy_tree_contents(&baseline_path, &candidate_dir)?;
        ensure_repository_clean(&candidate_dir, "candidate workspace")?;
        ensure!(
            fingerprint_tree(&baseline_path)? == fingerprint_tree(&candidate_dir)?,
            "candidate workspace does not exactly match the frozen baseline"
        );
        Ok(candidate_dir.clone())
    })();

    if result.is_err() {
        remove_created_directory(&candidate_dir);
    }
    result
}

/// Collect a binary-capable unified patch and deterministic change statistics.
///
/// A temporary Git index is used so untracked files can be represented in the
/// patch without altering the candidate's real index.
pub fn collect_diff(baseline_path: &Path, workspace: &Path, diff_path: &Path) -> Result<DiffStats> {
    let baseline_path = resolve_source(Some(baseline_path))?;
    let workspace = resolve_source(Some(workspace))?;
    ensure_internal_repository(&baseline_path)?;
    ensure_repository_clean(&baseline_path, "baseline")?;

    let projected_diff_path = canonicalize_allow_missing(diff_path)?;
    ensure!(
        !projected_diff_path.starts_with(&workspace),
        "diff artifact must be stored outside the candidate workspace"
    );
    validate_candidate_tree(&baseline_path, &workspace)?;

    let untracked_files = list_untracked_files(&baseline_path, &workspace)?;
    let index_directory = Builder::new()
        .prefix("dispatch-index-")
        .tempdir()
        .context("failed to create a temporary Git index")?;
    let index_path = index_directory.path().join("index");

    let mut read_tree = comparison_git_command(&baseline_path, &workspace);
    read_tree
        .env("GIT_INDEX_FILE", &index_path)
        .args(["read-tree", "HEAD"]);
    checked_output(read_tree, "failed to initialize temporary Git index")?;

    let mut add = comparison_git_command(&baseline_path, &workspace);
    add.env("GIT_INDEX_FILE", &index_path)
        .args(["add", "-A", "--", "."])
        .args(dispatch_exclusion_pathspecs());
    checked_output(add, "failed to stage candidate changes for diff collection")?;

    let mut diff = comparison_git_command(&baseline_path, &workspace);
    diff.env("GIT_INDEX_FILE", &index_path).args([
        "diff",
        "--cached",
        "--binary",
        "--full-index",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "HEAD",
        "--",
    ]);
    let patch = checked_output(diff, "failed to collect candidate diff")?.stdout;

    let mut numstat = comparison_git_command(&baseline_path, &workspace);
    numstat.env("GIT_INDEX_FILE", &index_path).args([
        "diff",
        "--cached",
        "--numstat",
        "--no-renames",
        "-z",
        "HEAD",
        "--",
    ]);
    let numstat = checked_output(numstat, "failed to collect candidate diff statistics")?;
    let (files_changed, lines_added, lines_removed, paths) = parse_numstat(&numstat.stdout)?;
    let mut changed_files = paths
        .into_iter()
        .map(|path| String::from_utf8_lossy(&path).into_owned())
        .collect::<Vec<_>>();
    changed_files.sort();

    if let Some(parent) = diff_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create diff directory {}", parent.display()))?;
    }
    fs::write(diff_path, patch)
        .with_context(|| format!("failed to write diff artifact {}", diff_path.display()))?;

    Ok(DiffStats {
        files_changed,
        lines_added,
        lines_removed,
        changed_files,
        untracked_files,
    })
}

/// Write the work-so-far of a live candidate workspace as a patch against the
/// baseline, for the mid-run coherence watcher. It uses the same hardened
/// temporary-index technique as `collect_diff` but does not validate the tree
/// (the agent is still writing it) and computes no statistics. The result is
/// advisory evidence only and is never applied.
pub fn snapshot_delta(baseline_path: &Path, workspace: &Path, out_patch: &Path) -> Result<()> {
    let index_directory = Builder::new()
        .prefix("dispatch-index-")
        .tempdir()
        .context("failed to create a temporary Git index")?;
    snapshot_delta_indexed(
        baseline_path,
        workspace,
        out_patch,
        &index_directory.path().join("index"),
    )
}

/// `snapshot_delta` with an index the caller keeps between snapshots of the
/// same workspace: Git's stat cache then re-hashes only files that changed,
/// which is what makes following work every tick affordable. The baseline's
/// HEAD never moves, so the index is initialized from it only once.
pub fn snapshot_delta_indexed(
    baseline_path: &Path,
    workspace: &Path,
    out_patch: &Path,
    index_path: &Path,
) -> Result<()> {
    if !index_path.exists() {
        let mut read_tree = comparison_git_command(baseline_path, workspace);
        read_tree
            .env("GIT_INDEX_FILE", index_path)
            .args(["read-tree", "HEAD"]);
        checked_output(read_tree, "failed to initialize temporary Git index")?;
    }

    let mut add = comparison_git_command(baseline_path, workspace);
    add.env("GIT_INDEX_FILE", index_path)
        .args(["add", "-A", "--", "."])
        .args(dispatch_exclusion_pathspecs());
    checked_output(add, "failed to stage the work in progress")?;

    let mut diff = comparison_git_command(baseline_path, workspace);
    diff.env("GIT_INDEX_FILE", index_path).args([
        "diff",
        "--cached",
        "--binary",
        "--full-index",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "HEAD",
        "--",
    ]);
    let patch = checked_output(diff, "failed to collect the work in progress")?.stdout;
    // Watchers and the owner read this file while it is rewritten, so publish
    // it by rename. No fsync: it is advisory evidence, not durable state.
    write_atomically(out_patch, &patch)
}

/// Follow a workspace after the first snapshot only when its contents differ
/// from the retained temporary index. `retained_patch` may differ from the
/// output so a caller can discard a capture interrupted by worktree removal.
/// The worktree column of porcelain status sees a second edit to a file that
/// was already changed in the patch.
pub(crate) fn snapshot_delta_if_changed_indexed(
    baseline_path: &Path,
    workspace: &Path,
    out_patch: &Path,
    index_path: &Path,
    retained_patch: &Path,
) -> Result<bool> {
    // An index staged against one baseline says nothing of another, and a
    // Work's S0 moves to a new baseline when it integrates the source.
    let staged_for = index_path.with_extension("baseline");
    let baseline = path_bytes(baseline_path);
    if fs::read(&staged_for).ok().as_deref() != Some(baseline.as_slice()) {
        match fs::remove_file(index_path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
            _ => write_atomically(&staged_for, &baseline)?,
        }
    }
    if index_path.exists() && retained_patch.exists() {
        let mut status = comparison_git_command(baseline_path, workspace);
        status.env("GIT_INDEX_FILE", index_path).args([
            "status",
            "--porcelain=v1",
            "-z",
            "--no-renames",
            "--untracked-files=all",
            "--",
            ".",
        ]);
        status.args(dispatch_exclusion_pathspecs());
        let output = checked_output(status, "failed to inspect the work in progress")?;
        let changed = output.stdout.split(|byte| *byte == 0).any(|entry| {
            entry.starts_with(b"??") || entry.get(1).is_some_and(|status| *status != b' ')
        });
        if !changed {
            return Ok(false);
        }
    }
    match snapshot_delta_indexed(baseline_path, workspace, out_patch, index_path) {
        Ok(()) => Ok(true),
        Err(error) => {
            // Staging may have succeeded before patch publication failed. A
            // retained index from that attempt would make the next status
            // look clean while out_patch still held the older contents.
            let _ = fs::remove_file(index_path);
            Err(error)
        }
    }
}

/// Hash the logical source tree, excluding Git and Dispatch state, and for Git
/// sources ignoring untracked files excluded by the repository's ignore rules.
/// Content, paths, symlink targets, file kinds, and permission bits are covered;
/// timestamps are intentionally ignored. An entry that disappears during the
/// walk (a build deleting its output) is a difference, never an error: it is
/// hashed as `VANISHED`, so the value equals no tree that held the entry.
pub fn fingerprint_tree(root: &Path) -> Result<String> {
    let root = resolve_source(Some(root))?;
    if !matches!(inspect_source(&root)?.0, SourceKind::Directory) {
        return fingerprint_git_tree(&root, &root);
    }
    let entries = list_tree(&root)?;
    hash_tree(&root, entries)
}

/// Strict mode fences the complete tree, including ignored build output.
pub(crate) fn fingerprint_tree_strict(root: &Path) -> Result<String> {
    let root = resolve_source(Some(root))?;
    hash_tree(&root, list_tree(&root)?)
}

/// An unreadable frozen configuration keeps the strict, fail-closed fence.
pub(crate) fn fingerprint_for_run(run: &RunRecord) -> Result<String> {
    let strict = crate::coherence::run_directory(run)
        .and_then(crate::coherence::run_config)
        .is_none_or(|config| config.coherence.accept == crate::config::AcceptMode::Strict);
    if strict {
        fingerprint_tree_strict(&run.source_path)
    } else {
        fingerprint_tree(&run.source_path)
    }
}

// Use the source's index and ignore rules even for the not-yet-initialized copy.
fn fingerprint_git_tree(root: &Path, repository: &Path) -> Result<String> {
    let mut list = git_command(repository);
    list.args(["ls-files", "-co", "--exclude-standard", "-z", "--", "."])
        .args(dispatch_exclusion_pathspecs());
    let listing = checked_output(list, "failed to list fingerprint inputs")?;
    let mut paths = HashSet::new();
    let mut directories = HashSet::new();
    for raw in listing
        .stdout
        .split(|byte| *byte == 0)
        .filter(|p| !p.is_empty())
    {
        let relative = bytes_to_path(raw.strip_suffix(b"/").unwrap_or(raw));
        if !parents_are_directories(root, &relative, &mut directories) {
            if let Some(link) = symlinked_parent(root, &relative)? {
                paths.insert(link);
            }
            continue;
        }
        let path = root.join(&relative);
        if unless_vanished(fs::symlink_metadata(&path), || {
            format!("failed to inspect {}", path.display())
        })?
        .is_none()
        {
            continue;
        }
        paths.insert(path);
        for parent in relative
            .ancestors()
            .skip(1)
            .filter(|p| !p.as_os_str().is_empty())
        {
            paths.insert(root.join(parent));
        }
    }
    let mut entries: Vec<_> = paths.into_iter().map(|path| (path, true)).collect();
    entries.sort_by(|(a, _), (b, _)| {
        path_bytes(a.strip_prefix(root).unwrap()).cmp(&path_bytes(b.strip_prefix(root).unwrap()))
    });
    hash_tree(root, entries)
}

/// Hashed in place of an entry's kind (`l`, `d` or `f`) when it vanished.
const VANISHED: &[u8] = b"x";

/// Every path under `root` but Git and Dispatch state, in fingerprint order,
/// each with whether it could be listed: a directory that vanished before the
/// walk read it is kept, as not listed.
fn list_tree(root: &Path) -> Result<Vec<(PathBuf, bool)>> {
    let mut entries = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(include_entry)
    {
        match entry {
            Ok(entry) if entry.depth() > 0 => entries.push((entry.into_path(), true)),
            Ok(_) => {}
            Err(error) => {
                let vanished = error.depth() > 0
                    && error
                        .io_error()
                        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound);
                match error.path() {
                    Some(path) if vanished => entries.push((path.to_owned(), false)),
                    _ => {
                        return Err(error)
                            .with_context(|| format!("failed to walk {}", root.display()));
                    }
                }
            }
        }
    }
    entries.sort_by(|(left, _), (right, _)| {
        path_bytes(left.strip_prefix(root).expect("walked path is under root")).cmp(&path_bytes(
            right.strip_prefix(root).expect("walked path is under root"),
        ))
    });
    Ok(entries)
}

/// `Ok(None)` when the entry was not found: it vanished since it was listed.
fn unless_vanished<T>(
    result: std::io::Result<T>,
    context: impl FnOnce() -> String,
) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(context),
    }
}

fn hash_tree(root: &Path, entries: Vec<(PathBuf, bool)>) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"dispatch-tree-v1\0");
    let mut buffer = vec![0_u8; 64 * 1024];
    for (path, listed) in entries {
        let relative = path.strip_prefix(root).expect("walked path is under root");
        let path_key = path_bytes(relative);
        hash_sized_bytes(&mut hasher, &path_key);

        let metadata = if listed {
            unless_vanished(fs::symlink_metadata(&path), || {
                format!("failed to inspect {}", path.display())
            })?
        } else {
            None
        };
        let Some(metadata) = metadata else {
            hasher.update(VANISHED);
            continue;
        };
        if metadata.file_type().is_symlink() {
            let Some(target) = unless_vanished(fs::read_link(&path), || {
                format!("failed to read symlink {}", path.display())
            })?
            else {
                hasher.update(VANISHED);
                continue;
            };
            ensure_symlink_stays_within(root, &path, &target)?;
            hasher.update(b"l");
            hash_mode(&mut hasher, &metadata);
            hash_sized_bytes(&mut hasher, &path_bytes(&target));
        } else if metadata.is_dir() {
            hasher.update(b"d");
            hash_mode(&mut hasher, &metadata);
        } else if metadata.is_file() {
            let Some(mut file) = unless_vanished(File::open(&path), || {
                format!("failed to read {}", path.display())
            })?
            else {
                hasher.update(VANISHED);
                continue;
            };
            hasher.update(b"f");
            hash_mode(&mut hasher, &metadata);
            hasher.update(metadata.len().to_le_bytes());
            loop {
                let read = file
                    .read(&mut buffer)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
        } else {
            bail!(
                "unsupported special file in source tree: {}",
                path.display()
            );
        }
    }

    Ok(hex::encode(hasher.finalize()))
}

/// What must still hold of the source for a patch to be applied: it has not
/// changed since snapshotting (the run's fingerprint), or it is the moved
/// world (`world::observe`'s digest) that coherence validation has already
/// judged compatible.
#[derive(Clone, Copy)]
pub enum ExpectedWorld<'a> {
    Fingerprint,
    Digest(&'a str),
}

impl ExpectedWorld<'_> {
    fn holds(self, run: &RunRecord, source: &Path) -> Result<bool> {
        Ok(match self {
            Self::Fingerprint => fingerprint_for_run(run)? == run.source_fingerprint,
            Self::Digest(expected) => {
                crate::coherence::world::observe(
                    source,
                    &run.baseline_path,
                    &run.baseline_commit,
                    &run.source_kind,
                )?
                .digest
                    == expected
            }
        })
    }
}

/// Apply `patch` to `run`'s source, all of it or nothing, while `expected`
/// holds: it is checked before Git's complete dry run and again before the
/// real apply. Returns the paths the patch changed.
pub fn apply_checked(
    run: &RunRecord,
    patch: &Path,
    expected: ExpectedWorld,
) -> Result<Vec<PathBuf>> {
    let source = resolve_source(Some(&run.source_path))?;
    ensure!(
        expected.holds(run, &source)?,
        "source has changed since this run was created; refusing to apply {}",
        patch.display()
    );

    let patch_metadata = fs::metadata(patch)
        .with_context(|| format!("failed to inspect candidate diff {}", patch.display()))?;
    ensure!(
        patch_metadata.is_file(),
        "candidate diff is not a file: {}",
        patch.display()
    );
    if patch_metadata.len() == 0 {
        return Ok(Vec::new());
    }

    let patch_paths = inspect_patch_paths(&source, patch)?;
    ensure!(
        !patch_paths.is_empty(),
        "candidate diff contains no applicable file changes"
    );
    for path in &patch_paths {
        ensure_safe_patch_path(path)?;
    }

    let mut check = git_command(&source);
    check
        .args(["apply", "--check", "--binary", "--whitespace=nowarn", "--"])
        .arg(patch);
    checked_output(
        check,
        "candidate cannot be applied cleanly; the source was left unchanged",
    )?;

    // Close the most useful check/apply race window. The real `git apply` also
    // validates every hunk before writing any file.
    ensure!(
        expected.holds(run, &source)?,
        "source changed during apply validation; the source was left unchanged"
    );

    let mut apply = git_command(&source);
    apply
        .args(["apply", "--binary", "--whitespace=nowarn", "--"])
        .arg(patch);
    checked_output(
        apply,
        "candidate application failed; Git did not apply the patch",
    )?;

    Ok(patch_paths)
}

/// Beside the run: what a 3-way landing applies instead of Δ.
pub const LANDED_PATCH: &str = "delta-landed.patch";

/// Merge `run`'s Δ (`delta`) 3-way from S0 into its source's world as it is
/// now and, only if that is the tree the merged checks passed on (`verified`),
/// write to `out` the patch that takes the world to it, binary and with full
/// blob ids like Δ. Returns that tree, whose objects are in the source's
/// repository.
pub(crate) fn write_landed_patch(
    run: &RunRecord,
    delta: &Path,
    out: &Path,
    verified: &str,
) -> Result<String> {
    let source = resolve_source(Some(&run.source_path))?;
    let patch = Patch::new(delta, &run.baseline_path);
    let (world, tree) = match crate::coherence::memo::staged_on_world(&source, &[patch])? {
        Some((world, Stage::Staged { tree })) => (world, tree),
        Some((_, Stage::Conflict { output })) => {
            bail!("the patch no longer merges into the source: {output}")
        }
        None => bail!("the source cannot take a 3-way merge"),
    };
    ensure!(
        tree == verified,
        "the 3-way merge is now tree {tree}, which no longer matches the tree {verified} \
         the merged checks verified; the source was left unchanged"
    );
    let mut diff = git_command(&source);
    diff.args([
        "diff-tree",
        "-r",
        "-p",
        "--binary",
        "--full-index",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--relative",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        &world,
        &tree,
    ]);
    let landed = checked_output(diff, "failed to write the merged patch")?.stdout;
    fs::write(out, landed).with_context(|| format!("failed to write {}", out.display()))?;
    Ok(tree)
}

/// Which of `paths` in `run`'s source do not hold what `tree` holds there
/// (kind, mode and bytes, or nothing where `tree` has nothing): all of them
/// when Git cannot say. Read through a temporary index; the source's own is
/// not touched.
pub(crate) fn unlanded(run: &RunRecord, paths: &[PathBuf], tree: &str) -> Vec<PathBuf> {
    differing(&run.source_path, paths, tree).unwrap_or_else(|error| {
        tracing::debug!("could not compare the landed files with {tree}: {error:#}");
        paths.to_vec()
    })
}

fn differing(source: &Path, paths: &[PathBuf], tree: &str) -> Result<Vec<PathBuf>> {
    let scratch = Builder::new()
        .prefix("dispatch-landed-")
        .tempdir()
        .context("failed to create a temporary Git index")?;
    let index = scratch.path().join("index");
    let mut read_tree = git_command(source);
    read_tree
        .env("GIT_INDEX_FILE", &index)
        .args(["read-tree", tree]);
    checked_output(read_tree, "failed to read the merged tree")?;
    let mut listed = Vec::new();
    for path in paths {
        listed.extend(path_bytes(path));
        listed.push(0);
    }
    let mut update = git_command(source);
    update.env("GIT_INDEX_FILE", &index).args([
        "update-index",
        "-z",
        "--add",
        "--remove",
        "--stdin",
    ]);
    let updated = run_git(update, Some(&listed), "failed to read the landed files")?;
    ensure!(
        updated.status.success(),
        "failed to read the landed files: {}",
        git_says(&updated)
    );
    let mut diff = git_command(source);
    diff.env("GIT_INDEX_FILE", &index).args([
        "diff-index",
        "--cached",
        "--name-only",
        "-z",
        "--relative",
        tree,
    ]);
    let differ = checked_output(diff, "failed to compare the landed files")?.stdout;
    Ok(differ
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(bytes_to_path)
        .collect())
}

/// Test helper for applying a patch inside a disposable workspace.
#[cfg(test)]
pub(crate) fn apply_patch_in_workspace(workspace: &Path, patch: &Path) -> Result<()> {
    let mut apply = git_command(workspace);
    apply
        .args(["apply", "--binary", "--whitespace=nowarn", "--"])
        .arg(patch);
    checked_output(apply, "candidate patch does not apply to the merged tree")?;
    Ok(())
}

/// Copy the files that make up the current source into `dest` for staging.
/// A Git source contributes exactly what `git ls-files -co --exclude-standard`
/// lists, a directory source its whole tree. Nested repositories that Git
/// lists as one directory entry are copied without their Git metadata.
/// A tracked file deleted from the worktree is simply absent; a file that
/// vanishes while it is copied is an error.
pub(crate) fn create_scratch_tree(source: &Path, kind: &SourceKind, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("failed to create {}", dest.display()))?;
    if matches!(kind, SourceKind::Directory) {
        return copy_tree_contents(source, dest);
    }
    let mut list = git_command(source);
    list.args(["ls-files", "-co", "--exclude-standard", "-z"]);
    let listing = checked_output(list, "failed to list the current source files")?;
    let mut real_directories = HashSet::new();
    for raw in listing.stdout.split(|byte| *byte == 0) {
        let relative = bytes_to_path(raw.strip_suffix(b"/").unwrap_or(raw));
        if raw.is_empty()
            || relative
                .components()
                .any(|part| is_excluded_name(part.as_os_str()))
        {
            continue;
        }
        let from = source.join(&relative);
        let to = dest.join(&relative);
        // Reading through a parent that became a symlink would leave the tree.
        if !parents_are_directories(source, &relative, &mut real_directories)
            || fs::symlink_metadata(&to).is_ok()
        {
            continue;
        }
        let metadata = match fs::symlink_metadata(&from) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to inspect {}", from.display()));
            }
        };
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        if metadata.is_dir() {
            fs::create_dir(&to).with_context(|| format!("failed to create {}", to.display()))?;
            copy_tree_contents(&from, &to)?;
        } else if metadata.file_type().is_symlink() {
            let link_target = fs::read_link(&from)
                .with_context(|| format!("failed to read symlink {}", from.display()))?;
            ensure_symlink_stays_within(source, &from, &link_target)?;
            create_symlink(&link_target, &to, &from)
                .with_context(|| format!("failed to copy symlink {}", from.display()))?;
        } else if metadata.is_file() {
            // `fs::copy` carries the permission bits, including execute.
            fs::copy(&from, &to).with_context(|| format!("failed to copy {}", from.display()))?;
        } else {
            bail!(
                "unsupported special file in source tree: {}",
                from.display()
            );
        }
    }
    Ok(())
}

/// The persistent, private check repository for this source path. Its Git
/// history is independent of the source, including when the source is a
/// linked worktree.
pub(crate) fn check_workspace_path(state_root: &Path, source: &Path) -> PathBuf {
    let key = hex::encode(Sha256::digest(source.to_string_lossy().as_bytes()));
    state_root.join("check-workspaces").join(key)
}

fn check_git_command(workspace: &Path, work_tree: &Path) -> Command {
    let mut command = plain_git_command();
    command
        .arg("--git-dir")
        .arg(workspace.join(".git"))
        .arg("--work-tree")
        .arg(work_tree)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ]);
    command
}

/// One patch for `stage`: the file, and the objects of the baseline it was
/// made against (a run's `baseline_path`), which hold the blobs a 3-way merge
/// starts from.
pub(crate) struct Patch<'a> {
    pub file: &'a Path,
    pub objects: PathBuf,
}

impl<'a> Patch<'a> {
    pub(crate) fn new(file: &'a Path, baseline: &Path) -> Self {
        Self {
            file,
            objects: baseline.join(".git").join("objects"),
        }
    }
}

/// What `stage` made of its patches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stage {
    /// Every patch applied: the tree, whose objects are all in the repository.
    Staged { tree: String },
    /// A patch applied neither as it is nor 3-way. Git's output says why.
    Conflict { output: String },
}

/// Apply `patches` in order onto the tree `base`, in a temporary index of the
/// repository `git` runs in, never in its own index or at its HEAD. Each patch
/// applies as it is if it can. Otherwise, with `three_way`, Git merges it 3-way
/// from the blobs it was made from, which its baseline's objects hold, and
/// from then on every command on the index sees them. Callers allow
/// `three_way` only for a world whose tree is exact (`memo::exact`).
pub(crate) fn stage(
    git: &dyn Fn() -> Command,
    base: &str,
    patches: &[Patch],
    three_way: bool,
) -> Result<Stage> {
    let scratch = Builder::new()
        .prefix("dispatch-stage-")
        .tempdir()
        .context("failed to create a temporary Git index")?;
    let index = scratch.path().join("index");
    // The objects of every baseline merged from so far.
    let mut alternates = Vec::new();
    let on_index = |alternates: &[&Path]| -> Result<Command> {
        let mut command = git();
        command.env("GIT_INDEX_FILE", &index);
        if !alternates.is_empty() {
            let joined = std::env::join_paths(alternates)
                .context("a baseline's path cannot be given to Git as an alternate")?;
            command.env("GIT_ALTERNATE_OBJECT_DIRECTORIES", joined);
        }
        Ok(command)
    };
    let mut read_tree = on_index(&alternates)?;
    read_tree.args(["read-tree", base]);
    checked_output(read_tree, "failed to read the tree to stage on")?;
    for patch in patches {
        let size = fs::metadata(patch.file)
            .with_context(|| format!("failed to inspect {}", patch.file.display()))?
            .len();
        if size == 0 {
            continue;
        }
        let mut apply = on_index(&alternates)?;
        apply
            .args(["apply", "--cached", "--binary", "--whitespace=nowarn", "--"])
            .arg(patch.file);
        let applied = run_git(apply, None, "failed to stage a patch")?;
        if applied.status.success() {
            continue;
        }
        if !three_way {
            return Ok(Stage::Conflict {
                output: git_says(&applied),
            });
        }
        alternates.push(patch.objects.as_path());
        let mut merge = on_index(&alternates)?;
        merge
            .args([
                "apply",
                "--3way",
                "--cached",
                "--binary",
                "--whitespace=nowarn",
            ])
            .arg("--")
            .arg(patch.file);
        let merged = run_git(merge, None, "failed to merge a patch 3-way")?;
        if !merged.status.success() {
            // The index keeps unmerged entries; it is thrown away.
            return Ok(Stage::Conflict {
                output: git_says(&merged),
            });
        }
    }
    let mut write_tree = on_index(&alternates)?;
    write_tree.arg("write-tree");
    let tree = checked_output(write_tree, "failed to write the staged tree")?.stdout;
    let tree = String::from_utf8_lossy(trim_ascii(&tree)).into_owned();
    if !alternates.is_empty() && tree != base {
        // Git writes no object it finds among the alternates. Copy what the
        // tree holds beyond `base` into the repository, which then reads it
        // without them; objects already there are not written again.
        let mut pack = on_index(&alternates)?;
        pack.args(["pack-objects", "--revs", "--stdout", "--quiet"]);
        let wanted = format!("{tree}\n^{base}\n");
        let packed = run_git(pack, Some(wanted.as_bytes()), "failed to pack the merge")?;
        ensure!(
            packed.status.success(),
            "failed to pack the merge: {}",
            git_says(&packed)
        );
        let mut unpack = git();
        unpack.args(["unpack-objects", "-q"]);
        let unpacked = run_git(unpack, Some(&packed.stdout), "failed to keep the merge")?;
        ensure!(
            unpacked.status.success(),
            "failed to keep the merge: {}",
            git_says(&unpacked)
        );
    }
    Ok(Stage::Staged { tree })
}

/// What Git printed, errors first.
fn git_says(output: &Output) -> String {
    let said = [&output.stderr, &output.stdout]
        .into_iter()
        .map(|bytes| String::from_utf8_lossy(trim_ascii(bytes)).into_owned())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    said.join("\n")
}

/// Stage the source's current, non-ignored world in a separate kept index,
/// stage `patches` onto it (`stage`, 3-way only for an exact source), and
/// update the private checkout from the result. The default index remains the
/// checkout's previous tree until read-tree, so unchanged files retain their
/// times and ignored build output stays in place.
pub(crate) fn prepare_check_workspace(
    source: &Path,
    kind: &SourceKind,
    workspace: &Path,
    patches: &[Patch],
) -> Result<String> {
    let parent = workspace
        .parent()
        .context("check workspace has no parent")?;
    fs::create_dir_all(parent)?;
    if let Ok(metadata) = fs::symlink_metadata(workspace) {
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "check workspace is not a directory: {}",
            workspace.display()
        );
    }
    if !workspace.join(".git").exists() {
        let mut init = plain_git_command();
        init.args(["init", "--quiet", "--"]).arg(workspace);
        checked_output(init, "failed to initialize the check workspace")?;
    }
    ensure!(
        fs::symlink_metadata(workspace.join(".git"))?.is_dir(),
        "check workspace is not an internal Git repository: {}",
        workspace.display()
    );
    for relative in [".git/info", ".git/objects"] {
        ensure!(
            fs::symlink_metadata(workspace.join(relative))?.is_dir(),
            "check workspace Git metadata is not a directory: {}",
            workspace.join(relative).display()
        );
    }
    if let Ok(metadata) = fs::symlink_metadata(workspace.join(".git/info/attributes")) {
        ensure!(
            metadata.is_file(),
            "check workspace attributes path is not a file"
        );
    }
    if let Ok(metadata) = fs::symlink_metadata(workspace.join(".git/check.index")) {
        ensure!(
            metadata.is_file(),
            "check workspace index path is not a file"
        );
    }
    // The checked tree must have the source's bytes, even if its
    // .gitattributes requests line-ending or other conversions. Also repair
    // an interrupted first setup before staging again.
    fs::write(
        workspace.join(".git/info/attributes"),
        b"* -text -filter -ident -working-tree-encoding -eol\n",
    )?;

    // Staging happens outside the checkout: neither source files nor build
    // output in the warm workspace are overwritten while constructing Δ.
    let scratch = Builder::new()
        .prefix("stage-")
        .tempdir_in(workspace.join(".git"))?;
    create_scratch_tree(source, kind, scratch.path())?;
    let index = workspace.join(".git/check.index");
    let mut empty = check_git_command(workspace, workspace);
    empty
        .env("GIT_INDEX_FILE", &index)
        .args(["read-tree", "--empty"]);
    checked_output(empty, "failed to reset the check staging index")?;
    let mut add = check_git_command(workspace, scratch.path());
    add.env("GIT_INDEX_FILE", &index)
        .args(["add", "-A", "-f", "--", "."]);
    checked_output(add, "failed to stage the current source for checks")?;
    let mut world = check_git_command(workspace, workspace);
    world.env("GIT_INDEX_FILE", &index).arg("write-tree");
    let world = checked_output(world, "failed to write the staged check tree")?.stdout;
    let world = std::str::from_utf8(trim_ascii(&world))?;
    let three_way = crate::coherence::memo::exact(source)?;
    let git = || check_git_command(workspace, workspace);
    let tree_id = match stage(&git, world, patches, three_way)? {
        Stage::Staged { tree } => tree,
        Stage::Conflict { output } => {
            bail!("candidate patch does not apply to the merged tree: {output}")
        }
    };

    // Checks may change file times without changing bytes. Refresh the
    // checkout index's stat data so read-tree leaves those files in place.
    // A check may also edit tracked files; read-tree below resets those.
    let mut refresh = check_git_command(workspace, workspace);
    refresh.args(["update-index", "--refresh"]);
    let _ = run_git(refresh, None, "failed to refresh the check workspace index")?;
    let mut checkout = check_git_command(workspace, workspace);
    checkout.args(["read-tree", "--reset", "-u", &tree_id]);
    checked_output(checkout, "failed to update the check workspace")?;
    let mut clean = check_git_command(workspace, workspace);
    clean.args(["clean", "-fd"]);
    checked_output(clean, "failed to remove check workspace leftovers")?;
    let mut checked_tree = check_git_command(workspace, workspace);
    checked_tree.arg("write-tree");
    let checked_tree = checked_output(checked_tree, "failed to verify the checked-out tree")?;
    ensure!(
        trim_ascii(&checked_tree.stdout) == tree_id.as_bytes(),
        "check workspace tree differs from the staged source and patch"
    );
    let mut modified = check_git_command(workspace, workspace);
    modified.args(["diff-files", "--name-only"]);
    ensure!(
        checked_output(modified, "failed to inspect the check workspace")?
            .stdout
            .is_empty(),
        "check workspace differs from the intended tree"
    );
    let mut leftovers = check_git_command(workspace, workspace);
    leftovers.args(["ls-files", "--others", "--exclude-standard"]);
    ensure!(
        checked_output(leftovers, "failed to inspect check workspace leftovers")?
            .stdout
            .is_empty(),
        "check workspace contains files outside the intended tree"
    );
    Ok(tree_id)
}

/// The first parent of `relative` under `root` that is a symlink, if any.
fn symlinked_parent(root: &Path, relative: &Path) -> Result<Option<PathBuf>> {
    let mut current = root.to_path_buf();
    let parent = relative.parent().unwrap_or(Path::new(""));
    for component in parent.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(Some(current)),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {}", current.display()));
            }
        }
    }
    Ok(None)
}

/// True when every parent directory of `relative` under `root` is a real
/// directory (not a symlink); verified parents are remembered in `known`.
pub(crate) fn parents_are_directories(
    root: &Path,
    relative: &Path,
    known: &mut HashSet<Vec<u8>>,
) -> bool {
    let Some(parent) = relative
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return true;
    };
    if known.contains(&path_bytes(parent)) {
        return true;
    }
    if !parents_are_directories(root, parent, known) {
        return false;
    }
    match fs::symlink_metadata(root.join(parent)) {
        Ok(metadata) if metadata.is_dir() => {
            known.insert(path_bytes(parent));
            true
        }
        _ => false,
    }
}

fn copy_tree_contents(source: &Path, destination: &Path) -> Result<()> {
    let mut directory_permissions = Vec::new();
    let mut entries = WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(include_entry);
    #[cfg_attr(not(target_os = "macos"), allow(clippy::while_let_on_iterator))]
    while let Some(entry) = entries.next() {
        let entry = entry.with_context(|| format!("failed to walk {}", source.display()))?;
        if entry.depth() == 0 {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(source)
            .expect("walked path is under source");
        let target = destination.join(relative);
        let metadata = fs::symlink_metadata(entry.path())
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;

        if metadata.is_dir() {
            match fs::symlink_metadata(&target) {
                Ok(existing) if !existing.is_dir() || existing.file_type().is_symlink() => bail!(
                    "snapshot destination unexpectedly contains {}",
                    target.display()
                ),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    #[cfg(target_os = "macos")]
                    if clone_clean_directory(entry.path(), &target) {
                        entries.skip_current_dir();
                        continue;
                    }
                    fs::create_dir(&target).with_context(|| {
                        format!("failed to create directory {}", target.display())
                    })?;
                }
                Err(error) => return Err(error.into()),
            }
            directory_permissions.push((entry.depth(), target, metadata.permissions()));
        } else if metadata.is_file() {
            ensure!(
                fs::symlink_metadata(&target).is_err(),
                "snapshot destination unexpectedly contains {}",
                target.display()
            );
            copy_file_contents(entry.path(), &target).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    entry.path().display(),
                    target.display()
                )
            })?;
            fs::set_permissions(&target, metadata.permissions()).with_context(|| {
                format!("failed to preserve permissions on {}", target.display())
            })?;
        } else if metadata.file_type().is_symlink() {
            ensure!(
                fs::symlink_metadata(&target).is_err(),
                "snapshot destination unexpectedly contains {}",
                target.display()
            );
            let link_target = fs::read_link(entry.path())
                .with_context(|| format!("failed to read symlink {}", entry.path().display()))?;
            create_symlink(&link_target, &target, entry.path())
                .with_context(|| format!("failed to copy symlink {}", entry.path().display()))?;
        } else {
            bail!(
                "unsupported special file in source tree: {}",
                entry.path().display()
            );
        }
    }

    // Apply child directory modes first so restrictive parents cannot prevent
    // the remaining metadata operations.
    directory_permissions.sort_by_key(|(depth, _, _)| std::cmp::Reverse(*depth));
    for (_, path, permissions) in directory_permissions {
        fs::set_permissions(&path, permissions)
            .with_context(|| format!("failed to preserve permissions on {}", path.display()))?;
    }
    Ok(())
}

/// Clone a directory whole when nothing under it would be filtered or refused,
/// as its listings show without opening a file. A failed clone is undone, so the
/// per-entry path starts again from a missing destination. A directory that is
/// not clean has its subdirectories walked again; each walk stops at the first
/// disqualifying entry, which keeps that cheap.
#[cfg(target_os = "macos")]
fn clone_clean_directory(source: &Path, destination: &Path) -> bool {
    let clean = WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .all(|entry| {
            entry.is_ok_and(|entry| {
                let kind = entry.file_type();
                include_entry(&entry) && (kind.is_dir() || kind.is_file() || kind.is_symlink())
            })
        });
    if !clean {
        return false;
    }
    match clonefile(source, destination) {
        Ok(()) => true,
        Err(error) => {
            tracing::debug!("cloning {} whole failed: {error}", source.display());
            remove_created_directory(destination);
            false
        }
    }
}

// From <sys/clonefile.h>; libc exposes clonefile but not its flags.
#[cfg(target_os = "macos")]
const CLONE_NOFOLLOW: u32 = 0x0001;

/// clonefile(2) by path, without following a final symlink. It never opens a
/// file, because on-access scanning intercepts every `open()`.
#[cfg(target_os = "macos")]
fn clonefile(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    // SAFETY: both paths are live, NUL-terminated strings for the duration of the call.
    if unsafe { libc::clonefile(source.as_ptr(), destination.as_ptr(), CLONE_NOFOLLOW) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Copy one file: on macOS a clone, and on any failure `std::fs::copy`.
fn copy_file_contents(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    if clonefile(source, destination).is_ok() {
        return Ok(());
    }
    fs::copy(source, destination).map(|_| ())
}

fn ensure_symlink_stays_within(root: &Path, link: &Path, target: &Path) -> Result<()> {
    use std::path::Component;

    ensure!(
        !target.is_absolute(),
        "source contains an absolute symlink that escapes candidate isolation: {} -> {}",
        link.display(),
        target.display()
    );
    let parent = link
        .parent()
        .context("source symlink has no parent")?
        .strip_prefix(root)
        .context("source symlink is outside the source root")?;
    let mut components = Vec::new();
    for component in parent.join(target).components() {
        match component {
            Component::Normal(value) => components.push(value.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(
                    components.pop().is_some(),
                    "source contains a symlink that escapes candidate isolation: {} -> {}",
                    link.display(),
                    target.display()
                );
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "source contains a symlink that escapes candidate isolation: {} -> {}",
                    link.display(),
                    target.display()
                );
            }
        }
    }
    Ok(())
}

/// Refuse a candidate whose Δ would be unsafe or too large: at most
/// `MAX_CANDIDATE_FILES` files of at most `MAX_CANDIDATE_FILE_BYTES` each and
/// `MAX_CANDIDATE_TREE_BYTES` in all, no special files, no symlink that leaves
/// the tree. Only paths that can enter Δ count: the baseline's tracked paths
/// and the workspace's untracked paths its ignore rules do not exclude, as the
/// trusted baseline repository lists them for `git add -A`. Ignored build
/// output (`target/`, `node_modules/`) can never enter Δ, so it is neither
/// counted nor refused.
fn validate_candidate_tree(baseline_path: &Path, root: &Path) -> Result<()> {
    let mut list = comparison_git_command(baseline_path, root);
    list.args([
        "ls-files",
        "--cached",
        "--others",
        "--exclude-standard",
        "-z",
        "--",
        ".",
    ])
    .args(dispatch_exclusion_pathspecs());
    let listing = checked_output(list, "failed to enumerate the candidate's changes")?;
    let mut files = 0_u64;
    let mut logical_bytes = 0_u64;
    let mut real_directories = HashSet::new();
    let mut checked_links = HashSet::new();
    for raw in listing.stdout.split(|byte| *byte == 0) {
        // An untracked nested repository is listed as `dir/`, not as files.
        if raw.is_empty() || raw.ends_with(b"/") {
            continue;
        }
        let relative = bytes_to_path(raw);
        // Under a parent that became a symlink, the link is what Git records.
        if !parents_are_directories(root, &relative, &mut real_directories) {
            if let Some(link) = symlinked_parent(root, &relative)?
                && checked_links.insert(link.clone())
            {
                let target = fs::read_link(&link)
                    .with_context(|| format!("failed to read symlink {}", link.display()))?;
                ensure_symlink_stays_within(root, &link, &target)?;
            }
            continue;
        }
        let path = root.join(&relative);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            // Deleted since the baseline: part of Δ, but nothing to inspect.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect candidate path {}", path.display())
                });
            }
        };
        if metadata.is_dir() {
            continue;
        }
        files = files.saturating_add(1);
        ensure!(
            files <= MAX_CANDIDATE_FILES,
            "candidate contains more than {MAX_CANDIDATE_FILES} files; diff collection was refused"
        );
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path)
                .with_context(|| format!("failed to read symlink {}", path.display()))?;
            ensure_symlink_stays_within(root, &path, &target)?;
        } else if metadata.is_file() {
            ensure!(
                metadata.len() <= MAX_CANDIDATE_FILE_BYTES,
                "candidate file {} is larger than the {} MiB safety limit",
                path.display(),
                MAX_CANDIDATE_FILE_BYTES / (1024 * 1024)
            );
            logical_bytes = logical_bytes.saturating_add(metadata.len());
            ensure!(
                logical_bytes <= MAX_CANDIDATE_TREE_BYTES,
                "candidate tree exceeds the {} GiB logical-size safety limit",
                MAX_CANDIDATE_TREE_BYTES / (1024 * 1024 * 1024)
            );
        } else {
            bail!(
                "candidate contains unsupported special file: {}",
                path.display()
            );
        }
    }
    Ok(())
}

/// Materialize a commit's tree as real files under `staging`, via a temporary
/// index scoped to this call (`GIT_INDEX_FILE`) checked out into `staging`
/// (`GIT_WORK_TREE`), rather than `repo`'s own working tree or index.
///
/// Chosen over `git archive --format=tar <commit> | tar -x`: it stays inside
/// this module's hardened `git_command`/`checked_output` process wrapper (one
/// external tool, the existing timeout and output-size safety net) instead of
/// piping two subprocesses together and reimplementing that safety net for a
/// second tool this codebase does not otherwise depend on. Verified
/// empirically that it preserves symlinks and the executable bit exactly
/// (mode 100755 and 120000 entries round-trip byte-for-byte), and that a
/// gitlink (mode 160000, a nested repository) is left unpopulated as a bare
/// directory rather than checked out — the same treatment `world::observe`
/// gives nested repositories.
fn export_commit_tree(repo: &Path, commit: &str, staging: &Path) -> Result<()> {
    let index_directory = Builder::new()
        .prefix("dispatch-checkout-")
        .tempdir()
        .context("failed to create a temporary Git index")?;
    let index_path = index_directory.path().join("index");

    let mut read_tree = git_command(repo);
    read_tree
        .env("GIT_INDEX_FILE", &index_path)
        .args(["read-tree", commit]);
    checked_output(
        read_tree,
        "failed to read the commit tree into a temporary index",
    )?;

    let mut checkout = git_command(repo);
    checkout
        .env("GIT_INDEX_FILE", &index_path)
        .env("GIT_WORK_TREE", staging)
        .args(["checkout-index", "--all", "--force"]);
    checked_output(checkout, "failed to materialize the commit tree")?;
    Ok(())
}

/// Initialize the private Git repository that backs a baseline (or a
/// republished contribution). When `honor_ignore` is true, staging uses the
/// source's own ignore rules (`.gitignore` files, already copied into `path`
/// by the caller, and `info/exclude`, copied here from `source`) so the
/// baseline commit tracks exactly the paths `world::observe` considers part of
/// the world; ignored files remain on disk for build caches. When false,
/// staging force-adds everything, matching a plain-directory source, which has
/// no ignore rules for its world.
fn initialize_internal_repository(path: &Path, source: &Path, honor_ignore: bool) -> Result<()> {
    let mut init = git_command(path);
    init.args([
        "-c",
        "init.defaultBranch=dispatch-baseline",
        "init",
        "--quiet",
    ]);
    checked_output(init, "failed to initialize internal baseline repository")?;

    install_internal_attributes(path)?;
    if honor_ignore {
        copy_source_info_exclude(source, path)?;
    }

    for (key, value) in [
        ("user.name", "Dispatch"),
        ("user.email", "dispatch@localhost"),
        ("commit.gpgSign", "false"),
        ("core.autocrlf", "false"),
    ] {
        let mut config = git_command(path);
        config.args(["config", "--local", key, value]);
        checked_output(config, "failed to configure internal baseline repository")?;
    }

    let mut add = git_command(path);
    if honor_ignore {
        add.args(["add", "-A", "--", "."]);
    } else {
        add.args(["add", "-A", "-f", "--", "."]);
    }
    checked_output(add, "failed to stage internal baseline")?;

    let mut commit = git_command(path);
    commit
        .env("GIT_AUTHOR_NAME", "Dispatch")
        .env("GIT_AUTHOR_EMAIL", "dispatch@localhost")
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
        .env("GIT_COMMITTER_NAME", "Dispatch")
        .env("GIT_COMMITTER_EMAIL", "dispatch@localhost")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
        .args([
            "commit",
            "--quiet",
            "--no-gpg-sign",
            "--no-verify",
            "--allow-empty",
            "-m",
            "Dispatch baseline",
        ]);
    checked_output(commit, "failed to commit internal baseline")?;
    Ok(())
}

fn configure_candidate_repository(path: &Path) -> Result<()> {
    let mut config = git_command(path);
    config.args(["config", "--local", "core.autocrlf", "false"]);
    checked_output(config, "failed to configure candidate repository")?;
    install_internal_attributes(path)
}

/// The private repositories must describe the literal local files. Higher
/// priority info attributes neutralize source/global clean filters (including
/// LFS), newline normalization, and working-tree encodings that could otherwise
/// make an internal commit differ from the bytes Dispatch froze.
fn install_internal_attributes(path: &Path) -> Result<()> {
    let mut command = git_command(path);
    command.args(["rev-parse", "--absolute-git-dir"]);
    let output = checked_output(command, "failed to locate internal Git directory")?;
    let git_dir = bytes_to_path(trim_ascii(&output.stdout));
    let info = git_dir.join("info");
    fs::create_dir_all(&info).with_context(|| format!("failed to create {}", info.display()))?;
    fs::write(
        info.join("attributes"),
        b"* -text -eol -filter -ident -working-tree-encoding\n",
    )
    .context("failed to disable content filters in internal repository")?;
    Ok(())
}

/// Copy the source repository's `info/exclude` into the internal repository's
/// `info/exclude`, so repository-local excludes are honored the same way
/// `git ls-files --exclude-standard` honors them in `world::observe`. Located
/// with `git rev-parse --git-path info/exclude` run in the source, which
/// resolves to the shared main repository even for a linked worktree. Global
/// excludes are already neutralized by `GIT_CONFIG_GLOBAL=/dev/null` in
/// `plain_git_command`, so there is nothing to do for them. A source with no
/// `info/exclude` (or none of interest) leaves the internal repository as
/// `git init` created it.
fn copy_source_info_exclude(source: &Path, internal_path: &Path) -> Result<()> {
    let mut locate = git_command(source);
    locate.args(["rev-parse", "--git-path", "info/exclude"]);
    let output = checked_output(
        locate,
        "failed to locate the source repository's info/exclude",
    )?;
    let relative = bytes_to_path(trim_ascii(&output.stdout));
    let exclude_path = if relative.is_absolute() {
        relative
    } else {
        source.join(relative)
    };
    let Ok(contents) = fs::read(&exclude_path) else {
        return Ok(());
    };

    let mut git_dir_command = git_command(internal_path);
    git_dir_command.args(["rev-parse", "--absolute-git-dir"]);
    let git_dir_output =
        checked_output(git_dir_command, "failed to locate internal Git directory")?;
    let git_dir = bytes_to_path(trim_ascii(&git_dir_output.stdout));
    let info = git_dir.join("info");
    fs::create_dir_all(&info).with_context(|| format!("failed to create {}", info.display()))?;
    fs::write(info.join("exclude"), contents)
        .context("failed to copy source info/exclude into internal repository")?;
    Ok(())
}

fn ensure_internal_repository(path: &Path) -> Result<()> {
    let mut command = git_command(path);
    command.args(["rev-parse", "--verify", "HEAD"]);
    checked_output(command, "path is not a usable baseline Git repository")?;
    Ok(())
}

fn ensure_repository_clean(path: &Path, description: &str) -> Result<()> {
    let mut status = git_command(path);
    status.args([
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--ignored=no",
    ]);
    let output = checked_output(status, &format!("failed to inspect {description}"))?;
    ensure!(
        output.stdout.is_empty(),
        "{description} has unexpected changes and cannot be used safely"
    );
    Ok(())
}

fn git_head_at(path: &Path) -> Result<Option<String>> {
    let mut command = git_command(path);
    command.args(["rev-parse", "--verify", "HEAD"]);
    let output = command.output().context("failed to query Git HEAD")?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8(output.stdout)
            .context("Git returned a non-UTF-8 commit identifier")?
            .trim()
            .to_owned(),
    ))
}

fn list_untracked_files(baseline_path: &Path, workspace: &Path) -> Result<Vec<String>> {
    let mut command = comparison_git_command(baseline_path, workspace);
    command
        .args([
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ])
        .args(dispatch_exclusion_pathspecs());
    let output = checked_output(command, "failed to enumerate untracked candidate files")?;
    let mut files = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn inspect_patch_paths(source: &Path, diff_path: &Path) -> Result<Vec<PathBuf>> {
    let mut command = git_command(source);
    command
        .args(["apply", "--numstat", "-z", "--"])
        .arg(diff_path);
    let output = checked_output(command, "candidate diff is not a valid Git patch")?;
    let (_, _, _, paths) = parse_numstat(&output.stdout)?;
    Ok(paths.into_iter().map(|path| bytes_to_path(&path)).collect())
}

fn parse_numstat(bytes: &[u8]) -> Result<(u64, u64, u64, Vec<Vec<u8>>)> {
    let mut files = 0_u64;
    let mut added = 0_u64;
    let mut removed = 0_u64;
    let mut paths = Vec::new();

    for record in bytes.split(|byte| *byte == 0).filter(|r| !r.is_empty()) {
        let mut fields = record.splitn(3, |byte| *byte == b'\t');
        let added_field = fields.next().context("invalid Git numstat output")?;
        let removed_field = fields.next().context("invalid Git numstat output")?;
        let path = fields.next().context("invalid Git numstat output")?;
        ensure!(!path.is_empty(), "invalid empty path in Git numstat output");

        files += 1;
        added += parse_numstat_number(added_field)?;
        removed += parse_numstat_number(removed_field)?;
        paths.push(path.to_vec());
    }
    Ok((files, added, removed, paths))
}

fn parse_numstat_number(value: &[u8]) -> Result<u64> {
    if value == b"-" {
        return Ok(0);
    }
    let value = std::str::from_utf8(value).context("invalid Git numstat number")?;
    value
        .parse::<u64>()
        .with_context(|| format!("invalid Git numstat number: {value}"))
}

fn ensure_safe_patch_path(path: &Path) -> Result<()> {
    ensure!(
        !path.is_absolute(),
        "candidate diff contains an absolute path"
    );
    for component in path.components() {
        match component {
            Component::Normal(value) => ensure!(
                !is_excluded_name(value),
                "candidate diff attempts to modify Dispatch or Git state: {}",
                path.display()
            ),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("candidate diff contains an unsafe path: {}", path.display())
            }
        }
    }
    Ok(())
}

fn dispatch_exclusion_pathspecs() -> [&'static str; 2] {
    [
        ":(exclude,glob).dispatch/**",
        ":(exclude,glob)**/.dispatch/**",
    ]
}

pub(crate) fn include_entry(entry: &DirEntry) -> bool {
    entry.depth() == 0 || !is_excluded_name(entry.file_name())
}

pub(crate) fn is_excluded_name(name: &OsStr) -> bool {
    EXCLUDED_DIRECTORIES
        .iter()
        .any(|excluded| name == OsStr::new(excluded))
}

fn remove_created_directory(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            let _ = fs::remove_dir_all(path);
        } else {
            let _ = fs::remove_file(path);
        }
    }
}

fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("failed to resolve current directory")?
            .join(path)
    };

    let mut cursor = absolute.as_path();
    let mut missing = Vec::<OsString>::new();
    loop {
        match fs::canonicalize(cursor) {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = cursor
                    .file_name()
                    .with_context(|| format!("could not resolve destination {}", path.display()))?;
                missing.push(name.to_os_string());
                cursor = cursor
                    .parent()
                    .with_context(|| format!("could not resolve destination {}", path.display()))?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to resolve destination {}", path.display()));
            }
        }
    }
}

pub(crate) fn plain_git_command() -> Command {
    #[cfg(target_os = "macos")]
    let git = {
        let command_line_tools = PathBuf::from("/Library/Developer/CommandLineTools/usr/bin/git");
        if command_line_tools.is_file() {
            command_line_tools
        } else {
            trusted_host_executable("git").unwrap_or_else(|_| PathBuf::from("/usr/bin/git"))
        }
    };
    #[cfg(not(target_os = "macos"))]
    let git = trusted_host_executable("git").unwrap_or_else(|_| PathBuf::from("/usr/bin/git"));
    let mut command = Command::new(git);
    command.env_clear();
    for name in ["TMPDIR", "SYSTEMROOT"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("PATH", trusted_host_path())
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        // Never take a lock Git calls optional: `git status` would otherwise
        // refresh the person's index under `index.lock` on every owner tick,
        // and their own `git commit` could fail while Dispatch watches.
        .env("GIT_OPTIONAL_LOCKS", "0");
    command
}

pub(crate) fn git_command(directory: &Path) -> Command {
    let mut command = plain_git_command();
    command.arg("-C").arg(directory);
    command
}

/// Compare a candidate worktree using only the trusted baseline's repository
/// metadata. Candidate-controlled `.git/config`, hooks, objects, and info
/// attributes must never influence a host-side Git process.
pub(crate) fn comparison_git_command(baseline_path: &Path, workspace: &Path) -> Command {
    let mut command = plain_git_command();
    command
        .arg("--git-dir")
        .arg(baseline_path.join(".git"))
        .arg("--work-tree")
        .arg(workspace)
        .arg("-c")
        .arg("core.hooksPath=/dev/null");
    command
}

pub(crate) fn checked_output(command: Command, context: &str) -> Result<Output> {
    let output = run_git(command, None, context)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            bail!("{context} (Git exited with {})", output.status);
        }
        bail!("{context}: {stderr}");
    }
    Ok(output)
}

/// Run Git under the same timeout and output limits as `checked_output`, but
/// leave the exit status to the caller and optionally feed standard input.
pub(crate) fn run_git(command: Command, input: Option<&[u8]>, context: &str) -> Result<Output> {
    run_git_within(command, input, context, GIT_COMMAND_TIMEOUT)
}

/// `run_git` with its safety timeout given. A thread waits for the child, so
/// the caller learns of its exit at once rather than at the next poll.
fn run_git_within(
    mut command: Command,
    input: Option<&[u8]>,
    context: &str,
    timeout: Duration,
) -> Result<Output> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().with_context(|| context.to_owned())?;
    let child_id = child.id();
    let stdin_writer = match input {
        Some(bytes) => {
            let mut stdin = child.stdin.take().context("Git stdin was not captured")?;
            let bytes = bytes.to_vec();
            // A write error means Git exited early; its exit status reports why.
            Some(thread::spawn(move || stdin.write_all(&bytes)))
        }
        None => None,
    };
    let stdout = child.stdout.take().context("Git stdout was not captured")?;
    let stderr = child.stderr.take().context("Git stderr was not captured")?;
    let stdout_reader = thread::spawn(move || read_limited(stdout));
    let stderr_reader = thread::spawn(move || read_limited(stderr));
    let (exited, exit) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let _ = exited.send(child.wait());
    });
    let status = match exit.recv_timeout(timeout) {
        Ok(status) => status.with_context(|| context.to_owned())?,
        Err(_) => {
            // The child leads its own group, so this kills it too and the
            // waiter returns. Elsewhere it is left to exit by itself.
            kill_command_group(child_id);
            #[cfg(unix)]
            {
                let _ = waiter.join();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
            }
            bail!(
                "{context}: Git exceeded the {} second safety timeout",
                timeout.as_secs()
            );
        }
    };
    let _ = waiter.join();
    kill_command_group(child_id);
    if let Some(writer) = stdin_writer {
        let _ = writer.join();
    }
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("{context}: stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("{context}: stderr reader panicked"))??;
    ensure!(
        stdout.total <= MAX_GIT_OUTPUT_BYTES as u64,
        "{context}: Git output exceeded the {} MiB safety limit",
        MAX_GIT_OUTPUT_BYTES / (1024 * 1024)
    );
    ensure!(
        stderr.total <= MAX_GIT_OUTPUT_BYTES as u64,
        "{context}: Git error output exceeded the {} MiB safety limit",
        MAX_GIT_OUTPUT_BYTES / (1024 * 1024)
    );
    Ok(Output {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
    })
}

struct LimitedOutput {
    bytes: Vec<u8>,
    total: u64,
}

fn read_limited(mut reader: impl Read) -> std::io::Result<LimitedOutput> {
    let mut bytes = Vec::with_capacity(MAX_GIT_OUTPUT_BYTES.min(64 * 1024));
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        let remaining = MAX_GIT_OUTPUT_BYTES.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(LimitedOutput { bytes, total })
}

fn kill_command_group(child_id: u32) {
    #[cfg(unix)]
    if let Ok(pgid) = i32::try_from(child_id) {
        // SAFETY: the command was placed in its own process group immediately
        // before spawn; a negative PID targets that group only.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

pub(crate) fn hash_sized_bytes(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

#[cfg(unix)]
fn hash_mode(hasher: &mut Sha256, metadata: &Metadata) {
    use std::os::unix::fs::MetadataExt;
    hasher.update((metadata.mode() & 0o7777).to_le_bytes());
}

#[cfg(not(unix))]
fn hash_mode(hasher: &mut Sha256, metadata: &Metadata) {
    hasher.update([u8::from(metadata.permissions().readonly())]);
}

#[cfg(unix)]
pub(crate) fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
pub(crate) fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(unix)]
pub(crate) fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

#[cfg(windows)]
pub(crate) fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(unix)]
fn create_symlink(target: &Path, destination: &Path, _source_link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, destination).map_err(Into::into)
}

#[cfg(windows)]
fn create_symlink(target: &Path, destination: &Path, source_link: &Path) -> Result<()> {
    let resolved_target = source_link
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(target);
    if resolved_target.is_dir() {
        std::os::windows::fs::symlink_dir(target, destination).map_err(Into::into)
    } else {
        std::os::windows::fs::symlink_file(target, destination).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, time::Instant};

    use chrono::Utc;
    use tempfile::TempDir;

    use super::*;
    use crate::{CandidateRecord, CandidateStatus, EnvironmentRecord, RunStatus};

    fn write(path: &Path, contents: impl AsRef<[u8]>) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn run_git(path: &Path, args: &[&str]) -> String {
        let output = plain_git_command()
            .arg("-C")
            .arg(path)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// The run's patch applied through the fingerprint fence, as accept
    /// applies onto a source unchanged since snapshotting.
    fn apply_unmoved(run: &RunRecord) -> Result<Vec<PathBuf>> {
        apply_checked(
            run,
            &run.candidates[0].diff_path,
            ExpectedWorld::Fingerprint,
        )
    }

    fn initialize_user_repository(path: &Path) {
        run_git(path, &["init", "--quiet"]);
        run_git(path, &["config", "user.name", "Test User"]);
        run_git(path, &["config", "user.email", "test@example.invalid"]);
        run_git(path, &["add", "-A"]);
        run_git(path, &["commit", "--quiet", "-m", "initial"]);
    }

    fn candidate_record(
        label: &str,
        workspace: PathBuf,
        diff: PathBuf,
        stats: DiffStats,
    ) -> CandidateRecord {
        CandidateRecord {
            id: label.to_lowercase(),
            label: label.to_owned(),
            harness_id: "fake".into(),
            harness_version: None,
            model: None,
            status: CandidateStatus::Completed,
            workspace_path: workspace.clone(),
            prompt_path: workspace.join("prompt.txt"),
            stdout_path: workspace.join("stdout.log"),
            stderr_path: workspace.join("stderr.log"),
            diff_path: diff,
            duration_ms: 1,
            exit_code: Some(0),
            timed_out: false,
            tokens: None,
            token_semantics: None,
            cost_usd: None,
            error: None,
            diff_stats: stats,
            checks: Vec::new(),
        }
    }

    fn run_record(
        source: &Path,
        snapshot: &SourceSnapshot,
        candidate: CandidateRecord,
    ) -> RunRecord {
        RunRecord {
            execution: None,
            id: "test-run".into(),
            task: "test task".into(),
            exact_prompt: "test prompt".into(),
            source_path: source.to_path_buf(),
            source_kind: snapshot.kind.clone(),
            source_git_head: snapshot.git_head.clone(),
            source_fingerprint: snapshot.fingerprint.clone(),
            baseline_path: snapshot.baseline_path.clone(),
            baseline_commit: snapshot.baseline_commit.clone(),
            baseline_history: Vec::new(),
            status: RunStatus::Evaluated,
            mode: crate::RunMode::Native,
            state_revision: 0,
            outcome: crate::RunOutcome::default(),
            created_at: Utc::now(),
            completed_at: None,
            environment: EnvironmentRecord {
                dispatch_version: "test".into(),
                os: "test".into(),
                architecture: "test".into(),
                execution_backend: "local".into(),
                timeout_secs: 1,
                cpus: 1.0,
                memory: "1g".into(),
                max_parallel: 1,
                docker_image: None,
                resource_limits_enforced: false,
                unsafe_local: false,
                forwarded_env: Vec::new(),
            },
            baseline_checks: Vec::new(),
            candidates: vec![candidate],
            attempts: Vec::new(),
            allocation: None,
            coherence: None,
            applied_candidate: None,
            attachment: None,
            historical: Default::default(),
        }
    }

    #[test]
    fn resolves_and_validates_source_directories() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        assert_eq!(
            resolve_source(Some(&source)).unwrap(),
            fs::canonicalize(&source).unwrap()
        );

        let file = temp.path().join("file");
        write(&file, "not a directory");
        assert!(resolve_source(Some(&file)).is_err());
    }

    #[test]
    fn identifies_plain_git_and_linked_worktree_sources() {
        let temp = TempDir::new().unwrap();
        let plain = temp.path().join("plain");
        fs::create_dir(&plain).unwrap();
        write(&plain.join("file.txt"), "plain\n");
        assert_eq!(
            inspect_source(&plain).unwrap(),
            (SourceKind::Directory, None)
        );

        let repository = temp.path().join("repository");
        fs::create_dir(&repository).unwrap();
        write(&repository.join("file.txt"), "git\n");
        initialize_user_repository(&repository);
        let expected_head = run_git(&repository, &["rev-parse", "HEAD"]);
        assert_eq!(
            inspect_source(&repository).unwrap(),
            (SourceKind::Git, Some(expected_head))
        );

        let worktree = temp.path().join("worktree");
        run_git(
            &repository,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "dispatch-source-test",
                worktree.to_str().unwrap(),
            ],
        );
        assert_eq!(
            inspect_source(&worktree).unwrap().0,
            SourceKind::GitWorktree
        );
    }

    #[test]
    fn a_git_failure_other_than_no_repository_is_an_error_not_a_plain_directory() {
        let temp = TempDir::new().unwrap();
        let repository = temp.path().join("repository");
        fs::create_dir(&repository).unwrap();
        write(&repository.join("file.txt"), "git\n");
        initialize_user_repository(&repository);
        write(&repository.join(".git/config"), "this is [not a config\n");
        let error = inspect_source(&repository).unwrap_err();
        assert!(format!("{error:#}").contains("bad config"), "{error:#}");
        assert!(repo_identity(&repository).is_err());

        // Git's own "not a git repository" still means a plain directory.
        let plain = temp.path().join("plain");
        write(&plain.join(".git/private"), "not a repository");
        assert_eq!(
            inspect_source(&plain).unwrap(),
            (SourceKind::Directory, None)
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_git_reports_exit_and_output_and_its_timeout_still_fires() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "cat; echo said >&2; exit 3"]);
        let output = super::run_git(command, Some(b"fed"), "the stand-in").unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stdout, b"fed");
        assert_eq!(output.stderr, b"said\n");

        // A child that leaves a grandchild holding its output is killed with
        // its group, so the timeout returns rather than waiting on the pipe.
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 60 & sleep 60"]);
        let started = Instant::now();
        let error =
            run_git_within(command, None, "the stand-in", Duration::from_millis(300)).unwrap_err();
        assert!(format!("{error:#}").contains("safety timeout"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(30));
    }

    #[cfg(unix)]
    fn copy_fixture(source: &Path) {
        use std::os::unix::fs::{PermissionsExt, symlink};

        write(&source.join("large"), vec![0x5a; 8 * 1024 * 1024]);
        write(&source.join("executable"), "#!/bin/sh\nexit 0\n");
        fs::set_permissions(source.join("executable"), fs::Permissions::from_mode(0o751)).unwrap();
        write(&source.join("read-only"), "read only\n");
        fs::set_permissions(source.join("read-only"), fs::Permissions::from_mode(0o444)).unwrap();
        fs::create_dir(source.join("empty")).unwrap();
        fs::set_permissions(source.join("empty"), fs::Permissions::from_mode(0o550)).unwrap();
        symlink("large", source.join("link")).unwrap();
    }

    #[cfg(unix)]
    fn assert_copy_fixture(source: &Path, destination: &Path) {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        assert_eq!(
            fingerprint_tree(source).unwrap(),
            fingerprint_tree(destination).unwrap()
        );
        for name in ["large", "executable", "read-only", "empty"] {
            let original = fs::metadata(source.join(name)).unwrap();
            let copied = fs::metadata(destination.join(name)).unwrap();
            assert_eq!(original.permissions().mode(), copied.permissions().mode());
            assert_ne!(original.ino(), copied.ino());
            if original.is_file() {
                assert_eq!(
                    fs::read(source.join(name)).unwrap(),
                    fs::read(destination.join(name)).unwrap()
                );
            }
        }
        assert!(
            fs::symlink_metadata(destination.join("link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_link(destination.join("link")).unwrap(),
            Path::new("large")
        );
        assert_eq!(fs::read_dir(destination.join("empty")).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_preserves_contents_modes_links_and_empty_directories() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        copy_fixture(&source);
        copy_tree_contents(&source, &destination).unwrap();
        assert_copy_fixture(&source, &destination);
        write(&destination.join("large"), "private edit");
        assert_eq!(
            fs::metadata(source.join("large")).unwrap().len(),
            8 * 1024 * 1024
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn copy_file_clones_without_opening_files() {
        const CHILD: &str = "DISPATCH_TEST_EXHAUST_DESCRIPTORS";
        if std::env::var_os(CHILD).is_none() {
            // Descriptor limits are process-wide: never exhaust the test runner's.
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "source::tests::copy_file_clones_without_opening_files",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            // A renamed test would run nothing in the child and still exit 0.
            assert!(
                output.status.success() && stdout.contains("test result: ok. 1 passed;"),
                "{stdout}"
            );
            return;
        }
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        write(&source, "clone without open\n");
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: the pointer addresses an initialized rlimit. Only this isolated
        // subprocess changes its soft descriptor limit, leaving the hard limit intact.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        let reduced = libc::rlimit {
            rlim_cur: limit.rlim_cur.min(64),
            rlim_max: limit.rlim_max,
        };
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &reduced) }, 0);
        let mut files = Vec::new();
        loop {
            match File::open("/dev/null") {
                Ok(file) => files.push(file),
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                    break;
                }
            }
        }
        let result = copy_file_contents(&source, &destination);
        drop(files);
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        result.unwrap();
        assert_eq!(fs::read(&source).unwrap(), fs::read(&destination).unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn copy_file_falls_back_after_real_clone_failure() {
        use std::{
            ffi::CString,
            os::unix::{ffi::OsStrExt, fs::PermissionsExt},
        };

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        copy_fixture(&source);
        copy_tree_contents(&source, &destination).unwrap();
        for name in ["large", "executable", "read-only"] {
            let from = source.join(name);
            let to = destination.join(name);
            fs::set_permissions(&to, fs::Permissions::from_mode(0o600)).unwrap();
            write(&to, "stale destination");
            let from_c = CString::new(from.as_os_str().as_bytes()).unwrap();
            let to_c = CString::new(to.as_os_str().as_bytes()).unwrap();
            // SAFETY: both C strings remain live through the call.
            assert_eq!(
                unsafe { libc::clonefile(from_c.as_ptr(), to_c.as_ptr(), CLONE_NOFOLLOW) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EEXIST)
            );
            copy_file_contents(&from, &to).unwrap();
            fs::set_permissions(&to, fs::metadata(&from).unwrap().permissions()).unwrap();
        }
        assert_copy_fixture(&source, &destination);
        // Existing files must still be refused by the tree walker, not overwritten.
        assert!(copy_tree_contents(&source, &destination).is_err());
    }

    /// Restores write access under a temporary directory on drop, so it can be
    /// removed even when a test fails with read-only directories in it.
    #[cfg(unix)]
    struct Writable<'a>(&'a Path);

    #[cfg(unix)]
    impl Drop for Writable<'_> {
        fn drop(&mut self) {
            fn make_writable(path: &Path) {
                use std::os::unix::fs::PermissionsExt;

                if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
                    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
                    for entry in fs::read_dir(path).into_iter().flatten().flatten() {
                        make_writable(&entry.path());
                    }
                }
            }
            make_writable(self.0);
        }
    }

    #[cfg(target_os = "macos")]
    const CLONE_MARK: &std::ffi::CStr = c"com.dispatch.test.clone-mark";

    /// Marks a directory: a whole clone keeps its extended attributes, while a
    /// directory the per-file path creates has none.
    #[cfg(target_os = "macos")]
    fn mark(directory: &Path) {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};

        let path = CString::new(directory.as_os_str().as_bytes()).unwrap();
        // SAFETY: the path, name and value are live for the call.
        let result = unsafe {
            libc::setxattr(
                path.as_ptr(),
                CLONE_MARK.as_ptr(),
                b"1".as_ptr().cast(),
                1,
                0,
                0,
            )
        };
        assert_eq!(result, 0, "{}", directory.display());
    }

    #[cfg(target_os = "macos")]
    fn cloned_whole(directory: &Path) -> bool {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};

        let path = CString::new(directory.as_os_str().as_bytes()).unwrap();
        // SAFETY: the path and name are live; a null buffer asks only for the size.
        unsafe {
            libc::getxattr(
                path.as_ptr(),
                CLONE_MARK.as_ptr(),
                std::ptr::null_mut(),
                0,
                0,
                0,
            ) == 1
        }
    }

    /// `copy_fixture` plus an absolute symlink, nested directories and a
    /// read-only directory with a file in it, its directories marked on macOS.
    #[cfg(unix)]
    fn clean_fixture(root: &Path) {
        use std::os::unix::fs::{PermissionsExt, symlink};

        fs::create_dir_all(root).unwrap();
        copy_fixture(root);
        symlink("/nonexistent/absolute", root.join("absolute")).unwrap();
        write(&root.join("nested/deep/file"), "deep\n");
        write(&root.join("locked/file"), "locked\n");
        #[cfg(target_os = "macos")]
        for directory in ["", "nested", "nested/deep", "locked"] {
            mark(&root.join(directory));
        }
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o555)).unwrap();
    }

    /// What `copy_tree_contents` must reproduce: every entry it copies, with
    /// its type and mode, and its contents or link target.
    #[cfg(unix)]
    fn copied_entries(root: &Path) -> Vec<(PathBuf, u32, Vec<u8>)> {
        use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};

        WalkDir::new(root)
            .min_depth(1)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(include_entry)
            .map(|entry| {
                let entry = entry.unwrap();
                let metadata = fs::symlink_metadata(entry.path()).unwrap();
                let contents = if metadata.is_file() {
                    fs::read(entry.path()).unwrap()
                } else if metadata.file_type().is_symlink() {
                    fs::read_link(entry.path())
                        .unwrap()
                        .as_os_str()
                        .as_bytes()
                        .to_vec()
                } else {
                    Vec::new()
                };
                let relative = entry.path().strip_prefix(root).unwrap().to_path_buf();
                (relative, metadata.mode(), contents)
            })
            .collect()
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn copy_tree_clones_a_clean_directory_whole_and_exactly() {
        let temp = TempDir::new().unwrap();
        let _writable = Writable(temp.path());
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        write(&source.join(".git/HEAD"), "ref: refs/heads/main\n");
        clean_fixture(&source.join("target"));
        fs::create_dir(&destination).unwrap();

        copy_tree_contents(&source, &destination).unwrap();

        assert_eq!(copied_entries(&source), copied_entries(&destination));
        assert!(fs::symlink_metadata(destination.join(".git")).is_err());
        for directory in ["target", "target/nested/deep", "target/locked"] {
            assert!(cloned_whole(&destination.join(directory)), "{directory}");
        }
        write(&destination.join("target/large"), "private edit");
        assert_eq!(
            fs::metadata(source.join("target/large")).unwrap().len(),
            8 * 1024 * 1024
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_never_copies_nested_git_or_state_but_copies_their_siblings() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let _writable = Writable(temp.path());
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        write(&source.join("vendor/.git/HEAD"), "ref: refs/heads/main\n");
        write(&source.join("vendor/lib.rs"), "pub fn f() {}\n");
        write(&source.join("a/b/c/.dispatch/state.json"), "{}\n");
        write(&source.join("a/b/c/kept"), "kept\n");
        // A linked worktree's or a submodule's `.git` is a file.
        write(&source.join("a/b/.git"), "gitdir: /elsewhere\n");
        write(&source.join("a/sibling"), "sibling\n");
        fs::create_dir(source.join("links")).unwrap();
        symlink("/elsewhere", source.join("links/.dispatch")).unwrap();
        write(&source.join("links/kept"), "kept\n");
        clean_fixture(&source.join("a/b/clean"));
        fs::create_dir(&destination).unwrap();

        copy_tree_contents(&source, &destination).unwrap();

        for excluded in [
            "vendor/.git",
            "a/b/c/.dispatch",
            "a/b/.git",
            "links/.dispatch",
        ] {
            assert!(
                fs::symlink_metadata(destination.join(excluded)).is_err(),
                "{excluded}"
            );
        }
        assert_eq!(copied_entries(&source), copied_entries(&destination));
        #[cfg(target_os = "macos")]
        assert!(
            cloned_whole(&destination.join("a/b/clean")),
            "a clean directory beside nested state is still cloned whole"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_refuses_a_special_file_at_any_depth() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};

        for fifo in ["fifo", "target/fifo", "target/nested/deep/fifo"] {
            let temp = TempDir::new().unwrap();
            let _writable = Writable(temp.path());
            let source = temp.path().join("source");
            let destination = temp.path().join("destination");
            clean_fixture(&source.join("target"));
            let path = CString::new(source.join(fifo).as_os_str().as_bytes()).unwrap();
            // SAFETY: the path is a live, NUL-terminated string for the call.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o644) }, 0);
            fs::create_dir(&destination).unwrap();

            let error = copy_tree_contents(&source, &destination).unwrap_err();
            assert!(
                format!("{error:#}").contains("unsupported special file"),
                "{fifo}: {error:#}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    const SANDBOXED_COPY: [&str; 2] = [
        "DISPATCH_TEST_SANDBOXED_SOURCE",
        "DISPATCH_TEST_SANDBOXED_DESTINATION",
    ];

    /// Runs `copy_tree_contents` in a child of this test binary under a
    /// sandbox profile, so the kernel refuses what the profile denies. The
    /// child is the calling test again, which copies in `sandboxed_copy`.
    #[cfg(target_os = "macos")]
    fn copy_in_sandbox(test: &str, profile: &str, source: &Path, destination: &Path) {
        let output = Command::new("/usr/bin/sandbox-exec")
            .args(["-p", profile])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", test])
            .env(SANDBOXED_COPY[0], source)
            .env(SANDBOXED_COPY[1], destination)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        // A renamed test would run nothing in the child and still exit 0.
        assert!(
            output.status.success() && stdout.contains("test result: ok. 1 passed;"),
            "{stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// In `copy_in_sandbox`'s child, copies and returns true.
    #[cfg(target_os = "macos")]
    fn sandboxed_copy() -> bool {
        let [Some(source), Some(destination)] = SANDBOXED_COPY.map(std::env::var_os) else {
            return false;
        };
        copy_tree_contents(Path::new(&source), Path::new(&destination)).unwrap();
        true
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn copy_tree_clones_whole_without_opening_a_file() {
        if sandboxed_copy() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let _writable = Writable(temp.path());
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        write(&source.join(".git/HEAD"), "ref: refs/heads/main\n");
        clean_fixture(&source.join("target"));
        fs::create_dir(&destination).unwrap();
        // Opening a regular file under the source is refused; cloning a
        // directory is not.
        let profile = format!(
            "(version 1)(allow default)(deny file-read-data (require-all \
             (subpath \"{}\") (vnode-type REGULAR-FILE)))",
            fs::canonicalize(&source).unwrap().display()
        );

        copy_in_sandbox(
            "source::tests::copy_tree_clones_whole_without_opening_a_file",
            &profile,
            &source,
            &destination,
        );

        assert_eq!(copied_entries(&source), copied_entries(&destination));
        assert!(cloned_whole(&destination.join("target")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn copy_tree_falls_back_to_an_identical_copy_when_a_directory_clone_fails() {
        if sandboxed_copy() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let _writable = Writable(temp.path());
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        clean_fixture(&source.join("target"));
        fs::create_dir(&destination).unwrap();

        // clonefile(2) of a directory fails with EPERM; of a file, it works.
        copy_in_sandbox(
            "source::tests::copy_tree_falls_back_to_an_identical_copy_when_a_directory_clone_fails",
            "(version 1)(allow default)(deny file-clone (vnode-type DIRECTORY))",
            &source,
            &destination,
        );

        assert_eq!(copied_entries(&source), copied_entries(&destination));
        assert!(!cloned_whole(&destination.join("target")));
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_still_fails_on_an_unreadable_directory() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let _writable = Writable(temp.path());
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        clean_fixture(&source.join("target"));
        write(&source.join("target/nested/hidden/file"), "hidden\n");
        fs::set_permissions(
            source.join("target/nested/hidden"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        fs::create_dir(&destination).unwrap();

        assert!(copy_tree_contents(&source, &destination).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_treats_existing_destination_entries_as_before() {
        use std::os::unix::fs::symlink;

        for case in [
            "a file where a directory goes",
            "a symlink where a directory goes",
            "a file already there",
        ] {
            let temp = TempDir::new().unwrap();
            let _writable = Writable(temp.path());
            let source = temp.path().join("source");
            let destination = temp.path().join("destination");
            let outside = temp.path().join("outside");
            clean_fixture(&source.join("target"));
            fs::create_dir(&destination).unwrap();
            fs::create_dir(&outside).unwrap();
            match case {
                "a file where a directory goes" => {
                    write(&destination.join("target"), "theirs\n");
                }
                "a symlink where a directory goes" => {
                    symlink(&outside, destination.join("target")).unwrap();
                }
                _ => write(&destination.join("target/nested/deep/file"), "theirs\n"),
            }
            let before = copied_entries(&destination);

            let error = copy_tree_contents(&source, &destination).unwrap_err();

            assert!(
                format!("{error:#}").contains("snapshot destination unexpectedly contains"),
                "{case}: {error:#}"
            );
            assert_eq!(fs::read_dir(&outside).unwrap().count(), 0, "{case}");
            let after = copied_entries(&destination);
            for entry in &before {
                assert!(after.contains(entry), "{case}: {entry:?} changed");
            }
        }

        // A directory already there is filled in, and what else it holds is kept.
        let temp = TempDir::new().unwrap();
        let _writable = Writable(temp.path());
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        clean_fixture(&source.join("target"));
        write(&destination.join("target/nested/extra"), "theirs\n");

        copy_tree_contents(&source, &destination).unwrap();

        fs::remove_file(destination.join("target/nested/extra")).unwrap();
        assert_eq!(copied_entries(&source), copied_entries(&destination));
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_never_clones_the_root() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp = TempDir::new().unwrap();
        let _writable = Writable(temp.path());
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        // Nothing under this root is filtered, so only the rule keeps it whole.
        clean_fixture(&source);
        fs::create_dir(&destination).unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o700)).unwrap();
        let before = fs::metadata(&destination).unwrap();

        copy_tree_contents(&source, &destination).unwrap();

        let after = fs::metadata(&destination).unwrap();
        assert_eq!(before.ino(), after.ino());
        assert_eq!(after.mode() & 0o7777, 0o700);
        assert_eq!(copied_entries(&source), copied_entries(&destination));

        // A destination that is missing is still an error, never a clone.
        let missing = temp.path().join("missing");
        assert!(copy_tree_contents(&source, &missing).is_err());
        assert!(fs::symlink_metadata(&missing).is_err());
    }

    #[test]
    fn snapshot_is_exact_private_and_excludes_state() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let run = temp.path().join("run");
        fs::create_dir(&source).unwrap();
        write(&source.join("src/main.rs"), "fn main() {}\n");
        write(&source.join("empty/.keep"), "");
        fs::create_dir_all(source.join("actually-empty")).unwrap();
        write(&source.join(".git/private"), "do not copy");
        write(&source.join(".dispatch/state"), "do not copy");

        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let script = source.join("script.sh");
            write(&script, "#!/bin/sh\n");
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            symlink("src/main.rs", source.join("main-link")).unwrap();
        }

        let original_fingerprint = fingerprint_tree(&source).unwrap();
        let snapshot = create_snapshot(&source, &run).unwrap();
        assert_eq!(snapshot.kind, SourceKind::Directory);
        assert_eq!(snapshot.fingerprint, original_fingerprint);
        assert_eq!(fingerprint_tree(&source).unwrap(), original_fingerprint);
        assert_eq!(
            // This source was a plain directory, including empty directories;
            // its private snapshot is now Git, whose fingerprint covers files.
            hash_tree(
                &snapshot.baseline_path,
                list_tree(&snapshot.baseline_path).unwrap()
            )
            .unwrap(),
            original_fingerprint
        );
        assert!(!snapshot.baseline_path.join(".git/private").exists());
        assert!(!snapshot.baseline_path.join(".dispatch").exists());
        assert!(snapshot.baseline_path.join(".git").is_dir());
        assert!(snapshot.baseline_path.join("actually-empty").is_dir());
        assert_eq!(
            run_git(&snapshot.baseline_path, &["status", "--porcelain"]),
            ""
        );
        assert_eq!(snapshot.baseline_commit.len(), 40);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(snapshot.baseline_path.join("script.sh"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o111,
                0o111
            );
            assert_eq!(
                fs::read_link(snapshot.baseline_path.join("main-link")).unwrap(),
                Path::new("src/main.rs")
            );
        }
    }

    #[test]
    fn rejects_a_run_directory_nested_in_the_source() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("file.txt"), "source\n");
        let nested_run = source.join("state/runs/test");

        let error = create_snapshot(&source, &nested_run)
            .unwrap_err()
            .to_string();
        assert!(error.contains("outside the source tree"));
        assert!(!nested_run.exists());
    }

    #[test]
    fn snapshot_of_git_source_uses_dirty_worktree_without_touching_original() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("tracked.txt"), "committed\n");
        initialize_user_repository(&source);
        let head = run_git(&source, &["rev-parse", "HEAD"]);
        write(&source.join("tracked.txt"), "dirty baseline\n");
        write(&source.join("untracked.txt"), "also baseline\n");
        let status_before = run_git(&source, &["status", "--porcelain"]);

        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        assert_eq!(snapshot.kind, SourceKind::Git);
        assert_eq!(snapshot.git_head.as_deref(), Some(head.as_str()));
        assert_eq!(
            fs::read_to_string(snapshot.baseline_path.join("tracked.txt")).unwrap(),
            "dirty baseline\n"
        );
        assert_eq!(
            fs::read_to_string(snapshot.baseline_path.join("untracked.txt")).unwrap(),
            "also baseline\n"
        );
        assert_eq!(run_git(&source, &["status", "--porcelain"]), status_before);
    }

    #[test]
    fn internal_git_keeps_literal_bytes_despite_source_attributes() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join(".gitattributes"), "*.txt text\n");
        write(&source.join("line-endings.txt"), b"one\r\ntwo\r\n");

        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        assert_eq!(
            fs::read(snapshot.baseline_path.join("line-endings.txt")).unwrap(),
            b"one\r\ntwo\r\n"
        );
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
        assert_eq!(
            fs::read(workspace.join("line-endings.txt")).unwrap(),
            b"one\r\ntwo\r\n"
        );

        write(&workspace.join("line-endings.txt"), b"one\r\nchanged\r\n");
        let diff_path = temp.path().join("candidate.patch");
        let stats = collect_diff(&snapshot.baseline_path, &workspace, &diff_path).unwrap();
        let run = run_record(
            &source,
            &snapshot,
            candidate_record("A", workspace, diff_path, stats),
        );
        apply_unmoved(&run).unwrap();
        assert_eq!(
            fs::read(source.join("line-endings.txt")).unwrap(),
            b"one\r\nchanged\r\n"
        );
    }

    #[test]
    fn diff_collection_ignores_candidate_controlled_git_configuration() {
        use std::io::Write as _;

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("tracked.txt"), "baseline\n");
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();

        let sentinel = temp.path().join("candidate-config-executed");
        let mut config = fs::OpenOptions::new()
            .append(true)
            .open(workspace.join(".git/config"))
            .unwrap();
        writeln!(
            config,
            "[filter \"dispatch-owned\"]\n\tclean = touch {}\n\tsmudge = cat\n\trequired = true",
            sentinel.display()
        )
        .unwrap();
        fs::write(
            workspace.join(".git/info/attributes"),
            "*.txt filter=dispatch-owned\n",
        )
        .unwrap();
        write(&workspace.join("tracked.txt"), "candidate\n");

        let diff_path = temp.path().join("candidate.patch");
        let stats = collect_diff(&snapshot.baseline_path, &workspace, &diff_path).unwrap();
        assert_eq!(stats.files_changed, 1);
        assert!(!sentinel.exists());
        assert!(
            fs::read_to_string(diff_path)
                .unwrap()
                .contains("+candidate")
        );
    }

    #[test]
    fn candidate_and_diff_include_untracked_text_binary_and_mode_changes() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("tracked.txt"), "one\ntwo\n");
        write(&source.join(".gitignore"), "ignored.log\n");
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let candidate = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &candidate).unwrap();

        write(&candidate.join("tracked.txt"), "one\nchanged\nthree\n");
        write(&candidate.join("new.txt"), "new line\n");
        write(&candidate.join("image.bin"), [0_u8, 1, 2, 0, 255]);
        write(&candidate.join("ignored.log"), "build output\n");
        write(&candidate.join(".dispatch/internal"), "state\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let executable = candidate.join("new.txt");
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
            symlink("new.txt", candidate.join("new-link")).unwrap();
        }

        let diff_path = temp.path().join("candidate.patch");
        let stats = collect_diff(&snapshot.baseline_path, &candidate, &diff_path).unwrap();
        let expected_files = if cfg!(unix) { 4 } else { 3 };
        assert_eq!(stats.files_changed, expected_files);
        assert!(stats.lines_added >= 3);
        assert!(stats.lines_removed >= 1);
        assert_eq!(
            stats.untracked_files,
            if cfg!(unix) {
                vec!["image.bin", "new-link", "new.txt"]
            } else {
                vec!["image.bin", "new.txt"]
            }
        );
        let patch = fs::read(&diff_path).unwrap();
        assert!(
            patch
                .windows(b"GIT binary patch".len())
                .any(|w| w == b"GIT binary patch")
        );
        assert!(!String::from_utf8_lossy(&patch).contains("ignored.log"));
        assert!(!String::from_utf8_lossy(&patch).contains(".dispatch"));
        assert_eq!(fingerprint_tree(&source).unwrap(), snapshot.fingerprint);
        assert_eq!(
            run_git(&snapshot.baseline_path, &["status", "--porcelain"]),
            ""
        );
    }

    #[test]
    fn safe_apply_updates_a_plain_source_only_when_it_has_not_drifted() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("old.txt"), "before\n");
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
        write(&workspace.join("old.txt"), "after\n");
        write(&workspace.join("new.txt"), "created\n");
        let diff_path = temp.path().join("candidate.patch");
        let stats = collect_diff(&snapshot.baseline_path, &workspace, &diff_path).unwrap();
        let run = run_record(
            &source,
            &snapshot,
            candidate_record("A", workspace, diff_path, stats),
        );

        assert_eq!(apply_unmoved(&run).unwrap().len(), 2);
        assert_eq!(
            fs::read_to_string(source.join("old.txt")).unwrap(),
            "after\n"
        );
        assert_eq!(
            fs::read_to_string(source.join("new.txt")).unwrap(),
            "created\n"
        );
    }

    #[test]
    fn safe_apply_rejects_source_drift_without_partial_changes() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("one.txt"), "baseline one\n");
        write(&source.join("two.txt"), "baseline two\n");
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
        write(&workspace.join("one.txt"), "candidate one\n");
        write(&workspace.join("two.txt"), "candidate two\n");
        let diff_path = temp.path().join("candidate.patch");
        let stats = collect_diff(&snapshot.baseline_path, &workspace, &diff_path).unwrap();
        let run = run_record(
            &source,
            &snapshot,
            candidate_record("B", workspace, diff_path, stats),
        );

        write(&source.join("two.txt"), "user edit\n");
        let error = apply_unmoved(&run).unwrap_err().to_string();
        assert!(error.contains("source has changed"));
        assert_eq!(
            fs::read_to_string(source.join("one.txt")).unwrap(),
            "baseline one\n"
        );
        assert_eq!(
            fs::read_to_string(source.join("two.txt")).unwrap(),
            "user edit\n"
        );
    }

    #[test]
    fn git_apply_check_prevents_partial_application_of_a_conflicting_patch() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("one.txt"), "baseline one\n");
        write(&source.join("two.txt"), "baseline two\n");
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
        write(&workspace.join("one.txt"), "candidate one\n");
        write(&workspace.join("two.txt"), "candidate two\n");
        let diff_path = temp.path().join("candidate.patch");
        let stats = collect_diff(&snapshot.baseline_path, &workspace, &diff_path).unwrap();
        let mut run = run_record(
            &source,
            &snapshot,
            candidate_record("C", workspace, diff_path, stats),
        );

        // Simulate a run record whose content fingerprint matches a different
        // baseline. This gets past the drift guard and directly exercises the
        // all-files apply check.
        write(&source.join("two.txt"), "incompatible contents\n");
        run.source_fingerprint = fingerprint_tree(&source).unwrap();
        let error = apply_unmoved(&run).unwrap_err().to_string();
        assert!(error.contains("cannot be applied cleanly"));
        assert_eq!(
            fs::read_to_string(source.join("one.txt")).unwrap(),
            "baseline one\n"
        );
        assert_eq!(
            fs::read_to_string(source.join("two.txt")).unwrap(),
            "incompatible contents\n"
        );
    }

    /// After an S0 move the baseline is in `s0/<n>/` of the run's directory;
    /// the fence still reads the run's own configuration, which is not strict
    /// here, so ignored build output is not drift.
    #[test]
    fn the_fingerprint_fence_reads_the_runs_configuration_after_an_s0_move() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        write(&source.join(".gitignore"), "target/\n");
        write(&source.join("a.txt"), "a\n");
        initialize_user_repository(&source);
        write(&source.join("target/out"), "built\n");
        let run_dir = temp.path().join("run");
        let snapshot = create_snapshot(&source, &run_dir).unwrap();
        write(&run_dir.join("config.snapshot.yml"), "{}\n");
        let candidate = candidate_record(
            "A",
            temp.path().join("workspace"),
            temp.path().join("delta.patch"),
            DiffStats::default(),
        );
        let mut run = run_record(&source, &snapshot, candidate);
        assert_eq!(fingerprint_for_run(&run).unwrap(), run.source_fingerprint);

        let moved = run_dir.join("s0").join("1").join(BASELINE_DIRECTORY);
        run.baseline_history.push(crate::EarlierBaseline {
            baseline_path: std::mem::replace(&mut run.baseline_path, moved),
            baseline_commit: run.baseline_commit.clone(),
            commit: "s0".into(),
            moved_to: "s1".into(),
            moved_at: Utc::now(),
            recorded_at: Utc::now(),
            landed_run: None,
            back: false,
        });

        assert_eq!(fingerprint_for_run(&run).unwrap(), run.source_fingerprint);
    }

    /// Lines `1` to `20`, one per line.
    fn twenty_lines() -> String {
        (1..=20).map(|n| format!("{n}\n")).collect()
    }

    /// A Git source of `a.txt` and `b.txt` (`twenty_lines` each), committed.
    fn twenty_line_source(temp: &TempDir) -> PathBuf {
        let source = temp.path().join("source");
        write(&source.join("a.txt"), twenty_lines());
        write(&source.join("b.txt"), twenty_lines());
        initialize_user_repository(&source);
        source
    }

    /// A run from `source` as it is now, named `name`, whose agent puts
    /// `edit` on line 15 of `file` and adds `name.txt`.
    fn edited_run(temp: &TempDir, source: &Path, name: &str, file: &str) -> RunRecord {
        let snapshot = create_snapshot(source, &temp.path().join(name)).unwrap();
        let workspace = temp.path().join(format!("{name}-workspace"));
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
        let edited = fs::read_to_string(workspace.join(file))
            .unwrap()
            .replace("\n15\n", &format!("\n15 {name}\n"));
        write(&workspace.join(file), edited);
        write(&workspace.join(format!("{name}.txt")), format!("{name}\n"));
        let diff_path = temp.path().join(format!("{name}.patch"));
        let stats = collect_diff(&snapshot.baseline_path, &workspace, &diff_path).unwrap();
        run_record(
            source,
            &snapshot,
            candidate_record("A", workspace, diff_path, stats),
        )
    }

    /// The source commits a change to line 13 of `file`: a context line of
    /// a hunk at line 15, not a line it edits.
    fn change_context(source: &Path, file: &str) {
        write(
            &source.join(file),
            twenty_lines().replace("\n13\n", "\n13 world\n"),
        );
        run_git(source, &["commit", "--quiet", "-am", "world"]);
    }

    #[test]
    fn stage_applies_patches_in_order_and_merges_3_way_only_where_needed() {
        let temp = TempDir::new().unwrap();
        let source = twenty_line_source(&temp);
        let one = edited_run(&temp, &source, "one", "b.txt");
        let two = edited_run(&temp, &source, "two", "a.txt");
        change_context(&source, "a.txt");
        let index = fs::read(source.join(".git/index")).unwrap();
        let head = run_git(&source, &["rev-parse", "HEAD"]);
        let world = run_git(&source, &["rev-parse", "HEAD^{tree}"]);
        let patches = [
            Patch::new(&one.candidates[0].diff_path, &one.baseline_path),
            Patch::new(&two.candidates[0].diff_path, &two.baseline_path),
        ];
        let git = || git_command(&source);

        let Stage::Conflict { output } = stage(&git, &world, &patches, false).unwrap() else {
            panic!("the second patch does not apply as it is");
        };
        assert!(output.contains("patch failed: a.txt"), "{output}");
        let Stage::Staged { tree } = stage(&git, &world, &patches, true).unwrap() else {
            panic!("the second patch merges 3-way");
        };

        // Read without the alternates: the blob of `two.txt`, which only the
        // second run's baseline held, was copied into the source's objects.
        let show = |path: &str| run_git(&source, &["show", &format!("{tree}:{path}")]);
        assert!(show("a.txt").contains("\n13 world\n14\n15 two\n"));
        assert!(show("b.txt").contains("\n13\n14\n15 one\n"));
        assert_eq!(
            (show("one.txt"), show("two.txt")),
            ("one".into(), "two".into())
        );
        assert_eq!(fs::read(source.join(".git/index")).unwrap(), index);
        assert_eq!(run_git(&source, &["rev-parse", "HEAD"]), head);
        assert_eq!(run_git(&source, &["status", "--porcelain"]), "");
    }

    #[test]
    fn stage_reports_a_3_way_conflict_with_git_output() {
        let temp = TempDir::new().unwrap();
        let source = twenty_line_source(&temp);
        let run = edited_run(&temp, &source, "agent", "a.txt");
        write(
            &source.join("a.txt"),
            twenty_lines().replace("\n15\n", "\n15 world\n"),
        );
        run_git(&source, &["commit", "--quiet", "-am", "world"]);
        let world = run_git(&source, &["rev-parse", "HEAD^{tree}"]);
        let patch = Patch::new(&run.candidates[0].diff_path, &run.baseline_path);

        let staged = stage(&|| git_command(&source), &world, &[patch], true).unwrap();

        let Stage::Conflict { output } = staged else {
            panic!("both sides edited line 15: {staged:?}");
        };
        assert!(output.contains("U a.txt"), "{output}");
        assert_eq!(run_git(&source, &["status", "--porcelain"]), "");
    }

    /// The tree `run`'s Δ merges into on `source`'s committed world, as the
    /// merged checks would have verified it.
    fn merged_tree(source: &Path, run: &RunRecord) -> String {
        let world = run_git(source, &["rev-parse", "HEAD^{tree}"]);
        let patch = Patch::new(&run.candidates[0].diff_path, &run.baseline_path);
        let staged = stage(&|| git_command(source), &world, &[patch], true).unwrap();
        let Stage::Staged { tree } = staged else {
            panic!("Δ merges 3-way: {staged:?}");
        };
        tree
    }

    #[test]
    fn a_3_way_landing_is_written_only_for_the_tree_the_merged_checks_verified() {
        let temp = TempDir::new().unwrap();
        let source = twenty_line_source(&temp);
        let run = edited_run(&temp, &source, "agent", "a.txt");
        change_context(&source, "a.txt");
        let verified = merged_tree(&source, &run);
        let delta = run.candidates[0].diff_path.clone();
        let frozen = fs::read(&delta).unwrap();
        let landed = temp.path().join(LANDED_PATCH);
        let world = run_git(&source, &["rev-parse", "HEAD^{tree}"]);

        let error = write_landed_patch(&run, &delta, &landed, &world).unwrap_err();

        let error = format!("{error:#}");
        assert!(
            error.contains("no longer matches the tree") && error.contains(&world),
            "{error}"
        );
        assert!(!landed.exists());
        assert_eq!(run_git(&source, &["status", "--porcelain"]), "");

        assert_eq!(
            write_landed_patch(&run, &delta, &landed, &verified).unwrap(),
            verified
        );
        assert_eq!(fs::read(&delta).unwrap(), frozen);
        let applied = apply_through_world(&run, &landed);
        assert_eq!(
            applied,
            [PathBuf::from("a.txt"), PathBuf::from("agent.txt")]
        );
        let merged = fs::read_to_string(source.join("a.txt")).unwrap();
        assert!(merged.contains("\n13 world\n14\n15 agent\n"), "{merged}");
    }

    #[test]
    fn unlanded_names_the_paths_that_do_not_hold_the_merge() {
        let temp = TempDir::new().unwrap();
        let source = twenty_line_source(&temp);
        let run = edited_run(&temp, &source, "agent", "a.txt");
        change_context(&source, "a.txt");
        let tree = merged_tree(&source, &run);
        let landed = temp.path().join(LANDED_PATCH);
        write_landed_patch(&run, &run.candidates[0].diff_path, &landed, &tree).unwrap();
        let paths = apply_through_world(&run, &landed);

        assert!(unlanded(&run, &paths, &tree).is_empty());
        write(&source.join("agent.txt"), "changed as it landed\n");
        assert_eq!(unlanded(&run, &paths, &tree), [PathBuf::from("agent.txt")]);
        fs::remove_file(source.join("agent.txt")).unwrap();
        assert_eq!(unlanded(&run, &paths, &tree), [PathBuf::from("agent.txt")]);
        // Git cannot read a tree that is not there: nothing is confirmed.
        assert_eq!(unlanded(&run, &paths, &"0".repeat(40)), paths);
    }

    /// `patch` applied to `run`'s source through the digest of its world now.
    fn apply_through_world(run: &RunRecord, patch: &Path) -> Vec<PathBuf> {
        let digest = crate::coherence::world::observe(
            &run.source_path,
            &run.baseline_path,
            &run.baseline_commit,
            &run.source_kind,
        )
        .unwrap()
        .digest;
        apply_checked(run, patch, ExpectedWorld::Digest(&digest)).unwrap()
    }

    #[test]
    fn git_fingerprints_ignore_build_output_but_cover_source_files() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join(".gitignore"), "target/\ntracked\n").unwrap();
        fs::write(root.join("tracked"), "one").unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join("target/cache"), "one").unwrap();
        let mut init = git_command(root);
        init.args(["init", "-q"]);
        checked_output(init, "init").unwrap();
        let mut add = git_command(root);
        add.args(["add", "-f", "tracked", ".gitignore"]);
        checked_output(add, "add").unwrap();
        let before = fingerprint_tree(root).unwrap();
        fs::write(root.join("target/cache"), "two").unwrap();
        fs::create_dir(root.join("target/new-directory")).unwrap();
        assert_eq!(before, fingerprint_tree(root).unwrap());
        fs::write(root.join("tracked"), "two").unwrap();
        let tracked = fingerprint_tree(root).unwrap();
        assert_ne!(before, tracked);
        fs::write(root.join("untracked"), "one").unwrap();
        let untracked = fingerprint_tree(root).unwrap();
        assert_ne!(tracked, untracked);
        fs::write(root.join("untracked"), "two").unwrap();
        assert_ne!(untracked, fingerprint_tree(root).unwrap());
    }

    #[test]
    fn fingerprints_ignore_state_but_detect_content_symlinks_and_execute_bits() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("file"), "value");
        let initial = fingerprint_tree(&source).unwrap();
        write(&source.join(".dispatch/state"), "ignored");
        write(&source.join(".git/index"), "ignored");
        assert_eq!(fingerprint_tree(&source).unwrap(), initial);

        write(&source.join("file"), "changed");
        assert_ne!(fingerprint_tree(&source).unwrap(), initial);

        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            write(&source.join("file"), "value");
            let normal = fingerprint_tree(&source).unwrap();
            fs::set_permissions(source.join("file"), fs::Permissions::from_mode(0o755)).unwrap();
            assert_ne!(fingerprint_tree(&source).unwrap(), normal);

            symlink("file", source.join("link")).unwrap();
            let first_link = fingerprint_tree(&source).unwrap();
            fs::remove_file(source.join("link")).unwrap();
            symlink("other", source.join("link")).unwrap();
            assert_ne!(fingerprint_tree(&source).unwrap(), first_link);
        }
    }

    #[test]
    fn an_entry_deleted_mid_walk_is_a_difference_not_an_error() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        write(&source.join("src/lib.rs"), "pub fn f() {}\n");
        write(&source.join("target/debug/a.o"), "object\n");
        write(&source.join("target/debug/b.o"), "object\n");
        let root = resolve_source(Some(&source)).unwrap();
        let before = fingerprint_tree(&root).unwrap();

        // Listed while present, gone by the time it is read.
        let listed = list_tree(&root).unwrap();
        fs::remove_file(root.join("target/debug/a.o")).unwrap();
        let moving = hash_tree(&root, listed).unwrap();
        let after = fingerprint_tree(&root).unwrap();

        assert_ne!(moving, before);
        assert_ne!(moving, after);
        assert_ne!(before, after);
    }

    #[test]
    fn world_commit_never_accepts_a_pair_rewritten_during_capture() {
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        };

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let staging = temp.path().join("staging");
        fs::create_dir(&staging).unwrap();
        let generation = |value: u64| format!("generation {value}\n").repeat(10_000);
        write(&source.join("a.txt"), generation(0));
        write(&source.join("b.txt"), generation(0));
        initialize_user_repository(&source);

        // The writer replaces a.txt, then b.txt, each whole by rename, so the
        // pair is mixed only between the two renames. It records each such
        // window: if it is descheduled there, the workspace itself holds the
        // mixed pair for that long, and a capture inside the window is honest.
        let stop = Arc::new(AtomicBool::new(false));
        let windows = Arc::new(Mutex::new(Vec::new()));
        let writer = thread::spawn({
            let (stop, windows) = (Arc::clone(&stop), Arc::clone(&windows));
            let source = source.clone();
            move || {
                let mut value = 0;
                while !stop.load(Ordering::Relaxed) {
                    value += 1;
                    for file in ["a.txt", "b.txt"] {
                        fs::write(staging.join(file), generation(value)).unwrap();
                    }
                    fs::rename(staging.join("a.txt"), source.join("a.txt")).unwrap();
                    let opened = Instant::now();
                    let closed = Instant::now();
                    fs::rename(staging.join("b.txt"), source.join("b.txt")).unwrap();
                    windows.lock().unwrap().push((opened, closed));
                    // Quiet now and then for long enough to capture.
                    let pause = if value % 4 == 0 { 200 } else { value % 3 };
                    thread::sleep(Duration::from_millis(pause));
                }
                value
            }
        });

        let mut accepted = Vec::new();
        let mut refused = 0;
        for _ in 0..40 {
            let started = Instant::now();
            let result = world_commit(&source);
            let finished = Instant::now();
            match result {
                Ok(commit) => accepted.push((commit, started, finished)),
                Err(error) => {
                    assert!(
                        error.to_string().contains(
                            "the workspace changed while its starting state was being captured"
                        ),
                        "{error:#}"
                    );
                    refused += 1;
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        let last = writer.join().unwrap();
        let windows = windows.lock().unwrap();

        // A file's generation, which must also be the whole of it.
        let read = |commit: &str, file: &str| -> u64 {
            let text = run_git(&source, &["show", &format!("{commit}:{file}")]);
            let value = text
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("generation "))
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| panic!("{file} was captured torn"));
            assert_eq!(
                text,
                generation(value).trim_end(),
                "{file} was captured torn"
            );
            value
        };
        let mut stalled = 0;
        for (commit, started, finished) in &accepted {
            let (a, b) = (read(commit, "a.txt"), read(commit, "b.txt"));
            if a == b {
                continue;
            }
            let held_throughout = a == b + 1 && {
                let (opened, closed) = windows[a as usize - 1];
                opened <= *started && *finished <= closed
            };
            assert!(held_throughout, "a mixed pair was accepted: a {a}, b {b}");
            stalled += 1;
        }
        eprintln!(
            "accepted {} ({stalled} inside a stalled writer's window), refused {refused}",
            accepted.len()
        );

        // Quiet again: the capture is the last generation in both files.
        let commit = world_commit(&source).unwrap();
        assert_eq!(
            (read(&commit, "a.txt"), read(&commit, "b.txt")),
            (last, last)
        );
    }

    #[test]
    fn a_capture_that_moved_is_made_once_more_then_refused() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        write(&source.join("a.txt"), "0\n");
        initialize_user_repository(&source);
        let source = resolve_source(Some(&source)).unwrap();

        // Moved during the first capture only: the second is accepted.
        let mut captures = 0;
        let commit = capture_settled(&source, || {
            captures += 1;
            let commit = capture_world(&source)?;
            if captures == 1 {
                write(&source.join("a.txt"), "moved\n");
            }
            Ok(commit)
        })
        .unwrap();
        assert_eq!(captures, 2);
        assert_eq!(
            run_git(&source, &["show", &format!("{commit}:a.txt")]),
            "moved"
        );

        // Moved during both: refused, and no third capture.
        let mut captures = 0;
        let error = capture_settled(&source, || {
            captures += 1;
            let commit = capture_world(&source)?;
            write(&source.join("a.txt"), "moved\n".repeat(captures + 1));
            Ok(commit)
        })
        .unwrap_err();
        assert_eq!(captures, 2);
        assert!(
            error
                .to_string()
                .contains("the workspace changed while its starting state was being captured"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_that_escape_candidate_isolation() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("nested")).unwrap();
        symlink("../../outside", source.join("nested/escape")).unwrap();

        let error = fingerprint_tree(&source).unwrap_err().to_string();
        assert!(error.contains("escapes candidate isolation"));

        fs::remove_file(source.join("nested/escape")).unwrap();
        symlink("/tmp/outside", source.join("absolute")).unwrap();
        let error = fingerprint_tree(&source).unwrap_err().to_string();
        assert!(error.contains("absolute symlink"));
    }

    #[test]
    fn world_commit_records_exactly_the_world_and_leaves_the_checkout_alone() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join(".gitignore"), "target/\n");
        write(&source.join("a.txt"), "committed\n");
        write(&source.join("b.txt"), "to be deleted\n");
        write(&source.join("src/keep.rs"), "pub fn keep() {}\n");
        initialize_user_repository(&source);
        let head = run_git(&source, &["rev-parse", "HEAD"]);

        write(&source.join("a.txt"), "dirty\n");
        fs::remove_file(source.join("b.txt")).unwrap();
        write(&source.join("new.txt"), "untracked\n");
        write(&source.join("target/out.bin"), "build output\n");
        let nested = source.join("vendor/dep");
        write(&nested.join("lib.rs"), "nested\n");
        run_git(&nested, &["init", "--quiet"]);
        let status_before = run_git(&source, &["status", "--porcelain"]);
        let index_before = fs::read(source.join(".git/index")).unwrap();

        let commit = world_commit(&source).unwrap();

        assert_eq!(run_git(&source, &["status", "--porcelain"]), status_before);
        assert_eq!(fs::read(source.join(".git/index")).unwrap(), index_before);
        assert_eq!(
            run_git(&source, &["rev-parse", &format!("{commit}^")]),
            head
        );
        let mut files: Vec<String> = run_git(&source, &["ls-tree", "-r", "--name-only", &commit])
            .lines()
            .map(str::to_owned)
            .collect();
        files.sort();
        assert_eq!(files, [".gitignore", "a.txt", "new.txt", "src/keep.rs"]);

        let baseline = materialize_baseline_from_commit(&source, &commit, &temp.path().join("run"))
            .unwrap()
            .baseline_path;
        assert_eq!(
            fs::read_to_string(baseline.join("a.txt")).unwrap(),
            "dirty\n"
        );
        assert!(!baseline.join("b.txt").exists() && !baseline.join("target").exists());

        // A repository with no commit yet has a world too.
        let fresh = temp.path().join("fresh");
        fs::create_dir(&fresh).unwrap();
        run_git(&fresh, &["init", "--quiet"]);
        write(&fresh.join("first.txt"), "first\n");
        let commit = world_commit(&fresh).unwrap();
        assert_eq!(
            run_git(&fresh, &["ls-tree", "-r", "--name-only", &commit]),
            "first.txt"
        );
    }

    /// Git trusts an entry's stat data only for a file older than the index
    /// file, so a same-size edit made in the second the index was written is
    /// caught only by reading it again; the capture's copy of the index must
    /// keep that. Whole-second stat checks and fixed times make the race
    /// exact here.
    #[test]
    fn world_commit_sees_a_same_size_edit_made_as_the_index_was_written() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("a.txt"), "1\n");
        initialize_user_repository(&source);
        run_git(&source, &["config", "core.checkStat", "minimal"]);
        run_git(&source, &["config", "core.trustCtime", "false"]);
        let second = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let set_time = |path: &Path| {
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(second)
                .unwrap()
        };
        set_time(&source.join("a.txt"));
        run_git(&source, &["update-index", "--refresh"]);

        write(&source.join("a.txt"), "2\n");
        set_time(&source.join("a.txt"));
        set_time(&source.join(".git/index"));
        assert_eq!(run_git(&source, &["diff-files", "--name-only"]), "a.txt");

        let commit = world_commit(&source).unwrap();
        assert_eq!(run_git(&source, &["show", &format!("{commit}:a.txt")]), "2");
    }

    #[cfg(unix)]
    #[test]
    fn ignored_build_output_never_limits_or_refuses_the_delta() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join(".gitignore"), "target/\nnode_modules/\n");
        write(&source.join("src/lib.rs"), "pub fn f() {}\n");
        initialize_user_repository(&source);
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
        let diff = temp.path().join("delta.patch");
        let collect = || collect_diff(&snapshot.baseline_path, &workspace, &diff);

        // What a build and an install leave behind: a file over the per-file
        // limit (sparse, so it costs nothing) and links out of the tree.
        fs::create_dir_all(workspace.join("target")).unwrap();
        fs::File::create(workspace.join("target/huge.bin"))
            .unwrap()
            .set_len(MAX_CANDIDATE_FILE_BYTES + 1)
            .unwrap();
        fs::create_dir_all(workspace.join("node_modules/.bin")).unwrap();
        symlink("/usr/bin/env", workspace.join("node_modules/.bin/env")).unwrap();
        write(&workspace.join("src/lib.rs"), "pub fn f() { 1 }\n");
        assert_eq!(collect().unwrap().changed_files, vec!["src/lib.rs"]);

        // Whatever can enter the delta is still held to the same limits.
        symlink("/usr/bin/env", workspace.join("src/escape")).unwrap();
        let error = collect().unwrap_err().to_string();
        assert!(error.contains("absolute symlink"), "{error}");
        fs::remove_file(workspace.join("src/escape")).unwrap();

        fs::File::create(workspace.join("src/huge.bin"))
            .unwrap()
            .set_len(MAX_CANDIDATE_FILE_BYTES + 1)
            .unwrap();
        let error = collect().unwrap_err().to_string();
        assert!(error.contains("safety limit"), "{error}");
        fs::remove_file(workspace.join("src/huge.bin")).unwrap();

        // A tracked directory replaced by a link out of the tree.
        fs::remove_dir_all(workspace.join("src")).unwrap();
        symlink("/tmp", workspace.join("src")).unwrap();
        let error = collect().unwrap_err().to_string();
        assert!(error.contains("absolute symlink"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn scratch_tree_copies_listed_files_only_and_never_follows_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        write(&source.join(".gitignore"), "target/\n");
        write(&source.join("src/lib.rs"), "pub fn f() {}\n");
        write(&source.join("gone.txt"), "deleted later\n");
        write(&source.join("run.sh"), "#!/bin/sh\n");
        fs::set_permissions(source.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("run.sh", source.join("link")).unwrap();
        initialize_user_repository(&source);
        fs::remove_file(source.join("gone.txt")).unwrap();
        write(&source.join("target/out.bin"), "ignored\n");
        write(&source.join("untracked.txt"), "u\n");
        write(&source.join("dep/lib.rs"), "nested\n");
        run_git(&source.join("dep"), &["init", "--quiet"]);

        let scratch = temp.path().join("scratch");
        create_scratch_tree(&source, &SourceKind::Git, &scratch).unwrap();

        assert!(!scratch.join("target").exists() && !scratch.join("gone.txt").exists());
        assert!(!scratch.join(".git").exists() && !scratch.join("dep/.git").exists());
        assert_eq!(
            fs::read_to_string(scratch.join("untracked.txt")).unwrap(),
            "u\n"
        );
        assert!(scratch.join("dep/lib.rs").is_file());
        assert_eq!(
            fs::metadata(scratch.join("run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0o111
        );
        assert_eq!(
            fs::read_link(scratch.join("link")).unwrap(),
            Path::new("run.sh")
        );

        // A symlink that leaves the tree is refused, and a parent swapped for a
        // symlink is not read through.
        symlink("../outside", source.join("escape")).unwrap();
        let error = create_scratch_tree(&source, &SourceKind::Git, &temp.path().join("s2"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("escapes candidate isolation"), "{error}");
        fs::remove_file(source.join("escape")).unwrap();
        fs::remove_dir_all(source.join("src")).unwrap();
        symlink("dep", source.join("src")).unwrap();
        let scratch = temp.path().join("s3");
        create_scratch_tree(&source, &SourceKind::Git, &scratch).unwrap();
        assert!(
            fs::symlink_metadata(scratch.join("src"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn refuses_oversized_sparse_candidate_files_before_git() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let run_dir = temp.path().join("run");
        fs::create_dir(&source).unwrap();
        write(&source.join("small.txt"), "baseline\n");
        let snapshot = create_snapshot(&source, &run_dir).unwrap();
        let candidate = create_candidate_workspace(
            &snapshot.baseline_path,
            &run_dir.join("candidate/workspace"),
        )
        .unwrap();
        let sparse = File::create(candidate.join("huge.bin")).unwrap();
        sparse.set_len(MAX_CANDIDATE_FILE_BYTES + 1).unwrap();

        let error = collect_diff(
            &snapshot.baseline_path,
            &candidate,
            &run_dir.join("candidate/diff.patch"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("safety limit"));
    }

    #[test]
    fn git_snapshot_does_not_track_ignored_files() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join(".gitignore"), "build/\n");
        write(&source.join("src/lib.rs"), "pub fn f() {}\n");
        initialize_user_repository(&source);
        write(&source.join("build/out.bin"), "stale build output\n");

        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        assert_eq!(snapshot.kind, SourceKind::Git);

        let tracked = run_git(
            &snapshot.baseline_path,
            &["ls-tree", "-r", "--name-only", "HEAD"],
        );
        let tracked: Vec<&str> = tracked.lines().collect();
        assert!(tracked.contains(&".gitignore"));
        assert!(tracked.contains(&"src/lib.rs"));
        assert!(!tracked.iter().any(|path| path.starts_with("build/")));

        // The ignored file is still present on disk in the baseline directory,
        // so a candidate workspace inherits it as a build cache.
        assert_eq!(
            fs::read_to_string(snapshot.baseline_path.join("build/out.bin")).unwrap(),
            "stale build output\n"
        );

        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
        assert_eq!(
            fs::read_to_string(workspace.join("build/out.bin")).unwrap(),
            "stale build output\n"
        );
        assert_eq!(
            fingerprint_tree(&snapshot.baseline_path).unwrap(),
            fingerprint_tree(&workspace).unwrap()
        );
    }

    #[test]
    fn regenerated_ignored_artifact_is_not_part_of_the_delta() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join(".gitignore"), "build/\n");
        write(&source.join("src/lib.rs"), "pub fn f() {}\n");
        initialize_user_repository(&source);
        write(&source.join("build/out.bin"), "stale build output\n");

        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();

        // A verify check regenerates the build artifact and the agent edits a
        // tracked source file; only the tracked edit should reach Δ.
        write(&workspace.join("build/out.bin"), "freshly rebuilt output\n");
        write(&workspace.join("src/lib.rs"), "pub fn f() { 1 }\n");

        let diff_path = temp.path().join("candidate.patch");
        let stats = collect_diff(&snapshot.baseline_path, &workspace, &diff_path).unwrap();
        assert_eq!(stats.files_changed, 1);
        assert_eq!(stats.changed_files, vec!["src/lib.rs".to_string()]);
        let patch = fs::read_to_string(&diff_path).unwrap();
        assert!(patch.contains("src/lib.rs"));
        assert!(!patch.contains("build/"));

        let delta_path = temp.path().join("delta.patch");
        snapshot_delta(&snapshot.baseline_path, &workspace, &delta_path).unwrap();
        let delta = fs::read_to_string(&delta_path).unwrap();
        assert!(delta.contains("src/lib.rs"));
        assert!(!delta.contains("build/"));
    }

    #[test]
    fn a_kept_index_snapshots_exactly_what_a_fresh_one_does() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("src/lib.rs"), "pub fn f() {}\n");
        write(&source.join("README.md"), "readme\n");
        initialize_user_repository(&source);
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
        let index = temp.path().join("kept.index");
        let (kept, fresh) = (
            temp.path().join("kept.patch"),
            temp.path().join("fresh.patch"),
        );
        let same = || {
            snapshot_delta_indexed(&snapshot.baseline_path, &workspace, &kept, &index).unwrap();
            snapshot_delta(&snapshot.baseline_path, &workspace, &fresh).unwrap();
            let kept = fs::read_to_string(&kept).unwrap();
            assert_eq!(kept, fs::read_to_string(&fresh).unwrap());
            kept
        };

        assert!(same().is_empty());
        write(&workspace.join("src/lib.rs"), "pub fn f() { 1 }\n");
        write(&workspace.join("new.txt"), "new\n");
        assert!(same().contains("new.txt"));
        // Same size, rewritten: the stat cache must not hide it.
        write(&workspace.join("src/lib.rs"), "pub fn f() { 2 }\n");
        assert!(same().contains("{ 2 }"));
        // Reverted and deleted work leaves the patch.
        write(&workspace.join("src/lib.rs"), "pub fn f() {}\n");
        fs::remove_file(workspace.join("new.txt")).unwrap();
        fs::remove_file(workspace.join("README.md")).unwrap();
        let patch = same();
        assert!(!patch.contains("src/lib.rs") && !patch.contains("new.txt"));
        assert!(patch.contains("deleted file mode"));
    }

    #[test]
    fn a_failed_patch_publication_does_not_lose_the_next_follow() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("file.txt"), "before\n");
        initialize_user_repository(&source);
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
        let index = temp.path().join("kept.index");
        let patch = temp.path().join("delta-live.patch");

        assert!(
            snapshot_delta_if_changed_indexed(
                &snapshot.baseline_path,
                &workspace,
                &patch,
                &index,
                &patch
            )
            .unwrap()
        );
        let before = fs::read(&patch).unwrap();
        write(&workspace.join("file.txt"), "after the edit\n");

        // Let Git stage the edit, but make publishing its patch fail. Restore
        // the previous patch afterward, as it would remain after a write error.
        let old = temp.path().join("old.patch");
        fs::rename(&patch, &old).unwrap();
        fs::create_dir(&patch).unwrap();
        assert!(
            snapshot_delta_if_changed_indexed(
                &snapshot.baseline_path,
                &workspace,
                &patch,
                &index,
                &patch
            )
            .is_err()
        );
        fs::remove_dir(&patch).unwrap();
        fs::rename(&old, &patch).unwrap();
        assert_eq!(fs::read(&patch).unwrap(), before);

        assert!(
            snapshot_delta_if_changed_indexed(
                &snapshot.baseline_path,
                &workspace,
                &patch,
                &index,
                &patch
            )
            .unwrap()
        );
        let updated = fs::read_to_string(&patch).unwrap();
        assert!(updated.contains("+after the edit"), "{updated}");
    }

    /// S0 moves to a new baseline when the Work integrates the source: the
    /// index kept against the old one must not make the workspace look
    /// unchanged against the new one.
    #[test]
    fn a_kept_index_serves_only_the_baseline_it_was_staged_against() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("file.txt"), "first\n");
        initialize_user_repository(&source);
        let first = create_snapshot(&source, &temp.path().join("first")).unwrap();
        write(&source.join("file.txt"), "second\n");
        let second = create_snapshot(&source, &temp.path().join("second")).unwrap();
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&first.baseline_path, &workspace).unwrap();
        write(&workspace.join("file.txt"), "second\n");
        write(&workspace.join("own.txt"), "own\n");
        // Old enough that Git trusts what it staged of them.
        let written = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for name in ["file.txt", "own.txt"] {
            fs::File::options()
                .write(true)
                .open(workspace.join(name))
                .unwrap()
                .set_modified(written)
                .unwrap();
        }
        let index = temp.path().join("kept.index");
        let patch = temp.path().join("delta-live.patch");
        let follow = |baseline: &Path| {
            snapshot_delta_if_changed_indexed(baseline, &workspace, &patch, &index, &patch).unwrap()
        };
        follow(&first.baseline_path);
        assert!(fs::read_to_string(&patch).unwrap().contains("file.txt"));

        follow(&second.baseline_path);
        let expected = temp.path().join("expected.patch");
        snapshot_delta(&second.baseline_path, &workspace, &expected).unwrap();
        let followed = fs::read_to_string(&patch).unwrap();
        assert_eq!(followed, fs::read_to_string(&expected).unwrap());
        assert!(
            followed.contains("own.txt") && !followed.contains("file.txt"),
            "{followed}"
        );
    }

    #[test]
    fn a_live_patch_is_never_seen_half_written() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("README.md"), "readme\n");
        initialize_user_repository(&source);
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        // Two workspaces whose patches are large and different, each with its
        // own kept index, published alternately into one live patch.
        let workspaces = ["a", "b"].map(|name| {
            let workspace = temp.path().join(name);
            create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
            let lines = (0..80_000).map(|line| format!("{name} line {line}\n"));
            write(&workspace.join("big.txt"), lines.collect::<String>());
            let index = temp.path().join(format!("{name}.index"));
            let patch = temp.path().join(format!("{name}.patch"));
            snapshot_delta_indexed(&snapshot.baseline_path, &workspace, &patch, &index).unwrap();
            (workspace, index, fs::read(&patch).unwrap())
        });
        assert!(
            workspaces
                .iter()
                .all(|(_, _, patch)| patch.len() >= 1 << 20)
        );
        let live = temp.path().join("live/delta-live.patch");
        write(&live, &workspaces[0].2);

        let done = AtomicBool::new(false);
        let (reads, torn) = thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let (mut reads, mut torn) = (0, 0);
                while !done.load(Ordering::Relaxed) {
                    let Ok(seen) = fs::read(&live) else { continue };
                    reads += 1;
                    if workspaces.iter().all(|(_, _, patch)| *patch != seen) {
                        torn += 1;
                    }
                }
                (reads, torn)
            });
            for (workspace, index, _) in workspaces.iter().cycle().take(60) {
                snapshot_delta_indexed(&snapshot.baseline_path, workspace, &live, index).unwrap();
            }
            done.store(true, Ordering::Relaxed);
            reader.join().unwrap()
        });
        assert!(reads > 0);
        assert_eq!(torn, 0, "{torn} of {reads} reads saw a partial patch");
    }

    #[test]
    fn a_failed_live_snapshot_keeps_the_last_patch() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("README.md"), "readme\n");
        initialize_user_repository(&source);
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let live = temp.path().join("live/delta-live.patch");
        write(&live, "last patch\n");

        let missing = temp.path().join("missing");
        let index = temp.path().join("missing.index");
        assert!(snapshot_delta_indexed(&snapshot.baseline_path, &missing, &live, &index).is_err());
        assert!(snapshot_delta(&snapshot.baseline_path, &missing, &live).is_err());

        assert_eq!(fs::read_to_string(&live).unwrap(), "last patch\n");
        let names = |directory: &Path| {
            fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(live.parent().unwrap()), ["delta-live.patch"]);

        // A publish that fails after staging (here the rename, onto a
        // directory) leaves no temporary file behind either.
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
        write(&workspace.join("new.txt"), "new\n");
        let blocked = temp.path().join("blocked/delta-live.patch");
        fs::create_dir_all(blocked.join("kept")).unwrap();
        assert!(snapshot_delta(&snapshot.baseline_path, &workspace, &blocked).is_err());
        assert_eq!(names(blocked.parent().unwrap()), ["delta-live.patch"]);
        assert_eq!(names(&blocked), ["kept"]);
    }

    #[test]
    fn info_exclude_is_honored() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("src/lib.rs"), "pub fn f() {}\n");
        initialize_user_repository(&source);
        write(&source.join(".git/info/exclude"), "scratch.txt\n");
        write(&source.join("scratch.txt"), "local scratch notes\n");

        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let tracked = run_git(
            &snapshot.baseline_path,
            &["ls-tree", "-r", "--name-only", "HEAD"],
        );
        let tracked: Vec<&str> = tracked.lines().collect();
        assert!(tracked.contains(&"src/lib.rs"));
        assert!(!tracked.contains(&"scratch.txt"));
        assert!(snapshot.baseline_path.join("scratch.txt").exists());
    }

    #[test]
    fn directory_snapshot_still_tracks_everything() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join(".gitignore"), "build/\n");
        write(&source.join("src/lib.rs"), "pub fn f() {}\n");
        write(&source.join("build/out.bin"), "stale build output\n");

        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        assert_eq!(snapshot.kind, SourceKind::Directory);
        let tracked = run_git(
            &snapshot.baseline_path,
            &["ls-tree", "-r", "--name-only", "HEAD"],
        );
        let tracked: Vec<&str> = tracked.lines().collect();
        assert!(tracked.contains(&"build/out.bin"));
    }

    #[test]
    fn repo_identity_of_a_linked_worktree_names_the_main_worktree() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        write(&repo.join("file.txt"), "content\n");
        initialize_user_repository(&repo);

        let worktree = temp.path().join("worktree");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "dispatch-identity-test",
                worktree.to_str().unwrap(),
            ],
        );

        let from_root = repo_identity(&repo).unwrap().unwrap();
        let from_worktree = repo_identity(&worktree).unwrap().unwrap();
        let canonical_repo = fs::canonicalize(&repo).unwrap();
        assert_eq!(from_root.key, from_worktree.key);
        assert_eq!(from_root.main_worktree, canonical_repo);
        assert_eq!(from_worktree.main_worktree, canonical_repo);
        assert_eq!(from_root.common_dir, from_worktree.common_dir);

        let plain = temp.path().join("plain");
        fs::create_dir(&plain).unwrap();
        assert!(repo_identity(&plain).unwrap().is_none());
    }

    /// The owner runs `git status` in the person's project every tick. With
    /// Git's optional locks, that refreshes and rewrites their index under
    /// `index.lock`, so their own `git commit` could fail while Dispatch
    /// watches (seen in the 0.4.13 build as a test's `git commit` exiting 128).
    #[test]
    fn dispatch_never_rewrites_the_persons_index() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        write(&repo.join("file.txt"), "one\n");
        initialize_user_repository(&repo);
        let index = repo.join(".git").join("index");
        // Same content, newer modification time: the index's stat data is
        // stale, which a refreshing `git status` writes back.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write(&repo.join("file.txt"), "one\n");
        let before = fs::read(&index).unwrap();

        let status = git_command(&repo)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(status.status.success());
        assert_eq!(
            fs::read(&index).unwrap(),
            before,
            "Dispatch rewrote the index"
        );

        // Without the setting the same command rewrites it, so the assertion
        // above is not vacuous.
        let refreshed = git_command(&repo)
            .env_remove("GIT_OPTIONAL_LOCKS")
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(refreshed.status.success());
        assert_ne!(fs::read(&index).unwrap(), before);
    }

    #[test]
    fn merge_base_finds_the_fork_point_and_none_without_history() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        write(&repo.join("file.txt"), "one\n");
        initialize_user_repository(&repo);
        let fork_commit = run_git(&repo, &["rev-parse", "HEAD"]);

        let workspace = temp.path().join("workspace");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "dispatch-merge-base-test",
                workspace.to_str().unwrap(),
            ],
        );

        // Diverge both sides from the fork point.
        write(&repo.join("file.txt"), "one\nroot change\n");
        run_git(&repo, &["add", "-A"]);
        run_git(&repo, &["commit", "--quiet", "-m", "root change"]);

        write(&workspace.join("workspace.txt"), "workspace change\n");
        run_git(&workspace, &["add", "-A"]);
        run_git(&workspace, &["commit", "--quiet", "-m", "workspace change"]);

        assert_eq!(merge_base(&repo, &workspace).unwrap(), Some(fork_commit));

        // An unrelated repository is refused rather than compared.
        let unrelated = temp.path().join("unrelated");
        fs::create_dir(&unrelated).unwrap();
        write(&unrelated.join("other.txt"), "other\n");
        initialize_user_repository(&unrelated);
        let error = merge_base(&repo, &unrelated).unwrap_err().to_string();
        assert!(error.contains("different repository"), "{error}");

        // An orphan branch in the workspace shares no history with root.
        run_git(
            &workspace,
            &["checkout", "--quiet", "--orphan", "dispatch-orphan-test"],
        );
        write(&workspace.join("orphan.txt"), "orphan\n");
        run_git(&workspace, &["add", "-A"]);
        run_git(&workspace, &["commit", "--quiet", "-m", "orphan commit"]);
        assert_eq!(merge_base(&repo, &workspace).unwrap(), None);
    }

    #[test]
    fn materialized_baseline_matches_the_commit_tree() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        write(&repo.join("tracked.txt"), "tracked\n");
        write(&repo.join("sub/nested.txt"), "nested\n");
        write(&repo.join(".gitignore"), "build/\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let script = repo.join("run.sh");
            write(&script, "#!/bin/sh\n");
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            symlink("run.sh", repo.join("link.sh")).unwrap();
        }
        initialize_user_repository(&repo);
        let commit = run_git(&repo, &["rev-parse", "HEAD"]);

        let snapshot =
            materialize_baseline_from_commit(&repo, &commit, &temp.path().join("run")).unwrap();
        assert_eq!(snapshot.kind, SourceKind::Git);
        assert_eq!(snapshot.git_head.as_deref(), Some(commit.as_str()));

        let commit_listing = run_git(&repo, &["ls-tree", "-r", &commit]);
        let baseline_listing = run_git(&snapshot.baseline_path, &["ls-tree", "-r", "HEAD"]);
        assert_eq!(commit_listing, baseline_listing);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::read_link(snapshot.baseline_path.join("link.sh")).unwrap(),
                Path::new("run.sh")
            );
            assert_eq!(
                fs::metadata(snapshot.baseline_path.join("run.sh"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o111,
                0o111
            );
        }

        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();

        let observation = crate::coherence::world::observe(
            &repo,
            &snapshot.baseline_path,
            &snapshot.baseline_commit,
            &SourceKind::Git,
        )
        .unwrap();
        assert!(observation.changes.is_empty(), "{:?}", observation.changes);
    }

    #[test]
    fn delta_from_a_linked_worktree_excludes_git_file_and_ignored_paths() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        write(&root.join("tracked.txt"), "one\n");
        write(&root.join(".gitignore"), "build/\n");
        initialize_user_repository(&root);
        let fork_commit = run_git(&root, &["rev-parse", "HEAD"]);

        let workspace = temp.path().join("workspace");
        run_git(
            &root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "dispatch-delta-test",
                workspace.to_str().unwrap(),
            ],
        );
        assert!(
            workspace.join(".git").is_file(),
            "a linked worktree names its git directory through a file, not a directory"
        );

        // Edits since S0: a tracked-file change, a new untracked file, and
        // ignored build output that must never appear in Δ.
        write(&workspace.join("tracked.txt"), "one\nedited\n");
        write(&workspace.join("new.txt"), "new\n");
        write(&workspace.join("build/out.bin"), "artifact\n");

        let baseline =
            materialize_baseline_from_commit(&root, &fork_commit, &temp.path().join("run"))
                .unwrap();

        let live_patch = temp.path().join("live.patch");
        snapshot_delta(&baseline.baseline_path, &workspace, &live_patch).unwrap();
        let live_text = fs::read_to_string(&live_patch).unwrap();
        assert!(live_text.contains("tracked.txt"));
        assert!(live_text.contains("edited"));
        assert!(!live_text.contains("out.bin"));
        assert!(!live_text.contains(".git"));

        let diff_path = temp.path().join("finish.patch");
        let stats = collect_diff(&baseline.baseline_path, &workspace, &diff_path).unwrap();
        assert_eq!(
            stats.changed_files,
            vec!["new.txt".to_string(), "tracked.txt".to_string()]
        );
        assert_eq!(stats.untracked_files, vec!["new.txt".to_string()]);
        assert!(
            !stats
                .changed_files
                .iter()
                .any(|path| path.contains("build"))
        );
        assert!(!stats.changed_files.iter().any(|path| path.contains(".git")));
        let finish_text = fs::read_to_string(&diff_path).unwrap();
        assert!(!finish_text.contains("out.bin"));
        assert!(!finish_text.contains(".git"));
    }
}
