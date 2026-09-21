#!/usr/bin/env python3
"""Natural-language retrieval: semble vs pixel's two search engines.

Each arm gets the identical query (a doc comment, see gen-queries.py) and is
scored on whether the file that comment documents appears in its top-k ranked
FILES, after de-duplicating repeated hits in the same file. recall@1/5/10, plus
what the answer cost in bytes and wall clock.

`gitnexus query` is deliberately absent: it returns execution flows, not a
ranked file list, so scoring it here would measure it on a question it does not
claim to answer. Its retrieval path is `context`/`impact`, benchmarked
separately in vs-gitnexus.md.

pixel has two engines that both take a phrase, so both are run: `search-meaning`
(semantic) and `find-code` (concept index). Reporting only the better of the two
would flatter pixel by letting it pick per query.
"""
import json
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

TOPK = 10
REPS = 3


def run(cmd, cwd):
    with tempfile.NamedTemporaryFile(delete=False) as fh:
        tmp = fh.name
    try:
        with open(tmp, "wb") as out:
            t0 = time.perf_counter()
            p = subprocess.run(cmd, cwd=cwd, stdout=out,
                               stderr=subprocess.DEVNULL, timeout=600)
            ms = (time.perf_counter() - t0) * 1000
        return ms, Path(tmp).read_bytes(), p.returncode
    finally:
        Path(tmp).unlink(missing_ok=True)


def dedup(paths):
    seen, out = set(), []
    for p in paths:
        if p and p not in seen:
            seen.add(p)
            out.append(p)
    return out


def semble_files(raw, repo):
    try:
        d = json.loads(raw.decode(errors="replace"))
    except json.JSONDecodeError:
        return []
    return dedup(r.get("file_path", "") for r in d.get("results", []))


def meaning_files(raw, repo):
    txt = raw.decode(errors="replace")
    hits = re.findall(r"^\s*\d+\.\s+RRF\s+\S+\s+cosine\s+\S+\s+(\S+)\s*:",
                      txt, re.MULTILINE)
    return dedup(rel(h, repo) for h in hits)


def findcode_files(raw, repo):
    txt = raw.decode(errors="replace")
    hits = re.findall(r"^(\S+?):\d+\s+\(", txt, re.MULTILINE)
    return dedup(rel(h, repo) for h in hits)


def rel(p, repo):
    try:
        return str(Path(p).resolve().relative_to(repo))
    except ValueError:
        return p


def main():
    repo = Path(sys.argv[1]).resolve()
    cases = json.load(open(sys.argv[2]))
    semble_bin = sys.argv[3]
    rows = []
    for c in cases:
        q, truth = c["query"], c["truth_file"]
        row = {"truth_file": truth, "symbol": c["symbol"], "query": q}
        for tool, cmd, parse in (
            ("semble", [semble_bin, "search", q, str(repo), "--top-k", str(TOPK),
                        "--format", "json"], semble_files),
            ("pixel_search_meaning", ["pixel", "search-meaning", q,
                                      "--metrics", "off"], meaning_files),
            ("pixel_find_code", ["pixel", "find-code", q, "--metrics", "off"],
             findcode_files),
        ):
            times, out, rc = [], b"", 0
            for i in range(REPS + 1):
                ms, out, rc = run(cmd, repo)
                if i:
                    times.append(ms)
            files = parse(out, repo)
            times.sort()
            rank = files.index(truth) + 1 if truth in files else None
            row.update({
                f"{tool}_rank": rank,
                f"{tool}_r1": int(rank == 1) if rank else 0,
                f"{tool}_r5": int(bool(rank) and rank <= 5),
                f"{tool}_r10": int(bool(rank) and rank <= TOPK),
                f"{tool}_ms_p50": round(times[len(times) // 2], 1),
                f"{tool}_bytes": len(out),
                f"{tool}_returned": len(files),
                f"{tool}_rc": rc,
            })
        rows.append(row)
        print(f"  {truth[-46:]:46s} "
              f"semble={str(row['semble_rank']):>4s} "
              f"meaning={str(row['pixel_search_meaning_rank']):>4s} "
              f"findcode={str(row['pixel_find_code_rank']):>4s}", file=sys.stderr)
    json.dump(rows, sys.stdout, indent=2)


if __name__ == "__main__":
    main()
