//! `scripts/install.sh` against a local release directory: what a release
//! publishes (the archives and `SHA256SUMS`), served from `file://` or from
//! a local server that redirects as the website and GitHub do. Nothing is
//! fetched from the network, and the install directory, home and state are
//! temporary.
#![cfg(unix)]

use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
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
        ("macos", "x86_64") => ("macos-x86_64", None),
        ("linux", "x86_64") => ("linux-x86_64", None),
        ("linux", "aarch64") => ("linux-aarch64", None),
        _ => ("linux-x86_64", Some(("Linux", "x86_64"))),
    }
}

/// Every archive a release publishes.
const PLATFORMS: [&str; 4] = [
    "macos-arm64",
    "macos-x86_64",
    "linux-x86_64",
    "linux-aarch64",
];

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

    /// Publish this build as the release; `checksum` overrides the one
    /// `SHA256SUMS` lists.
    fn publish(&self, checksum: Option<&str>) {
        let binary = fs::read(assert_cmd::cargo_bin!("dispatch")).unwrap();
        self.release_of(&[(platform().0, &binary)], checksum);
    }

    /// Publish a release of these platforms' archives, each holding its
    /// `dispatch`, and nothing else.
    fn release_of(&self, archives: &[(&str, &[u8])], checksum: Option<&str>) {
        fs::remove_dir_all(&self.release).unwrap();
        fs::create_dir_all(&self.release).unwrap();
        let mut sums = format!("{}  dispatch-other.tar.gz\n", "0".repeat(64));
        for (platform, binary) in archives {
            let package = self.temp.path().join("package");
            let _ = fs::remove_dir_all(&package);
            fs::create_dir_all(&package).unwrap();
            fs::write(package.join("dispatch"), binary).unwrap();
            fs::set_permissions(package.join("dispatch"), fs::Permissions::from_mode(0o755))
                .unwrap();
            fs::write(package.join("LICENSE"), "test\n").unwrap();
            let asset = format!("dispatch-{platform}.tar.gz");
            let archive = self.release.join(&asset);
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
            sums.push_str(&format!("{}  {asset}\n", checksum.unwrap_or(&digest)));
        }
        fs::write(self.release.join("SHA256SUMS"), sums).unwrap();
    }

    /// A `sysctl` that says whether this shell runs under Rosetta, or, with
    /// `None`, fails as on an Intel Mac, which lacks the OID.
    fn sysctl(&self, translated: Option<&str>) {
        let path = self.tools.join("sysctl");
        let answer = match translated {
            Some(value) => format!("echo {value}"),
            None => "echo 'unknown oid' >&2; exit 1".into(),
        };
        fs::write(
            &path,
            format!("#!/bin/sh\n[ \"$*\" = \"-n sysctl.proc_translated\" ] || exit 1\n{answer}\n"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
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
         for macOS on Apple silicon (arm64) and Intel (x86_64), and Linux on x86_64 and \
         aarch64\n"
    );
    assert!(!install.bin.exists());
}

/// A `dispatch` that says which archive it came from.
fn fake(platform: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho \"dispatch from {platform}\"\n").into_bytes()
}

/// Each machine installs its own archive from a release of all four: what
/// `uname` says, and on an Intel-looking Mac whether the shell is a Rosetta
/// one on Apple silicon.
#[test]
fn each_platform_installs_its_own_archive() {
    let install = Install::new();
    let fakes: Vec<_> = PLATFORMS.iter().map(|p| (*p, fake(p))).collect();
    let archives: Vec<_> = fakes.iter().map(|(p, b)| (*p, b.as_slice())).collect();
    install.release_of(&archives, None);
    for (system, machine, translated, expected) in [
        ("Darwin", "arm64", Some("0"), "macos-arm64"),
        ("Darwin", "x86_64", Some("0"), "macos-x86_64"),
        ("Darwin", "x86_64", None, "macos-x86_64"),
        ("Darwin", "x86_64", Some("1"), "macos-arm64"),
        // Only a Mac asks.
        ("Linux", "x86_64", Some("1"), "linux-x86_64"),
        ("Linux", "aarch64", Some("1"), "linux-aarch64"),
        ("Linux", "arm64", Some("1"), "linux-aarch64"),
    ] {
        install.uname(system, machine);
        install.sysctl(translated);
        let output = install.install();
        let case = format!("{system} {machine} {translated:?}");
        assert!(output.status.success(), "{case}: {}", text(&output));
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(&format!(
                "Downloading dispatch-{expected}.tar.gz from file://"
            )),
            "{case}: {}",
            text(&output)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(&format!(
                "Installed dispatch from {expected} at {}",
                install.installed().display()
            )),
            "{case}: {}",
            text(&output)
        );
        assert!(output.stderr.is_empty(), "{case}: {}", text(&output));
    }
}

