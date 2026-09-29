use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::{EventRecord, RunRecord};

#[derive(Debug, Clone)]
pub struct State {
    pub root: PathBuf,
}

impl State {
    pub fn discover(explicit: Option<PathBuf>) -> Result<Self> {
        let root = explicit
            .or_else(|| std::env::var_os("DISPATCH_HOME").map(PathBuf::from))
            .or_else(|| dirs::home_dir().map(|p| p.join(".dispatch")))
            .context("could not determine Dispatch state directory; set DISPATCH_HOME")?;
        let root = if root.is_absolute() {
            root
        } else {
            std::env::current_dir()
                .context("failed to resolve relative Dispatch state directory")?
                .join(root)
        };
        Ok(Self { root })
    }

    pub fn initialize(&self) -> Result<()> {
        let root_is_new = !self.root.exists();
        fs::create_dir_all(&self.root)
            .with_context(|| format!("failed to create {}", self.root.display()))?;
        anyhow::ensure!(
            self.root.is_dir(),
            "Dispatch state path is not a directory: {}",
            self.root.display()
        );
        let runs_dir = self.runs_dir();
        let runs_is_new = !runs_dir.exists();
        fs::create_dir_all(&runs_dir)
            .with_context(|| format!("failed to create {}", runs_dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if root_is_new {
                fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))?;
            }
            if runs_is_new {
                fs::set_permissions(&runs_dir, fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(())
    }

    pub fn db_path(&self) -> PathBuf {
        self.root.join("dispatch.db")
    }
    pub fn runs_dir(&self) -> PathBuf {
        self.root.join("runs")
    }
    pub fn datasets_dir(&self) -> PathBuf {
        self.root.join("datasets")
    }
    pub fn run_dir(&self, run_id: &str) -> PathBuf {
        self.runs_dir().join(run_id)
    }
    pub fn metadata_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("metadata.json")
    }
    pub fn events_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("events.jsonl")
    }

    /// Write the run's `metadata.json` projection. Unchanged content is not
    /// rewritten, so reading a run (`status`, every `serve` tick) leaves its
    /// files alone.
    pub fn save_run(&self, run: &RunRecord) -> Result<()> {
        let path = self.metadata_path(&run.id);
        let bytes = serde_json::to_vec_pretty(run)?;
        if fs::read(&path).is_ok_and(|current| current == bytes) {
            return Ok(());
        }
        write_atomically(&path, &bytes)
    }

    pub fn append_event(&self, event: &EventRecord) -> Result<()> {
        let path = self.events_path(&event.run_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        serde_json::to_writer(&mut file, event)?;
        writeln!(file)?;
        Ok(())
    }

    pub fn load_run(&self, id_or_prefix: &str) -> Result<RunRecord> {
        let id = self.resolve_run_id(id_or_prefix)?;
        let projected = fs::read(self.metadata_path(&id))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<RunRecord>(&bytes).ok());
        let database = if self.db_path().is_file() {
            Some(crate::db::Database::open(self.db_path())?)
        } else {
            None
        };
        let committed = database
            .as_ref()
            .map(|database| database.committed_run_projection(&id))
            .transpose()?
            .flatten();
        let mut run = match (projected, committed) {
            (Some(projected), Some(committed)) => {
                if committed.execution.is_some()
                    || committed.state_revision >= projected.state_revision
                {
                    self.save_run(&committed)?;
                    committed
                } else {
                    projected
                }
            }
            (Some(projected), None) => projected,
            (None, Some(committed)) => {
                self.save_run(&committed)?;
                committed
            }
            (None, None) => bail!("run {id} has no readable metadata or committed projection"),
        };
        anyhow::ensure!(
            run.id == id,
            "run metadata identity does not match directory {id}"
        );
        if let Some(database) = database {
            crate::orchestrator::native::repair_abandoned(self, &database, &mut run)?;
            if let Some(policy) = &mut run.execution {
                policy.questions = database.questions_for_run(&id)?;
            }
            self.repair_event_projection(&id, &database.events_for_run(&id)?)?;
        }
        Ok(run)
    }

