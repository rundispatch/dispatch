//! `scripts/stats.py` against recorded GitHub API responses in
//! `tests/fixtures/stats`: the exact CSVs, the merge with rows already on the
//! `stats` branch, a second run on the same day, and traffic left alone
//! without a token. Nothing is fetched from the network.
#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const DATE: &str = "2026-10-21";

fn stats(out: &Path, traffic_token: Option<&str>) -> Output {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut command = Command::new("python3");
    command
        .arg(root.join("scripts/stats.py"))
        .arg("--out")
        .arg(out)
        .args(["--date", DATE, "--responses"])
        .arg(root.join("tests/fixtures/stats"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env_remove("GH_TOKEN")
        .env_remove("STATS_TOKEN")
        .env_remove("GITHUB_API_URL");
    if let Some(token) = traffic_token {
        command.env("STATS_TOKEN", token);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn read(out: &Path, name: &str) -> String {
    fs::read_to_string(out.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn files(out: &Path) -> Vec<(PathBuf, String)> {
    let mut files: Vec<_> = fs::read_dir(out)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let text = fs::read_to_string(&path).unwrap();
            (path, text)
        })
        .collect();
    files.sort();
    files
}

/// Releases are paginated to the empty page; a draft has no public
/// downloads and no row.
const DOWNLOADS: &str = "\
date,tag,asset,downloads
2026-10-21,v0.4.0-rc.1,dispatch-macos-arm64.tar.gz,3
2026-10-21,v0.4.16,SHA256SUMS,41
2026-10-21,v0.4.16,dispatch-linux-x86_64.tar.gz,12
2026-10-21,v0.4.16,dispatch-macos-arm64.tar.gz,37
";

#[test]
fn without_a_traffic_token_records_downloads_and_leaves_traffic_files_alone() {
    let temp = tempfile::tempdir().unwrap();
    let out = temp.path();
    let views = "date,count,uniques\n2026-09-30,5,1\n";
    fs::write(out.join("views.csv"), views).unwrap();

    let output = stats(out, None);
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("traffic: skipped, STATS_TOKEN is not set"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(read(out, "downloads.csv"), DOWNLOADS);
    assert_eq!(read(out, "views.csv"), views);
    let names: Vec<_> = files(out)
        .into_iter()
        .map(|(path, _)| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["README.md", "downloads.csv", "views.csv"]);
    let readme = read(out, "README.md");
    for line in [
        "`downloads.csv`: `date,tag,asset,downloads`",
        "`views.csv` and `clones.csv`: `date,count,uniques`",
        "`referrers.csv`: `date,referrer,count,uniques`",
        "`paths.csv`:\n  `date,path,count,uniques`",
        "downloads `install.sh`, this platform's archive and `SHA256SUMS` once each",
    ] {
        assert!(readme.contains(line), "{line}\n{readme}");
    }
}

/// With rows already recorded: the snapshot date's rows and the days the
/// API returns are replaced, every other row is kept, and a second run the
/// same day changes nothing.
#[test]
fn merges_into_recorded_rows_and_a_second_run_the_same_day_changes_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let out = temp.path();
    fs::write(
        out.join("downloads.csv"),
        "date,tag,asset,downloads\n\
         2026-10-21,v0.4.16,dispatch-macos-arm64.tar.gz,1\n\
         2026-10-20,v0.4.16,dispatch-macos-arm64.tar.gz,30\n",
    )
    .unwrap();
    fs::write(
        out.join("views.csv"),
        "date,count,uniques\n2026-10-19,99,9\n2026-09-30,5,1\n",
    )
    .unwrap();
    fs::write(
        out.join("referrers.csv"),
        "date,referrer,count,uniques\n2026-10-20,github.com,6,2\n2026-10-21,old.example,1,1\n",
    )
    .unwrap();

    stats(out, Some("recorded"));
    assert_eq!(
        read(out, "downloads.csv"),
        format!(
            "date,tag,asset,downloads\n2026-10-20,v0.4.16,dispatch-macos-arm64.tar.gz,30\n{}",
            DOWNLOADS.split_once('\n').unwrap().1
        )
    );
    assert_eq!(
        read(out, "views.csv"),
        "date,count,uniques\n\
         2026-09-30,5,1\n\
         2026-10-19,12,4\n\
         2026-10-20,15,6\n\
         2026-10-21,3,2\n"
    );
    assert_eq!(
        read(out, "clones.csv"),
        "date,count,uniques\n2026-10-20,7,3\n"
    );
    assert_eq!(
        read(out, "referrers.csv"),
        "date,referrer,count,uniques\n\
         2026-10-20,github.com,6,2\n\
         2026-10-21,github.com,8,3\n\
         2026-10-21,news.ycombinator.com,2,2\n\
         2026-10-21,rundispatch.sh,20,7\n"
    );
    assert_eq!(
        read(out, "paths.csv"),
        "date,path,count,uniques\n\
         2026-10-21,/rundispatch/dispatch,25,8\n\
         2026-10-21,/rundispatch/dispatch/releases,5,3\n"
    );

    let first = files(out);
    stats(out, Some("recorded"));
    assert_eq!(files(out), first);
}
