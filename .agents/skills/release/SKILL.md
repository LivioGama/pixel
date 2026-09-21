---
name: release
description: Cut a pixel release end to end — pick the version, prepare the release commit (changelog cut, lockstep version bump, Cargo.lock, check-release) with prepare.sh in a pull request into main, tag vX.Y.Z on its merge to trigger .github/workflows/release.yml, and verify the published release, the install.sh asset and the Homebrew tap with evidence. Also covers resuming an interrupted release, patch releases while main is not releasable, and recovering from a failed Release run. Use when asked to release, publish, tag or ship a new pixel version, bump the version, cut the changelog, check a published release, or when the Release workflow failed.
---

# Releasing pixel

`main` is the only long-lived branch: pull requests merge into it by
default, and a release is a tag on it. The one exception is a patch that
cannot wait for `main` to be releasable: the fix still merges into `main`,
then a maintenance-release pull request targets a `release/x.y` branch cut
from the line's last tag (see "Patch release while `main` is not releasable").

The tag is the release. Pushing `vX.Y.Z` runs `.github/workflows/release.yml`,
and nothing else gates it: CI does not run on tags. The workflow has four
jobs, each needing the previous one:

| Job | Does | A failure means |
| --- | --- | --- |
| `verify` | `check-release $GITHUB_REF` (tag = `crates/pixel` version, `Cargo.lock` fresh for all 17 members, `## [x.y.z]` heading and empty Unreleased), then `cargo test --workspace --locked` | nothing built, nothing published |
| `build` | musl x86_64 + aarch64 via `cross` (`--no-default-features --features model2vec`), `aarch64-apple-darwin` natively; tarball + `.sha256` each; `fail-fast` | nothing published |
| `release` | writes `pixel.rb` with the real hashes, cuts the release body from the `## [x.y.z]` section of `CHANGELOG.md`, creates the GitHub release (3 tarballs, 3 `.sha256`, `pixel.rb`, `install.sh`), commits `pixel x.y.z` to `LivioGama/homebrew-tap` with `HOMEBREW_TAP_TOKEN` | published, possibly partially: see Recovery |
| `smoke` (×3, `fail-fast: false`) | on each target's own runner, from an empty `HOME`: the release asset (checksum, run), the documented one-liner through `releases/latest/download/install.sh` (when the tag is the latest release), `brew install LivioGama/tap/pixel` + `brew test` (macOS, when the tap was pushed); each binary's `--version` must print `pixel x.y.z` and `commit: <tag commit>` | already published: the next patch is due |

Three facts shape everything below:

- **The macOS binary is first compiled by the tag, and first run by
  `smoke`.** `ci.yml` and `cross-build.yml` run on `ubuntu-26.04` only; a
  green `main` proves the musl lane, never `aarch64-apple-darwin`.
