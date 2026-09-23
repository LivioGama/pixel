"""Print the repository roots with a .pixel/actions.jsonl among the sessions of a window.

Usage: python3 roots.py <window>   (12h, 7d, 3w, or an ISO date)

`pixel recall sessions` caps --limit at 200, has no offset, filters
`ts_last >= since AND ts_first <= until` and orders by ts_last, so no page
cursor is exact. The window is cut into slices instead: a slice that returns
fewer than 200 sessions is complete, a full one is split in two. A session
spanning several slices is counted once (by id).
"""
import datetime, json, os, re, subprocess, sys

CAP = 200
UTC = datetime.timezone.utc


def parse_window(arg, now):
    m = re.fullmatch(r"(\d+)([hdw])", arg)
    if m:
        hours = {"h": 1, "d": 24, "w": 168}[m.group(2)] * int(m.group(1))
        return now - datetime.timedelta(hours=hours)
    start = datetime.datetime.fromisoformat(arg)
    return start if start.tzinfo else start.replace(tzinfo=UTC)


def sessions(since, until):
    cmd = ["pixel", "recall", "sessions", "--since", since.isoformat(),
           "--until", until.isoformat(), "--limit", str(CAP), "--subagents",
           "--json", "--metrics", "off"]
    out = subprocess.run(cmd, capture_output=True, text=True, check=True).stdout
    return json.loads(out)["sessions"]


def collect(since, until, seen, incomplete):
    page = sessions(since, until)
    if len(page) < CAP:
        seen.update((s["id"], s["cwd"]) for s in page)
    elif until - since <= datetime.timedelta(seconds=1):
        seen.update((s["id"], s["cwd"]) for s in page)
        incomplete.append(since)
    else:
        mid = since + (until - since) / 2
        collect(since, mid, seen, incomplete)
        collect(mid, until, seen, incomplete)


now = datetime.datetime.now(UTC)
seen, incomplete = set(), []
collect(parse_window(sys.argv[1], now), now, seen, incomplete)
roots = set()
for d in {cwd for _, cwd in seen}:
    r = subprocess.run(["git", "-C", d, "rev-parse", "--show-toplevel"],
                       capture_output=True, text=True)
    top = r.stdout.strip()
    if r.returncode == 0 and os.path.isfile(os.path.join(top, ".pixel", "actions.jsonl")):
        roots.add(top)
print(f"{len(seen)} sessions, {len({c for _, c in seen})} cwds", file=sys.stderr)
for t in incomplete:
    print(f"warning: {CAP}+ sessions overlap {t.isoformat()}, some may be missing", file=sys.stderr)
print("\n".join(sorted(roots)))
