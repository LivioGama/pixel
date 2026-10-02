#!/usr/bin/env bash
# Arena entrypoint: per-arm repo preparation (indexing + codex wiring), then
# the scored codex run. $ARM_TOOL selects the arm; $PROMPT/$OUT are the task.
set -uo pipefail
export PATH="$HOME/.local/bin:$PATH"
cd /repo

prep_semble()  { :; }                     # indexes lazily on first query
prep_graft()   { graft init --yes; }
prep_stacklit() {
  npx -y stacklit init
  npx -y stacklit generate
  npx -y stacklit derive > /root/.codex/AGENTS.md
}
prep_gitnexus() { gitnexus setup && gitnexus analyze /repo; }
prep_gortex()  {
  gortex daemon start
  gortex track /repo
  sleep 20   # daemon indexes async; give the warm-up a window
  codex mcp add gortex -- gortex mcp
}
prep_pixel()   { pixel install && pixel build-index --history /repo; }

case "${ARM_TOOL:-raw}" in
  raw)      : ;;
  semble)   prep_semble ;;
  graft)    prep_graft ;;
  stacklit) prep_stacklit ;;
  gitnexus) prep_gitnexus ;;
  gortex)   prep_gortex ;;
  pixel)    prep_pixel ;;
esac

# MCP arms: register the tool's stdio server with codex
case "${ARM_TOOL:-raw}" in
  semble)   codex mcp add semble -- uvx --from 'semble[mcp]' semble ;;
  graft)    codex mcp add graft -- graft mcp ;;
  stacklit) codex mcp add stacklit -- npx -y stacklit serve ;;
  gitnexus) : ;;   # gitnexus setup wrote its own MCP entries
  gortex)   : ;;   # registered in prep_gortex
  pixel)    : ;;   # pixel wires itself via pixel install
esac

# The container is the sandbox: docker's default seccomp blocks codex's
# bubblewrap namespaces, so read-only mode would fail every command.
exec codex exec --json --sandbox danger-full-access --skip-git-repo-check "$PROMPT" > "$OUT" 2> /tmp/arena-err
