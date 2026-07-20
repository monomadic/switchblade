#!/usr/bin/env bash
#
# Deterministic ffmpeg fixture corpus for the benchmark harness
# (benchmarks/TASKS.md Phase 2; benchmarks/design/phase-0-contracts.md).
#
# The matrix targets switchblade's known performance fault lines: the VT
# hardware path (h264/hevc), the heavy 4K60 + hw-scale path, the 10-bit
# 4:2:0 scale_vt gate, long-GOP exact-seek cost, VFR pts stamping, the
# rotation decode-dims swap, and the software (VP9) decode path.
#
# Fixtures are gitignored and regenerated on demand — identical ffmpeg
# args do NOT guarantee identical files across ffmpeg builds, so the
# emitted manifest.json records the ffmpeg version, the exact argv, and
# each artifact's sha256 as provenance (never an equality contract).
#
# Usage:  ./generate.sh [--force]   (--force rebuilds existing fixtures)
#
# testsrc2 carries a moving test pattern; timecode is NOT burned in (this
# ffmpeg has no drawtext/libfreetype), which is fine — pacing is measured
# from decoded pts, not pixels.

set -euo pipefail

FIX_DIR="$(cd "$(dirname "$0")" && pwd)"
FORCE=0
[[ "${1:-}" == "--force" ]] && FORCE=1

command -v ffmpeg >/dev/null || { echo "ffmpeg not on PATH" >&2; exit 1; }
command -v ffprobe >/dev/null || { echo "ffprobe not on PATH" >&2; exit 1; }

FFVER="$(ffmpeg -hide_banner -version | head -1)"

sha()  { shasum -a 256 "$1" | awk '{print $1}'; }
jesc() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }  # JSON-escape a scalar
cmd_json() {  # <argv...> -> JSON array string
  local out="[" first=1 a
  for a in "$@"; do
    [[ $first -eq 0 ]] && out+=","
    out+="\"$(jesc "$a")\""
    first=0
  done
  printf '%s]' "$out"
}

ENTRIES=()

# add_entry <name> <codec> <w> <h> <fps> <dur> <ffmpeg args, no output path>
# Runs ffmpeg (unless the file exists and --force wasn't given), then
# records the argv + sha256 into the manifest either way.
add_entry() {
  local name=$1 codec=$2 w=$3 h=$4 fps=$5 dur=$6; shift 6
  local out="$FIX_DIR/$name"
  local -a cmd=("$@" "$out")
  if [[ $FORCE -eq 1 || ! -f "$out" ]]; then
    echo ">> generating $name"
    "${cmd[@]}"
  else
    echo "== $name exists (skip; --force to rebuild)"
  fi
  ENTRIES+=("    {\"name\":\"$name\",\"codec\":\"$codec\",\"width\":$w,\"height\":$h,\"fps\":$fps,\"duration\":$dur,\"sha256\":\"$(sha "$out")\",\"cmd\":$(cmd_json "${cmd[@]}")}")
}

# 1. Baseline VT hardware path.
add_entry h264_1080p30.mp4 h264 1920 1080 30 10 \
  ffmpeg -y -f lavfi -i "testsrc2=size=1920x1080:rate=30:duration=10" \
  -c:v libx264 -preset ultrafast -pix_fmt yuv420p

# 2. Heavy path: 4K60 + the hw-scale chain (4K60→1440p was the PERF.md win).
add_entry hevc_2160p60.mp4 hevc 3840 2160 60 10 \
  ffmpeg -y -f lavfi -i "testsrc2=size=3840x2160:rate=60:duration=10" \
  -c:v libx265 -preset ultrafast -pix_fmt yuv420p -tag:v hvc1

# 3. 10-bit 4:2:0 — the scale_vt pix_fmt gate.
add_entry hevc_1080p30_10bit.mp4 hevc 1920 1080 30 10 \
  ffmpeg -y -f lavfi -i "testsrc2=size=1920x1080:rate=30:duration=10" \
  -c:v libx265 -preset ultrafast -pix_fmt yuv420p10le -tag:v hvc1

# 4. Single-GOP (one keyframe for the whole clip) — the exact-seek worst
#    case: a seek near the end decodes ~480 frames forward. Deliberately
#    sparser than x264's ~250-frame default so it contrasts with the
#    other fixtures regardless of encoder defaults.
add_entry h264_1080p30_longgop.mp4 h264 1920 1080 30 16 \
  ffmpeg -y -f lavfi -i "testsrc2=size=1920x1080:rate=30:duration=16" \
  -c:v libx264 -preset ultrafast -pix_fmt yuv420p \
  -g 100000 -keyint_min 100000 -sc_threshold 0

# 5. VFR: two rate segments concatenated, timings preserved (pts-delta path).
add_entry h264_720p30_vfr.mp4 h264 1280 720 30 8 \
  ffmpeg -y \
  -f lavfi -i "testsrc2=size=1280x720:rate=60:duration=4" \
  -f lavfi -i "testsrc2=size=1280x720:rate=15:duration=4" \
  -filter_complex "[0:v][1:v]concat=n=2:v=1:a=0[v]" -map "[v]" \
  -fps_mode vfr -c:v libx264 -preset ultrafast -pix_fmt yuv420p

# 6. Rotated 90° via a Display Matrix — consumers must swap decode dims or
#    portrait phone footage stretches. Two-step: ffmpeg 8 dropped the
#    legacy `rotate` metadata on output, so a landscape base is remuxed
#    with `-display_rotation` (an input option) to bake the matrix.
rot_out="$FIX_DIR/h264_1080p30_rot90.mp4"
rot_base="${TMPDIR:-/tmp}/sb_fixture_rot_base.mp4"
if [[ $FORCE -eq 1 || ! -f "$rot_out" ]]; then
  echo ">> generating h264_1080p30_rot90.mp4"
  ffmpeg -y -v error -f lavfi -i "testsrc2=size=1920x1080:rate=30:duration=10" \
    -c:v libx264 -preset ultrafast -pix_fmt yuv420p "$rot_base"
  ffmpeg -y -v error -display_rotation:v:0 90 -i "$rot_base" -c copy "$rot_out"
  rm -f "$rot_base"
else
  echo "== h264_1080p30_rot90.mp4 exists (skip; --force to rebuild)"
fi
rot_cmd=(ffmpeg -y -display_rotation:v:0 90 -i "<landscape testsrc2 base>" -c copy "$rot_out")
ENTRIES+=("    {\"name\":\"h264_1080p30_rot90.mp4\",\"codec\":\"h264\",\"width\":1920,\"height\":1080,\"fps\":30,\"duration\":10,\"rotation\":90,\"sha256\":\"$(sha "$rot_out")\",\"cmd\":$(cmd_json "${rot_cmd[@]}")}")

# 7. Software decode path (VP9 — benchmarked slower through VT, so sw).
add_entry vp9_720p30.webm vp9 1280 720 30 10 \
  ffmpeg -y -f lavfi -i "testsrc2=size=1280x720:rate=30:duration=10" \
  -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -pix_fmt yuv420p

# Manifest: provenance, not an equality contract.
{
  echo "{"
  echo "  \"ffmpeg_version\": \"$(jesc "$FFVER")\","
  echo "  \"note\": \"Regenerate with generate.sh. sha256 is ffmpeg-build/machine specific — provenance for a report, never an equality check.\","
  echo "  \"fixtures\": ["
  printf '%s,\n' "${ENTRIES[@]}" | sed '$ s/,$//'
  echo "  ]"
  echo "}"
} > "$FIX_DIR/manifest.json"

echo "wrote $FIX_DIR/manifest.json (${#ENTRIES[@]} fixtures)"