- **A published release is immutable.** GitHub's release immutability is on:
  once `release` has published, its assets cannot be added, replaced or
  deleted, and its tag cannot move or be deleted while the release exists (a
  deleted release's tag name cannot be reused). The `release tags: immutable`
  ruleset also refuses deleting or moving any `v*` tag, with a bypass for the
  Admin role only. A fix ships as the next patch.
- **Users install releases, not `main`.** `install.sh` is a release asset
  (`releases/latest/download/install.sh`), so `main` may be ahead of the latest
  release, broken script included, without breaking an install.

## Authority

Being asked to fix, merge or ship a change is not an authorization to
release. A release starts on the user's explicit ask, and each outward step
gets its own go: merging the prepare PR, pushing the tag, deleting a tag
(an Admin bypass of the tag ruleset). An ask that names the steps ("do
the release, merge and tag") is the go for those steps; say each command in a
progress line just before running it. Anything the ask did not name (deleting
a tag, force-pushing, retagging) still waits for its own go.

Commands use the current names (`new-branch`, `repo-state`, `commit`). A
`pixel` that rejects them predates 0.2.5: update it before releasing.

## The release record

A release spans a workflow run, a PR and possibly a context reset.
Keep its state in `release-x.y.z.md` in the session scratchpad, written at
the start and updated after every step, so a resumed session reads it instead
of reconstructing the release from `gh` output:

```markdown
# pixel x.y.z
- version / reason: x.y.z, patch|minor because …
- prepare PR: #NNN (merged <merge sha> | open)
- tag: vx.y.z → <sha> (pushed | not yet)
- release run: <run-id> <url>; verify ✓/✗, build ✓/✗, release ✓/✗, smoke ✓/✗
- publication: assets ✓/✗, install.sh ✓/✗, body ✓/✗, tap ✓/✗, binary ✓/✗
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
gh run list --branch main -L 4                     # CI and Cross-build green on origin/main's head
git merge-base --is-ancestor "$LAST" origin/main && echo "$LAST is on main"
git log --oneline "$LAST"..origin/main | head      # something to release
gh secret list | grep HOMEBREW_TAP_TOKEN
```

- Pick the last tag by version sort, not `git describe`: v0.2.4's commit is
  not an ancestor of `main` (it was replayed before the histories were
  joined), so `describe` can answer an older tag.
- A red `Cross-build` on `main` means the musl lane fails with `--locked`:
  the `build` job will fail the same way. A red `Dependency policy
  (cargo-deny)` blocks every PR, the prepare PR included: a fresh RustSec
  advisory is a `chore(deps)` PR (`cargo update -p <crate>`) merged before
  step 3.
- The local gates need room: a workspace build plus `target/debug/incremental`
  can fill the disk mid-gates (`No space left on device` from
  `cargo nextest`). Check `df -h .` first; `target/debug`,
  `target/dev-release` and `target/release` are rebuildable, and
  `CARGO_INCREMENTAL=0` keeps a one-off gate run from growing the cache.
- Without `HOMEBREW_TAP_TOKEN` the tap is not updated (a warning, not a
  failure).
- `main` holds work that must not ship yet? Stop and read "Patch release while
  `main` is not releasable" below.

## 2. Pick the version

Read `changelog.d/` in the repository root and the last tag. Pre-1.0
convention in this repo:

- **patch** (`0.2.4` → `0.2.5`): fixes, additions, renames that keep the old
  spelling as an alias. 0.2.x patches have shipped `Added` sections.
- **minor** (`0.2.x` → `0.3.0`): something a user of the previous minor must
  act on — a removed command or flag with no alias, a changed JSON output or
  protocol field, a changed on-disk format under `.pixel/`, an install layout
  that `pixel install` does not migrate.
- `1.0.0` also removes the hidden pre-rename aliases (0.2.5's `Changed`
  entry promises it); do not cut it by accident.

Propose the version with the one-line reason; the user decides. Start the
record.

## 3. Prepare the release commit

From an up-to-date `main`, on a `release-x.y.z` branch:

```bash
pixel new-branch release-x.y.z --from origin/main --request-id "release-x.y.z-branch"
.agents/skills/release/prepare.sh x.y.z        # --date YYYY-MM-DD to override today
```

`prepare.sh` refuses before writing when the tag or the `## [x.y.z]` heading
already exists, `changelog.d/` holds no fragment, or `## [Unreleased]` still
carries a bullet of its own. Otherwise it:

1. sets `[package] version` to `x.y.z` in **every** workspace member
   (lockstep, as 0.2.4 did), and `version` in every plugin manifest
   (`pixel_release::PLUGIN_MANIFESTS`), then regenerates the plugin prompt
   surfaces with `scripts/gen-plugin-assets.sh`: Claude Code and Codex deliver
   a plugin update only when its version changes;
2. folds the fragments into a new `## [x.y.z] - DATE` under a kept, now empty
   `## [Unreleased]`, grouped by section in the order the headings have always
   used, and deletes the fragments;
3. runs `cargo update --workspace` so `Cargo.lock` follows;
4. lists the pull requests merged into `main` since the last tag, then
   the commits since the tag that belong to no merged pull request (a push
   straight to `main`);
5. runs `cargo run -q -p pixel-cli -- check-release vx.y.z --repo .`, the
   verify job's command, from the tree. It uses the tree's CLI on purpose: an
   installed 0.2.4 binary only knows the old `release-check` name.

It must end with `release-check: all checks passed`. Then:

- **Changelog completeness.** Every `feat`, `fix` and `perf` pull request
  merged since the last tag, and any other with a user-visible effect, needs an
  entry (CONTRIBUTING.md exempts pure refactors and CI/deps chores). The run
  lists the fragments it released above the pull requests, and a fragment named
  after its pull request matches one line to one, so an entry nobody wrote
  shows as a pull request with no fragment. Write the missing one straight into
  the new `## [x.y.z]` section, in the same commit: re-running `prepare.sh`
  would refuse the heading it has already written. The commits listed as
  belonging to no pull request are the ones nobody filed an entry for: each
  user-visible one needs an entry too. Check whether a feature already shipped
  with `git cat-file -e v<last>:<path>` before calling it new.
- **Release body.** Read the new `## [x.y.z]` section as a stranger: it is
  published verbatim. Fix wording or section order now, not after the tag.
- **Diff.** `pixel review-changes`: 17 `Cargo.toml` one-liners, `Cargo.lock`,
  `CHANGELOG.md`, the deleted `changelog.d/*.md` fragments, the 7 plugin
  manifests. Anything else is a bug.
- **Gates.** `GIT_CONFIG_GLOBAL=/dev/null scripts/gates.sh --force` (fmt,
  clippy, tests). Without `GIT_CONFIG_GLOBAL`, a developer's global git
  config fails tests that CI passes (`blame.ignoreRevsFile`,
  `rerere`/`mergiraf` in the provenance and reconcile tests); a red gate that
  CI does not reproduce is not a release blocker. The verify job reruns the
  tests, but a red one there costs a tag deletion.

```bash
pixel commit -m "release: prepare x.y.z" --request-id "release-x.y.z-prepare"
git push -u origin release-x.y.z
gh pr create --base main --title "release: prepare x.y.z" --body-file <body>
```

Body: the version, the reason for patch/minor, the gate output, "tag `vx.y.z`
follows on this PR's merge commit". Wait for its CI, then merge it (the
user's go).

## 4. Tag

Tag the prepare PR's merge commit, not whatever `main`'s head is by then:
another PR merged in between would ship unreviewed in the release. Wait for
that commit's push CI (CI, Cross-build) to be green:

```bash
git fetch origin
SHA=$(gh pr view <n> --json mergeCommit --jq .mergeCommit.oid)
git merge-base --is-ancestor "$SHA" origin/main && echo "on main"
git show --stat "$SHA" | head -5                      # the merge of release: prepare x.y.z
git show "${SHA}:crates/pixel/Cargo.toml" | sed -n 3p # version = "x.y.z"; braces: zsh reads "$SHA:c" as a modifier
gh run list --branch main --commit "$SHA"             # CI and Cross-build: success
git tag -a vx.y.z -m "pixel x.y.z" "$SHA"             # annotated, as v0.2.4
git push origin vx.y.z                                # ← the user's go first
```

Record the SHA, then find the run and watch it in the background, never with
a foreground sleep loop:

```bash
gh run list --workflow release.yml -L 1               # the run for vx.y.z
gh run watch <run-id> --exit-status                   # run_in_background: true
```

Pass run ids literally. Under zsh (Claude Code's command tool) an unquoted
`$var` is not split into words: `set -- $ids` or `for x in $list` over a
space-separated string sees one word, and a watcher built that way reports
failures that never happened.

About 12 minutes to the end of `smoke` (0.3.0: verify 5 min, builds 6 min,
publish and smoke under a minute). A failed `build` on
`aarch64-apple-darwin` is the likeliest surprise (see the first fact above).

## 5. Verify the publication

Start from the `smoke` jobs: `gh run view <run-id> --repo LivioGama/pixel`
must show all three green, and each job log names what it installed and
the `--version` it read. A step that printed a `::notice::` (install.sh on a
tag that is not the latest) or was skipped (Homebrew without the token) is a
caveat to report, not a pass.

`smoke` does not see the release body or the formula hashes against the tap.
Check those from the published state, never the local checkout, downloading
into a fresh directory so nothing local vouches for the release. The binary
lines repeat `smoke` by hand: run them when a `smoke` job failed, was
skipped, or predates the job (0.2.4 and older):

```bash
V=vx.y.z; D=$(mktemp -d); cd "$D"
gh release view $V --repo LivioGama/pixel --json isDraft,isPrerelease,isImmutable,body \
  --jq '{isDraft, isPrerelease, isImmutable, body: .body[0:200]}'   # false, false, true, the changelog section (not "See [CHANGELOG.md]")
gh release download $V --repo LivioGama/pixel
ls                                                     # 3 .tar.gz, 3 .sha256, pixel.rb, install.sh
shasum -a 256 -c ./*.sha256                            # 3 × OK
for f in ./*.sha256; do grep -c "$(awk '{print $1}' "$f")" pixel.rb; done   # darwin 2, each musl 1: the formula carries the real hashes (darwin is also the formula's top-level url)
git -C <repo> show "${V}:scripts/install.sh" | diff - install.sh && echo "install.sh == tag's"
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
matches the workflow's own `echo "::warning::…"` line on every run. The Linux
tarballs cannot run here; their hashes and the formula are the evidence for
them.

Report in this shape, and copy it into the record:

```text
Release vx.y.z: published and verified | NOT verified
- run <url>: verify ✓, build ✓, release ✓, smoke ✓✓✓
- smoke: asset ✓✓✓, install.sh ✓✓✓ | notice, brew ✓ | skipped
- assets: 8, 3 checksums OK, formula hashes match, install.sh == tag's, immutable
- body: CHANGELOG ## [x.y.z] section
- tap: Formula/pixel.rb == release pixel.rb (commit "pixel x.y.z")
- binary: aarch64-apple-darwin prints pixel x.y.z, commit <sha> == tag
Caveats: <anything not checked, and why>
```

## Recovery

Classify the failure before touching anything, from the failed step's log
(`gh run view <run-id> --log-failed`), and write it into the record:

| Class | Looks like | Retry? |
| --- | --- | --- |
| **infra** | runner lost, network or registry timeout, GitHub 5xx, rate limit | `gh run rerun <run-id> --failed` once; a second identical failure is not infra |
| **tooling** | an action or `cross` image broke, an expired `HOMEBREW_TAP_TOKEN`, a toolchain change | a rerun cannot help: it replays the workflow file and commit of the tag. Fix on `main`, then re-tag (unpublished) or next patch (published) |
| **code** | `check-release` or a test fails, a target does not compile or link | fix on `main` through a PR, then re-tag (unpublished) or next patch (published) |

Then by how far the run got:

| Where it failed | State | Do |
| --- | --- | --- |
| `verify` or `build` | tag pushed, nothing published | with the user's go, delete the tag (`git push origin :refs/tags/vx.y.z && git tag -d vx.y.z`; the tag ruleset lets only an Admin do it) and re-tag the fixed commit. No release exists, so reusing the version is safe. A musl failure should have shown on `main`'s `Cross-build`: find out why it was green. |
| `release`, before or during "Upload release assets" | nothing published (the action uploads into a draft and publishes last; a leftover draft is reused by a rerun) | as above, or a rerun for infra |
| `release`, tap steps only | GitHub release published and immutable, tap stale | do not rerun the job: it replays "Upload release assets", which an immutable release refuses. Copy the `pixel.rb` release asset into `Formula/pixel.rb` of `LivioGama/homebrew-tap` by hand, commit `pixel x.y.z`. |
| `smoke`, install.sh only | binaries fine, the published script is broken | the script ships with the release and cannot be replaced: fix it on `main` through a PR and release the next patch |
| `smoke`, asset or brew, infra | unknown | `gh run rerun <run-id> --failed` reruns only the failed `smoke` jobs |
| `smoke`, asset or brew, code (wrong version or commit, crash, `brew test` fails) — or any later report of a bad binary | users may have it | never move the tag: fix on `main`, release `x.y.z+1`, say in its changelog what was wrong with `x.y.z`. |

## Patch release while `main` is not releasable

An urgent fix normally needs nothing special: merge it into `main` and cut
the next patch with steps 1 to 5. Only when `main` holds work that must not
ship yet:

1. Merge the fix into `main` first, as any PR.
2. Push `release/x.y` from `vx.y.<last>` if the line has none yet
   (`git push origin vx.y.<last>^{commit}:refs/heads/release/x.y`). On a
   `release-x.y.z` branch from `origin/release/x.y`,
   `git cherry-pick -x <fix merge sha>` (`-m 1` for a merge commit), add its
   `changelog.d/<slug>.<section>.md` fragment, then `prepare.sh x.y.z` and the
   gates as in step 3; PR into `release/x.y`.
3. Tag that PR's merge commit (step 4) and verify (step 5). `smoke` notices
   install.sh as not checked when a newer line is already the latest release.
4. On `main`, a follow-up PR adds the `## [x.y.z] - DATE` section with that
   entry, so the changelog on `main` records every release.
