# Releasing

Always loaded: the only sanctioned release path.

- **To release, run the `release` skill** (`.agents/skills/release/SKILL.md`).
  It owns the whole flow: version pick, `prepare.sh` (changelog cut +
  lockstep version bump + `Cargo.lock` + `check-release`) in a
  `release-x.y.z` pull request into `main`, then the `vX.Y.Z` tag on its
  merge, which triggers `release.yml`.
- **`main` is the only long-lived branch.** Every pull request targets it,
  and a release is a tag on its history. The one exception is a
  `release/x.y` maintenance branch, cut from the line's last tag only when a
  patch cannot wait for `main` to be releasable (see the skill).
- A request to "ship", "tag", "publish" or "release" a version — or a failed
  Release workflow run — is the `release` skill's trigger. Do not improvise
  the steps; the skill encodes them and its scratchpad record
  (`release-x.y.z.md`) survives a context reset.
