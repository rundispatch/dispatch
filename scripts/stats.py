#!/usr/bin/env python3
"""Record this repository's release downloads and traffic as CSV files.

  python3 scripts/stats.py --out <stats checkout> [--repo owner/name]
                           [--date YYYY-MM-DD] [--responses <dir>]

The nightly stats workflow runs it on the `stats` branch. Downloads are read
with GH_TOKEN; traffic (views, clones, referrers, paths) only with
STATS_TOKEN, a token with read access to the repository's Administration,
and otherwise the traffic files are left as they are. Each run replaces the
snapshot date's rows, so running twice in one day records the day once.

--responses reads each API response from a JSON file in that directory
instead of GitHub, named after the request: releases?per_page=100&page=2 is
releases-per_page-100-page-2.json, traffic/popular/paths is
traffic-popular-paths.json.

Nothing here is part of Dispatch, which reports nothing anywhere.
"""
import argparse, csv, datetime, json, os, re, sys, urllib.error, urllib.request
from pathlib import Path

README = """# Dispatch download and traffic statistics

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
"""


def fetch(api, repo, token, responses, path):
    if responses:
        name = re.sub(r'[/?&=]+', '-', path) + '.json'
        return json.loads((Path(responses) / name).read_text())
    headers = {'Accept': 'application/vnd.github+json', 'User-Agent': 'dispatch-stats',
               'X-GitHub-Api-Version': '2022-11-28'}
    if token:
        headers['Authorization'] = f'Bearer {token}'
    request = urllib.request.Request(f'{api}/repos/{repo}/{path}', headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        raise SystemExit(f'GET /repos/{repo}/{path}: {error.code} {error.reason}')


def merge(path, header, rows, replace):
    """Write `rows` into the CSV at `path`, dropping its rows whose date is in
    `replace` and keeping every other row, sorted by its columns."""
    kept = []
    if path.exists():
        with path.open(newline='') as f:
            existing = list(csv.reader(f))
        if existing and existing[0] != header:
            raise SystemExit(f'{path} has columns {existing[0]}, not {header}')
        kept = [row for row in existing[1:] if row[0] not in replace]
    numeric = [i for i, column in enumerate(header) if column in ('downloads', 'count', 'uniques')]
    rows = sorted(kept + [[str(value) for value in row] for row in rows],
                  key=lambda row: [row[i] for i in range(len(row)) if i not in numeric])
    with path.open('w', newline='') as f:
        writer = csv.writer(f, lineterminator='\n')
        writer.writerow(header)
        writer.writerows(rows)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--out', type=Path, default=Path('.'))
    parser.add_argument('--repo', default=os.environ.get('GITHUB_REPOSITORY', 'rundispatch/dispatch'))
    parser.add_argument('--date', default=datetime.datetime.now(datetime.timezone.utc).date().isoformat())
    parser.add_argument('--responses', help='read API responses from this directory instead of GitHub')
    args = parser.parse_args()
    api = os.environ.get('GITHUB_API_URL', 'https://api.github.com')
    get = lambda token, path: fetch(api, args.repo, token, args.responses, path)
    args.out.mkdir(parents=True, exist_ok=True)
    (args.out / 'README.md').write_text(README)

    downloads, page = [], 1
    while True:
        releases = get(os.environ.get('GH_TOKEN', ''), f'releases?per_page=100&page={page}')
        if not releases:
            break
        downloads += [(args.date, release['tag_name'], asset['name'], asset['download_count'])
                      for release in releases if not release.get('draft') for asset in release['assets']]
        page += 1
    merge(args.out / 'downloads.csv', ['date', 'tag', 'asset', 'downloads'], downloads, {args.date})
    print(f'downloads: {len(downloads)} assets on {args.date}')

    token = os.environ.get('STATS_TOKEN', '')
    if not token:
        print('traffic: skipped, STATS_TOKEN is not set')
        return
    for kind in ('views', 'clones'):
        days = [(day['timestamp'][:10], day['count'], day['uniques'])
                for day in get(token, f'traffic/{kind}')[kind]]
        merge(args.out / f'{kind}.csv', ['date', 'count', 'uniques'], days, {day[0] for day in days})
        print(f'{kind}: {len(days)} days')
    for kind, column in (('referrers', 'referrer'), ('paths', 'path')):
        top = [(args.date, entry[column], entry['count'], entry['uniques'])
               for entry in get(token, f'traffic/popular/{kind}')]
        merge(args.out / f'{kind}.csv', ['date', column, 'count', 'uniques'], top, {args.date})
        print(f'{kind}: {len(top)} on {args.date}')


if __name__ == '__main__':
    sys.exit(main())
