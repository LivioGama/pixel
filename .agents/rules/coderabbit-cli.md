# CodeRabbit CLI Before Every Pull Request

Always loaded: when the `coderabbit` CLI is installed locally, its review is
part of opening a pull request, not an optional extra.

- **Probe it cheaply, once per PR:** `command -v coderabbit`. When it is
  absent, `coderabbit doctor` fails (not authenticated, backend unreachable),
  or the review hits a rate limit, do not wait it out: open the pull request
  anyway. The GitHub CodeRabbit review still runs where it runs at all (it
  skips drafts, Dependabot bumps and titles containing `WIP` or
  `DO NOT MERGE`) and is the pass that counts. Never block on the CLI.
- **Review before opening.** After the gates pass and before `gh pr create`,
  run `coderabbit review --committed --base <pr-base> --agent` on the branch,
  `<pr-base>` being the branch the pull request will target (`main`,
  `release/x.y`, or the branch below a stacked one). It reviews the same diff
  the PR will carry, so findings arrive minutes before the PR instead of
  after the first round trip.
- **Converge, then open.** Fix every actionable finding, or verify it against
  the code and record why it does not apply (in the PR description, since
  there is no thread yet). Commit each fix — amend or a new commit; the
  rerun reviews commits, not the working tree — and re-run the review until
  it reports no new actionable findings. A finding that survives must be one
  you can defend, not one you got tired of.
- **The local pass does not replace the GitHub review.** Findings on the open
  pull request still owe an answer in their own thread
  ([CONTRIBUTING.md](../../CONTRIBUTING.md), "CodeRabbit reviews"); local
  convergence only removes round trips, not that obligation.
