#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Gource contributors
# SPDX-License-Identifier: GPL-3.0-or-later
set -Eeuo pipefail

script_dir=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
repo_root=$(CDPATH='' cd -- "$script_dir/.." && pwd -P)
repository=$repo_root
launch_view=false
repository_set=false

usage() {
    cat <<'EOF'
Usage: scripts/dogfood.sh [--view] [REPOSITORY]

Run the complete local quality gates, build the release binary, ingest the
selected Git repository through serial and parallel replay, exercise a private
cache miss and hit, render the complete selected repository history to FFV1
video, export deterministic PNG reference frames, and build a native package.

Options:
  --view       Launch the interactive viewer on REPOSITORY after all automated
               checks pass. The viewer runs until it is closed.
  -h, --help   Show this help.

Environment:
  DOGFOOD_THREADS               Diagnose worker count (default: 4).
  DOGFOOD_HISTORY_SECONDS       Repository-time window used by diagnose from
                                the first commit (default: 604800, one week).
  DOGFOOD_VIDEO_VIEWPORT        Full-history video WIDTHxHEIGHT (default:
                                1280x720).
  DOGFOOD_VIDEO_FRAME_RATE      Full-history video FPS or ratio (default: 30).
  DOGFOOD_VIDEO_SECONDS_PER_DAY Full-history pacing (default: 0.1).
  CARGO_TARGET_DIR              Cargo target directory; artifacts are kept
                                below it.
EOF
}

while (($#)); do
    case $1 in
        --view)
            launch_view=true
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --*)
            printf 'dogfood: unknown option: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
        *)
            if [[ $repository_set == true ]]; then
                printf 'dogfood: only one repository path may be supplied\n' >&2
                exit 2
            fi
            repository=$1
            repository_set=true
            ;;
    esac
    shift
done

for command in cargo git python3 ffmpeg ffprobe; do
    if ! command -v "$command" >/dev/null 2>&1; then
        printf 'dogfood: required command not found: %s\n' "$command" >&2
        exit 1
    fi
done

if ! [[ ${DOGFOOD_THREADS:-4} =~ ^[1-9][0-9]*$ ]]; then
    printf 'dogfood: DOGFOOD_THREADS must be a positive integer\n' >&2
    exit 2
fi
threads=${DOGFOOD_THREADS:-4}
if ! [[ ${DOGFOOD_HISTORY_SECONDS:-604800} =~ ^[1-9][0-9]*$ ]]; then
    printf 'dogfood: DOGFOOD_HISTORY_SECONDS must be a positive integer\n' >&2
    exit 2
fi
history_seconds=${DOGFOOD_HISTORY_SECONDS:-604800}
video_viewport=${DOGFOOD_VIDEO_VIEWPORT:-1280x720}
if [[ $video_viewport =~ ^([1-9][0-9]*)x([1-9][0-9]*)$ ]]; then
    video_width=${BASH_REMATCH[1]}
    video_height=${BASH_REMATCH[2]}
else
    printf 'dogfood: DOGFOOD_VIDEO_VIEWPORT must be WIDTHxHEIGHT\n' >&2
    exit 2
fi
video_frame_rate=${DOGFOOD_VIDEO_FRAME_RATE:-30}
video_seconds_per_day=${DOGFOOD_VIDEO_SECONDS_PER_DAY:-0.1}
python3 -c 'import math, sys; value = float(sys.argv[1]); assert math.isfinite(value) and value > 0' \
    "$video_seconds_per_day" || {
    printf 'dogfood: DOGFOOD_VIDEO_SECONDS_PER_DAY must be finite and positive\n' >&2
    exit 2
}

repository=$(CDPATH='' cd -- "$repository" && pwd -P)
git -C "$repository" rev-parse --is-inside-work-tree >/dev/null
history_start=$(
    git -C "$repository" log --format=%ct HEAD |
        python3 -c 'import sys; values = [int(line) for line in sys.stdin if line.strip()]; print(min(values)) if values else sys.exit(1)'
) || {
    printf 'dogfood: repository HEAD contains no commits\n' >&2
    exit 1
}
history_end=$(
    python3 -c 'import sys; value = int(sys.argv[1]) + int(sys.argv[2]); assert value <= 9223372036854775807; print(value)' \
        "$history_start" "$history_seconds"
) || {
    printf 'dogfood: bounded repository-time window exceeds i64\n' >&2
    exit 1
}

