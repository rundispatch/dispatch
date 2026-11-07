//! Cross-process ownership: `flock`-based operation locks on a run, a source
//! or a serve root, and the shutdown signals every foreground owner honors.
//! These locks are what let `run`, `attach`, `finish`, `accept` and `serve`
//! coexist as separate processes without a daemon.

use std::{fs, path::Path, time::Duration};

use anyhow::{Result, bail};

use crate::executor::CancellationToken;

pub(crate) struct OperationLock {
    file: fs::File,
    #[cfg(not(unix))]
    path: PathBuf,
}

/// Outcome of a single, non-blocking acquisition attempt.
enum TryAcquire {
    Acquired(OperationLock),
    /// The lock is held by someone else. Carries the OS error text captured
    /// at the moment of failure (unix only; empty on other platforms, where
    /// the original error text never included it either), so callers can
    /// format the busy message without re-reading `errno` later, which
    /// could otherwise be clobbered by the failed attempt's own cleanup.
    Busy(String),
}

impl OperationLock {
    pub(crate) fn acquire(path: &Path, busy_message: &str) -> Result<Self> {
        match Self::try_acquire(path)? {
            TryAcquire::Acquired(lock) => Ok(lock),
            TryAcquire::Busy(detail) => {
                #[cfg(unix)]
                {
                    bail!("{busy_message}: {detail}");
                }
                #[cfg(not(unix))]
                {
                    let _ = detail;
                    bail!("{busy_message}");
                }
            }
        }
    }

    /// Like `acquire`, but wait up to `timeout` for the lock, retrying every
    /// 100 ms. On expiry the error is the same busy message with the wait
    /// appended, so callers and tests can recognize both cases. Used by
    /// `auto_apply` for the run and source locks, where a bounded wait lets a
    /// finishing owner queue behind a human review or another applier instead
    /// of failing immediately.
    pub(crate) fn acquire_wait(path: &Path, busy_message: &str, timeout: Duration) -> Result<Self> {
        match Self::acquire_within(path, timeout)? {
            Some(lock) => Ok(lock),
            None => bail!(
                "{busy_message}: still busy after waiting {}s",
                timeout.as_secs()
            ),
        }
    }

    /// `acquire_wait` for a caller that tries again later: `None` when the
    /// lock is still held after `timeout`.
    pub(crate) fn acquire_within(path: &Path, timeout: Duration) -> Result<Option<Self>> {
        let start = std::time::Instant::now();
        loop {
            if let TryAcquire::Acquired(lock) = Self::try_acquire(path)? {
                return Ok(Some(lock));
            }
            if start.elapsed() >= timeout {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Whether another process holds the lock, without keeping it: `true`
    /// only for lock contention. A refused symlink or a filesystem error is
    /// an error, never taken to mean someone holds the lock.
    pub(crate) fn is_held(path: &Path) -> Result<bool> {
        Ok(matches!(Self::try_acquire(path)?, TryAcquire::Busy(_)))
    }

    fn try_acquire(path: &Path) -> Result<TryAcquire> {
        if let Some(parent) = path.parent() {
            let parent_is_new = !parent.exists();
            fs::create_dir_all(parent)?;
            #[cfg(unix)]
            if parent_is_new {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
        }
        if let Ok(metadata) = fs::symlink_metadata(path) {
            anyhow::ensure!(
                !metadata.file_type().is_symlink(),
                "refusing operation lock symlink {}",
                path.display()
            );
        }

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)?;
            // SAFETY: the descriptor remains owned by this guard until Drop.
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                // Captured now, before `file` is dropped: a successful
                // close() below must not be allowed to clobber `errno`.
                let detail = std::io::Error::last_os_error().to_string();
                return Ok(TryAcquire::Busy(detail));
            }
            Ok(TryAcquire::Acquired(Self { file }))
        }

        #[cfg(not(unix))]
        {
            match fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(file) => Ok(TryAcquire::Acquired(Self {
                    file,
                    path: path.to_path_buf(),
                })),
                Err(_) => Ok(TryAcquire::Busy(String::new())),
            }
        }
    }
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: this unlocks only the descriptor locked by acquire.
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(crate) struct SignalListener {
    task: tokio::task::JoinHandle<()>,
}

impl SignalListener {
    pub(crate) fn install(cancellation: CancellationToken) -> Self {
        let task = tokio::spawn(async move {
            shutdown_signal().await;
            cancellation.cancel();
        });
        Self { task }
    }
}

impl Drop for SignalListener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(unix)]
pub(crate) async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    if let (Ok(mut terminate), Ok(mut hangup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
            _ = hangup.recv() => {}
        }
    } else {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(not(unix))]
