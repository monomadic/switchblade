# Performance improvement tasks

This is the implementation queue produced by the 2026-07 performance and
efficiency review. [PLAN.md](PLAN.md) remains the source of truth for product
scope and settled architecture; [PERF.md](PERF.md) remains the source for
measured live-video facts and the deferred NV12 decision.

The current architecture is fundamentally sound: hardware scaling, paced
bounded queues, serialized warm-up, strict media priorities, focus pause, and
occlusion throttling already address the expensive decoder problems. The next
gains are primarily in scheduling, memory residency, and work proportionality.

## Rules for this queue

- Measure before and after on the same machine, media set, window size, zoom,
  and animation level.
- Preserve the live-player invariants in `AGENTS.md`: promotion before pruning,
  bounded-queue warmth, serialized warm spawns, and drop waking the condvar.
- Do not begin NV12/GPU color conversion unless a `PERF.md` entry criterion is
  met.
- Do not add SQLite, a text stack, or a new dependency for any task here.
- Keep stdin streaming and never move file stats or media work onto the render
  thread.
- Prefer changes that M9's view-indirection layer can reuse rather than building
  a second indexing model.

## Recommended execution order

Task numbers are stable IDs, not sequence. This list is the sequence. It was
re-prioritised 2026-07-16 around two principles: the two indefinite-runaway-CPU
defects (P0.2, P0.4) lead and are not gated behind instrumentation, and the one
genuine data-corruption bug (P0.6) is pulled out of the larger dedup refactor it
was buried in.

**Do first, out of band (unblocks the lint gate for everything below):**

0. T2 Restore a clean clippy baseline — three mechanical `collapsible_if` fixes.
   Every task's verification uses `clippy -D warnings`, which currently *stops*
   on these. Trivial, so clear it before the gate matters.

**Short-term focus — the runaway-CPU and corruption fixes:**

1. P0.2 Stop background work from forcing continuous redraw. Carve the slim
   "why did the app report `animating`" redraw-reason counter out of P0.1 and
   land it here — it is the acceptance instrument for this task and nothing else
   needs to block on it.
2. P0.4 Handle failed and offscreen live lanes. The failed-lane reap is the
   trivial, high-confidence half; the offscreen-parking half can land separately.
3. P0.6 Eliminate the deterministic temp-file race — unique temp name + atomic
   rename. Small, independent, stops cache corruption. (Extracted from P1.2.)

**Next — first-frame stall and residency:**

4. P0.3 Bound ingest and media result draining.
5. P0.1 (residency + frame-time half) then P0.5 Right-size the atlas — paired:
   the byte estimate is both the warning P0.5 wants and the evidence for the
   shrink.

**P1 — proportionality and duplication:**

6. P1.3 Reduce warm-player RGBA residency (alloc-after-wait) with T1 test
   hardening — paired; T1 makes the warm-player suite trustworthy for this change.
7. P1.2 Coalesce media jobs by artifact — now pure dedup/promotion, since the
   corruption half moved to P0.6.
8. P1.4 Schedule redraws around live-frame deadlines — the direct extension of
   P0.2's scheduling shape.
9. P1.1 Make spring work proportional — prefer to land *with* M9's view
   indirection rather than as a standalone pass M9 would then rework.

10. P2 tasks only after the preceding measurements identify a remaining cost.

---

## P0 — immediate efficiency and runaway-work fixes

### P0.1 — Add baseline and residency telemetry

**Status: DONE (2026-07-16) in reduced scope.** Landed:

- Redraw-reason counter (landed with P0.2): once-a-second debug line with
  frames/s, idle count, and cause tallies.
- GPU residency report at startup (`sb-window::render`, info level): atlas
  bytes + slot count, hires incl. mips, backdrop chain at initial window
  size, and the total; plus a >512 MiB atlas warning reporting bytes and
  slots without prescribing a size. Current default reports:
  `atlas 983 MiB (777 slots of 768x432) + hires 18 MiB + backdrop 20 MiB
  = 1022 MiB`.
- Atlas sizing evidence in the debug redraw line: high-water `slots used`
  vs `zone demand` (visible+prefetch statics+anims capped by library, +2
  live lanes) — the direct input for P0.5. At a 1280×800 default-zoom
  viewport, zone demand is ~92 of the 777 reserved slots.

