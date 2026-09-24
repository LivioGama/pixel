#!/bin/sh
# Prepare a release commit's content, without committing or pushing anything.
#
#   .agents/skills/release/prepare.sh 0.2.5          # or v0.2.5
#   .agents/skills/release/prepare.sh 0.2.5 --date 2026-09-15
#   .agents/skills/release/prepare.sh --check        # validate the fragments only
#
# Steps, each one refused before any write when its precondition fails:
# 1. the version is x.y.z (optional -suffix), has no `vx.y.z` tag and no
#    `## [x.y.z]` heading yet;
# 2. `changelog.d/` holds at least one fragment, every fragment is a
#    `<slug>.<section>.md` with a section from SECTIONS and a first line that
#    carries the entry, opens on its scope, names its pull request and fits
#    HARD_LIMIT, an optional
#    `_highlights.md` carries no `## ` heading and fits HIGHLIGHTS_LIMIT, and
#    `## [Unreleased]` carries no `- ` entry;
# 3. every workspace member's `[package] version` is set to x.y.z (the
#    members move in lockstep, as 0.2.4 did);
# 4. the fragments are folded into a new `## [x.y.z] - DATE` under a kept,
#    empty `## [Unreleased]`, grouped by section and led by `_highlights.md`
#    when there is one, and deleted;
# 5. `cargo update --workspace` refreshes Cargo.lock for the members only;
# 6. the fragments this run released are listed next to the pull requests
#    merged into main since the last tag, so each user-visible one can be
#    matched to an entry by eye, then the commits since the tag that no
#    merged pull request contains (pushed straight to main, so nobody filed
#    an entry for them); skipped when `gh` is missing or offline;
# 7. `pixel check-release` runs from the tree exactly as the Release
#    workflow's verify job runs it, and its exit code is the script's.
#
# Review the result with `git diff`, then commit `release: prepare x.y.z`.
set -eu

usage() { sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//'; }

# Entries live under changelog.d/, one file per entry: two pull requests then
# never edit the same lines of CHANGELOG.md, and an entry written on a branch
# cut before a release cannot land in that release's section by accident.
# `<slug>.<section>.md`, the section naming the Keep a Changelog heading the
# entry is filed under. The slug is free; start it with the pull request
# number when the number is known, so step 6 can be matched by eye. Emitted in
# this order, which is the order the headings have always appeared in.
SECTIONS="added changed deprecated removed fixed security"

# The shape of an entry, gated here because a rule only in CONTRIBUTING.md is a
# rule the release finds out about. `**<scope>:** ` first: the scope is what
# makes a released section scannable, and the 0.4.0 cut, written without one,
# has to be read line by line to find the command a bullet is about. Then the
# length. The entry states what changed and what it means for a user; why that
# design and not another is the pull request's job, and the link in the entry is
# what carries the reader there. Measured against a project that does this well
# (mise v2026.8.2: 19 entries, 171 to 498 bytes), 0.4.0 ran 264 to 1265
# with a median of 715, and the fragment open at the time was 1428 in a single
# paragraph. SOFT_LIMIT is that project's observed ceiling, so a warning means
# "longer than anyone else's longest"; HARD_LIMIT is what refuses the essay
# while leaving room for an entry that genuinely carries a before/after
# measurement.
SCOPE_PATTERN='^\*\*[a-z][a-z0-9 ,.-]*:\*\* [^[:space:]]'
SOFT_LIMIT=500
HARD_LIMIT=900

# The release's own narrative, once per release instead of once per entry.
# Without somewhere to put "what this release is about", every entry carries a
# sentence of it, which is how 0.4.0 ended up with a median bullet of 715
# bytes. Optional: a release of three fixes needs no chapeau. Emitted verbatim
# under the version heading, above the sections, so the GitHub release body --
# which release.yml cuts from that heading to the next `## ` -- opens on it.
# `###` and below are the file's to use (mise leads with a paragraph, then a
# `## Highlights` list of three); `#` and `##` are the changelog's structure
# and would end the section the notes are cut from. The limit is mise
# v2026.8.2's own lead plus highlights (1275 bytes) with room to spare: enough
# for a paragraph and three bullets, not enough for the prose this whole
# exercise moved out of the entries.
HIGHLIGHTS="changelog.d/_highlights.md"
HIGHLIGHTS_LIMIT=2000

VERSION=""
DATE="$(date +%Y-%m-%d)"
CHECK=0
while [ $# -gt 0 ]; do
    case "$1" in
        --date) DATE="$2"; shift 2 ;;
        --check) CHECK=1; shift ;;
        -h|--help) usage; exit 0 ;;
        -*) echo "prepare.sh: unknown flag: $1" >&2; exit 2 ;;
        *) VERSION="${1#v}"; shift ;;
    esac
