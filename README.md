# Dispatch download and traffic statistics

The nightly `stats` workflow writes these files from GitHub's API
(`scripts/stats.py` on the default branch). Dispatch itself sends nothing
anywhere: these are GitHub's own counts. Dates are UTC, rows are sorted, and
a rerun on the same day replaces that day's rows.

- `downloads.csv`: `date,tag,asset,downloads`. One row per release asset per
  snapshot date, with the cumulative download count GitHub reports for it
  on that date. An install with `curl -fsSL https://rundispatch.sh/install | sh`
  downloads `install.sh`, this platform's archive and `SHA256SUMS` once each.
- `views.csv` and `clones.csv`: `date,count,uniques`, per day. GitHub keeps
  14 days; each run rewrites the days it returns and keeps every older row.
  The latest day is partial until the next run.
- `referrers.csv`: `date,referrer,count,uniques`, and `paths.csv`:
  `date,path,count,uniques`. GitHub's top referring sites and most viewed
  paths over the 14 days before the snapshot date.

Traffic needs the `STATS_TOKEN` secret, a fine-grained token with read-only
Administration access to the repository. Without it, only downloads are
recorded.
