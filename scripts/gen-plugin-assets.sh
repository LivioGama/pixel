#!/bin/sh
# gen-plugin-assets.sh — regenerate every plugin-manifest surface from the
# canonical agent prompt so each agent CLI can install pixel through its own
# native plugin mechanism (ponytail-style).
#
# Source of truth: crates/pixel-install/assets/pixel-agent-prompt.md
#
# Usage:
#   scripts/gen-plugin-assets.sh          # write all derived files
#   scripts/gen-plugin-assets.sh --check  # exit 1 if any derived file is stale
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
SRC="$ROOT/crates/pixel-install/assets/pixel-agent-prompt.md"
VERSION=$(grep -m1 '^version' "$ROOT/crates/pixel/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')

[ -f "$SRC" ] || { echo "missing $SRC" >&2; exit 1; }

# Header injected into every distributed surface. Plugin installs reach users
# whose PATH may lack the binary — the prompt itself must bootstrap it. No
# hooks: the agent runs the installer, matching the shell-wrapper model.
bootstrap() {
  cat <<'EOF'
## Setup — the `pixel` binary is required

Pixel is a CLI, not just instructions. Before relying on any command below,
verify it exists:

    command -v pixel || curl -fsSL https://raw.githubusercontent.com/LivioGama/pixel/main/scripts/install.sh | sh

Then make sure the repo is indexed (once per clone/worktree):

    pixel index

If `.pixel/` already exists in the repo root, skip straight to the commands.

EOF
}

emit() { # $1 = dest, $2 = frontmatter (may be empty)
  dest="$1"
  front="$2"
  tmp="${dest}.tmp.$$"
  {
    [ -n "$front" ] && { printf '%s\n' "$front"; }
    bootstrap
    cat "$SRC"
  } >"$tmp"
  mv "$tmp" "$dest"
}

# --check mode: verify generated files are in sync without rewriting
if [ "${1:-}" = "--check" ]; then
  stale=0
  for f in \
    "skills/pixel/SKILL.md" \
    ".openclaw/skills/pixel/SKILL.md" \
    ".cursor/rules/pixel.mdc" \
    ".windsurf/rules/pixel.md" \
    ".kiro/steering/pixel.md" \
    ".qoder/rules/pixel.md" \
    ".clinerules/pixel.md" \
    ".agents/rules/pixel.md" \
    "PIXEL.md"; do
    [ -f "$ROOT/$f" ] || { echo "STALE: $f missing"; stale=1; }
  done
  # Rebuild into a scratch dir and diff each file
  TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
  for f in skills/pixel/SKILL.md .openclaw/skills/pixel/SKILL.md \
           .cursor/rules/pixel.mdc .windsurf/rules/pixel.md \
           .kiro/steering/pixel.md .qoder/rules/pixel.md \
           .clinerules/pixel.md .agents/rules/pixel.md PIXEL.md; do
    mkdir -p "$TMP/$(dirname "$f")"
  done
  # shellcheck disable=SC2034 # ROOT switch for the emit pass below
  OUT_ROOT="$TMP" "$0" >/dev/null
  for f in skills/pixel/SKILL.md .openclaw/skills/pixel/SKILL.md \
           .cursor/rules/pixel.mdc .windsurf/rules/pixel.md \
           .kiro/steering/pixel.md .qoder/rules/pixel.md \
           .clinerules/pixel.md .agents/rules/pixel.md PIXEL.md; do
    if ! cmp -s "$ROOT/$f" "$TMP/$f" 2>/dev/null; then
      echo "STALE: $f"
      stale=1
    fi
  done
  [ "$stale" -eq 0 ] && echo "plugin assets in sync (v$VERSION)"
  exit "$stale"
fi

# OUT_ROOT lets --check render into a scratch dir; default is the repo root.
OUT_ROOT=${OUT_ROOT:-$ROOT}

SKILL_FRONT='---
name: pixel
description: >
  Deterministic code retrieval: indexed search, concept resolve, impact
  analysis, caller/callee tracing, task targets, plan generation, and git
  history archaeology via the `pixel` CLI. Use when the repo has a `.pixel`
  directory, when the user mentions pixel, or before editing a symbol when
  blast radius matters. Bootstraps the binary via the install script when
  `pixel` is not on PATH.
license: MIT
---
'

CURSOR_FRONT='---
description: Pixel retrieval layer — mandatory usage protocol for the pixel CLI (impact, targets, plan, resolve, excavate). Bootstraps the binary if missing.
alwaysApply: true
---
'

write_file() { # $1 = rel path, $2 = frontmatter
  dest="$OUT_ROOT/$1"
  mkdir -p "$(dirname "$dest")"
  emit "$dest" "$2"
  echo "wrote $1"
}

write_file "skills/pixel/SKILL.md"            "$SKILL_FRONT"
write_file ".openclaw/skills/pixel/SKILL.md"  "$SKILL_FRONT"
write_file ".cursor/rules/pixel.mdc"          "$CURSOR_FRONT"
write_file ".windsurf/rules/pixel.md"         ""
write_file ".kiro/steering/pixel.md"          ""
write_file ".qoder/rules/pixel.md"            ""
write_file ".clinerules/pixel.md"             ""
write_file ".agents/rules/pixel.md"           ""
write_file "PIXEL.md"                          ""

echo "plugin assets generated from pixel-agent-prompt.md (v$VERSION)"
