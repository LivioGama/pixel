---
name: release
description: Cut a pixel release end to end — pick the version, prepare the release commit (changelog cut, lockstep version bump, Cargo.lock, check-release) with prepare.sh, tag vX.Y.Z to trigger .github/workflows/release.yml, bring main to the released tree, and verify the published release and the Homebrew tap with evidence. Also covers resuming an interrupted release, hotfix releases from main, and recovering from a failed Release run. Use when asked to release, publish, tag or ship a new pixel version, bump the version, cut the changelog, check a published release, or when the Release workflow failed.
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

Two facts shape everything below:

- **The macOS binary is first compiled by the tag.** `ci.yml` and
  `cross-build.yml` run on `ubuntu-latest` only; a green `develop` proves the
  musl lane, never `aarch64-apple-darwin`.
- **A published tag is immutable.** Once `release` has run, users can install
  it: never move, delete or reuse it; a fix ships as the next patch.

## Authority

Being asked to fix, merge or ship a change is not an authorization to
release. A release starts on the user's explicit ask, and each outward step
gets its own go: pushing the tag, pushing to `develop`/`main`, merging a PR,
deleting a tag. Show the exact command before running it.

Commands use the post-rename names (`new-branch`, `repo-state`, `commit`);
a pixel 0.2.4 binary only knows the old ones (`branch`, `inspect`, `publish`).

## The release record

A release spans a 10-minute workflow, two PRs and possibly a context reset.
Keep its state in `release-x.y.z.md` in the session scratchpad, written at
the start and updated after every step, so a resumed session reads it instead
of reconstructing the release from `gh` output:

```markdown
# pixel x.y.z
- version / reason: x.y.z, patch|minor because …
- prepare PR: #NNN (merged <sha> | open)
- tag: vx.y.z → <sha> (pushed | not yet)
- release run: <run-id> <url>; verify ✓/✗, build ✓/✗, release ✓/✗
- main PR: #NNN (merged | open)
- publication: assets ✓/✗, body ✓/✗, tap ✓/✗, binary ✓/✗
- failure: <job, step, class (code|tooling|infra), evidence>
- next action: <one command, and whether it needs the user's go>
```

On resume, trust the record only as a map: re-read the live state it points
at (`gh pr view`, `gh run view`, `git ls-remote origin refs/tags/vx.y.z`)
before acting, and fix the record where it is stale.

## 1. Preconditions

```bash
git fetch origin --tags
pixel repo-state                                   # clean tree
LAST=$(git tag --list 'v[0-9]*' --sort=-v:refname | head -n 1)
gh run list --branch develop -L 4                  # CI and Cross-build green on origin/develop's head
git diff --quiet "$LAST" origin/main && echo "main holds $LAST, nothing unreleased"
git show origin/develop:CHANGELOG.md | grep -F "## [${LAST#v}]"   # the last release reached develop
gh secret list | grep HOMEBREW_TAP_TOKEN
```