done
if [ "$CHECK" -eq 1 ]; then
    [ -z "$VERSION" ] || { echo "prepare.sh: --check takes no version" >&2; exit 2; }
else
    [ -n "$VERSION" ] || { usage >&2; exit 2; }
fi

if [ "$CHECK" -eq 0 ]; then
    if ! printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$'; then
        echo "prepare.sh: '$VERSION' is not x.y.z" >&2
        exit 2
    fi
fi
if ! printf '%s\n' "$DATE" | grep -Eq '^[0-9]{4}-[0-9]{2}-[0-9]{2}$'; then
    echo "prepare.sh: --date '$DATE' is not YYYY-MM-DD" >&2
    exit 2
fi

REPO="$(git rev-parse --show-toplevel)"
cd "$REPO"

if [ "$CHECK" -eq 0 ]; then
    if git rev-parse -q --verify "refs/tags/v$VERSION" >/dev/null; then
        echo "prepare.sh: tag v$VERSION already exists" >&2
        exit 1
    fi
    if grep -Fq "## [$VERSION]" CHANGELOG.md; then
        echo "prepare.sh: CHANGELOG.md already has a ## [$VERSION] heading" >&2
        exit 1
    fi
fi

# The Keep a Changelog heading a fragment is filed under, from the section it
# names.
section_title() {
    case "$1" in
        added) printf 'Added' ;;
        changed) printf 'Changed' ;;
        deprecated) printf 'Deprecated' ;;
        removed) printf 'Removed' ;;
        fixed) printf 'Fixed' ;;
        security) printf 'Security' ;;
    esac
}

# The section a fragment names: the part after the last dot of its basename,
# so `fix-the-thing.fixed.md` files under Fixed. A name with no section is
# rejected below rather than guessed.
fragment_section() {
    name="${1##*/}"
    case "${name%.md}" in
        *.*) printf '%s' "${name%.md}" | sed 's/.*\.//' ;;
        *) printf '' ;;
    esac
}

# Fragments in filename order. An unmatched glob is the literal pattern, which
# is why the existence test is what says whether a fragment is there at all.
FRAGMENTS=""
FRAGMENT_COUNT=0
WARNINGS=""
for fragment in changelog.d/*.md; do
    [ -e "$fragment" ] || continue
    # `_highlights.md` is the release's narrative, not an entry: no section, no
    # scope, no bullet. Any other underscore name is a typo of it, refused
    # rather than skipped -- silently ignoring `_highlight.md` would drop the
    # chapeau from the release it was written for.
    case "${fragment##*/}" in
        _*)
            if [ "${fragment##*/}" != "_highlights.md" ]; then
                echo "prepare.sh: $fragment: the only underscore-named file changelog.d/ takes is _highlights.md; an entry is <slug>.<section>.md" >&2
                exit 1
            fi
            continue ;;
    esac
    FRAGMENT_COUNT=$((FRAGMENT_COUNT + 1))
    FRAGMENTS="${FRAGMENTS}${FRAGMENTS:+
}$fragment"
    section="$(fragment_section "$fragment")"
    case " $SECTIONS " in
        *" $section "*) ;;
        *) echo "prepare.sh: $fragment: name it <slug>.<section>.md, <section> one of: $SECTIONS" >&2
           exit 1 ;;
    esac
    # `-s` is not enough. A file of newlines is not empty, and the renderer
    # below turns its first line into the bullet, so it would file a bare
    # `- ` under a released heading, where check-release -- which reads only
    # `## [Unreleased]` -- never looks again.
    if ! grep -q '[^[:space:]]' "$fragment"; then
        echo "prepare.sh: $fragment is empty; it carries the entry's text" >&2
        exit 1
    fi
    if ! head -n 1 "$fragment" | grep -q '[^[:space:]]'; then
        echo "prepare.sh: $fragment starts with a blank line; its first line is the entry" >&2
        exit 1
    fi
    if ! head -n 1 "$fragment" | grep -Eq "$SCOPE_PATTERN"; then
        echo "prepare.sh: $fragment: open the entry with the scope it changes, \`**graph:** the entry\`, as the commit subject's scope does" >&2
        exit 1
    fi
    # The entry's own bytes: the newlines a wrapped fragment carries are the
    # file's, not the prose's, and the cut reflows them under the bullet. Bytes
    # and not characters because `wc -m` is locale-dependent where `wc -c` is
    # not, and an entry close enough to the cap for a multi-byte dash to decide
    # it is an entry to cut anyway.
    LENGTH="$(tr -d '\n' < "$fragment" | wc -c | tr -d ' ')"
    if [ "$LENGTH" -gt "$HARD_LIMIT" ]; then
        echo "prepare.sh: $fragment: $LENGTH bytes, over the $HARD_LIMIT cap; state the change and what it means for a user, and leave the reasoning to the pull request the entry links" >&2
        exit 1
    fi
    if [ "$LENGTH" -gt "$SOFT_LIMIT" ]; then
        WARNINGS="${WARNINGS}${WARNINGS:+
}  $fragment: $LENGTH bytes, over the $SOFT_LIMIT the style aims at"
    fi
    # The reader of a short entry needs somewhere to go for the rest. The link
    # is in the text, or derivable by eye from a slug that opens on the number.
    # A refusal, not a warning: the number only exists once the pull request
    # is open, so the fragment is written without it and the rename is a step
    # to remember afterwards. As a warning nothing surfaced it before the
    # release (three entries merged without it in a row, #253-#255); refused,
    # the pull request's own CI run goes red on it, when the number is known.
    # In the text it is the pull request's URL, not any `#<n>`: `Fixes issue
    # #42` names an issue and would otherwise pass for the reference.
    if ! grep -Eq '/pull/[0-9]+' "$fragment" && ! printf '%s' "${fragment##*/}" | grep -Eq '^[0-9]+-'; then
        echo "prepare.sh: $fragment: no pull request referenced; once the pull request is open, name the number first in the slug (git mv $fragment changelog.d/<number>-${fragment##*/}) and end the entry with its link: ([#<number>](https://github.com/LivioGama/pixel/pull/<number>))" >&2
        exit 1
    fi