Deliberately dropped: ingest/media drain-burst counters (P0.3's budgets
made the burst case structurally impossible), per-player buffered-frame
gauges, and CPU-vs-GPU frame timing — add only if a later task needs them.

**Problem**

Several proposed changes need measurements that the app does not currently
surface: total GPU texture reservation, queued live-frame bytes, per-frame app
time, ingest/media drain bursts, and redraw cadence by reason.

**Outcome**

A reproducible baseline makes later tasks independently verifiable and prevents
large architectural work from being justified by intuition alone.

**Suggested implementation**

- Log estimated startup GPU allocation:
  - thumbnail atlas;
  - hires texture including mips;
  - quickview backdrop at the initial window size;
  - total known texture bytes.
- Add debug-only or `RUST_LOG=debug` counters for:
  - frames rendered per second;
  - why the app reported `Frame.animating`;
  - maximum ingest items drained in one frame;
  - maximum media results/uploads drained in one frame;
  - active selected/hover/warm players and their buffered frames;
  - maximum visible tile/instance count.
- Record app-frame CPU time separately from GPU presentation time if practical.
- Add a warning threshold for unusually large atlas reservations. The warning
  should report bytes and slot count, not prescribe one universal size.

**Alternative approaches**

- Minimal: debug logs and counters reset once per second.
- Richer: a small internal metrics snapshot dumped on exit or on a debug key.
- Avoid an always-visible overlay; real text is still deferred until M7.

**Acceptance criteria**

- A normal launch reports estimated known GPU texture residency and atlas slots.
- A debug run can distinguish visual animation, live playback, ingest service,
  media completion, and explicit wake timers as redraw causes.
- Measurements add negligible work when debug logging is disabled.

**Validation**

- Capture a baseline for:
  - demo mode;
  - a warm-cache library;
  - a cold-cache library;
  - quickview at the configured maximum resolution;
  - a large streamed input.

---

### P0.2 — Stop background work from forcing continuous redraw

**Status: DONE (2026-07-16).** Approach A (event-driven wakeups) landed:

- `sb-window` owns a new `Waker` (coalescing wake handle; `App::waker()`
  returns the app's instance, `run()` arms it with an event-loop proxy;
  each wake becomes ONE redraw via `user_event`, latch cleared per redraw).
  No winit types cross the boundary — workers see a plain closure.
- Media workers notify after each result; ingest readers notify per
  delivered item and once on producer exit (prompt `Disconnected`).
- `frame.animating` dropped `jobs_total > 0` and `rx.is_some()`; ingest
  arrival wakes only when a newcomer lands in the visible+prefetch rows;
  `GenDone` no longer wakes (the delivering wake repaints the bar), with a
  one-shot wake on batch completion for the bar's 0.7s fade-out.
- The redraw-reason counter (carved from P0.1) logs once a second at debug
  level: frames/s, idle count, and cause tallies (motion / sheets /
  transition / live / timer).

