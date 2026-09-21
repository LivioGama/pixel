#!/usr/bin/env python3
"""Repo-map comparison: `stacklit derive` vs `pixel repo-map --markdown`.

These two optimise opposite ends of the same trade-off, so the benchmark
reports BOTH axes and refuses to collapse them into one score:

  size     -- bytes/tokens the map costs to carry
  coverage -- share of the repo's tracked source files the map names by path

A map that is small because it names almost nothing is not "efficient", and a
map that covers everything is not "better" if no agent can afford to load it.
Coverage is measured by substring match of each tracked source path (and of its
basename, since a module-level map may name a directory rather than a file).
"""
import json
import subprocess
import sys
import time
from pathlib import Path

SRC_EXT = (".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".rb", ".go", ".java")


def run(cmd, cwd):
    t0 = time.perf_counter()
    p = subprocess.run(cmd, cwd=cwd, capture_output=True, timeout=900)
    return (time.perf_counter() - t0) * 1000, p.stdout, p.returncode


def coverage(text, repo):
    files = [f for f in subprocess.run(["git", "ls-files"], cwd=repo,
                                       capture_output=True, text=True)
             .stdout.splitlines() if f.endswith(SRC_EXT)]
    by_path = sum(1 for f in files if f in text)
    by_dir = sum(1 for f in files if str(Path(f).parent) in text)
    return {"source_files": len(files),
            "named_by_path": by_path,
            "path_coverage": round(by_path / len(files), 4) if files else None,
            "dir_named": by_dir,
            "dir_coverage": round(by_dir / len(files), 4) if files else None}


def main():
    repo = Path(sys.argv[1]).resolve()
    stacklit = sys.argv[2]
    out = {"repo": repo.name}

    gen_ms, _, rc = run([stacklit, "generate"], repo)
    ms, raw, rc = run([stacklit, "derive"], repo)
    text = raw.decode(errors="replace")
    out["stacklit"] = {"generate_ms": round(gen_ms), "derive_ms": round(ms),
                       "rc": rc, "bytes": len(raw),
                       "approx_tokens": len(raw) // 4, **coverage(text, repo)}

    ms, raw, rc = run(["pixel", "repo-map", "--markdown", "--metrics", "off"], repo)
    text = raw.decode(errors="replace")
    out["pixel_repo_map"] = {"ms": round(ms), "rc": rc, "bytes": len(raw),
                             "approx_tokens": len(raw) // 4,
                             **coverage(text, repo)}

    ms, raw, rc = run(["pixel", "list-areas", "--metrics", "off"], repo)
    text = raw.decode(errors="replace")
    out["pixel_list_areas"] = {"ms": round(ms), "rc": rc, "bytes": len(raw),
                               "approx_tokens": len(raw) // 4,
                               **coverage(text, repo)}

    json.dump(out, sys.stdout, indent=2)
    for k in ("stacklit", "pixel_repo_map", "pixel_list_areas"):
        d = out[k]
        print(f"  {k:18s} {d['approx_tokens']:>8d} tok  "
              f"path-cov {str(d['path_coverage']):>7s}  "
              f"dir-cov {str(d['dir_coverage']):>7s}  rc={d['rc']}",
              file=sys.stderr)


if __name__ == "__main__":
    main()