done
if [ -n "$WARNINGS" ]; then
    echo "prepare.sh: entries that do not fit the style (not a refusal):" >&2
    printf '%s\n' "$WARNINGS" >&2
fi

HAS_HIGHLIGHTS=0
if [ -e "$HIGHLIGHTS" ]; then
    HAS_HIGHLIGHTS=1
    if ! grep -q '[^[:space:]]' "$HIGHLIGHTS"; then
        echo "prepare.sh: $HIGHLIGHTS is empty; it carries the release's lead paragraph and highlights, or it is not there at all" >&2
        exit 1
    fi
    if grep -Eq '^##? ' "$HIGHLIGHTS"; then
        echo "prepare.sh: $HIGHLIGHTS: use \`###\` and below; a \`#\` or \`##\` heading ends the section the release body is cut from" >&2
        exit 1
    fi
    HIGHLIGHTS_LENGTH="$(tr -d '\n' < "$HIGHLIGHTS" | wc -c | tr -d ' ')"
    if [ "$HIGHLIGHTS_LENGTH" -gt "$HIGHLIGHTS_LIMIT" ]; then
        echo "prepare.sh: $HIGHLIGHTS: $HIGHLIGHTS_LENGTH bytes, over the $HIGHLIGHTS_LIMIT cap; a lead paragraph and two or three highlights, not the entries again" >&2
        exit 1
    fi
fi

# An entry written straight into CHANGELOG.md would be released only by
# accident: the cut below takes its text from the fragments and leaves the file
# alone. check-release refuses the tag while one is still there; refusing it
# here first is what lets the message say where the entry belongs. It runs
# before the --check return, which otherwise reports the section as empty
# without having looked.
STRAY="$(awk '
    /^## / { inside = index($0, "## [Unreleased]") == 1; next }
    inside && /^[[:space:]]*- / { n++ }
    END { print n + 0 }
' CHANGELOG.md)"
if [ "$STRAY" -ne 0 ]; then
    echo "prepare.sh: $STRAY bullet(s) still under ## [Unreleased] in CHANGELOG.md; entries live in changelog.d/, one file per entry" >&2
    exit 1
fi

# An empty changelog.d/ is well formed: it is the state every release leaves
# behind, and the release pull request that leaves it there runs --check on
# every push like any other pull request. Only the cut below needs a fragment,
# so its refusal moved under this return; above it, --check failed the release
# pull request of every version, 0.4.0 included.
if [ "$CHECK" -eq 1 ]; then
    HIGHLIGHTS_NOTE=""
    if [ "$HAS_HIGHLIGHTS" -eq 1 ]; then HIGHLIGHTS_NOTE=" plus _highlights.md"; fi
    if [ "$FRAGMENT_COUNT" -eq 0 ]; then
        echo "prepare.sh: changelog.d/ holds no entry$HIGHLIGHTS_NOTE, which is well formed between a release and the next entry; ## [Unreleased] empty"
    else
        echo "prepare.sh: $FRAGMENT_COUNT fragment(s) under changelog.d/$HIGHLIGHTS_NOTE, all well formed, ## [Unreleased] empty"
    fi
    exit 0
