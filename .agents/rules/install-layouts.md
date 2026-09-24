---
paths:
  - "crates/pixel-install/**"
---

# Install Layouts

Loaded when `pixel-install` is in play. `install`, `uninstall` and `doctor`
run on machines whose layout the author did not choose; six CodeRabbit
findings on #222 and #225 were layouts nobody had tested.

- **The repository can be the home directory.** `pixel install --repo "$HOME"`
  makes the repo's `.claude/settings.json` the global one and puts the
  repo-local and global backups in one place. A repo-scoped write that
  resolves to a global file is refused (compare with `same_file` before
  writing, not after), and a cleanup that deletes a shared backup first
  checks that no install on the other side still uses it. Test the
  `repo == home` case beside the ordinary one.
- **A foreign config is not a broken install.** Another tool's hooks or
  project settings without a Pixel install are reported as absent, never as
  red: `doctor` judges what Pixel wrote.
- **Quote every path in a command the user is told to paste.** A hint like
  `rm <backup>` or `pixel install --repo <root>` goes through
  `routing::quoted_executable`, and its test uses a path with a space and an
  apostrophe. `root.display()` in a suggested command names the wrong file
  the first time a path has a space in it.
- **Global and repo-local state stay apart.** A repo-local adoption (a
  delegate guard, an RTK backup) is recorded under the repository, never in
  a `~/.claude/` file that every other repository reads.
