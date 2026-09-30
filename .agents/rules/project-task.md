# A Task Exists Before Work Starts

Always loaded: every unit of work in this repository is tracked on the
project board before the first edit, and the board follows the work.

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
- **The board follows the PR lifecycle, automatically.** The `Status` field
  on project 3 is set from the pull request state, never left behind:
  - **Todo → In Progress** the moment the PR carrying `Task <number>` is
    opened (`gh project item-edit --id <item-id> --project-id PVT_kwHOAGnAQs4BlIxX
    --field-id <status-field-id> --single-select-option-id <in-progress-id>`).
  - **In Progress → Done** when the PR merges — and only then. A green CI
    alone is not Done; a merged PR is. Do not set Done at push time.
  - When the pull request is closed unmerged, the status returns to
    **Todo**, so the board never shows finished work that does not exist.
  - Finding the ids once is cheap: `gh project item-list 3 --owner
    LivioGama --format json` gives the item id, and `gh project field-list
    3 --owner LivioGama --format json` gives the Status field and option
    ids. In a CI workflow, this sync is done by an automation job reading
    the PR's `Task <number>` line; locally, the session does it at the two
    lifecycle moments.
- **Keep the board honest while working.** No status may outrun the PR:
  an agent never silently moves an item to Done as it pushes, and a status
  set by hand ahead of the PR is a claim nothing supports. When a task
  turns out bigger than its issue says, stop and split the issue before
  continuing.
- **Exceptions are declared, not silent.** A one-line typo fix, a CI rerun
  or an emergency revert may skip the issue only when named in the PR body
  ("no task: <reason>" on the first line) — and accordingly nothing to
  sync. Everything else opens with a task and carries the lifecycle above.
