//! `scripts/install.sh` against a local release directory: what a release
//! publishes (the archive and `SHA256SUMS`), served from `file://`. Nothing
//! is fetched from the network, and the install directory, home and state
//! are temporary.
#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};

use serde_json::Value;
use sha2::{Digest, Sha256};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The release archive this machine installs. Elsewhere `uname` is made to
/// claim Linux on x86_64, whose archive then holds this machine's binary.
fn platform() -> (&'static str, Option<(&'static str, &'static str)>) {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => ("macos-arm64", None),
        ("linux", "x86_64") => ("linux-x86_64", None),
        _ => ("linux-x86_64", Some(("Linux", "x86_64"))),
    }
}

struct Install {
    temp: tempfile::TempDir,
    release: PathBuf,
    bin: PathBuf,
    home: PathBuf,
    /// Commands the script finds first, such as a `uname` that says
    /// otherwise.
    tools: PathBuf,
}

impl Install {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let install = Self {
            release: base.join("release"),
            bin: base.join("bin"),
            home: base.join("home"),
            tools: base.join("tools"),
            temp,
        };
        for dir in [&install.release, &install.home, &install.tools] {
            fs::create_dir_all(dir).unwrap();
        }
        if let (_, Some((system, machine))) = platform() {
            install.uname(system, machine);
        }
        install.publish(None);
        install
    }

    fn asset(&self) -> String {
        format!("dispatch-{}.tar.gz", platform().0)
    }

    /// Publish this build as the release; `checksum` overrides the one
    /// `SHA256SUMS` lists.
    fn publish(&self, checksum: Option<&str>) {
        let package = self.temp.path().join("package");
        let _ = fs::remove_dir_all(&package);
        fs::create_dir_all(&package).unwrap();
        fs::copy(assert_cmd::cargo_bin!("dispatch"), package.join("dispatch")).unwrap();
        fs::write(package.join("LICENSE"), "test\n").unwrap();
        let archive = self.release.join(self.asset());
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&package)
            .args(["LICENSE", "dispatch"])
            .status()
            .unwrap();
        assert!(status.success());
        let digest = hex::encode(Sha256::digest(fs::read(&archive).unwrap()));
        fs::write(
            self.release.join("SHA256SUMS"),
            format!(
                "{}  dispatch-other.tar.gz\n{}  {}\n",
                "0".repeat(64),
                checksum.unwrap_or(&digest),
                self.asset()
            ),
        )
        .unwrap();
    }

    /// A `uname` that reports `system` and `machine`.
    fn uname(&self, system: &str, machine: &str) {
        let path = self.tools.join("uname");
        fs::write(
            &path,
            format!("#!/bin/sh\ncase \"$1\" in -s) echo {system} ;; -m) echo {machine} ;; esac\n"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `sh scripts/install.sh` with only the system's directories on `PATH`.
    fn run(&self, path: &str) -> Output {
        Command::new("sh")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh"))
            .env_clear()
            .env("HOME", &self.home)
            .env("SHELL", "/bin/zsh")
            .env("PATH", format!("{}:{path}", self.tools.display()))
            .env(
                "DISPATCH_RELEASE_URL",
                format!("file://{}", self.release.display()),
            )
            .env("DISPATCH_INSTALL_DIR", &self.bin)
            .output()
            .unwrap()
    }

    fn install(&self) -> Output {
        self.run(SYSTEM_PATH)
    }

    fn installed(&self) -> PathBuf {
        self.bin.join("dispatch")
    }
}

const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only probes.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(300);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn installs_into_the_directory_and_prints_the_path_line_without_editing_a_dotfile() {
    let install = Install::new();
    let output = install.install();
    assert!(output.status.success(), "{}", text(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!(
            "Installed dispatch {VERSION} at {}",
            install.installed().display()
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "{} is not on your PATH. Add this line to ~/.zshrc, then open a new terminal:\n\n  \
             export PATH=\"{}:$PATH\"\n",
            install.bin.display(),
            install.bin.display()
        )),
        "{stdout}"
    );
    let mode = fs::metadata(install.installed())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    let version = Command::new(install.installed())
        .arg("version")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&version.stdout),
        format!("dispatch {VERSION}\n")
    );
    assert_eq!(
        fs::read_dir(&install.home).unwrap().count(),
        0,
        "no dotfile"
    );
    assert_eq!(
        fs::read_dir(&install.bin).unwrap().count(),
        1,
        "no leftovers"
    );

    // On PATH already: nothing more to say.
    let output = install.run(&format!("{}:{SYSTEM_PATH}", install.bin.display()));
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        !text(&output).contains("not on your PATH"),
        "{}",
        text(&output)
    );
}

