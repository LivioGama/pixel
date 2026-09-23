#!/usr/bin/env bash
# Renders every composition into docs/examples/, in two forms:
#   <name>.mp4   1600x1000 H.264 for the website, with a <name>.jpg poster
#   <name>.webp  800x500 animated WebP at 15 fps for the README, which
#                GitHub renders inline where it would not play a video
#
# Usage: scripts/render.sh [CompositionId ...]   (default: all six)
# Needs ffmpeg and img2webp (brew install ffmpeg webp).
set -euo pipefail

cd "$(dirname "$0")/.."
examples=../examples
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

name_of() {
  case $1 in
    PixelComparison) echo pixel-measured-comparison ;;
    PixelImpact) echo pixel-impact-comparison ;;
    PixelScope) echo pixel-scope-comparison ;;
    PixelRollback) echo pixel-rollback-comparison ;;
    PixelPublish) echo pixel-publish-comparison ;;
    PixelRewrite) echo pixel-rewrite-comparison ;;
    *) echo "unknown composition $1" >&2; exit 1 ;;
  esac
}

ids=("$@")
[ ${#ids[@]} -gt 0 ] || ids=(PixelScope PixelComparison PixelImpact PixelRewrite PixelRollback PixelPublish)

for id in "${ids[@]}"; do
  name=$(name_of "$id")
  echo "== $id -> $name"
  bunx remotion render src/index.ts "$id" "$tmp/$name.mp4" --codec=h264 --crf=26 --log=error
  ffmpeg -loglevel error -y -i "$tmp/$name.mp4" -c copy -movflags +faststart "$examples/$name.mp4"
  # The poster is the last frame: the finished state, for reduced motion and
  # for the moment before the video starts.
  ffmpeg -loglevel error -y -sseof -0.1 -i "$tmp/$name.mp4" -frames:v 1 -q:v 3 "$examples/$name.jpg"
  mkdir -p "$tmp/$name"
  ffmpeg -loglevel error -y -i "$tmp/$name.mp4" -vf "fps=15,scale=800:500:flags=lanczos" "$tmp/$name/%04d.png"
  img2webp -loop 0 -lossy -q 70 -m 6 -d 67 "$tmp/$name"/*.png -o "$examples/$name.webp" >/dev/null
  ls -lh "$examples/$name.mp4" "$examples/$name.webp" | awk '{print "   ", $5, $9}'
done