- Pick the last tag by version sort, not `git describe`: a release tag is
  not always an ancestor of `develop` (v0.2.4's commit was replayed there).
- `main` is never an ancestor of `develop`: release PRs are squash-merged
  (0.2.1, 0.2.4). Compare trees, not ancestry. A `main` whose tree differs
  from the last tag carries an unreleased hotfix; a `develop` without the last
  release's changelog heading never got that hotfix merged back. Settle
  either first.
- A red `Cross-build` on develop means the musl lane fails with `--locked`:
  the `build` job will fail the same way.
- Without `HOMEBREW_TAP_TOKEN` the tap is not updated (a warning, not a
  failure).

## 2. Pick the version

Read `## [Unreleased]` in `CHANGELOG.md` and the last tag. Pre-1.0
convention in this repo:

- **patch** (`0.2.4` → `0.2.5`): fixes, additions, renames that keep the old
  spelling as an alias. 0.2.x patches have shipped `Added` sections.
- **minor** (`0.2.x` → `0.3.0`): something a 0.2.x user must act on — a
  removed command or flag with no alias, a changed JSON output or protocol
  field, a changed on-disk format under `.pixel/`, an install layout that
  `pixel install` does not migrate.
- `1.0.0` also removes the hidden pre-rename aliases (the Unreleased `Changed`
  entry promises it); do not cut it by accident.

Propose the version with the one-line reason; the user decides. Start the
record.

## 3. Prepare the release commit

From an up-to-date `develop`, on a `release-x.y.z` branch:

```bash
pixel new-branch release-x.y.z --from origin/develop --request-id "release-x.y.z-branch"
.agents/skills/release/prepare.sh x.y.z        # --date YYYY-MM-DD to override today
```

`prepare.sh` refuses before writing when the tag or the `## [x.y.z]` heading
already exists or Unreleased is empty. Otherwise it:

1. sets `[package] version` to `x.y.z` in **every** workspace member
   (lockstep, as 0.2.4 did);
2. inserts `## [x.y.z] - DATE` under a kept, now empty `## [Unreleased]`;
3. runs `cargo update --workspace` so `Cargo.lock` follows;
4. lists the pull requests merged into `develop` since the last tag;
5. runs `cargo run -q -p pixel-cli -- check-release vx.y.z --repo .`, the
   verify job's command, from the tree. It uses the tree's CLI on purpose: an
   installed 0.2.4 binary only knows the old `release-check` name.

It must end with `release-check: all checks passed`. Then:

- **Changelog completeness.** Changelog entries carry no PR number, so match
  the listed PRs by hand: every `feat`, `fix` and `perf` PR, and any other
  with a user-visible effect, needs an entry under `## [x.y.z]`
  (CONTRIBUTING.md exempts pure refactors and CI/deps chores). Add the
  missing ones now, in the same commit.
- **Release body.** Read the new `## [x.y.z]` section as a stranger: it is
  published verbatim. Fix wording or subsection order now, not after the tag.
- **Diff.** `pixel review-changes`: 17 `Cargo.toml` one-liners, `Cargo.lock`,
  `CHANGELOG.md`. Anything else is a bug.
- **Gates.** `scripts/gates.sh --force` (fmt, clippy, tests). The verify job
  reruns the tests, but a red one there costs a tag deletion.

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
git show --stat "$SHA" | head -5                      # the release: prepare x.y.z commit or its merge
git show "$SHA:crates/pixel/Cargo.toml" | sed -n 3p   # version = "x.y.z"
git tag -a vx.y.z -m "pixel x.y.z" "$SHA"             # annotated, as v0.2.4
git push origin vx.y.z                                # ← the user's go first
```

Record the SHA, then find the run and watch it in the background, never with
a foreground sleep loop:

```bash
gh run list --workflow release.yml -L 1               # the run for vx.y.z
gh run watch <run-id> --exit-status                   # run_in_background: true
```

About 10 minutes end to end (0.2.3 and 0.2.4 both took 10 min). A failed
`build` on `aarch64-apple-darwin` is the likeliest surprise (see the first
fact above).

## 5. Bring `main` to the release

`main` only receives releases and must hold the tagged tree. Open a PR
`develop` → `main` titled `release: x.y.z`:

```bash
gh pr create --base main --head develop --title "release: x.y.z" --body-file <body>
```

Body (0.2.4's #101): "Brings `main` to the `release: prepare x.y.z` commit
(`<sha>`), which is tagged `vx.y.z`. Release notes: the `## [x.y.z] - DATE`
section of CHANGELOG.md."

If `develop` has moved past the tag since, do not ship those commits to `main`
under the release title: push the tag's commit as its own head and open the PR
from it.

```bash
git push origin "vx.y.z^{commit}:refs/heads/release-x.y.z-main"
gh pr create --base main --head release-x.y.z-main --title "release: x.y.z" --body-file <body>
```

Merging is the maintainer's call; 0.2.1 and 0.2.4 were squash-merged. After
the merge, `git diff --quiet vx.y.z origin/main` must succeed.

## 6. Verify the publication

Check the published state, never the local checkout, and download into a
fresh directory so nothing local vouches for the release:

```bash
V=vx.y.z; D=$(mktemp -d); cd "$D"
gh release view $V --repo LivioGama/pixel --json isDraft,isPrerelease,body \
  --jq '{isDraft, isPrerelease, body: .body[0:200]}'   # false, false, the changelog section (not "See [CHANGELOG.md]")
gh release download $V --repo LivioGama/pixel
ls                                                     # 3 .tar.gz, 3 .sha256, pixel.rb
shasum -a 256 -c ./*.sha256                            # 3 × OK
for f in ./*.sha256; do grep -c "$(awk '{print $1}' "$f")" pixel.rb; done   # 3 × 1: the formula carries the real hashes
gh api repos/LivioGama/homebrew-tap/contents/Formula/pixel.rb --jq .content \
  | base64 -d | diff - pixel.rb && echo "tap == release formula"
tar xzf pixel-$V-aarch64-apple-darwin.tar.gz
HOME="$D/home" ./pixel-$V-aarch64-apple-darwin/bin/pixel --version
git -C <repo> rev-list -n 1 $V                         # must equal the `commit:` line above
gh run view <run-id> --repo LivioGama/pixel         # no ANNOTATIONS section = no no-token/no-changelog warning
```

`pixel --version` prints `pixel x.y.z` and `commit: <sha>`: the commit line
proves the binary was built from the tag, which the version number alone
does not. The run in an empty `HOME` proves it starts without this machine's
config. Read warnings from the run summary's annotations, not from
`--log | grep '::warning::'`: the log echoes each step's script, so that grep
matches the workflow's own `echo "::warning::…"` line on every run. The Linux tarballs cannot run here; their hashes and the formula
are the evidence for them.

Report in this shape, and copy it into the record:

```text
Release vx.y.z: published and verified | NOT verified
- run <url>: verify ✓, build ✓, release ✓
- assets: 7, 3 checksums OK, formula hashes match
- body: CHANGELOG ## [x.y.z] section
- tap: Formula/pixel.rb == release pixel.rb (commit "pixel x.y.z")
- binary: aarch64-apple-darwin prints pixel x.y.z, commit <sha> == tag
- main: tree == vx.y.z (PR #NNN) | PR #NNN open
Caveats: <anything not checked, and why>
```

## Recovery

Classify the failure before touching anything, from the failed step's log
(`gh run view <run-id> --log-failed`), and write it into the record:

| Class | Looks like | Retry? |
| --- | --- | --- |
| **infra** | runner lost, network or registry timeout, GitHub 5xx, rate limit | `gh run rerun <run-id> --failed` once; a second identical failure is not infra |
| **tooling** | an action or `cross` image broke, an expired `HOMEBREW_TAP_TOKEN`, a toolchain change | a rerun cannot help: it replays the workflow file and commit of the tag. Fix on `develop`, then re-tag (unpublished) or next patch (published) |
| **code** | `check-release` or a test fails, a target does not compile or link | fix on `develop` through a PR, then re-tag (unpublished) or next patch (published) |

Then by how far the run got:

| Where it failed | State | Do |
| --- | --- | --- |
| `verify` or `build` | tag pushed, nothing published | with the user's go, delete the tag (`git push origin :refs/tags/vx.y.z && git tag -d vx.y.z`) and re-tag the fixed commit. No release exists, so reusing the version is safe. A musl failure should have shown on develop's `Cross-build`: find out why it was green. |
| `release`, before "Upload release assets" | nothing published | as above, or a rerun for infra |
| `release`, tap steps only | GitHub release published, tap stale | infra: `gh run rerun <run-id> --failed` (the tap step exits 0 when the formula is already current). Otherwise copy the `pixel.rb` release asset into `Formula/pixel.rb` of `LivioGama/homebrew-tap` by hand, commit `pixel x.y.z`. |
| after publication, a bad binary | users may have it | never move the tag: fix on `develop`, release `x.y.z+1`, say in its changelog what was wrong with `x.y.z`. |

## Hotfix release

For a fix that cannot wait for `develop`'s content:

1. `hotfix-x.y.z` from `origin/main`, the fix plus its `CHANGELOG.md` entry
   under Unreleased, then `prepare.sh x.y.z` and the gates as in step 3.
2. PR into `main`; after the merge, tag `main`'s head (step 4) and verify
   (step 6).
3. Merge `main` back into `develop` with a PR. `CHANGELOG.md` conflicts: keep
   `develop`'s Unreleased entries, drop the hotfix entry from it (it now lives
   under `## [x.y.z]`), and take `x.y.z` for the member versions and
   `Cargo.lock`. Step 1's changelog check fails until this lands.