    fn repair_event_projection(&self, run_id: &str, events: &[EventRecord]) -> Result<()> {
        let mut bytes = Vec::new();
        for event in events {
            serde_json::to_writer(&mut bytes, event)?;
            bytes.push(b'\n');
        }
        let path = self.events_path(run_id);
        if fs::read(&path).ok().as_deref() == Some(bytes.as_slice()) {
            return Ok(());
        }
        write_atomically(&path, &bytes)
    }

    pub fn resolve_run_id(&self, id_or_prefix: &str) -> Result<String> {
        anyhow::ensure!(
            !id_or_prefix.is_empty()
                && id_or_prefix.len() <= 26
                && id_or_prefix
                    .chars()
                    .all(|value| value.is_ascii_alphanumeric()),
            "invalid run ID or prefix: {id_or_prefix:?}"
        );
        let normalized = id_or_prefix.to_ascii_uppercase();
        if self.run_dir(&normalized).is_dir() {
            return Ok(normalized);
        }
        let mut matches = Vec::new();
        if self.runs_dir().is_dir() {
            for entry in fs::read_dir(self.runs_dir())? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(&normalized) {
                    matches.push(name);
                }
            }
        }
        matches.sort();
        match matches.as_slice() {
            [only] => Ok(only.clone()),
            [] => bail!("run not found: {id_or_prefix}"),
            _ => bail!("run prefix is ambiguous: {id_or_prefix}"),
        }
    }

    pub fn list_metadata_paths(&self) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        if !self.runs_dir().is_dir() {
            return Ok(paths);
        }
        for entry in fs::read_dir(self.runs_dir())? {
            let path = entry?.path().join("metadata.json");
            if path.is_file() {
                paths.push(path);
            }
        }
        paths.sort();
        paths.reverse();
        Ok(paths)
    }
}

/// Replace `path` with `bytes` through a uniquely named temporary file in the
/// same directory and an atomic rename. Several processes rewrite a run's
/// projections concurrently (an owner loop under its lock, `serve` and every
/// `load_run` reader repairing a stale copy), and a shared temporary name let
/// one rename consume the other's file. Every writer produces the committed
/// projection, so whichever rename lands last leaves a complete, current file.
/// How each of `ids` is shown: its first 8 characters, or more when another
/// of `ids` shares them. Run IDs made within the same quarter second share
/// their first 8, and Work launched together is exactly what gets compared.
pub(crate) fn short_ids<'a>(ids: &[&'a str]) -> std::collections::HashMap<&'a str, &'a str> {
    ids.iter()
        .map(|&id| {
            let shared = ids
                .iter()
                .filter(|&&other| other != id)
                .map(|other| {
                    id.bytes()
                        .zip(other.bytes())
                        .take_while(|(a, b)| a == b)
                        .count()
                })
                .max()
                .unwrap_or(0);
            (id, &id[..(shared + 1).max(8).min(id.len())])
        })
        .collect()
}

pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("projection path has no parent")?;
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("projection");
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .suffix(".tmp")
        .tempfile_in(parent)
        .with_context(|| format!("failed to stage {}", path.display()))?;
    temporary.write_all(bytes)?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

/// `write_atomically`, and durable before it returns: the bytes are synced
/// before the rename and the directory after, so a crash cannot lose them.
pub(crate) fn write_durably(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("durable path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".durable.")
        .suffix(".tmp")
        .tempfile_in(parent)
        .with_context(|| format!("failed to stage {}", path.display()))?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn write_text(path: &Path, value: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, value).with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod short_id_tests {
    use super::short_ids;

    #[test]
    fn short_ids_grow_only_as_far_as_needed_to_tell_runs_apart() {
        let ids = [
            "01M3QJF0JQ3W5G6T6KGGAG2DXT",
            "01M3QJF0JZEXYGR88J1BKR10H4",
            "01M3QJEXS32PNWDWKW006JZ6N7",
            "01M4AAAAAAAAAAAAAAAAAAAAAA",
        ];
        let short = short_ids(&ids);
        assert_eq!(short[ids[0]], "01M3QJF0JQ");
        assert_eq!(short[ids[1]], "01M3QJF0JZ");
        assert_eq!(short[ids[2]], "01M3QJEX");
        assert_eq!(short[ids[3]], "01M4AAAA");
        assert_eq!(short_ids(&[ids[0]])[ids[0]], "01M3QJF0");
    }
}
