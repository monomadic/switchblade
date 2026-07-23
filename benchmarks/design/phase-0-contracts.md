# Phase 0 — definitions & contracts

These are the load-bearing agreements the runner is built on. They exist so
measurements are honest and comparable *before* any runner code is written. Nothing
here declares a performance number "good" or "bad" — that judgment is retrospective
and agentic (see [HARNESS.md](../HARNESS.md)). File:line references are point-in-time
anchors to the hooks each contract leans on; verify against current code when
implementing.

Status: **0.2 is implemented** (`sb-window::schedule`). 0.1, 0.3, 0.4, 0.5 are
specified here and implemented in Phase 1–3.

---

## 0.1 Measurement & event dictionary

Two record types. **Counters/gauges** are cheap, monotonic, and compiled in always.
**Events** are timestamped, carry identity, and are traced only during bench runs.
Latency is *always* derived from a matched pair of events — never a single
pre-aggregated number — because the interesting latencies start and end in different
layers (app action vs. media first-frame) and on different lane incarnations.

### Identity carried on every live-lane event

- `clip_id` — stable across shuffle/D-swap (use the clip's path, not its live index;
  indices churn, see the shuffle remap and `pending_reselect`).
- `lane_class` — `selected` | `warm` | `hover`.
- `lane_gen` — a monotonic incarnation id minted at decoder spawn. **Load-bearing:**
  a frame served by an obsolete lane (target moved before its first frame arrived)
  must never be credited to the current action. Without `lane_gen` a late cold-spawn
  result gets mis-attributed to whatever the selection is *now*.

### Latency classes (each = end_event − start_event, kept separate)

| Name | Start | End | Tier |
|---|---|---|---|
| `decode_spawn_to_ready` | decoder spawn request | first frame decoded & queued | A |
| `action_to_served` | user/attention action retargets the selected lane | first frame of the new clip served by the app into the hires texture | A |
| `action_to_presented` | same action | first frame actually presented (GPU) | **B only** |
| `promotion_to_served` | warm lane promoted to selected | its already-queued frame served | A |

**Why `promotion_to_served` is not a spawn measurement:** a warm lane
(`warm: Vec<SelLive>`) runs its decoder and parks frames via bounded-queue
backpressure *long before* the promotion action. Its `first_frame: Option<Instant>`
is already `Some` at promotion time. Pooling it into a spawn histogram would
understate cold cost and overstate warm cost. The hook is the promotion site
(sb-app `update_live`, ~`lib.rs:3104` — `self.warm.remove(pos)`); `first_frame`
already exists on both selected and warm `SelLive` (~`lib.rs:236`/`251`).

**Re-anchors are media-layer events.** Pacing re-anchors happen inside
`SeekablePlayer` (sb-media `seekable.rs:~657`, the `LATE_SLACK` branch) — `sb-app`
cannot observe them. The media layer must emit `frame_reanchored { clip_id, lane_gen }`
directly. This is the single clearest proof that a one-`Probe`-in-sb-app design (the
pre-review plan) could not reconstruct the timings.

### Always-on counters (fold in the existing `RedrawStats`, ~`lib.rs:514`)

`frames`, `idle`, and the redraw-cause tallies (`motion` / `transition` / `live` /
`timer`), plus `slots_used` / `slots_demand` (atlas high-water). Add: `late_frames`,
`reanchors`, `drain_budget_hits`, `evictions`. These stay compiled in (one
`log_enabled!`-class check); the timestamped event stream is gated behind the bench
build/flag.

---

## 0.2 Scheduler policy — IMPLEMENTED

Extracted to [`crates/sb-window/src/schedule.rs`](../../crates/sb-window/src/schedule.rs):
`next_frame(animating, occluded, redraw_at, last_frame, now) -> NextFrame`, with
`IDLE_TICK` / `MIN_FRAME` moved alongside. `about_to_wait` now calls it; 5 unit tests
pin the animating/idle/occluded/live-deadline branches.