fi

if [ "$FRAGMENT_COUNT" -eq 0 ]; then
    echo "prepare.sh: changelog.d/ holds no fragment; write one entry per user-visible change as changelog.d/<slug>.<section>.md" >&2
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

SECTION_FILE="$(mktemp)"
trap 'rm -f "$SECTION_FILE"' EXIT
{
    printf '## [%s] - %s\n' "$VERSION" "$DATE"
    if [ "$HAS_HIGHLIGHTS" -eq 1 ]; then
        printf '\n'
        # Verbatim, minus the trailing blank lines: the one blank line before
        # the first `### Section` is the cut's, like every other one here.
        awk '{ line[NR] = $0 }
             END { last = NR
                   while (last > 0 && line[last] ~ /^[[:space:]]*$/) last--
                   for (i = 1; i <= last; i++) print line[i] }' "$HIGHLIGHTS"
    fi
    for section in $SECTIONS; do
        first=1
        for fragment in changelog.d/*.md; do
            [ -e "$fragment" ] || continue
            case "${fragment##*/}" in _*) continue ;; esac
            [ "$(fragment_section "$fragment")" = "$section" ] || continue
            if [ "$first" -eq 1 ]; then
                printf '\n### %s\n' "$(section_title "$section")"
                first=0
            fi
            # The file is the entry's text; the bullet and the two-space
            # continuation indent belong to CHANGELOG.md, not to the fragment.
            awk 'NR == 1 { printf "- %s\n", $0; next }
                 /^[[:space:]]*$/ { next }
                 { printf "  %s\n", $0 }' "$fragment"
        done
    done
} > "$SECTION_FILE"

# The new section goes under a kept, empty ## [Unreleased]; the blank lines
# between the two headings are the cut's, so they are consumed here and not
# copied, which is what keeps the file from growing a blank line per release.
awk -v block="$SECTION_FILE" '
    state == 2 { print; next }
    state == 1 { if (/^## \[/) { print; state = 2 } next }
    /^## \[Unreleased\]$/ {
        print
        print ""
        while ((getline line < block) > 0) print line
        close(block)
        print ""
        state = 1
        next
    }
    { print }
' CHANGELOG.md > "$SECTION_FILE.new"
mv "$SECTION_FILE.new" CHANGELOG.md

# Deleted right after the cut, the point of no return: the entries are in
# CHANGELOG.md now, and a re-run would refuse the ## [x.y.z] heading anyway.
rm -f changelog.d/*.md

cargo update --workspace --quiet

echo "prepare.sh: $FRAGMENT_COUNT changelog entr$( [ "$FRAGMENT_COUNT" -eq 1 ] && printf 'y' || printf 'ies' ) released as $VERSION ($DATE):"
printf '%s\n' "$FRAGMENTS" | sed 's/^/  /'
if [ "$HAS_HIGHLIGHTS" -eq 1 ]; then echo "led by $HIGHLIGHTS"; fi
echo "members bumped:"
for m in $MEMBERS; do printf '  %s\n' "$m"; done
echo

# Highest version tag, not `git describe`: v0.2.4's commit is not an
# ancestor of main (it was replayed there before the history was unified), so
# describe can answer an older tag and the list reaches back a release too far.
LAST_TAG="$(git tag --list 'v[0-9]*' --sort=-v:refname | head -n 1)"
if [ -n "$LAST_TAG" ] && command -v gh >/dev/null 2>&1; then
    SINCE="$(git log -1 --format=%cI "$LAST_TAG")"
    if PRS="$(gh pr list --state merged --base main --search "merged:>$SINCE" --limit 200 \
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
        echo "pull requests merged into main since $LAST_TAG; each user-visible one needs a changelog.d/ fragment, one of those released above:"
        if [ -n "$UNRELEASED" ]; then printf '%s\n' "$UNRELEASED"; else echo "  (none)"; fi
    else
        echo "prepare.sh: could not list the pull requests merged since $LAST_TAG (gh offline or unauthenticated); check the fragments released above against them by hand"
    fi
    echo
    # A commit pushed straight to main never appears above. The commits
    # since the tag are taken by patch, not by date or ancestry: an old tag
    # need not be an ancestor of main (v0.2.4's commit was replayed), and a
    # commit authored before the tag can still be missing from it. The commits API
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
