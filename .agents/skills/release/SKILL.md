---
name: release
description: Cut a pixel release end to end — pick the version, prepare the release commit (changelog cut, lockstep version bump, Cargo.lock, check-release) with prepare.sh, tag vX.Y.Z to trigger .github/workflows/release.yml, bring main to the released commit, and verify the GitHub release and the Homebrew tap. Also covers hotfix releases from main and recovering from a failed Release run. Use when asked to release, publish, tag or ship a new pixel version, bump the version, cut the changelog, or when the Release workflow failed.
---

# Releasing pixel

The tag is the release. Pushing `vX.Y.Z` runs `.github/workflows/release.yml`,
and nothing else gates it: CI does not run on tags. The workflow has three
jobs, each needing the previous one:

| Job | Does | A failure means |
| --- | --- | --- |
| `verify` | `check-release $GITHUB_REF` (tag = `crates/pixel` version, `Cargo.lock` fresh for all 17 members, `## [x.y.z]` heading and empty Unreleased), then `cargo test --workspace --locked` | nothing built, nothing published |
| `build` | musl x86_64 + aarch64 via `cross` (`--no-default-features --features model2vec`), `aarch64-apple-darwin` natively; tarball + `.sha256` each; `fail-fast` | nothing published |
| `release` | writes `pixel.rb` with the real hashes, cuts the release body from the `## [x.y.z]` section of `CHANGELOG.md`, creates the GitHub release (3 tarballs, 3 `.sha256`, `pixel.rb`), commits `pixel x.y.z` to `LivioGama/homebrew-tap` with `HOMEBREW_TAP_TOKEN` | published, possibly partially: see Recovery |

Once `release` has run, users can install that tag: never move or reuse a
published tag, cut the next patch instead.

Commands use the post-rename names (`new-branch`, `repo-state`, `commit`);
a pixel 0.2.4 binary only knows the old ones (`branch`, `inspect`, `publish`).

Every step that pushes a tag, pushes to `develop`/`main` or merges a PR is
outward-facing: show the user what is about to happen and wait for a go.

## 1. Preconditions

```bash
git fetch origin --tags
pixel repo-state                      # clean tree, on the branch you expect
gh run list --branch develop -L 3     # CI and Cross-build green on origin/develop's head
git merge-base --is-ancestor origin/main origin/develop && echo "main is behind develop: ok"
```

- A red `Cross-build` on develop means the musl lane fails with `--locked`: the
  `build` job will fail the same way. Fix it first.
- `main` not an ancestor of `develop` means a hotfix landed on `main` and was
  never merged back: merge `main` into `develop` before releasing.
- `gh secret list` must show `HOMEBREW_TAP_TOKEN`; without it the tap is not
  updated (a warning, not a failure).

## 2. Pick the version

Read `## [Unreleased]` in `CHANGELOG.md` and the last tag (`git describe --tags
--abbrev=0`). Pre-1.0 convention in this repo:

- **patch** (`0.2.4` → `0.2.5`): fixes, additions, renames that keep the old
  spelling as an alias. 0.2.x patches have shipped `Added` sections.
- **minor** (`0.2.x` → `0.3.0`): something a 0.2.x user must act on — a
  removed command or flag with no alias, a changed JSON output or protocol
  field, a changed on-disk format under `.pixel/`, an install layout that
  `pixel install` does not migrate.
- `1.0.0` also removes the hidden pre-rename aliases (the Unreleased `Changed`
  entry promises it); do not cut it by accident.

Propose the version with the one-line reason; the user decides.

## 3. Prepare the release commit

From an up-to-date `develop`, on a `release-x.y.z` branch:

```bash
pixel new-branch release-x.y.z --from origin/develop --request-id "release-x.y.z-branch"
.agents/skills/release/prepare.sh x.y.z        # --date YYYY-MM-DD to override today
```

`prepare.sh` refuses before writing when the tag or the `## [x.y.z]` heading
already exists or Unreleased is empty. Otherwise it:

1. sets `[package] version` to `x.y.z` in **every** workspace member
   (lockstep, as 0.2.4 did; CONTRIBUTING's "any crate that changed" predates it);
2. inserts `## [x.y.z] - DATE` under a kept, now empty `## [Unreleased]`;
3. runs `cargo update --workspace` so `Cargo.lock` follows;
4. runs `cargo run -q -p pixel-cli -- check-release vx.y.z --repo .`, the
   verify job's command, from the tree. It uses the tree's CLI on purpose: an
   installed 0.2.4 binary only knows the old `release-check` name.

It must end with `release-check: all checks passed`. Then:

- Review `pixel review-changes`: 17 `Cargo.toml` one-liners, `Cargo.lock`,
  two added lines in `CHANGELOG.md`. Anything else is a bug.
- Read the new `## [x.y.z]` section as a release body: it is published verbatim.
  Fix wording or subsection order now, not after the tag.
- Gates: `scripts/gates.sh --force` (fmt, clippy, tests). The verify job reruns
  the tests, but a red one there costs a deleted tag.

```bash
pixel commit -m "release: prepare x.y.z" --request-id "release-x.y.z-prepare"
git push -u origin release-x.y.z
gh pr create --base develop --title "release: prepare x.y.z" --body-file <body>
```

Body: the version, the reason for patch/minor, the gate output, "tag `vx.y.z`
follows once this is on develop". Wait for its CI and for the merge.

## 4. Tag

Tag the commit on `origin/develop` that carries the prepare changes, after its
push CI (CI, Cross-build) is green:

```bash
git fetch origin
SHA=$(git rev-parse origin/develop)
git show --stat "$SHA" | head -5                 # the release: prepare x.y.z commit or its merge
git show "$SHA:crates/pixel/Cargo.toml" | sed -n 3p   # version = "x.y.z"
git tag -a vx.y.z -m "pixel x.y.z" "$SHA"         # annotated, as v0.2.4
git push origin vx.y.z                           # ← confirm with the user first
```

Watch the run in the background, never with a foreground sleep loop:

```bash
gh run list --workflow release.yml -L 1          # the run for vx.y.z
gh run watch <run-id> --exit-status              # run_in_background: true
```

About 10 minutes end to end (0.2.3 and 0.2.4 both took 10 min).

## 5. Bring `main` to the release

`main` only receives releases and must hold the tagged tree. As for 0.2.3 and
0.2.4, open a PR `develop` → `main` titled `release: x.y.z`:

```bash
gh pr create --base main --head develop --title "release: x.y.z" --body-file <body>
```

Body (0.2.4's #101): "Fast-forwards `main` to the `release: prepare x.y.z`
commit (`<sha>`), which is tagged `vx.y.z`. Release notes: the `## [x.y.z] -
DATE` section of CHANGELOG.md."

If `develop` has moved past the tag since, do not ship those commits to `main`
under the release title: push the tag's commit as its own head and open the PR
from it.

```bash
git push origin "vx.y.z^{commit}:refs/heads/release-x.y.z-main"
gh pr create --base main --head release-x.y.z-main --title "release: x.y.z" --body-file <body>
```

Merging is the maintainer's call. After the merge, `git diff vx.y.z origin/main
--stat` must print nothing.

## 6. Verify the publication

```bash
gh release view vx.y.z --json assets --jq '.assets[].name'   # 7 names: 3 .tar.gz, 3 .sha256, pixel.rb
gh release view vx.y.z --json body --jq .body | head -5      # the changelog section, not "See [CHANGELOG.md]"
gh api repos/LivioGama/homebrew-tap/commits --jq '.[0].commit.message'   # pixel x.y.z
gh run view <run-id> --log | grep -E '::warning::' || true   # no-token or no-changelog-section warnings
```

Then install it the way users do and check the version:

```bash
brew update && brew upgrade pixel && pixel --version          # Homebrew
mise upgrade pixel && pixel --version                         # mise
```

Report to the user: tag, run URL, release URL, tap commit, and anything that
did not check out.

## Recovery

| Where it failed | State | Do |
| --- | --- | --- |
| `verify` (check-release or tests) | tag pushed, nothing published | fix on `develop` through a PR; with the user's go, delete the tag (`git push origin :refs/tags/vx.y.z && git tag -d vx.y.z`), then re-tag the fixed commit. No release exists, so reusing the version is safe. |
| `build` | same as above | same as above. A musl link or `--locked` failure should have shown on develop's `Cross-build`: check why it was green. |
| `release`, before "Upload release assets" | nothing published | `gh run rerun <run-id> --failed` if transient; otherwise fix and re-tag as above. |
| `release`, tap steps only | GitHub release published, tap stale | `gh run rerun <run-id> --failed` (the tap step exits 0 when the formula is already current); or copy the `pixel.rb` release asset into `Formula/pixel.rb` of `LivioGama/homebrew-tap` by hand, commit `pixel x.y.z`. |
| after publication, a bad binary | users may have it | never move the tag: fix on `develop`, release `x.y.z+1`. |

## Hotfix release

For a fix that cannot wait for `develop`'s content:

1. `hotfix-x.y.z` from `origin/main`, the fix plus its `CHANGELOG.md` entry
   under Unreleased, then `prepare.sh x.y.z` and the gates as in step 3.
2. PR into `main`; after the merge, tag `main`'s head (step 4).
3. Merge `main` back into `develop` with a PR. `CHANGELOG.md` conflicts: keep
   `develop`'s Unreleased entries, drop the hotfix entry from it (it now lives
   under `## [x.y.z]`), and take `x.y.z` for the member versions and
   `Cargo.lock`.
