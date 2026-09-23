"""Print the repository roots with a .pixel/actions.jsonl among the sessions of a window.

Usage: python3 roots.py <window>   (24h, 7d, an ISO date). Pages through
`pixel recall sessions`, whose --limit is capped at 200, with --until.
"""
import datetime, json, os, subprocess, sys
since, until, cwds = sys.argv[1], None, set()
while True:
    cmd = ["pixel", "recall", "sessions", "--since", since, "--limit", "200",
           "--subagents", "--json", "--metrics", "off"]
    if until:
        cmd += ["--until", until]
    page = json.loads(subprocess.run(cmd, capture_output=True, text=True, check=True).stdout)["sessions"]
    cwds.update(s["cwd"] for s in page)
    if len(page) < 200:
        break
    oldest = min(s["ts_last"] for s in page) - 1
    until = datetime.datetime.fromtimestamp(oldest / 1000, datetime.timezone.utc).isoformat()
roots = set()
for d in cwds:
    r = subprocess.run(["git", "-C", d, "rev-parse", "--show-toplevel"], capture_output=True, text=True)
    if r.returncode == 0 and os.path.isfile(os.path.join(r.stdout.strip(), ".pixel", "actions.jsonl")):
        roots.add(r.stdout.strip())
print(f"{len(cwds)} session cwds", file=sys.stderr)
print("\n".join(sorted(roots)))