Validated live: demo idles at the 10Hz tick after ~1s of startup animation;
an open silent stdin pipe idles at 10Hz (previously display-rate); slowly
streamed clips ingest + thumb promptly, with the remaining 60fps correctly
attributed to live playback (P1.4's territory). Tests:
`background_jobs_do_not_force_continuous_animation`,
`open_ingest_pipe_does_not_force_continuous_animation`, plus `Waker`
coalescing/pre-arm/unarmed unit tests in `sb-window`.

**Problem**

`jobs_total > 0` and `rx.is_some()` currently keep `Frame.animating` true. A
long generation sweep or slow stdin producer can therefore keep presenting at
display rate while the visible grid is static.

**Desired behavior**

Separate three concepts:

- continuous visual animation;
- occasional background-service polling;
- a one-shot redraw because new state arrived.

**Approach A — event-driven wakeups (preferred)**

- Add a normalized wake mechanism owned by `sb-window`.
- Media and ingest completion signal the event loop through a proxy or wake
  channel.
- Request a redraw when work arrives; do not make the app continuously
  animating merely because a producer is still open.
- Retain a slow safety tick for tuning-file polling and defensive recovery.

**Approach B — service tick first**

- Remove `jobs_total > 0` and `rx.is_some()` from continuous animation.
- Drain them on the existing 100 ms idle tick.
- Use `wake_until` only when arrivals trigger a visible fade, progress update,
  or layout change.
- This is simpler but can add up to 100 ms latency before streamed tiles appear.

**Approach C — adaptive service cadence**

- Use a faster service tick while ingest/media backlogs exist, such as 16–33 ms,
  and return to 100 ms when waiting on an open but idle producer.
- This avoids a new cross-boundary event type but makes loop state more complex.

**Implementation constraints**

- `sb-window` continues to own winit types and the event loop.
- `sb-app` must not receive a winit proxy directly.
- Occluded windows must remain on the non-spinning path.
- Progress-bar changes should redraw promptly without running continuously
  between changes.

**Acceptance criteria**

- A static window with a multi-minute background generation sweep does not
  present continuously.
- A pipe that remains open but sends no new path does not keep the GPU busy.
- New streamed clips and completed visible thumbnails appear promptly.
- Config hot reload still works while otherwise idle.
- Idle and occlusion safeguards remain effective.

**Tests**

- Add an app-level test that pending background jobs alone do not require
  continuous animation.
- Add a window-loop or state-machine test for service wake versus animation
  wake if the cadence logic is extracted into testable code.

---

### P0.3 — Bound ingest queues and per-frame drains

**Status: DONE (2026-07-16).**

- All three ingest sources use `mpsc::sync_channel(1024)` — a full channel
  parks the reader thread (stdin stays sacred: backpressure lands on the
  producer, never the UI).
- `drain_ingest` takes at most `INGEST_DRAIN_BUDGET` (256) items per frame;
  `drain_media` accepts at most `MEDIA_UPLOAD_BUDGET` (64) texture uploads
  per frame (pixel-free results don't count). Hitting either budget fires
  the P0.2 waker, so the backlog continues next frame with no idle gap.
- Order, `pending_reselect`, and disconnect handling unchanged
  (`open_parent_swaps_to_siblings_and_reselects` still green).

Test: `ingest_drains_with_a_per_frame_budget` (1000-item prefilled backlog:
first frame takes exactly the budget, the rest lands over later frames, in
order). Live: 5000 paths flooded over stdin ingest fully within ~1s of
budgeted frames, then the loop idles at the 10Hz tick.

**Problem**

Ingest uses unbounded channels and `drain_ingest` consumes the complete backlog
in one frame. Media results are also drained without an item or time budget. A
fast scan or warm-cache completion burst can cause excess memory use and a long
UI frame.

**Suggested implementation**

- Change ingest sources to a bounded `sync_channel`.
- Start with a capacity in the 512–2048 item range and tune from measurement.
- Limit `drain_ingest` using either:
  - an item budget, initially around 256; or
  - a time budget, initially around 1–2 ms.
- Apply a separate result/upload budget to `drain_media`.
- Keep the loop awake or request another service redraw while a known backlog
  remains.
- Preserve streamed order exactly.

**Approach trade-offs**

- Item budgets are deterministic and easy to test.
- Time budgets adapt better across machines but make exact tests harder.
- A hybrid cap prevents one pathological item from monopolizing the frame while
  also avoiding tiny batches on fast machines.

**Acceptance criteria**

- A producer cannot queue an unbounded number of `Ingested` values in memory.
- A very large directory does not create a single multi-frame-time ingest
  drain.
- Cached thumbnail bursts do not upload hundreds of textures in one frame.
- The grid continues to populate progressively and in source order.
- Closing the producer and the `D` sibling-swap reselect behavior still work.

**Tests**

- Stream thousands of synthetic inputs faster than the app consumes them and
  verify order plus bounded progress.
- Verify a drain budget leaves work for a later frame.
- Keep `open_parent_swaps_to_siblings_and_reselects`.

---

### P0.4 — Reap failed live lanes and park offscreen selection playback

**Status: DONE (2026-07-16).**

- `update_live` reaps selected/hover/warm players reporting `failed()`
  (thumbnail takes over; hover slot freed) and records the path in
  `live_retry`; `start_sel_live`/`start_live` skip cooling paths
  (`LIVE_RETRY_COOLDOWN_S` = 30s time-cooldown — the "transient recovers"
  option from the open choice).
- Offscreen parking: when the selection's row leaves the visible range and
  neither quickview nor fullview is open, `sel_parked` gates the sel-lane
  `take_frame` — bounded backpressure stalls the decoder (no decode, copy,
  upload, or mips) while the stream object and timeline survive — and the
  `animating` live term ignores a parked lane, so the loop can idle.
  Panning back resumes the same decoder; no respawn. Only trackpad panning
  hits this (keyboard moves always scroll to the selection).

Tests: `failed_live_lane_is_reaped_with_cooldown` (garbage file; asserts
reap + cooldown blocks respawn) and
`offscreen_selection_parks_and_resumes_without_respawn` (real clip; asserts
no uploads while parked, loop idles, same `spawned` instant on resume).

**Problem**

`SeekablePlayer::failed()` is not consumed by `sb-app`. A dead selected or hover
lane can remain installed indefinitely and keep the app animating. Separately,
the selected decoder continues being drained and uploaded when trackpad panning
leaves the selected tile offscreen, even if quickview/fullview is closed.

**Suggested implementation**

- Reap selected, hover, and warm players that report `failed()`.
- Fall back to the static thumbnail.
- Add a per-path retry cooldown so a permanently broken clip does not respawn a
  decoder every frame after the settle delay.
- Determine whether the selected tile is in the visible row range.
- When the selection is offscreen and neither quickview nor fullview is active:
  - keep the player object;
  - stop calling `take_frame`;
  - let bounded backpressure park the decoder;
  - resume draining when the selection returns onscreen.

**Open choice: retry policy**

- Time cooldown: simple and allows transient recovery.
- Session failure set: cheapest, but a transient failure stays failed until
  restart.
- Bounded attempts plus cooldown: most robust, slightly more state.

**Acceptance criteria**

- A failed live lane cannot keep `Frame.animating` true forever.
- Broken clips do not enter a rapid respawn loop.
- Panning the selected tile offscreen parks its decoder and stops hires uploads.
- Quickview/fullview always continue to drain the foreground stream.
- Returning the selected tile onscreen resumes from the parked stream without a
  cold spawn when possible.

**Tests**

- Inject or fake a failed player and verify the lane is reaped.
- Verify cooldown behavior.
- Verify offscreen selection stops taking frames and onscreen return resumes.

---

### P0.5 — Right-size the checked-in atlas configuration

**Status: DONE (2026-07-16) — measured, and the default stays.** The live
measurement (M3, ~400-clip library, minimum zoom, fast scrolling) showed
zone demand reaching ~770 of 777 slots with sheets cycling and ~390 with
sheets off — the review's "disproportionate" framing only holds at normal
zoom (~92 slots). The user chose to keep 777 slots (983 MiB): it is the
measured sheets-on worst case for zoomed-out browsing, not idle headroom.
The config comment now documents the measurement and how to re-measure
(startup residency log + the debug `slots used / zone demand` gauge, which
was also fixed to count anim slots only when sheets actually cycle).
Byte-cost reduction, if ever wanted, is P2.3's smaller-grid-artifact
territory, not slot-count shrinking.

**Problem**

The checked-in config reserves a 16,128×15,984 RGBA atlas, approximately
983 MiB before hires textures, backdrop textures, live queues, libav state, and
the rest of the process. Because the cwd config wins during development and is
also embedded by `--init`, this is an important practical default.

**Short-term approach**

- Measure maximum simultaneous static, animation, and live slot demand at:
  - the smallest supported zoom;
  - the largest expected window/display;
  - visible rows plus `PREFETCH_ROWS`;
  - quickview filmstrip activity.
- Choose a default with explicit headroom over that measured demand.
- Keep atlas dimensions configurable for unusually large displays/libraries.
- Update config comments with the measured scenario and estimated bytes.

**Alternative**

- Keep the large expert/dev config but make `--init` embed a conservative
  separate template. This avoids changing the user's tuned repo experience but
  creates two defaults that can drift.

**Acceptance criteria**

- Default residency is justified by a documented worst-case slot calculation.
- The normal target viewport does not churn visible statics.
- Startup logs show the new byte estimate and slot count.
- A lower-memory default does not regress scroll-back behavior in a live run.

**Live validation**

- Test the actual clip library at minimum zoom and on the largest display mode
  normally used.
- Watch for black edges, repeated disk-cache decodes, or slot eviction churn.

---

### P0.6 — Eliminate the deterministic temp-file race

**Status: DONE (2026-07-16).** `staging_path()` gives every generation a
unique temp name (pid + process-wide sequence — also unique across two
switchblade processes) beside the destination; `extract_frame`, the anim
tile pass, AND `make_anim`'s per-cell `animf_*` files all use it, so
concurrent duplicates can no longer interleave — atomic rename publishes
only complete files, last writer wins. `meta.json` writes (thumb gen +
reprobe heal) also go through staging + rename (`write_atomic`) — same
corruption class. `--cleanup-cache` already sweeps stale `*.tmp` staging.
Tests: `staging_paths_are_unique_per_generation`,
`concurrent_generation_publishes_a_clean_artifact` (two threads, three
rounds, artifact + meta.json both verified).

**Problem**

`ensure_thumb_file` and the anim path generate to a temporary named
deterministically from the destination — `dst.with_extension("jpg.tmp")` in
`extract_frame` ([crates/sb-media/src/lib.rs:857](crates/sb-media/src/lib.rs)),
and the `animf_{k}.jpg` / `.jpg.tmp` staging in `make_anim`. Both the
background-gen job and a visible-thumb job can pass the same `!jpg.exists()`
check for one artifact and run concurrently on separate workers. Two ffmpeg
processes then write the *same* temp path and both rename it — the surviving
file can be a truncated/interleaved JPEG that is decoded and cached as if valid.

This is the corruption half of P1.2, separated because it is a small, standalone
correctness fix that should not wait for the larger dedup refactor. P1.2 still
eliminates the *duplicated work*; P0.6 only stops the two duplicates from
corrupting each other.

**Suggested implementation**

- Give each generation a unique temp name (per-worker id or a monotonic counter;
  no `Math.random`/wall-clock needed — a worker-local counter suffices).
- Write to the unique temp, then atomically `rename` onto the final artifact.
- The last writer wins with a *complete* file; a concurrent duplicate wastes CPU
  (fixed by P1.2) but can no longer corrupt the cache entry.
- Apply the same treatment to the `make_anim` per-frame staging files.

**Acceptance criteria**

- Two concurrent generations of the same missing artifact can never leave a
  partial or interleaved file in the cache.
- A completed artifact is always a fully written JPEG (atomic publish).
- No behavior change when only one worker touches an artifact.

**Tests**

- Drive two concurrent generations of the same missing artifact (or fault-inject
  a slow writer) and assert the published file decodes cleanly.

---

## P1 — make work proportional and remove duplication

### P1.1 — Replace the all-library spring scan with active spring state

**Status:** Ready after P0 measurements. **Prefer to land as part of M9's
view-indirection work, not before it.** The scan is O(library) but cheap per
element (two lerps + a compare); at a few thousand clips it is microseconds per
frame, so the standalone win is small. Its real value is composing with M9's
path-stable, ordered/filtered index — building an independent active-index model
now risks the exact "second incompatible indexing layer" the M9 note below warns
against. Do it early only if P0.1 measurement shows this loop is a real cost on
the target library size.

**Problem**

Every active frame updates `scale` and `emph` for every clip, although only the
current/previous selection and hover can normally be moving. Live playback makes
this O(library size) loop run continuously.

**Approach A — active index set**

- Track indices whose scale/emphasis may be unsettled.
- Insert current and previous selection/hover indices when targets change.
- Update only those entries.
- Snap completed entries exactly to their target and remove them.

**Approach B — derive animation from transition records**

- Remove persistent spring fields from every `Clip`.
- Store small transition records with start/current/target state for active
  indices only.
- Resolve inactive clips directly to their rest state.

**M9 interaction**

M9 introduces view indirection and path-stable selection. Prefer path-stable
active state or ensure view rebuilds remap active indices safely. Do not bake a
second incompatible indexing layer.

**Acceptance criteria**

- Spring-update work is proportional to changing clips, not library size.
- Selection and hover feel remains visually identical at 60/120 Hz.
- Sorting/filtering or sibling swaps cannot animate the wrong clip after index
  remapping.
- Idle detection remains correct.

**Tests**

- Previous selection/hover settles after the target moves.
- An active spring survives or safely resets across a view rebuild.
- No active entries remain after all motion settles.

---

### P1.2 — Coalesce media work by artifact and allow priority promotion

**Status: DONE (2026-07-16).** Implemented inside `Queues` (all under the
existing mutex, tier semantics untouched):

- Artifacts key as `(path, Art::{Thumb,Anim})` — visible-thumb and gen
  target the same `Thumb` artifact, both sheet tiers the same `Anim`.
- Membership mirrors (`in_thumbs`/`in_gen`/`in_anims*`, O(1) checks) + an
  `inflight` set; `push_thumb` absorbs a queued gen entry (promotion to
  tier 1) or joins in-flight work; `push_anim_now` moves a queued bulk
  sheet above the gen tier; duplicate gens coalesce.
- Result multiplicity is preserved via owed counters (`gen_owed`/
  `thumb_owed`/`anim_owed`): the app counts one result per request, so a
  merged request still gets its `GenDone`/`Ready` — the worker emits the
  owed batch on completion, including the foreground decode for a visible
  request that joined an in-flight generation. Progress accounting stays
  balanced (the D-swap duplicate-gen case included).

Tests: `visible_request_absorbs_queued_gen`,
`visible_request_joins_inflight_gen`, `duplicate_gen_requests_coalesce`,
`quickview_sheet_promotes_queued_bulk_anim`, plus the kept
`quickview_sheet_outranks_gen_sweep_but_not_visible_thumbs`. Cold-cache
live run: 6/6 thumbs served, no errors, jobs completed.

**Problem**

Ingest queues a background generation job for every file. A visible clip can
also queue a foreground thumbnail request for the same artifact. Multiple
workers can observe the same missing file and duplicate probe/extract work.
(The temporary-path corruption this could previously cause is handled in P0.6.)

**Preferred model**

Track each artifact as:

- absent;
- queued at a priority;
- in flight;
- complete;
- failed for this session.

A visible request should promote queued background work. If generation is
already in flight, it should attach a request to decode/send RGBA after the
artifact is ready rather than launch a duplicate generator.

**Alternative approaches**

- Queue-level dedup maps keyed by `(path fingerprint, recipe, artifact kind)`.
- Per-entry lock/sentinel files around generation. This also protects across
  processes but requires stale-lock recovery.
- Unique temporary names plus atomic rename. This fixes corruption/races but
  still duplicates expensive work, so it is only a partial solution.

**Constraints**

- Preserve the five-tier priority semantics.
- Quickview's `anims_now` must continue to outrank the generation sweep.
- A promotion must not enqueue a duplicate result or double-count progress.
- Cache remains filesystem-first; no SQLite.

**Acceptance criteria**

- One artifact is generated at most once per process at a time.
- A visible request promotes or joins existing work.
- Progress accounting remains correct when requests merge or promote.
- Temporary-file races cannot corrupt a cache entry.

**Tests**

- Queue `Gen`, then `Thumb`, for the same missing artifact and assert one
  generation plus one foreground result.
- Queue duplicate anim requests across priority tiers and assert promotion.
- Keep `quickview_sheet_outranks_gen_sweep_but_not_visible_thumbs`.

---

### P1.3 — Reduce warm-player RGBA buffer residency

**Status: DONE (2026-07-16) — Approach A.** `push_rgba` now parks on the
full-queue condvar BEFORE allocating/copying the RGBA frame: a parked lane
retains only its 3 queued frames (~42 MiB at 1440p), never a fourth
pre-copied one, and a seek that lands while parked skips the copy
entirely. The due-stamp moved after the park too, so a long-parked frame
re-anchors instead of carrying a stale deadline. The lock drops during the
copy (room only grows — one reader per player); close/seek are re-checked
before the push. Approach B (buffer recycling) stays deferred until
profiling shows the alloc itself matters. The pacing/stall/drop trio and
the full app-level live tests all pass; live smoke shows unchanged
first-frame latency and pacing.

**Problem**

`SeekablePlayer::push_rgba` allocates and copies a full frame before checking
whether the bounded queue has space. A parked 2560×1440 lane can retain three
queued RGBA frames plus the next allocated frame waiting on backpressure:
roughly 56 MiB per lane before libav/filter buffers.

**Approach A — wait before allocating/copying (recommended first)**

- Check queue capacity before allocating the output `Vec`.
- Wake/recheck on seek, drop, or consumer pop.
- After capacity is reserved, copy the filtered frame and enqueue it.
- Recheck seek/drop after waking so stale frames are not copied.

**Approach B — recycle buffers**

- Keep a small free-buffer pool per player.
- Return superseded/dropped due frames to the pool.
- Reuse capacity for the next copy.

**Approach C — shared staging or zero-copy redesign**

- Carry reference-counted frame storage or GPU-upload staging buffers across the
  app boundary.
- This is more invasive and should wait for measurements or the NV12
  renderer work.

**Acceptance criteria**

- A fully parked player retains no extra pre-queue RGBA allocation.
- Warm promotion still serves a queued frame immediately.
- Seek and drop still wake a reader parked for space.
- No frame copy occurs for a seek-stale frame after the reader wakes.

**Tests**

- Keep the seekable pacing/stall/drop trio.
- Replace fixed sleeps in warm-player tests with a bounded wait for
  `buffered() > 0`, then assert the first `take_frame` is immediate. This removes
  the contention-sensitive parallel-suite flake observed during review.

---

### P1.4 — Redraw around the next live-frame deadline

**Status: DONE (2026-07-17) — Approach A with a push-notify assist.**

- `SeekablePlayer::next_due()` exposes the head frame's due time;
  `set_notify` installs a wake fired when a frame lands in a DRY queue
  (the deadline sleeper's blind spot).
- `Frame.redraw_at: Option<Instant>` carries the earliest deadline across
  the app boundary (plain `std::time::Instant`, no winit types); the
  window's idle path waits until `min(idle tick, redraw_at)` — ignored
  while animating (UI motion owns the cadence) or occluded.
- `frame.animating` dropped the `live` term entirely: live lanes now pace
  by deadline. Parked/dry lanes report nothing. UI tweens, input wakes,
  and the timer path are unchanged.

Measured live: a static grid playing a 30fps clip presents ~27–29
frames/s (was display-rate 60), with the loop otherwise idle. Warm-lane
pacing/stall/promotion tests all green.

**Problem**

Once any live lane exists, the window currently renders continuously at display
cadence even when the next decoded frame is not due. A 24/30 fps clip on a
60/120 Hz display can therefore produce redundant app frames and presents.

**Approach A — expose next due time**

- Add `next_due()` or equivalent to the player contract.
- The app reports the earliest deadline across selected and hover lanes.
- The window waits until the earlier of:
  - a UI animation deadline;
  - a live-frame due time;
  - a service/config deadline;
  - an external wake event.

**Approach B — player wake event**

- Have the player signal when a frame becomes due/available.
- This is more event-driven but introduces another cross-thread wake source.

**Caution**

Do not delay UI motion or pointer response to video cadence. UI animation and
input must continue to request display-rate redraws when active.

**Acceptance criteria**

- A static grid playing 24/30 fps video does not render at 60/120 fps between
  video frames.
- UI tweens remain display-rate smooth.
- Due frames are not presented late beyond the existing pacing tolerance.
- Warm parked lanes do not schedule redraws.

---

## P2 — measured follow-up optimizations

### P2.1 — Cache the quickview frosted backdrop

**Status:** Measure after P0/P1

Quickview currently renders the grid offscreen and walks the blur mip chain on
every frame. Cache that result until backdrop inputs change.

**Invalidate on**

- camera position or zoom;
- viewport/scale change;
- grid tile/texture arrival or fade;
- selection/hover appearance that belongs below the blur split;
- tuning changes affecting the grid or blur.

**Possible approaches**

- App supplies a monotonically increasing backdrop revision.
- Renderer hashes or compares a compact backdrop state.
- Renderer freezes the backdrop while quickview is open. This is cheapest but
  conflicts with the current visibly live grid and background arrivals; use only
  if that visual change is explicitly accepted.

**Acceptance criteria**

- During steady quickview playback, offscreen grid and blur passes run only
  when invalidated.
- Opening/closing quickview and grid updates never show stale geometry.

---

### P2.2 — Reuse frame-construction scratch buffers

**Status:** Measure after P1.1

The app allocates tile vectors and clones the completed tile list each frame;
the renderer allocates a second `Vec<Instance>`.

**Possible approaches**

- Store reusable tile/group scratch vectors in `Switchblade` and clear them.
- Reserve capacity from the prior visible-instance count.
- Store a reusable instance scratch vector in `Gpu`.
- Replace unconditional `last_tiles = tiles.clone()` with snapshots only when a
  column-count reflow can begin, or maintain a dedicated previous-layout cache.

**Acceptance criteria**

- Steady-state frame construction performs no growing allocations after warmup.
- Reflow crossfade behavior remains identical.

---

### P2.3 — Separate grid thumbnail resolution from emphasized poster quality

**Status:** Architectural option; measure atlas pressure first

The atlas stores full configured thumbnail dimensions even when dense-grid tiles
are displayed much smaller. Consider a smaller grid artifact plus a higher
quality on-demand poster/live source for the selected or emphasized clip.

**Approach options**

- Add a second cached poster recipe requested only for selection/quickview.
- Keep the static thumb as the poster and generate a smaller grid-specific
  artifact.
- Use the existing hires live texture for selected quality and accept static
  softness before live arrives.

**Trade-offs**

- More cache artifact types and queue states.
- Lower atlas memory and upload bandwidth.
- Potential static-to-live quality or color transitions must be checked
  visually.

**Acceptance criteria**

- Dense-grid visual quality remains acceptable.
- Emphasized/selected tiles do not look materially worse before live arrives.
- Total atlas residency falls substantially for the same slot count.

---

### P2.4 — Replace the monolithic atlas with lazily allocated pages

**Status:** Long-term option; do not start unless right-sizing is insufficient

Use several smaller textures/pages allocated as slot demand grows, with page
selection carried per instance.

**Possible implementations**

- Fixed texture array if adapter limits and page dimensions fit.
- Multiple atlas bind groups with draw batches per page.
- Bindless/resource arrays only if wgpu portability and complexity are
  acceptable.

**Trade-offs**

- Avoids reserving maximum memory at startup.
- Adds shader/bind/draw complexity and can weaken the current one-draw-call
  simplicity.
- Must preserve eviction classes and live-slot guarantees.

**Entry criterion**

Proceed only if P0.5 cannot achieve acceptable residency without visible reload
churn on target usage.

---

### P2.5 — NV12 upload and GPU color conversion

**Status:** Deferred by `PERF.md`

Do not pull this forward merely because RGBA copies are visible in the code.
The existing entry criteria still apply:

- profiling shows conversion/upload costs at least one core or causes gaps; or
- multiple simultaneous hires streams become required (combine both
  texture-format disruptions in one renderer change).

When an entry criterion is met, follow the color-space/range metadata and PSNR
verification plan in [PERF.md](PERF.md). This is not a substitute for the
scheduling and residency tasks above.

---

## Maintenance tasks discovered during validation

### T1 — Make warm-player tests contention-tolerant

**Priority:** P1

**Status: DONE (2026-07-16, pulled forward).** The flake resurfaced during
P0.6's gate (the new concurrent-generation test adds parallel ffmpeg load),
so this landed early instead of waiting for P1.3. All four fixed-sleep
sites — `unwatched_*_stalls_then_serves_instantly` and
`dropped_*_releases_its_reader` for both `LivePlayer` and `SeekablePlayer`
— now use a bounded `wait_buffered(player, LIVE_QUEUE_DEPTH, 15s)` before
asserting bounded depth, immediate first take, and drop-release. Three
consecutive parallel workspace runs green.

### T2 — Restore a clean clippy baseline

**Priority:** Do first (step 0 in the execution order).

**Status: DONE (2026-07-16).** The visible three `collapsible_if` warnings in
`sb-media` were only the first failing crate — clearing them surfaced 14 more
`collapsible_if` and 2 `items_after_test_module` in `sb-app` (clippy stops at
the first crate that fails, so the review undercounted). All 19 fixed
mechanically (let-chains; moved `is_cloud_placeholder` and the `App` impl tail
above their test modules). `cargo fmt --all` also re-established a fmt-clean
tree — the checked-in code had drifted from the current toolchain's rustfmt in
~30 places, which would otherwise churn into every future patch. Gate verified:
fmt --check, clippy -D warnings, workspace tests, serial sb-media tests, and a
release build all pass.

## Standard verification for completed tasks

At minimum:

```sh
cargo test --workspace
cargo test -p sb-media --lib -- --test-threads=1
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release
```

For live-player changes, also run the serial performance/regression matrix from
[PERF.md](PERF.md). For UI/render-loop changes, rebuild/install and validate on
the real clip library, including:

- cold and warm cache;
- minimum zoom;
- quickview and filmstrip hover;
- selection panned offscreen and returned;
- focus loss and occlusion;
- a slow open stdin producer;
- a large fast directory scan.
