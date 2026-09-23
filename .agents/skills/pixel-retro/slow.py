"""Group the slow read ops of a window by how they were served.

Usage: python3 slow.py <window> <actions.jsonl>... [--min-ms 5000]
       (window: 12h, 7d, 3w, or an ISO date, as for roots.py)

Each slow line is attributed from its `serve` steps (pixel >= #241): the
route that answered, the in-process reason, and the phase that took the
most time. Lines are grouped on (command, route, reason, phase) with their
count, total and worst duration, and the invocation ids of the three worst,
so every number in the report can be re-opened. A line without `serve`
predates the field and lands in the `unattributed` group: its cause is
unknown, and a warm replay will not reproduce a cold start.

Runs of pixel's own test suite (cwd under a `crates/` directory or the
system temp dir) are counted apart, never mixed into the groups.
"""
import collections, datetime, json, re, sys

UTC = datetime.timezone.utc
READ_OPS = re.compile(r"(search-|find-).*|impact|who-calls|call-path|scope-task|pack-context|recall|status|repo-state")
PHASES = ("probe_ms", "start_ms", "request_ms", "open_ms", "handle_ms")
TEST_CWD = re.compile(r"/crates/|^/(private/)?var/folders/")


def parse_window(arg, now):
    m = re.fullmatch(r"(\d+)([hdw])", arg)
    if m:
        hours = {"h": 1, "d": 24, "w": 168}[m.group(2)] * int(m.group(1))
        return now - datetime.timedelta(hours=hours)
    start = datetime.datetime.fromisoformat(arg)
    return start if start.tzinfo else start.replace(tzinfo=UTC)


def attribute(event):
    """(route, reason, dominant phase) of one event; the step with the
    longest phase decides, since the first request usually pays a cold start."""
    best = None
    for step in event.get("serve") or []:
        for phase in PHASES:
            ms = step.get(phase)
            if isinstance(ms, int) and (best is None or ms > best[0]):
                best = (ms, step.get("route", "?"), step.get("reason", "-"), phase)
    if best is None:
        return ("unattributed", "-", "-")
    return best[1:]


def main(argv):
    min_ms = 5000
    if "--min-ms" in argv:
        i = argv.index("--min-ms")
        min_ms = int(argv[i + 1])
        del argv[i:i + 2]
    if len(argv) < 2:
        sys.exit(__doc__)
    since_ms = parse_window(argv[0], datetime.datetime.now(UTC)).timestamp() * 1000
    groups = collections.defaultdict(list)
    skipped = 0
    for path in argv[1:]:
        with open(path) as log:
            for line in log:
                try:
                    event = json.loads(line)
                except ValueError:
                    continue
                if (event.get("ts_ms", 0) < since_ms
                        or event.get("duration_ms", 0) < min_ms
                        or not READ_OPS.fullmatch(event.get("command", ""))):
                    continue
                if TEST_CWD.search(event.get("cwd", "")):
                    skipped += 1
                    continue
                key = (event["command"],) + attribute(event)
                groups[key].append((event["duration_ms"], event.get("invocation_id", "?"), path))
    print(f"slow read ops >= {min_ms} ms since {argv[0]}: {sum(map(len, groups.values()))} line(s)"
          f", {skipped} from the test suite left out")
    for key, rows in sorted(groups.items(), key=lambda kv: -sum(r[0] for r in kv[1])):
        rows.sort(reverse=True)
        command, route, reason, phase = key
        repos = len({r[2] for r in rows})
        worst = ", ".join(r[1] for r in rows[:3])
        print(f"{command:<15} {route:<14} {reason:<19} {phase:<10} n={len(rows):<3} repos={repos} "
              f"total={sum(r[0] for r in rows)} max={rows[0][0]}  worst: {worst}")


if __name__ == "__main__":
    main(sys.argv[1:])
