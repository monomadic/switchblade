#!/usr/bin/env bash
# Build a sweep-vs-playback corpus and emit its scenario toml.
#
# The gen-sweep-cap commit (6f82ad9) protects the *presenting* stream from the
# background thumbnail sweep's CPU / VT-media-engine / disk contention. To make
# that contention observable in Tier A we need a corpus large enough that the
# sweep is still grinding through the WHOLE playback window: one modest played
# clip (index 0, smooth 1080p live decode) followed by many heavy 4K sources
# whose seeked extracts are the expensive sweep work.
#
# The default cache key is `size_mtime` and ignores the path, so plain copies of
# one file collide into a single cache entry (= one gen job). We give each copy
# a DISTINCT mtime second (`touch -t`) so every copy is its own fingerprint =
# its own gen job — a real N-clip sweep without N re-encodes.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
fixtures="$here/../fixtures"
corpus="$fixtures/corpus"
count="${1:-40}"          # heavy sweep clips (plus one played clip)
played="$fixtures/h264_1080p30.mp4"
heavy="$fixtures/hevc_2160p60.mp4"

for f in "$played" "$heavy"; do
  [ -f "$f" ] || { echo "missing fixture $f — run benchmarks/fixtures/generate.sh" >&2; exit 1; }
done

rm -rf "$corpus"
mkdir -p "$corpus"

# Played clip sorts + ingests first (index 0 = the stream we watch).
cp "$played" "$corpus/aa_played_h264_1080p30.mp4"
touch -t 202001010000.00 "$corpus/aa_played_h264_1080p30.mp4"

# Heavy sweep clips, each with a distinct mtime second → distinct cache entry.
for i in $(seq -w 1 "$count"); do
  dst="$corpus/sweep_${i}_hevc_2160p60.mp4"
  cp "$heavy" "$dst"
  # Distinct minute/second per copy (safe well past 60 via arithmetic).
  n=$((10#$i))
  mm=$(printf "%02d" $(( n / 60 )))
  ss=$(printf "%02d" $(( n % 60 )))
  touch -t "202002010${mm}.${ss}" "$dst" 2>/dev/null || touch -t "2020020100${mm}.${ss}" "$dst"
done

# Emit the scenario with an explicit, ordered inputs list (deterministic index 0).
scen="$here/sweep_vs_playback.toml"
{
  cat <<'HEADER'
name = "sweep_vs_playback"

# Prose intent — read retrospectively, never parsed.
intent = """
Cold start, a large corpus: one modest 1080p clip first (the one we watch)
followed by many heavy 4K/60 clips. Open the first clip in quickview and let
it play for 8s while the background thumbnail gen sweep grinds through the 4K
sources — exactly the load the "cap gen-sweep concurrency while a live stream
presents" commit (6f82ad9) targets.

Expected behavior (the commit's thesis): while the stream presents, the sweep
must not starve it. Watch late_frames / reanchors DURING the 8s window and the
selected lane's spawn_to_served — the capped build should hold them at/near
zero; an uncapped build lets the sweep's concurrent 4K extracts contend for
CPU and the VideoToolbox media engine.

Fidelity note: on a fast LOCAL SSD the disk-band half of the commit
(taskpolicy -b) has little to bite on — files are ~20MB and warm in the page
cache. This scenario exercises the CPU + VT-media-engine contention; the
disk-starvation half needs an external-drive corpus (Phase 5.1) or Tier B.
"""

[setup]
animation = "normal"
viewport = [1280, 800]
refresh_hz = 60.0
max_wall_s = 90.0
inputs = [
HEADER
  printf '    "benchmarks/fixtures/corpus/aa_played_h264_1080p30.mp4",\n'
  for i in $(seq -w 1 "$count"); do
    printf '    "benchmarks/fixtures/corpus/sweep_%s_hevc_2160p60.mp4",\n' "$i"
  done
  cat <<FOOTER
]

# Wait for the whole corpus to ingest.
[[step]]
action = "wait_until"
cond = "library_count"
n = $((count + 1))
timeout = 30.0

# Settle a beat in the grid.
[[step]]
action = "wait"
secs = 0.5

# Open quickview on the default-selected first clip (the 1080p one).
[[step]]
action = "key"
key = "space"

# The modal stream must serve a first frame before we start the clock.
[[step]]
action = "wait_until"
cond = "selected_served"
timeout = 20.0

# Watch for 8s WHILE the sweep runs — late_frames / reanchors accrue here.
[[step]]
action = "wait"
secs = 8.0

[validity]
require = ["library_count", "selected_served"]
FOOTER
} > "$scen"

bytes=$(du -sh "$corpus" | cut -f1)
echo "corpus: $((count + 1)) clips ($bytes) → $corpus"
echo "scenario: $scen"
