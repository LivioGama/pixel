#!/usr/bin/env python3
"""Contract of .cargo/mutants.toml: an exclusion only ever covers a bench or
a crate-root build script.

`exclude_globs` is the one place where a file can leave the mutation gate
without anyone noticing: cargo-mutants reports nothing for a path it was
told to skip, so the `Mutants` job stays green over code it never mutated.
The trap this file exists for: `**/build.rs` reads as "cargo build scripts"
but matches any source file of that name, and this workspace has one --
`crates/pixel-graph/src/build.rs`, the 2000-line module behind `build_graph`,
`tree_delta` and the freshness signature. It sat outside the gate for as
long as the glob did.

So the rule below is deliberately narrow: every Rust file an exclusion
covers must be a bench (not shipped, no tests of its own) or a build script
at a crate root (a separate compilation unit; a mutated one changes what the
build reports, not what a test asserts). Anything else is a module, and a
module belongs to the gate.
"""

from pathlib import Path
import re
import unittest

REPO = Path(__file__).resolve().parent.parent
CONFIG = REPO / ".cargo/mutants.toml"

#: A bench: the dedicated crate, or a `benches/` directory in any crate.
BENCH = re.compile(r"^crates/pixel-bench/|(^|/)benches/")
#: A cargo build script: `build.rs` directly at a crate root, never deeper.
BUILD_SCRIPT = re.compile(r"^crates/[^/]+/build\.rs$")


def exclude_globs() -> list[str]:
    """The `exclude_globs` array, read without a TOML dependency."""
    text = CONFIG.read_text()
    match = re.search(r"^exclude_globs\s*=\s*\[(.*?)\]", text, re.M | re.S)
    assert match, f"{CONFIG} declares no exclude_globs"
    return re.findall(r'"([^"]+)"', match.group(1))


def to_regex(glob: str) -> re.Pattern[str]:
    """Translate one glob to a regex with globset's semantics.

    The distinction this contract turns on: `*` stops at a path separator,
    `**` crosses them. Anything else is matched literally.
    """
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


def rust_files() -> list[str]:
    """Every tracked-looking Rust file, repo-relative, excluding build output."""
    skip = {"target", ".git", ".pixel", "node_modules"}
    found = []
    for path in REPO.rglob("*.rs"):
        rel = path.relative_to(REPO)
        if skip.isdisjoint(rel.parts):
            found.append(rel.as_posix())
    return sorted(found)


class MutantsConfigContract(unittest.TestCase):
    def test_every_excluded_rust_file_is_a_bench_or_a_crate_root_build_script(self):
        patterns = [to_regex(g) for g in exclude_globs()]
        files = rust_files()
        self.assertTrue(files, "found no Rust files to check")
        excluded = [f for f in files if any(p.match(f) for p in patterns)]
        offenders = [
            f for f in excluded if not BENCH.search(f) and not BUILD_SCRIPT.match(f)
        ]
        self.assertEqual(
            offenders,
            [],
            "these files are excluded from the mutation gate but are neither a "
            "bench nor a crate-root build script, so they are modules leaving "
            "the gate unnoticed: " + ", ".join(offenders),
        )

    def test_the_graph_builder_module_is_inside_the_gate(self):
        """The regression this contract was written for.

        `crates/pixel-graph/src/build.rs` is a source module, not a build
        script. A glob that excludes it hides `build_graph`, `tree_delta`,
        `apply_tree_delta` and the freshness signature from the gate.
        """
        module = "crates/pixel-graph/src/build.rs"
        self.assertIn(module, rust_files(), "the module moved; update this test")
        patterns = [to_regex(g) for g in exclude_globs()]
        hit = [g for g, p in zip(exclude_globs(), patterns) if p.match(module)]
        self.assertEqual(hit, [], f"{module} is excluded by {hit}")

    def test_the_cli_build_script_stays_excluded(self):
        """The exclusion the config is actually for keeps working."""
        script = "crates/pixel/build.rs"
        self.assertIn(script, rust_files(), "the build script moved; update this test")
        patterns = [to_regex(g) for g in exclude_globs()]
        self.assertTrue(
            any(p.match(script) for p in patterns),
            f"{script} is a cargo build script and should stay excluded",
        )

    def test_the_glob_translation_separates_star_from_double_star(self):
        """`*` must not cross a separator, or the contract above proves nothing."""
        single = to_regex("crates/*/build.rs")
        self.assertTrue(single.match("crates/pixel/build.rs"))
        self.assertFalse(single.match("crates/pixel-graph/src/build.rs"))
        double = to_regex("**/build.rs")
        self.assertTrue(double.match("crates/pixel/build.rs"))
        self.assertTrue(double.match("crates/pixel-graph/src/build.rs"))
        self.assertTrue(double.match("build.rs"))
        self.assertTrue(to_regex("crates/pixel-bench/**").match("crates/pixel-bench/a/b.rs"))
        self.assertFalse(to_regex("crates/pixel-bench/**").match("crates/pixel/a.rs"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
