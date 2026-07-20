# Benchmark instrumentation — task plan

Goal: repeatable, scripted benchmark runs of the real app that record everything and
compute per-run summaries; **interpretation is retrospective and agentic**, not baked
into the apparatus. A scenario states its intent in prose; the runner only measures.

## Settled design decisions

- **Two tiers, one scenario format.** Tier A drives `sb-app` headlessly through the
  `App` trait (real decoders, real ffmpeg workers, real cache — no GPU/display).
  Tier B (`--bench` flag on the real binary) runs the same script inside the real
  winit/wgpu loop for GPU-side truth.
- **Tier claim boundaries are explicit** (Phase 0.5): some questions are inherently
  Tier B — upload stalls, present-to-present gaps, blur cost, visible jank. Tier A
  may support decode/pacing/latency/cache claims only, and only comparatively.
  Neither tier replaces the live feel evaluation PLAN §15 requires for the
  attention-lane verdict.
- **Tier A shares the real redraw scheduler.** It must NOT step `frame()` at a fixed
  cadence — the real loop's behavior hangs off `Frame.animating`, `redraw_at`,
  worker wakes, the 10Hz idle tick, `MIN_FRAME`, and the dt clamp
  (`sb-window/src/lib.rs` `RedrawRequested` + `about_to_wait`). Fixed polling would
  manufacture idle redraws AND inflate `drain_media` throughput (upload budget is
  per-frame), making cache-fill timings fiction. The scheduling policy gets
  extracted into a small shared component both sb-window and the harness use, so
  they cannot drift.
- **One child process per repetition.** `cache_root()` and the cache-key fingerprint
  are process-global `OnceLock`s (first write wins) — in-process repeats would
  silently reuse run 1's cache root. The orchestrator prepares each temp
  environment, then launches a fresh runner process per run; panic/exit codes feed
  the validity gate.
- **No expectation engine.** No thresholds, no pass/fail assertions, no baseline
  regression flags in code. Expected behavior lives as **prose** in the scenario
  file, handed to whoever (usually an agent) interprets the recorded data later.
- **Derive mechanically, judge agentically.** The runner emits the full event stream
  (JSONL) *and* a computed summary (percentiles, counts, curves). Computing a p95 is
  code's job; deciding whether it means the change helped is the reader's.
- **Thin validity gate, not assertions.** A run is stamped `valid`/`invalid` on
  mechanical grounds only (clean exit, script completed, required readiness
  conditions met, ticks recorded). Never `good`/`bad`. Mechanical requirements
  ("the stream spawned") live in the scenario as explicit validity conditions,
  separate from the prose performance intent.
- **Hermetic environment.** Temp `HOME` isolates the cache; `no_config: true` +
  programmatic `Tuning` overrides; synthesized fixture clips on the internal disk
  (external-drive I/O gets its own dedicated scenario, never mixed into decode
  measurements).
- **"Cold" is three axes, recorded per run:** app cache (empty / seeded), source
  page cache (uncontrolled / warmed — flushing the OS cache is intrusive, so we
  label rather than control it), decoder (cold / resident / warm-promoted).
  Warm-up-run discard is scenario-specific: discarding the first run of a
  cold-start scenario discards the condition being measured.
- Scenario files are TOML under `benchmarks/scenarios/`; runs land under
  `benchmarks/reports/`.
- **Retention:** committed reports are self-contained — each report gets a
  compressed bundle (summaries + raw JSONL + environment fingerprint) alongside it,
  or is explicitly local-only. No links from committed markdown into gitignored
  paths.

## Phase 0 — definitions & contracts (before any implementation)

Specified in [design/phase-0-contracts.md](design/phase-0-contracts.md). 0.2 is
implemented; the rest are design contracts consumed by Phase 1–3.

- [x] **0.1 Measurement/event dictionary**: latency taxonomy (decode_spawn_to_ready
      / action_to_served / action_to_presented[B] / promotion_to_served, kept
      separate), identity contract (clip_id / lane_class / lane_gen), always-on
      counters vs. bench-only events, media-emitted re-anchor events. → contracts §0.1
- [x] **0.2 Scheduler policy extraction**: DONE — factored into
      [`sb-window::schedule`](../crates/sb-window/src/schedule.rs)
      (`next_frame` + `IDLE_TICK`/`MIN_FRAME`), `about_to_wait` rewired, 5 unit
      tests. Tier A drives `frame()` through this, never a fixed cadence. → §0.2
