#!/bin/sh
# Prepare a release commit's content, without committing or pushing anything.
#
#   .agents/skills/release/prepare.sh 0.2.5          # or v0.2.5
#   .agents/skills/release/prepare.sh 0.2.5 --date 2026-09-15
#
# Steps, each one refused before any write when its precondition fails:
# 1. the version is x.y.z (optional -suffix), has no `vx.y.z` tag and no
#    `## [x.y.z]` heading yet;
# 2. `## [Unreleased]` has at least one `- ` entry to release;
# 3. every workspace member's `[package] version` is set to x.y.z (the
#    members move in lockstep, as 0.2.4 did);
# 4. an empty `## [Unreleased]` is kept and `## [x.y.z] - DATE` is inserted
#    under it, so the entries move to the release section untouched;
# 5. `cargo update --workspace` refreshes Cargo.lock for the members only;
# 6. the pull requests merged into develop since the last tag are listed, so
#    each user-visible one can be matched to a changelog entry by eye
#    (entries carry no PR number), then the commits since the tag that no
#    merged pull request contains (pushed straight to develop, so nobody filed
#    an entry for them); skipped when `gh` is missing or offline;
# 7. `pixel check-release` runs from the tree exactly as the Release
#    workflow's verify job runs it, and its exit code is the script's.
#
# Review the result with `git diff`, then commit `release: prepare x.y.z`.
set -eu

usage() { sed -n '2,5p' "$0" | sed 's/^# \{0,1\}//'; }

VERSION=""
DATE="$(date +%Y-%m-%d)"
while [ $# -gt 0 ]; do
    case "$1" in
        --date) DATE="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        -*) echo "prepare.sh: unknown flag: $1" >&2; exit 2 ;;
        *) VERSION="${1#v}"; shift ;;
    esac
done
[ -n "$VERSION" ] || { usage >&2; exit 2; }

if ! printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$'; then
    echo "prepare.sh: '$VERSION' is not x.y.z" >&2
    exit 2
fi
if ! printf '%s\n' "$DATE" | grep -Eq '^[0-9]{4}-[0-9]{2}-[0-9]{2}$'; then
    echo "prepare.sh: --date '$DATE' is not YYYY-MM-DD" >&2
    exit 2
fi

REPO="$(git rev-parse --show-toplevel)"
cd "$REPO"

if git rev-parse -q --verify "refs/tags/v$VERSION" >/dev/null; then
    echo "prepare.sh: tag v$VERSION already exists" >&2
    exit 1
fi
if grep -Fq "## [$VERSION]" CHANGELOG.md; then
    echo "prepare.sh: CHANGELOG.md already has a ## [$VERSION] heading" >&2
    exit 1
fi

# Entries under Unreleased: the lines starting with "- " between the
# Unreleased heading and the next "## " heading (the rule check-release uses).
ENTRIES="$(awk '
    /^## / { inside = index($0, "## [Unreleased]") == 1; next }
    inside && /^[[:space:]]*- / { n++ }
    END { print n + 0 }
' CHANGELOG.md)"
if [ "$ENTRIES" -eq 0 ]; then
    echo "prepare.sh: ## [Unreleased] has no entries; nothing to release" >&2
    exit 1
fi