pub(crate) async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod operation_lock_tests {
    use super::*;

    /// `OperationLock` intentionally does not implement `Debug`, so extract
    /// the error text by hand instead of via `Result::unwrap_err`.
    fn expect_err(result: Result<OperationLock>) -> String {
        match result {
            Ok(_) => panic!("expected an error, got a lock"),
            Err(err) => err.to_string(),
        }
    }

    #[test]
    fn acquire_wait_returns_immediately_when_free() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("lock");
        let start = std::time::Instant::now();
        let lock = OperationLock::acquire_wait(&path, "busy", Duration::from_secs(5)).unwrap();
        assert!(start.elapsed() < Duration::from_millis(100));
        drop(lock);
    }

    #[test]
    fn acquire_wait_waits_for_a_held_lock() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("lock");
        let holder = OperationLock::acquire(&path, "busy").unwrap();

        let waiter_path = path.clone();
        let handle = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let lock =
                OperationLock::acquire_wait(&waiter_path, "busy", Duration::from_secs(5)).unwrap();
            (start.elapsed(), lock)
        });

        std::thread::sleep(Duration::from_millis(300));
        drop(holder);

        let (elapsed, lock) = handle.join().unwrap();
        assert!(elapsed >= Duration::from_millis(250));
        assert!(elapsed < Duration::from_secs(3));
        drop(lock);
    }

    #[test]
    fn acquire_wait_times_out_with_the_busy_message() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("lock");
        let holder = OperationLock::acquire(&path, "busy").unwrap();

        let waiter_path = path.clone();
        let handle = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let result = OperationLock::acquire_wait(
                &waiter_path,
                "another apply",
                Duration::from_millis(300),
            );
            (start.elapsed(), result)
        });

        let (elapsed, result) = handle.join().unwrap();
        drop(holder);

        let err = expect_err(result);
        assert!(err.contains("another apply"), "{err}");
        assert!(err.contains("still busy after waiting 0s"), "{err}");
        assert!(elapsed >= Duration::from_millis(300));
    }

    #[cfg(unix)]
    #[test]
    fn is_held_means_contention_and_nothing_else() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("lock");
        assert!(!OperationLock::is_held(&path).unwrap());
        let holder = OperationLock::acquire(&path, "busy").unwrap();
        assert!(OperationLock::is_held(&path).unwrap());
        drop(holder);
        assert!(!OperationLock::is_held(&path).unwrap());

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let error = OperationLock::is_held(&link).unwrap_err().to_string();
        assert!(error.contains("refusing operation lock symlink"), "{error}");
    }

    #[test]
    fn acquire_wait_zero_timeout_matches_acquire() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("lock");
        let holder = OperationLock::acquire(&path, "busy").unwrap();

        let start = std::time::Instant::now();
        let err = expect_err(OperationLock::acquire_wait(&path, "busy", Duration::ZERO));
        assert!(start.elapsed() < Duration::from_millis(100));
        assert!(err.contains("busy"), "{err}");
        drop(holder);
    }

    #[cfg(unix)]
    #[test]
    fn acquire_wait_does_not_retry_non_busy_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"not a lock").unwrap();
        let link = dir.path().join("lock");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let start = std::time::Instant::now();
        let err = expect_err(OperationLock::acquire_wait(
            &link,
            "busy",
            Duration::from_secs(5),
        ));
        assert!(start.elapsed() < Duration::from_millis(100));
        assert!(err.contains("refusing operation lock symlink"), "{err}");
    }
}