- [x] **0.3 Process-per-run orchestration contract**: one child process per rep
      (cache-root `OnceLock`), orchestrator↔runner protocol, exit-code validity. → §0.3
- [x] **0.4 Readiness semantics**: `wait_until` primitive (named condition +
      timeout + recorded outcome), the condition table, fixtures by role not index,
      intent-vs-validity separation. → §0.4
- [x] **0.5 Tier A vs Tier B claim boundary doc**: per-metric tier ownership. → §0.5

## Phase 1 — probes (useful standalone) — IMPLEMENTED

Landed in [`sb-media::probe`](../crates/sb-media/src/probe.rs) (shared types),
`SeekablePlayer::attach_probe` (media-thread events), and the sb-app wiring
(`Switchblade::probe()` accessor). Tests: `probe::tests::*` (3),
`attached_probe_records_decode_ready_from_the_reader_thread` (media thread → sink).

- [x] **1.1 Two-layer probe contract**: `Probe` holds always-on monotonic
      `Counters` (frames, late_frames, reanchors, drain_budget_hits, evictions,
      thumbs_cached) + a bench-only event buffer armed by `record_events()`. Emitted
      from BOTH layers: app-side via `Probe::mark`, media-side via a `LaneProbe`
      attached to each decoder so the reader thread can emit re-anchors it alone
      sees. Emission is a no-op (one relaxed load, no `Event` built) until armed.
- [x] **1.2 Latency events per the 0.1 dictionary**: `DecodeSpawn` /
      `DecodeReady` / `FrameServed` / `Promotion` / `Reanchor`, each carrying clip
      path + `Lane` + a monotonic `lane_gen` minted per spawn (`instrument_lane`).
      Spawn/served/promotion from sb-app; ready/reanchor from the sb-media reader.
- [x] **1.3 Cache progress probe**: `thumbs_cached` counter bumped on the
      false→true `cached` transition; the runner samples it per tick for the fill
      curve (no per-tick dir stats).
- [x] **1.4 Buffered event sink**: events accumulate in a bounded in-memory Vec
      (`EVENT_CAP`, overflow counted not written) and are drained/serialized after
      the window via `Probe::drain(anchor) -> (Vec<RelEvent>, dropped)` — no JSONL
      I/O inside the measured interval.

## Phase 2 — fixtures

- [x] **2.1 Fixture generator** — [`fixtures/generate.sh`](fixtures/generate.sh),
      gitignored corpus + `manifest.json` (ffmpeg version + exact argv + sha256 as
      provenance, never an equality check). 7 fixtures on the fault lines:
      `h264_1080p30` (VT baseline), `hevc_2160p60` (heavy 4K60 + hw-scale),
      `hevc_1080p30_10bit` (scale_vt pix_fmt gate), `h264_1080p30_longgop`
      (single GOP over 16s — exact-seek worst case), `h264_720p30_vfr` (VFR, two
      concatenated rate segments), `h264_1080p30_rot90` (Display Matrix rotation —
      ffmpeg 8 needs a `-display_rotation` copy remux, the legacy `rotate` tag is
      gone), `vp9_720p30` (software decode path). No timecode burn-in (this ffmpeg
      lacks drawtext; pacing is measured from pts, not pixels). All verified via
      ffprobe (codec/dims/pix_fmt/fps/rotation/keyframe-count).
- [ ] **2.2 Cache seeding helper** — DEFERRED to Phase 3: warming the cache means
      running the app's real gen sweep, which needs the headless driver 3.2 builds.
      A standalone seeder would duplicate the cache-layout logic and risk drift, so
      it lands as a runner sub-mode (drive ingest over a fixture set → wait for
      `cache_swept(all)` → snapshot the temp `HOME` cache dir).

## Phase 3 — headless runner (Tier A)

Runner lives in [`sb_app::bench`](../crates/sb-app/src/bench.rs) (crate-internal, so
it can reach the private `layout`/`tile_rect` geometry) with a thin
[`sb-bench` bin](../crates/sb-app/src/bin/sb-bench.rs). Self-test
`bench::tests::runs_a_scenario_and_measures_selected_latency`. Verified live on both
seed scenarios (cold h264 first-frame ~255ms, hover-lane ~62ms, 4K60 warm spawn the
383ms tail — real comparative data).