MEMBERS="$(sed -n '/^members *= *\[/,/\]/p' Cargo.toml | grep -o '"[^"]*"' | tr -d '"')"
[ -n "$MEMBERS" ] || { echo "prepare.sh: no workspace members in Cargo.toml" >&2; exit 1; }

for m in $MEMBERS; do
    # Only the first `version = ` line, which is the [package] one: no member
    # pins a sibling by version, and [dependencies.x] tables come later.
    perl -0pi -e 's/^version = "[^"]*"/version = "'"$VERSION"'"/m' "$m/Cargo.toml"
done

# Plugin manifests carry the version too: Claude Code and Codex deliver a
# plugin update only when it changes. Same list as
# pixel_release::PLUGIN_MANIFESTS (a test fails if they drift).
for manifest in .claude-plugin/plugin.json .codex-plugin/plugin.json .devin-plugin/plugin.json \
    .qoder-plugin/plugin.json gemini-extension.json package.json; do
    perl -0pi -e 's/^  "version": "[^"]*"/  "version": "'"$VERSION"'"/m' "$manifest"
done
perl -0pi -e 's/^version: .*$/version: '"$VERSION"'/m' plugin.yaml
sh scripts/gen-plugin-assets.sh >/dev/null

perl -0pi -e 's/^## \[Unreleased\]\n/## [Unreleased]\n\n## ['"$VERSION"'] - '"$DATE"'\n/m' CHANGELOG.md

cargo update --workspace --quiet

echo "prepare.sh: $ENTRIES changelog entries released as $VERSION ($DATE); members bumped:"
for m in $MEMBERS; do printf '  %s\n' "$m"; done
echo

# Highest version tag, not `git describe`: a release tag is not always an
# ancestor of develop (v0.2.4's commit was replayed there), so describe
# answers an older tag and the list reaches back a release too far.
LAST_TAG="$(git tag --list 'v[0-9]*' --sort=-v:refname | head -n 1)"
if [ -n "$LAST_TAG" ] && command -v gh >/dev/null 2>&1; then
    SINCE="$(git log -1 --format=%cI "$LAST_TAG")"
    if PRS="$(gh pr list --state merged --base develop --search "merged:>$SINCE" --limit 200 \
        --json number,title,mergeCommit --jq '.[] | "\(.mergeCommit.oid) #\(.number) \(.title)"' 2>/dev/null)"; then
        # The search goes by date, and the tagged commit is usually the merge
        # of the previous prepare PR, committed a second before GitHub records
        # its merged_at (v0.2.5: 16:39:14 vs 16:39:15), so that PR comes back.
        # Keep only the pull requests whose merge commit the tag does not
        # contain; one whose merge commit is not in this clone stays listed.
        UNRELEASED="$(printf '%s\n' "$PRS" | while read -r oid pr; do
            [ -n "$oid" ] || continue
            git merge-base --is-ancestor "$oid" "$LAST_TAG" 2>/dev/null || printf '  %s\n' "$pr"
        done)"
        echo "pull requests merged into develop since $LAST_TAG; each user-visible one needs an entry under ## [$VERSION]:"
        if [ -n "$UNRELEASED" ]; then printf '%s\n' "$UNRELEASED"; else echo "  (none)"; fi
    else
        echo "prepare.sh: could not list the pull requests merged since $LAST_TAG (gh offline or unauthenticated); check CHANGELOG.md against them by hand"
    fi
    echo
    # A commit pushed straight to develop never appears above. The commits
    # since the tag are taken by patch, not by date or ancestry: the tag is
    # not an ancestor of develop (its commit is replayed there), and a commit
    # authored before the tag can still be missing from it. The commits API
    # lists the pull requests containing a commit, open ones included: only a
    # merged one counts.
    NWO="$(gh repo view --json nameWithOwner --jq .nameWithOwner 2>/dev/null || true)"
    if [ -n "$NWO" ]; then
        DIRECT=""
        for sha in $(git log --no-merges --cherry-pick --right-only --format=%H "$LAST_TAG...HEAD" | head -n 200); do
            merged="$(gh api "repos/$NWO/commits/$sha/pulls" \
                --jq 'map(select(.merged_at != null)) | length' 2>/dev/null || echo "?")"
            if [ "$merged" = "0" ]; then
                DIRECT="$DIRECT
  $(git log -1 --format='%h %an: %s' "$sha")"
            fi
        done
        echo "commits since $LAST_TAG in no merged pull request (no one filed a changelog entry for them):"
        if [ -n "$DIRECT" ]; then printf '%s\n' "$DIRECT" | sed '/^$/d'; else echo "  (none)"; fi
        echo
    fi
fi

cargo run -q -p pixel-cli -- check-release "v$VERSION" --repo .