**Contract for Tier A:** the headless runner MUST drive `frame()` through
`schedule::next_frame`, not a fixed cadence. Fixed polling would (a) manufacture idle
redraws that never happen in the real loop and (b) — because `drain_media`'s upload
work is budgeted *per frame* — inflate cache-sweep throughput, making cache-fill
timings fiction. The runner owns the wall clock and `last_frame`/`occluded` state and
advances exactly as the verdict says: `Now{poll:true}` → step immediately;
`Now{poll:false}` → step (idle deadline reached); `At(t)` → advance the clock to `t`
(real sleep in wall-clock mode, or a virtual jump in fast-replay mode) then step.

---

## 0.3 Process-per-run orchestration contract

**One child process per repetition. No exceptions.** Cause: `cache_root()` and the
cache-key fingerprint are process-global `OnceLock`s, first-write-wins (sb-media
`lib.rs:~1481`). In-process repeats would silently reuse run 1's cache root even with
a rotated temp `HOME`. Fresh processes also make panic/exit-code validity reliable and
stop one run's decoder threads / allocator state leaking into the next.

Orchestrator ↔ runner protocol:

1. Orchestrator prepares the per-run temp environment: temp `HOME` (→ isolated
   `cache_root`), seeds or empties the cache, materializes/links fixtures, writes the
   resolved scenario.
2. Orchestrator spawns the runner as a child: `sb-bench run <scenario> --out <dir>`
   with `HOME` and `XDG_CACHE_HOME` set into the temp env.
3. Runner executes, streams events to `<dir>/events.jsonl` (buffered, see 0.1 / task
   1.4), writes `<dir>/summary.json` and `<dir>/run.json` (env fingerprint) at the
   end.
4. Runner exit code feeds the validity gate: `0` = completed; non-zero / signal =
   invalid (panic, timeout, readiness failure). The orchestrator never trusts a
   summary from a non-zero exit.

Repetitions are **serialized** (never parallel — ffmpeg contention skews timing).

---

## 0.4 Readiness semantics

Scenario actions are ordered by **conditions**, not just timestamps. A bare "at t=1s,
hover tile 49" races async ingest, layout, thumb arrival, and motion settle, producing
noise that looks like signal. `wait_until` is a first-class primitive: a named
condition + a timeout + a **recorded outcome** (met, and when / timed out). A timeout
marks the run invalid via the gate; it is never silently skipped.

Named conditions (each maps to existing observable state):

| Condition | Signal |
|---|---|
| `library_count(n)` | `clips.len() >= n` |
| `ingest_closed` | producer EOF — "stdin closed — N clips ingested" (`lib.rs:~2287`) |
| `selected_served` | selected lane `first_frame.is_some()` |
| `grid_settled` | `Frame.animating == false` |
| `cache_swept(n)` | ≥ n clips `cached` (flips on GenDone / any artifact) |
| `cache_thumbs(n)` | ≥ n thumb artifacts on disk |

Actions reference fixtures by **name/role** ("the 4K60 hevc clip", "last clip"), not
raw tile index — indices shift with layout, zoom, and library changes. The runner
resolves a role to live geometry via `tile_rect` at action time.

`intent` (prose) and `[validity]` (mechanical requirements like "selected_served must
have fired") are **separate fields**. Validity is never a performance judgment.

---

## 0.5 Tier A vs Tier B claim boundary

Each metric is owned by exactly one tier. A report must not source a Tier-B claim from
Tier-A data.

**Tier A (headless) may support:** decode timing (`decode_spawn_to_ready`),
app-served latency (`action_to_served`, `promotion_to_served`), pacing health
(`late_frames`, `reanchors` — a frame the player couldn't serve on time is a real
miss regardless of GPU), cache-fill progress and timing, atlas slot demand/eviction,
`frame()` tick CPU cost, thread/decoder leak canaries. All **comparative, not
absolute** — Tier A has no vsync-blocking present, so "served on time" is a proxy for
"would have presented on time".

**Tier B only (real winit/wgpu):** present-to-present intervals and visible hitches,
`action_to_presented`, GPU upload bytes/frame and `MEDIA_UPLOAD_BUDGET_LIVE` pressure,
blur/backdrop cost, any "does it *look* smooth" claim.

**Neither tier** substitutes for the DESIGN.md §15 live-feel evaluation. Instrumentation
can measure decoder churn and action-to-frame latency for the attention-lane verdict,
but hover feel and misclick-modal cost are human judgments; the bench informs that
decision, it does not make it.
