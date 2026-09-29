# A Task Exists Before Work Starts

Always loaded: every unit of work in this repository is tracked on the
project board before the first edit.

- **The task comes first.** Before starting any work — a fix, a feature, a
  rule, a docs pass — check [project 3, view 1](https://github.com/users/LivioGama/projects/3/views/1)
  for an existing issue covering it. When none exists, open one
  (`gh issue create --title "<what and why>"`), add it to the project
  (`gh project item-add 3 --owner LivioGama --url <issue-url>`), and work
  with that number. A task-out-of-band conversation note is not tracking.
- **Name the task everywhere the PR does.** The first PR body line carries
  `Task <number>`; the branch, commit subjects and the `changelog.d/`
  fragment reference it when the issue is not itself the PR's origin. The
  pull request is one artefact of the task, not the task itself.
- **Keep the board honest while working.** Status changes are moved by the
  human or the session owner — an agent never silently moves an item to
  Done as it pushes. When a task turns out bigger than its issue says, stop
  and split the issue before continuing.
- **Exceptions are declared, not silent.** A one-line typo fix, a CI rerun
  or an emergency revert may skip the issue only when named in the PR body
  ("no task: <reason>" on the first line). Everything else opens with one.
