#!/usr/bin/env python3
"""Pull the review-05 comparison table out of a set of sb-bench bundles.

Usage: python3 extract.py <bundle-dir> [<bundle-dir> ...]

Everything printed here comes straight from summary.json — this script
only selects and formats. The instruments themselves are described in
docs/perf-reviews/05-zoom-storm-scheduler.md §1.
"""
import json
import sys


def row(d):
    s = json.load(open(f"{d}/summary.json"))
    c = s["counters"]
    gap = next((g for g in s["frame_gap_ms"] if g["lane"] == "selected"), None)
    over = {t["over_ms"]: t["count"] for t in s["tick_over"]}
    conds = {c["cond"]: c.get("at_s") for c in (s.get("conditions") or []) if c.get("met")}
    lat = {f"{l['lane']}/{l['metric']}": l for l in s["latencies"]}
    sel = lat.get("selected/spawn_to_served")
    proc = s.get("proc_curve") or []
    # proc_curve rows are [t_s, rss_mb, threads, ...]
    rss = max((p[1] for p in proc), default=0.0)
    thr = max((p[2] for p in proc), default=0.0)
    return {
        "run": d.rstrip("/").split("/")[-1],
        "scenario": s["scenario"],
        "valid": s["valid"],
        "wall_s": s["wall_s"],
        "thumb_requests": c.get("thumb_requests", 0),
        "visible_tiles_max": c.get("visible_tiles_max", 0),
        "atlas_slots": s.get("atlas_slots", 0),
        "atlas_full_drops": c.get("atlas_full_drops", 0),
        "qw_thumb_n": c.get("queue_wait_thumb_count", 0),
        "qw_thumb_mean_ms": (c.get("queue_wait_thumb_us", 0) / max(c.get("queue_wait_thumb_count", 1), 1)) / 1000.0,
        "qw_thumb_max_ms": c.get("queue_wait_thumb_max_us", 0) / 1000.0,
        "qw_gen_mean_ms": (c.get("queue_wait_gen_us", 0) / max(c.get("queue_wait_gen_count", 1), 1)) / 1000.0,
        "qw_gen_max_ms": c.get("queue_wait_gen_max_us", 0) / 1000.0,
        "decode_reads": c.get("decode_reads", 0),
        "decode_mean_ms": (c.get("decode_read_us", 0) / max(c.get("decode_reads", 1), 1)) / 1000.0,
        "decode_max_ms": c.get("decode_read_max_us", 0) / 1000.0,
        "decode_over_100ms": c.get("decode_read_over_100ms", 0),
        "decode_over_1s": c.get("decode_read_over_1s", 0),
        "render_stall_max_ms": c.get("render_stall_max_us", 0) / 1000.0,
        "render_meta_n": c.get("render_stall_meta_count", 0),
        "tick_max_ms": s["tick_ms"]["max"],
        "over_16": over.get(16.0, 0),
        "over_100": over.get(100.0, 0),
        "over_250": over.get(250.0, 0),
        "over_1000": over.get(1000.0, 0),
        "gap_p95_ms": gap["p95_ms"] if gap else None,
        "gap_max_ms": gap["max_ms"] if gap else None,
        "late": c.get("late_frames", 0),
        "reanchors": c.get("reanchors", 0),
        "util": s["worker_utilisation"],
        "jobs": c.get("jobs_started", 0),
        "thumbs_cached": c.get("thumbs_cached", 0),
        "sel_served_ms": sel["max_ms"] if sel else None,
        "lib_met_s": conds.get("library_count"),
        "selserved_met_s": conds.get("selected_served"),
        "rss_peak_mb": rss,
        "threads_peak": thr,
    }


FIELDS = [
    ("scenario", "{}"), ("valid", "{}"), ("wall_s", "{:.0f}"),
    ("thumb_requests", "{}"),
    ("visible_tiles_max", "{}"), ("atlas_slots", "{}"), ("atlas_full_drops", "{}"),
    ("qw_thumb_n", "{}"), ("qw_thumb_mean_ms", "{:.0f}"), ("qw_thumb_max_ms", "{:.0f}"),
    ("qw_gen_mean_ms", "{:.0f}"), ("qw_gen_max_ms", "{:.0f}"),
    ("decode_reads", "{}"), ("decode_mean_ms", "{:.2f}"), ("decode_max_ms", "{:.1f}"),
    ("decode_over_100ms", "{}"), ("decode_over_1s", "{}"),
    ("render_stall_max_ms", "{:.1f}"), ("render_meta_n", "{}"),
    ("tick_max_ms", "{:.1f}"), ("over_16", "{}"), ("over_100", "{}"),
    ("over_250", "{}"), ("over_1000", "{}"),
    ("gap_p95_ms", "{:.1f}"), ("gap_max_ms", "{:.1f}"),
    ("late", "{}"), ("reanchors", "{}"),
    ("util", "{:.2f}"), ("jobs", "{}"), ("thumbs_cached", "{}"),
    ("sel_served_ms", "{:.0f}"), ("lib_met_s", "{:.1f}"), ("selserved_met_s", "{:.1f}"),
    ("rss_peak_mb", "{:.0f}"), ("threads_peak", "{:.0f}"),
]

rows = [row(d) for d in sys.argv[1:]]
w = max(len(f) for f, _ in FIELDS) + 2
hdr = "".ljust(w) + "".join(r["run"].rjust(16) for r in rows)
print(hdr)
print("-" * len(hdr))
for f, fmt in FIELDS:
    line = f.ljust(w)
    for r in rows:
        v = r.get(f)
        line += ("—" if v is None else fmt.format(v)).rjust(16)
    print(line)