/// Kills an owner a test left running.
struct Owner(Option<i32>);

impl Drop for Owner {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // SAFETY: the owner this test started.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// Installing over a binary that is running, as a watching owner: the file
/// is replaced, never written into, so the owner keeps running; and the
/// installed path is the executable its record names, so the new binary may
/// replace it on another version.
#[test]
fn an_upgrade_replaces_a_running_binary_and_its_owner_keeps_watching() {
    let install = Install::new();
    assert!(install.install().status.success());
    let project = install.temp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("dispatch.yml"), "coherence:\n  poll_secs: 1\n").unwrap();
    let project = fs::canonicalize(project).unwrap();
    let state = install.temp.path().join("state");
    let dispatch = |args: &[&str]| {
        Command::new(install.installed())
            .arg("--state-dir")
            .arg(&state)
            .args(args)
            .current_dir(&project)
            .output()
            .unwrap()
    };
    let key = hex::encode(Sha256::digest(project.to_string_lossy().as_bytes()));
    let record_path = state.join("watchers").join(format!("{key}.json"));
    let record = || -> Value { serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap() };
    let pid = |record: &Value| record["identity"]["pid"].as_i64().unwrap() as i32;

    let started = dispatch(&["start"]);
    assert!(started.status.success(), "{}", text(&started));
    let mut owner = Owner(Some(pid(&record())));
    assert_eq!(record()["exe"], install.installed().to_str().unwrap());
    let before = fs::metadata(install.installed()).unwrap().ino();

    let upgrade = install.install();
    assert!(upgrade.status.success(), "{}", text(&upgrade));
    assert_ne!(
        fs::metadata(install.installed()).unwrap().ino(),
        before,
        "a new file, renamed into place"
    );
    assert!(alive(owner.0.unwrap()), "the running owner was not killed");
    let status = dispatch(&["status"]);
    assert!(status.status.success(), "{}", text(&status));
    assert!(status.stderr.is_empty(), "{}", text(&status));
    assert_eq!(pid(&record()), owner.0.unwrap());

    // As if the owner still ran the version installed before.
    let mut old = record();
    old["dispatch_version"] = "0.0.1".into();
    fs::write(&record_path, old.to_string()).unwrap();
    let status = dispatch(&["status"]);
    assert!(status.status.success(), "{}", text(&status));
    assert!(
        String::from_utf8_lossy(&status.stderr).contains(&format!(
            "Restarted watching {} with Dispatch {VERSION} (was 0.0.1).",
            project.display()
        )),
        "{}",
        text(&status)
    );
    let previous = owner.0.unwrap();
    wait_until("the old owner to exit", || !alive(previous));
    owner.0 = Some(pid(&record()));
    assert_ne!(owner.0, Some(previous));

    let stop = dispatch(&["stop"]);
    assert!(stop.status.success(), "{}", text(&stop));
    let stopped = owner.0.take().unwrap();
    wait_until("the owner to exit", || !alive(stopped));
}

#[test]
fn a_bad_checksum_installs_nothing() {
    let install = Install::new();
    install.publish(Some(&"1".repeat(64)));
    let output = install.install();
    assert!(!output.status.success(), "{}", text(&output));
    assert!(
        text(&output).contains("does not match SHA256SUMS"),
        "{}",
        text(&output)
    );
    assert!(!install.installed().exists());

    // Over an installed binary, which stays as it was.
    install.publish(None);
    assert!(install.install().status.success());
    let before = fs::metadata(install.installed()).unwrap().ino();
    install.publish(Some(&"1".repeat(64)));
    assert!(!install.install().status.success());
    assert_eq!(fs::metadata(install.installed()).unwrap().ino(), before);
    assert_eq!(
        fs::read_dir(&install.bin).unwrap().count(),
        1,
        "no leftovers"
    );
}

#[test]
fn another_platform_is_refused_saying_which() {
    let install = Install::new();
    install.uname("FreeBSD", "amd64");
    let output = install.install();
    assert!(!output.status.success(), "{}", text(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "dispatch install: there is no Dispatch release for FreeBSD amd64; releases are built \
         for macOS on Apple silicon (arm64) and Linux on x86_64\n"
    );
    assert!(!install.bin.exists());
}