/// 0.4.16 published macOS on arm64 and Linux on x86_64 only: on another of
/// the four machines its release says it has no build for it, before
/// downloading anything.
#[test]
fn a_release_without_this_platforms_archive_says_so() {
    let install = Install::new();
    let fakes = [
        ("macos-arm64", fake("macos-arm64")),
        ("linux-x86_64", fake("linux-x86_64")),
    ];
    install.release_of(
        &[(fakes[0].0, &fakes[0].1), (fakes[1].0, &fakes[1].1)],
        None,
    );
    for (system, machine, name, asset) in [
        ("Linux", "aarch64", "Linux on aarch64", "linux-aarch64"),
        ("Darwin", "x86_64", "macOS on Intel", "macos-x86_64"),
    ] {
        install.uname(system, machine);
        install.sysctl(Some("0"));
        let output = install.install();
        assert!(!output.status.success(), "{}", text(&output));
        assert_eq!(
            text(&output),
            format!(
                "dispatch install: this release has no build for {name}: file://{}/SHA256SUMS \
                 lists no dispatch-{asset}.tar.gz; nothing was installed\n",
                install.release.display()
            )
        );
        assert!(!install.bin.exists());
    }
}

/// Serves a release the way the website and GitHub do, through redirects.
struct Server(Child, u16);

impl Server {
    fn start(root: &Path) -> Self {
        let mut child = Command::new("python3")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/release_server.py"))
            .arg(root)
            .arg(format!("v{VERSION}"))
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let port = line.trim().parse().expect("the server prints its port");
        Self(child, port)
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.1)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `curl -fsSL <site>/install | sh`, from a directory that holds nothing:
/// the script comes through the site's redirect and GitHub's own, reads
/// nothing beside itself, and fetches its archive and `SHA256SUMS` through
/// GitHub's redirects too.
#[test]
fn installs_piped_from_curl_through_redirects() {
    let install = Install::new();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh"),
        install.release.join("install.sh"),
    )
    .unwrap();
    let server = Server::start(&install.release);
    let elsewhere = install.temp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let output = Command::new("sh")
        .args([
            "-c",
            "curl -fsSL \"$1/install\" | sh",
            "sh",
            &server.url(""),
        ])
        .current_dir(&elsewhere)
        .env_clear()
        .env("HOME", &install.home)
        .env("SHELL", "/bin/zsh")
        .env("PATH", format!("{}:{SYSTEM_PATH}", install.tools.display()))
        .env(
            "DISPATCH_RELEASE_URL",
            server.url("/releases/latest/download"),
        )
        .env("DISPATCH_INSTALL_DIR", &install.bin)
        .output()
        .unwrap();
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
        stdout.contains("not on your PATH"),
        "the script ran to its end: {stdout}"
    );
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}

/// The archive `scripts/package-local.py` names, by this machine or by an
/// explicit target, is the one `install.sh` asks for there, and its
/// `BUILD.json` says the same platform and architecture.
#[test]
fn the_packager_names_each_archive_as_the_installer_asks_for_it() {
    let install = Install::new();
    // The packager and what it reads, in a repository of their own: it
    // hashes the tracked sources and records HEAD.
    let source = install.temp.path().join("source");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for file in [
        "scripts/package-local.py",
        "Cargo.toml",
        "LICENSE",
        "docs/release-install.md",
    ] {
        fs::create_dir_all(source.join(file).parent().unwrap()).unwrap();
        fs::copy(root.join(file), source.join(file)).unwrap();
    }
    for args in [
        &["init", "-q"][..],
        &["add", "-A"],
        &[
            "-c",
            "user.name=Dispatch Tests",
            "-c",
            "user.email=tests@dispatch.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "source",
        ],
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(&source)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
    let built = install.temp.path().join("built");
    fs::create_dir_all(&built).unwrap();
    let binary = built.join("dispatch");
    fs::write(&binary, format!("#!/bin/sh\necho \"dispatch {VERSION}\"\n")).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    // This machine's own name only where it is one of the four.
    let native = platform().1.is_none().then_some(None);
    for target in native
        .into_iter()
        .chain([Some("linux-aarch64"), Some("macos-x86_64")])
    {
        fs::remove_dir_all(&install.release).unwrap();
        let mut package = Command::new("python3");
        package
            .arg(source.join("scripts/package-local.py"))
            .arg(&binary)
            .arg(&install.release)
            .env("PYTHONDONTWRITEBYTECODE", "1");
        if let Some(target) = target {
            package.args(["--target", target]);
        }
        let output = package.output().unwrap();
        assert!(output.status.success(), "{target:?}: {}", text(&output));
        let expected = target.unwrap_or(platform().0);
        assert!(
            install
                .release
                .join(format!("dispatch-{expected}.tar.gz"))
                .is_file(),
            "{target:?}: {}",
            text(&output)
        );
        let build: Value =
            serde_json::from_slice(&fs::read(install.release.join("BUILD.json")).unwrap()).unwrap();
        assert_eq!(
            format!(
                "{}-{}",
                build["platform"].as_str().unwrap(),
                build["architecture"].as_str().unwrap()
            ),
            expected
        );
        match expected {
            "linux-aarch64" => install.uname("Linux", "aarch64"),
            "macos-x86_64" => {
                install.uname("Darwin", "x86_64");
                install.sysctl(Some("0"));
            }
            _ => {}
        }
        let output = install.install();
        assert!(output.status.success(), "{target:?}: {}", text(&output));
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains(&format!("Downloading dispatch-{expected}.tar.gz from")),
            "{target:?}: {}",
            text(&output)
        );
    }
}