case ${CARGO_TARGET_DIR:-target} in
    /*) target_dir=${CARGO_TARGET_DIR:-target} ;;
    *) target_dir=$repo_root/${CARGO_TARGET_DIR:-target} ;;
esac
mkdir -p "$target_dir/dogfood"
artifacts=$(mktemp -d "$target_dir/dogfood/run.XXXXXX")
binary=$target_dir/release/gource-app
fixture=$repo_root/tests/fixtures/visual-hierarchy-activity.log
frames=$artifacts/frames
video=$artifacts/repository-history.mkv
cache=$artifacts/cache

printf 'dogfood: repository=%s\n' "$repository"
printf 'dogfood: replay-window=%s..%s\n' "$history_start" "$history_end"
printf 'dogfood: full-video=%s at %s fps, %s seconds/day\n' \
    "$video_viewport" "$video_frame_rate" "$video_seconds_per_day"
printf 'dogfood: artifacts=%s\n' "$artifacts"

cd "$repo_root"
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked --release --package gource-app

"$binary" diagnose --input "$repository" --threads "$threads" --end "$history_end" \
    >"$artifacts/diagnose.json"
python3 packaging/archive.py --verify-diagnose "$artifacts/diagnose.json"

"$binary" diagnose --input "$fixture" --threads "$threads" \
    --cache-dir "$cache" --cache-bytes 1073741824 \
    >"$artifacts/diagnose-cache-miss.json"
python3 packaging/archive.py --verify-diagnose "$artifacts/diagnose-cache-miss.json"

"$binary" diagnose --input "$fixture" --threads "$threads" \
    --cache-dir "$cache" --cache-bytes 1073741824 \
    >"$artifacts/diagnose-cache-hit.json"
python3 packaging/archive.py --verify-diagnose "$artifacts/diagnose-cache-hit.json"

"$binary" export --input "$fixture" --viewport 320x180 --frame-rate 2 \
    --realtime --output "$frames"
shopt -s nullglob
frame_files=("$frames"/frame_*.png)
shopt -u nullglob
if ((${#frame_files[@]} != 130)); then
    printf 'dogfood: expected 130 PNG frames, found %s\n' "${#frame_files[@]}" >&2
    exit 1
fi
[[ -s $frames/manifest.toml ]]

"$binary" export --input "$repository" --viewport "$video_viewport" \
    --frame-rate "$video_frame_rate" --seconds-per-day "$video_seconds_per_day" \
    --auto-skip-seconds 3 --camera track --seed 31 --video --output "$video"
ffprobe -v error -select_streams v:0 \
    -show_entries stream=codec_name,width,height -show_entries format=duration \
    -of default=noprint_wrappers=1 "$video" >"$artifacts/video-probe.txt"
if ! grep -Fxq 'codec_name=ffv1' "$artifacts/video-probe.txt" ||
   ! grep -Fxq "width=$video_width" "$artifacts/video-probe.txt" ||
   ! grep -Fxq "height=$video_height" "$artifacts/video-probe.txt"; then
    printf 'dogfood: full-history FFV1 metadata did not match the requested viewport\n' >&2
    exit 1
fi
[[ -s ${video%.mkv}.manifest.toml ]]

case "$(uname -s):$(uname -m)" in
    Linux:x86_64)
        packaging/linux-x86_64.sh --output-dir "$artifacts/package"
        ;;
    Darwin:x86_64|Darwin:arm64)
        packaging/macos.sh --output-dir "$artifacts/package"
        ;;
    *)
        printf 'dogfood: no native package wrapper for %s; runtime checks completed\n' \
            "$(uname -s):$(uname -m)"
        ;;
esac

printf 'dogfood: PASS\n'
printf 'dogfood: artifacts=%s\n' "$artifacts"
printf 'dogfood: full-history-video=%s\n' "$video"

if [[ $launch_view == true ]]; then
    exec "$binary" view --input "$repository" --viewport 1280x720
fi
