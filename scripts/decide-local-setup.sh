#!/usr/bin/env bash
# decide-local-setup.sh — set up the local 4B decision backend for
# `pixel classify --backend remote --remote-preset local`.
#
# The `local` preset points the same OpenAI-compatible adapter at a
# llama.cpp server (or plain Ollama) on localhost, so a 4B model runs the
# decision on your own CPU/GPU with $0 marginal cost per decision.
#
# This script downloads the GGUF and prints the launch command. It does not
# start a long-running server in the foreground (you run that yourself in a
# terminal or as a service).
#
# Requires: a llama.cpp build (`llama-server` on PATH) OR Ollama, and curl.
set -euo pipefail

# The Qwen model this repo's decision recipe targets. Override with the env
# var to pick a different GGUF or an Ollama tag.
MODEL_REPO="${QWEN_GGUF_REPO:-Qwen/Qwen3-4B-Instruct-GGUF}"
MODEL_FILE="${QWEN_GGUF_FILE:-qwen3-4b-instruct-q4_k_m.gguf}"
MODEL_DIR="${QWEN_GGUF_DIR:-$HOME/.cache/pixel/decide-local}"

# Which runtime to prepare: `llama` or `ollama`.
RUNTIME="${1:-llama}"

if [[ "$RUNTIME" == "ollama" ]]; then
  # Ollama path: `ollama pull qwen3:4b && ollama serve`
  if ! command -v ollama >/dev/null 2>&1; then
    echo "error: 'ollama' not found. Install Ollama, then re-run." >&2
    exit 1
  fi
  echo "Pulling qwen3:4b (this can take a few minutes on first run)…"
  ollama pull qwen3:4b
  echo
  echo "Next, run in a terminal:"
  echo "  ollama serve"
  echo
  echo "Then classify with:"
  echo "  pixel classify --text '...' --label a --label b --backend remote --remote-preset local --remote-model qwen3:4b"
  echo
  echo "Model cached for Ollama. The '--remote-model' above matches the Ollama tag."
  exit 0
fi

if ! command -v llama-server >/dev/null 2>&1; then
  echo "error: 'llama-server' not found on PATH." >&2
  echo "Build llama.cpp (or install a packaged llama-server) first, e.g.:"
  echo "  cmake -B build && cmake --build build -j --config Release"
  echo "then add build/bin to PATH." >&2
  exit 1
fi
if ! command -v curl >/dev/null 2>&1; then
  echo "error: 'curl' not found." >&2
  exit 1
fi

mkdir -p "$MODEL_DIR"
MODEL_PATH="$MODEL_DIR/$MODEL_FILE"

if [[ ! -f "$MODEL_PATH" ]]; then
  URL="https://huggingface.co/$MODEL_REPO/resolve/main/$MODEL_FILE"
  echo "Downloading $URL → $MODEL_PATH (this can be a few GB)…"
  curl -L --fail --progress-bar -o "$MODEL_PATH.tmp" "$URL"
  mv "$MODEL_PATH.tmp" "$MODEL_PATH"
fi

echo
echo "Model ready at: $MODEL_PATH"
echo
echo "Launch the server in a terminal:"
echo "  llama-server -m '$MODEL_PATH' --port 11434 -c 8192"
echo
echo "Verify it answers:"
echo "  curl -s http://localhost:11434/v1/chat/completions -H 'Content-Type: application/json' \\"
echo "    -d '{\"model\":\"qwen3-4b\",\"messages\":[{\"role\":\"user\",\"content\":\"ping\"}]}'"
echo
echo "Then classify with:"
echo "  pixel classify --text '...' --label a --label b --backend remote --remote-preset local --remote-model qwen3-4b"
