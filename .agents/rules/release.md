# Releasing

Always loaded: the only sanctioned release path.

- **To release, run the `release` skill** (`.agents/skills/release/SKILL.md`).
  It owns the whole flow: version pick, `prepare.sh` (changelog cut +
  lockstep version bump + `Cargo.lock` + `check-release`), the `vX.Y.Z` tag
  that triggers `release.yml`, and the sync branch that brings `main` to the
  released tree.
- **Never open a `develop` → `main` pull request.** Release PRs are
  squash-merged, so `main` is never an ancestor of `develop`: that PR is a
  guaranteed, unresolvable conflict. `main` catches up through the skill's
  sync branch, which starts from `main` and takes the released tree — not
  through a branch merge.
- A request to "ship", "tag", "publish" or "release" a version — or a failed
  Release workflow run — is the `release` skill's trigger. Do not improvise
  the steps; the skill encodes them and its scratchpad record
  (`release-x.y.z.md`) survives a context reset.