- [x] **3.1 Scenario TOML format + parser**: `[setup]` (fixtures by name/role,
      animation, viewport, refresh_hz vsync stand-in, max_wall), sequential
      `[[step]]` list (`wait` / `wait_until` / `key` / `hover` / `click` /
      `scroll`) with an explicit `action` discriminator, `[validity].require`,
      and `intent = """…"""` prose. Reuses the existing toml dep.
- [x] **3.2 Runner binary**: hermetic temp `HOME` set in the bin BEFORE any
      sb-media call (cache-root OnceLock); drives `frame()` on a **vsync
      stand-in** while animating (headless has no present to pace it — using the
      raw `MIN_FRAME` scheduler would free-run at 250fps and inflate per-frame
      `drain_media` work) and defers to `sb_window::schedule::next_frame` when
      idle. Semantic targets resolve through live `tile_rect`. Records via the
      buffered probe sink; writes `summary.json` + `events.jsonl`.
- [x] **3.3 Validity gate**: `valid`/`invalid` from step completion + required
      `[validity]` conditions + `wait_until` timeouts + a `max_wall` ceiling; the
      bin's exit code mirrors it. Reasons recorded, never a perf verdict.
- [~] **3.4 Summary computation**: DONE for app-level metrics — latency
      percentiles **per 0.1 class** (spawn_to_ready / spawn_to_served /
      promotion_to_served, matched by lane_gen, never pooled), tick-duration
      percentiles, compacted thumbs-over-time curve, counters snapshot. **CPU
      time + peak RSS move to the orchestrator** (it spawns the child, so
      `RUSAGE_CHILDREN`/`wait4` there captures the whole process tree incl.
      ffmpeg workers — the in-process runner can't see its own children's RSS
      cleanly). Thread-count leak canary still TODO.
- [ ] **3.5 Repeat orchestration**: N runs/scenario (default 5), one child
      process per run (the bin already isolates HOME), serialized, warm-up
      discard opt-in, env fingerprint (git SHA, dirty, machine, power, ffmpeg
      version, cold-axes) + process-tree CPU/RSS. Pairs with Phase 4.
- [x] **3.6 Seed scenarios** ([scenarios/](scenarios/)): `cold_open_quickview`
      (cold → ingest → idle → Space → selected serves → play, watching pacing +
      cache fill) and `hover_last_tile` (cold → ingest → hover last → hover-lane
      first-frame latency). Both use readiness waits, not raw timestamps.

## Phase 4 — reporting

- [ ] **4.1 Report generator**: subcommand that takes run summaries →
      `benchmarks/reports/<date>-<sha>-<label>.md` — per-scenario tables (median ±
      spread), environment fingerprint, the scenario's prose intent quoted at the
      top, plus the compressed raw-trace bundle per the retention policy.
      Markdown first; an HTML/chart variant only if tables prove insufficient.
- [ ] **4.2 Compare mode**: same generator, two labeled run sets side by side with
      computed deltas (numbers only, no verdicts) — the artifact an agent or human
      reads to answer "did the change help?". Supports interleaved A/B runs
      (alternate binaries back-to-back to dodge thermal drift).

## Phase 5 — expansion (as experiments demand)

- [ ] **5.1 More scenarios**: warm-pool advance latency (repeated `l`), quickview
      open cost, shuffle under load, focus-pause resume, background-sweep-while-
      watching (priority-inversion guard), external-drive I/O.
- [ ] **5.2 Tier B `--bench` mode**: real winit/wgpu loop driven by the same
      scenario script; owns the metrics Tier A is barred from (0.5) —
      present-to-present intervals, upload bytes/frame, `MEDIA_UPLOAD_BUDGET_LIVE`
      pressure, blur cost.

## Caveats (stated once, up front)

- Tier A numbers are **comparative, not absolute** — even with scheduler fidelity
  it has no vsync-blocking present. "Frame served on time" is a proxy for "would
  have presented on time". Right signal for A/B; wrong for FPS marketing.
- "Video memory" budgets are simulated via atlas dimensions — that's what actually
  gates GPU residency here — not a real VRAM cap.
- Thermal state can swamp small deltas; repeats + medians + interleaved A/B
  mitigate, never eliminate.
- The OS page cache is labeled, not controlled (see the cold axes).
