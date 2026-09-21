#!/usr/bin/env python3
"""Decide whether a `Mutants` run actually gated anything, and say so.

`cargo mutants --in-diff` exits 0 both when every mutant was caught and when
it produced no mutants at all. The second case is not a pass: it is the gate
having nothing to say. PR #203 hit it -- its whole diff lived in a file
`.cargo/mutants.toml` excluded, so the job went green in 56 s, tested
nothing, and uploaded no `mutants.out`. A reviewer reading the check saw a
mutation gate pass over code that was never mutated.

This script runs after the mutants step and reports what was tested. A diff
that touches no mutable Rust has nothing to answer for -- docs, benches, a
crate-root build script, `Cargo.toml` alone. A diff that touches Rust the
config does NOT exclude, and still produced zero mutants, is the vacuous
case: it gets a warning annotation and an unmissable summary block.

Usage:
    mutants-gate.py --diff pr.diff --list mutants-list.txt [--fail-on-vacuous]

`--list` is the stdout of `cargo mutants --list --in-diff <diff>`, which
costs no build. Exit 0 unless `--fail-on-vacuous` is passed and the diff is
vacuous.
"""

import argparse
from pathlib import Path
import re
import sys

REPO = Path(__file__).resolve().parent.parent
CONFIG = REPO / ".cargo/mutants.toml"


def exclude_globs(config: Path = CONFIG) -> list[str]:
    """The `exclude_globs` array, read without a TOML dependency."""
    match = re.search(
        r"^exclude_globs\s*=\s*\[(.*?)\]", config.read_text(), re.M | re.S
    )
    return re.findall(r'"([^"]+)"', match.group(1)) if match else []


def to_regex(glob: str) -> re.Pattern[str]:
    """One glob to a regex with globset's semantics: `*` stops at `/`, `**` does not."""
    out = ["^"]
    i = 0
    while i < len(glob):
        if glob.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
        elif glob.startswith("**", i):
            out.append(".*")
            i += 2
        elif glob[i] == "*":
            out.append("[^/]*")
            i += 1
        elif glob[i] == "?":
            out.append("[^/]")
            i += 1
        else:
            out.append(re.escape(glob[i]))
            i += 1
    out.append("$")
    return re.compile("".join(out))


def diff_paths(diff_text: str) -> list[str]:
    """Repo-relative paths the diff writes to, deleted files aside."""
    paths = []
    for line in diff_text.splitlines():
        if line.startswith("+++ "):
            target = line[4:].strip()
            if target == "/dev/null":
                continue
            paths.append(target[2:] if target.startswith("b/") else target)
    return sorted(set(paths))


def mutable_rust(paths: list[str], globs: list[str]) -> list[str]:
    """The `.rs` paths a mutant could come from: Rust, minus what is excluded."""
    patterns = [to_regex(g) for g in globs]
    return [
        p for p in paths if p.endswith(".rs") and not any(x.match(p) for x in patterns)
    ]


def count_mutants(list_text: str) -> int:
    """Mutants in `cargo mutants --list` output: one `path.rs:line:col: ...` each."""
    return sum(1 for line in list_text.splitlines() if re.match(r"^\S+\.rs:\d+:\d+:", line))


def verdict(mutants: int, mutable: list[str]) -> tuple[str, str]:
    """`(status, message)` for a run that produced `mutants` over `mutable` files.

    - `tested`: mutants were generated, so the job's pass or fail means something.
    - `not-applicable`: the diff holds no mutable Rust; nothing was owed.
    - `vacuous`: mutable Rust changed and no mutant came out of it. The gate
      reports nothing about this diff and must not be read as coverage.
    """
    if mutants > 0:
        return "tested", f"{mutants} mutant(s) tested from the diff."
    if not mutable:
        return (
            "not-applicable",
            "No mutable Rust in the diff (docs, benches, build scripts or "
            "manifests only), so no mutants were owed.",
        )
    listed = ", ".join(mutable)
    return (
        "vacuous",
        "0 mutants tested, yet the diff changes Rust the mutation config does "
        f"not exclude: {listed}. This check proves nothing about that code -- "
        "do not read it as coverage. Either the change generates no mutants "
        "(a comment- or test-only edit) or an exclusion in .cargo/mutants.toml "
        "is swallowing the file.",
    )


def render(status: str, message: str, mutants: int, mutable: list[str]) -> str:
    """The job-summary block. The tested count is always stated outright."""
    lines = [
        "### Mutation gate outcome",
        "",
        f"- **mutants tested:** {mutants}",
        f"- **mutable Rust files in the diff:** {len(mutable)}",
        f"- **verdict:** `{status}`",
        "",
        message,
    ]
    if status == "vacuous":
        lines[0] = "### :warning: Mutation gate tested nothing"
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--diff", required=True, type=Path)
    parser.add_argument("--list", dest="listing", required=True, type=Path)
    parser.add_argument("--config", type=Path, default=CONFIG)
    parser.add_argument(
        "--fail-on-vacuous",
        action="store_true",
        help="exit 1 when the diff changed mutable Rust and no mutant came out",
    )
    parser.add_argument("--summary", type=Path, help="append the report here")
    args = parser.parse_args(argv)

    paths = diff_paths(args.diff.read_text(errors="replace"))
    mutable = mutable_rust(paths, exclude_globs(args.config))
    mutants = count_mutants(args.listing.read_text(errors="replace"))
    status, message = verdict(mutants, mutable)

    report = render(status, message, mutants, mutable)
    print(report, end="")
    if args.summary:
        with args.summary.open("a") as fh:
            fh.write(report)
    if status == "vacuous":
        # A workflow command, so the verdict lands on the pull request itself
        # and not only in a summary nobody opens.
        print(f"::warning title=Mutation gate tested nothing::{message}")
        if args.fail_on_vacuous:
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
