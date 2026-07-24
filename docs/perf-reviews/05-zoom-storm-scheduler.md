# 05 — The zoom-out thumb storm: tier-1 flood and the render-thread freeze (2026-07-24)

Third review in the slow-disk/big-library sequence, and the first to test
the user's **own reproduction steps** rather than an approximation of them:

> open library with many entries (> 100), **zoom out**, wait for the entire
> screen to populate with 'loading' entries and the workers for thumbs to
> stack up. Click any movie and open in quicklook or fullview. Movie will
> not play. In some cases the application locks up and needs to be killed.

[03](03-slow-disk-scheduler.md) measured a big library on a USB HDD (UI
stall did not reproduce); [04](04-network-storage-scheduler.md) measured
the same library over SMB and *did* catch a 1.49 s render-thread stall, but
neither scenario ever **zoomed out** — and the zoom-out turns out to be the
load-bearing step, not incidental colour in the bug report.

**Headline: the zoom-out is causal for the UI freeze.** It roughly doubles
the tier-1 thumb flood (79 → 136 jobs, the atlas budget bound), which
saturates the 3-worker pool and the SMB link. Against that contention, two
threads were caught blocking — one of them attributable to the zoom-out,
one measured for the first time but not yet attributable:

- **The UI thread** — one `frame()` blocked **901 ms**, entirely inside a
  single `clip_meta` filesystem call. Across the run, 7 such calls cost
  **2.81 s** (mean ~401 ms each) against **0.07 ms each** in the control.
  This one is solid: it reproduced on a *warm* link, with clean controls run
  either side of it (§2).
- **The video thread** — a **1,036 ms** gap between served frames, matching
  a **1,011 ms** block inside `av_read_frame` on the live decoder's reader
  thread. This is the half nothing could previously see, and the block is
  real — but it appeared only in the coldest run, so **attributing it to the
  zoom-out specifically is not yet supported** (§5).

**Then a second round, after the user pointed out the first one never
zoomed all the way out.** It didn't — it zoomed four steps, and `evictions=0`
proves the atlas never even filled. Zooming to the floor (§7) found the
bigger problem, and it is not a scheduling problem at all: **428 tiles
visible against 144 atlas slots**, of which only 136 are ever requested at
tier 1. The ~292-tile remainder is served solely by the gen sweep, pinned at
**one job wide** with a backlog of 4,231 that grows faster than it drains.
That is the "screen full of 'loading' that never populates".

The scheduler itself is **not** thrashing. Worker utilisation was 0.35–0.39
in every run: the pool is *parked*, not overloaded. What starves is
attention work, and it starves on the **filesystem** and on **atlas
capacity**, never in the queue. An earlier draft of this review recommended
throttling the tier-1 flood; §8.6 records why that was wrong.

## Environment

| | |
|---|---|
| Commit | `68f1bf2` — *perf(bench): split render-stall by op class; SMB network-drive review (04)* |
| Working tree | dirty — this review's instruments (§1) were added on top of `68f1bf2` and are what the runs measured |
| Date | 2026-07-24 |
| Machine | **M4 MacBook Pro, Apple M4 Pro, 14 cores, 48 GB RAM** (host `pro.lan`, macOS 26.5). Review 04 recorded the same host and specs as an "M4 Mac mini" — same box, one of the two labels is wrong; the user identifies it as the MacBook Pro. |
| ffmpeg | 8.1.2 (Homebrew); rsmpeg in-process libav for live lanes |
| Binary | `cargo build --release -p sb-app --bin sb-bench` — release, since debug inflates render-thread cost ~10×, the quantity under test |
| Library | `/Volumes/Tower/Movies/Porn/Downloads` — 8,586 video candidates over **SMB (smbfs)**, `//nom@m4.local/Tower`, gigabit LAN. **Read-only**; every cache write went to a per-run temp `HOME`. |
| Raw data | `benchmarks/reports/05-zoom-storm/<run>/` — `summary.json` + gzipped `events.jsonl` for all six runs, **force-added** past `benchmarks/.gitignore` (the "meaningful baseline" case), since §5's ordering argument only checks out if all four bundles are inspectable. `python3 benchmarks/reports/05-zoom-storm/extract.py <bundle>…` prints the §2 table straight from them. |

Machine was otherwise quiet; runs were strictly sequential (two readers on
the same share would invalidate both).

## 1. Instruments added

03 added job/queue/phase/ingest/canary instruments; 04 added the
`render_stall_*` op-class split. All of those measured either the *worker
pool* or the *UI thread*. Two blind spots remained, and the user's report
named both ("stalls ui threads **and video thread**"):

- **`decode_read_*`** (`crates/sb-media/src/probe.rs`, wired at the
  `av_read_frame` call in `seekable.rs`) — wall time the live lane's reader
  thread spends blocked demuxing the source. Counters:
  `decode_reads` / `_us` / `_max_us` / `_over_100ms` / `_over_1s`.

  **This is the direct measure of "movie will not play", and nothing else
  can see it.** A reader parked on a slow SMB read emits no frames, no
  events, and costs zero tick time — the app's own instruments look
  *perfectly idle* while playback is frozen. Reviews 03/04 could only infer
  this indirectly from served-frame gaps, which conflate I/O blocking with
  pacing decisions. The `>100ms`/`>1s` tails are explicit for the same
  reason the render-stall tails are: the mean stays at 0.02–0.10 ms even in
  a run containing a full-second freeze.

  The `LaneProbe` is cached in a local once attached, so timing a read costs
  no mutex on the per-packet path.

- **`queue_wait_*`** (`Queues::pop_timed`, stamped at push) — how long a
  request **sits** in its queue before a worker takes it, split thumb
  (tier 1) vs gen (tier 5). `JobStart`/`JobEnd` already measured how long a
  job *runs*; "the scheduler thrashes" is a claim about **waiting**, and
  nothing measured it. Enqueue stamps ride parallel deques kept in lockstep
  with the two tiers that carry them (including at the gen→thumb absorption
  site, which removes at an arbitrary index).

- **`thumb_requests`** — tier-1 visible-thumb requests pushed, so the flood
  a zoom-out admits can be sized against library size and against the
  zoomed-in control.

- **`atlas_full_drops`** — finished artifacts discarded because
  `alloc_slot` found no slot, which resets the clip to `Thumb::None` and
  re-arms its request. Added to catch a suspected runaway
  request→generate→drop→re-request loop. **It reads 0 everywhere** — the
  loop does not exist (§7). Kept anyway: it is the canary that would catch
  it if the tier-1 budget ever rose above the slot count.

- **`visible_tiles_max`** + `atlas_slots` in the summary — strictly-visible
  tile count against the atlas capacity that bounds the request walk. This
  is the instrument that found §7, and the one that turns "zoomed out feels
  broken" into a number: `428 vs 144 slots <-- OVER CAPACITY`.

All are always-on relaxed atomics (free when not recording) and land in
`summary.json` plus two new `sb-bench` summary lines.

**New scenarios** (all take `$SB_BENCH_LIBRARY`):
`zoomed_out_thumb_storm.toml` (the report's steps),
`zoomed_in_control.toml` (identical minus the four zoom-out keypresses),
`zoom_storm_instant_open.toml` (opens quickview at the storm's *peak*
instead of 25 s later), and `zoom_out_max.toml` (zooms to `zoom_min`, holds
120 s, then opens quickview — the scenario that found §7).

Note on scenario design: `validity.require` deliberately omits
`selected_served` in the storm scenarios. If the movie never plays, that is
the finding — the run must stay valid enough to read.

## 2. The measurements

Six sequential runs, never concurrent (two readers on one SMB share
invalidate both). `storm1` first (coldest link), then `control1`,
`instant1`, `control2` — see §5 on the ordering confound — then the two
max-zoom runs of §7. The table below covers the first four; §7 covers
`maxzoom1`/`maxzoom2`.

| | control1 | control2 | storm1 | instant1 |
|---|--:|--:|--:|--:|
| run order | 2nd | 4th | **1st (coldest)** | 3rd |
| zoomed out? | no | no | **yes** | **yes** |
| quickview opened | +35 s | +35 s | +41 s | **+1 s (peak)** |
| `thumb_requests` | 79 | 79 | **136** | **136** |
| queue wait thumb mean / max | 4905 / 9227 ms | 5245 / 9608 ms | 2854 / 11814 ms | 3161 / 8887 ms |
| queue wait gen mean / max | 124 / 243 s | 121 / 238 s | 166 / 339 s | 124 / 233 s |
| **`decode_read` max** | 532 ms | 5.0 ms | **1011 ms** | 1.8 ms |
| **`decode_read` >100ms / >1s** | 1 / 0 | 0 / 0 | **7 / 1** | 0 / 0 |
| **`render_stall` max** | 0.1 ms | 0.1 ms | **350.6 ms** | **901.4 ms** |
| `render_stall` meta total (n) | 0.2 ms (3) | 0.2 ms (3) | 410 ms (7) | **2807 ms (7)** |
| **`tick_ms` max** | 3.1 ms | 4.7 ms | **350.8 ms** | **902.7 ms** |
| frames >16 / >100 / >250 ms | 0 / 0 / 0 | 0 / 0 / 0 | 3 / 1 / 1 | **4 / 4 / 4** |
| **served-frame gap max** | 479 ms | 48 ms | **1036 ms** | **903 ms** |
| gap p50 / p95 | 16.7 / 20.6 ms | 16.7 / 20.8 ms | 16.7 / 20.5 ms | 16.7 / 20.6 ms |
| `late` / `reanchors` | 1 / 1 | 0 / 0 | **18 / 18** | 4 / 4 |
| `worker_utilisation` | 0.37 | 0.37 | 0.37 | 0.39 |
| `selected_served` | 143 ms | 142 ms | 200 ms | 160 ms |
| `library_count` met | 106 s | 105 s | **207 s** | 107 s |
| RSS peak / threads peak | 1026 MB / 142 | 1095 MB / 142 | 798 MB / 143 | 776 MB / 143 |

**The two controls bracket the storm runs in time** (2nd and 4th, cold and
warm link) and are indistinguishable from each other: 0 frames over 16 ms,
`render_stall` max 0.1 ms, 79 thumb requests, both. That is what makes the
zoom-out attribution in §3 safe rather than an ordering artifact.

In `instant1` the served-frame gap max (903 ms) equals `tick_ms` max
(902.7 ms): the blocked render thread is *also* what stopped frames being
served that moment. The UI and video symptoms are not always independent —
a render thread stuck in `clip_meta` freezes presentation too.

## 3. Finding: the UI freeze is `clip_meta` on the render thread, and the zoom-out multiplies it

In `instant1` the worst `frame()` (902.7 ms) and `render_stall_max_us`
(901.4 ms) agree to within 1.3 ms: **the entire frame was one synchronous
filesystem call.** That is review 04 §2's signature, reproduced — but where
04 caught it intermittently while browsing, the zoom-out + immediate open
produces it reliably, and worse (901 ms vs 04's 1.49 s ceiling, from a
scenario an order of magnitude shorter).

The mechanism is a clean chain, and every link is measured:

1. **Zoom-out floods tier 1.** `request_visible_thumbs`
   (`sb-app/src/lib.rs:3808`) queues one `Thumb` job per visible tile,
   bounded by the atlas budget: 12×12 slots − 8 headroom = **136**. Measured
   `thumb_requests` = exactly 136 zoomed out, 79 zoomed in.
2. **Tier 1 is subject to no throttle.** In `Queues::pop`
   (`sb-media/src/lib.rs:260`) the live cap gates only the sweep:
   ```rust
   let capped = anim_in_flight
       || (self.live && self.gen_live_cap != 0 && self.gen_running >= self.gen_live_cap);
   ```
   `Thumb` is popped unconditionally at the top of the ladder. **A zoom-out
   therefore converts throttled background work into unthrottled
   top-priority foreground work — precisely when the user is about to ask
   for playback.** 3 workers go to 3 concurrent `made` ffmpeg jobs on SMB.
3. **Lane spawns then touch the same saturated link, on the render
   thread.** Both `clip_meta` call sites (`lib.rs:4688` warm, `lib.rs:4759`
   selected) sit in live-lane spawn paths, so render-thread fs calls scale
   with **spawn count** — and the zoomed-out visible set produced 5–6 warm
   spawns vs 2 in the control.
4. **Each call inherits the contended SMB tail.** Same 7 calls, wildly
   different cost: **0.07 ms each** in the control, **~401 ms each** in
   `instant1` (2807 ms / 7), worst **901 ms**.

So it is not that `clip_meta` is called more often (7 vs 3 is minor); it is
that the storm makes each call **~5,700× more expensive**. The fix review 04
already identified — move `clip_meta` off the render thread — is the right
one, and this review raises its priority: the zoom-out makes it a reliable
freeze rather than an intermittent one.

## 4. Finding: the video thread genuinely blocks on SMB — first direct measurement

`storm1` recorded a **1,011 ms** block inside a single `av_read_frame`, and
a matching **1,036 ms** gap between served frames. Seven reads exceeded
100 ms; one exceeded 1 s.

**The block is real; its attribution is not settled.** `storm1` was the
coldest run of the four, and the warm-link storm run (`instant1`) read clean
(max 1.8 ms, zero over 100 ms). Meanwhile `control1` — no zoom-out, but the
colder of the two controls — recorded a 532 ms read, and `control2` (warmest)
recorded 5 ms. Read across all four, the `decode_read` tail tracks **link
state** at least as well as it tracks the zoom-out. See §5.

What *is* established is that the live decoder blocks on SMB for over a
second at a time, and that this is invisible to every other instrument.

This is the "video thread" half of the user's report, and it is the first
time it has been measured directly rather than inferred. Two things make it
readable only through the new counter:

- **The mean is useless here** — 0.10 ms across 37,697 reads in the very run
  containing a full-second freeze. Only the explicit tails show it.
- **Playback quality metrics look fine.** Gap p50/p95 were 16.7/20.5 ms —
  indistinguishable from the control. The freeze is one event in a
  6-minute run, which is exactly the shape of a user saying "sometimes".

`late`/`reanchors` rose 18× with the storm (18 vs 1), which is the pacing
layer's view of the same contention: the `SeekablePlayer` re-anchor
invariant is converting jittery SMB reads into repaced playback rather than
stutter — doing its job, and now with a number attached.

## 5. What did NOT reproduce, and what is confounded

Reported honestly, because two of these limit the conclusions:

- **The hard lockup / crash did not reproduce.** In all four runs the app
  exited cleanly and playback *started* every time — `selected_served` was
  143–200 ms even at peak contention. A 902 ms frozen frame is a severe
  hitch and plausibly reads as "locked up" in the moment, but it is not the
  "needs to be killed" state the user describes. Consistent with 03 and 04,
  which also failed to reproduce the crash. **The crash remains unexplained.**
- **"Movie will not play" did not reproduce as a failure to start.** It
  reproduced as a ~1 s freeze *during* playback. Whether the user's symptom
  is this or something worse is not settled by these runs.
- **Run-order confound — resolved for the UI freeze, still open for the
  video freeze.** `storm1` ran first against the coldest link
  (`library_count` met at 207 s vs ~105 s for every later run), so part of
  its badness could have been cache warmth rather than the zoom-out. Two
  things settle it for §3: `instant1` ran **third**, on a warm link, and
  still produced the *worst* render stall of the set (901 ms); and the two
  controls, run **2nd and 4th** either side of it, are identical to each
  other (0.1 ms). Cache warmth does not explain a 901 ms stall appearing
  only in the zoomed-out runs.

  It is **not** resolved for §4. The `decode_read` tail appears only in
  `storm1` — the one cold run — while warm-link `instant1` read clean
  (max 1.8 ms). So "the zoom-out worsens video-thread blocking" rests on a
  single cold run and must be treated as unconfirmed. A repeat of the storm
  scenario on a warm link is the run that would close it; that the *control*
  also showed a 532 ms read on the colder of its two runs suggests link
  state, not zoom, may drive that particular tail.
- **Single runs.** One run per configuration (plus one repeat control).
  Review 04 established this stall is intermittent — an identical browse
  scenario gave 1.49 s once and 11.9 ms the next time. So "did not
  reproduce" here means "did not reproduce this time", not "does not
  happen".
- **No memory or thread pathology.** `pending_bytes_peak` 2.6 MB in every
  run, RSS 798–1026 MB, threads plateaued at 142–143. Not a leak, and not a
  crash mechanism.
- **No scheduler thrash.** Utilisation 0.37–0.39 throughout. The pool is
  parked on the gen throttle (03 §2's finding, unchanged), not thrashing.

## 6. Finding: the pool is idle while the user waits

Worth stating separately because it is counter-intuitive and it is the
number that most directly contradicts "the scheduler thrashes":

**Worker utilisation was 0.37 while 133 visible thumbs waited a mean of
2.9 s (worst 11.8 s).** Two of three workers idle, for most of the run,
with tier-1 work queued that the user is actively looking at.

The gen sweep meanwhile waited a mean of **166 s** (worst **339 s**) — it is
being held by the throttle, as designed. But the throttle parks *workers*,
and a parked worker cannot pick up the visible thumb that arrives a moment
later; it must be woken. The result is a pool that is neither serving the
sweep nor promptly serving attention work. 03 §2 measured that uncapping
the sweep bought +63 % thumbs with no measured cost to the watched stream;
this review adds that the same throttle also delays *foreground* work.

## 7. Finding: at full zoom-out the atlas cannot hold the screen — a capacity ceiling, not a delay

> ### ⚠️ CORRECTION (2026-07-24, same day): this section measured the HARNESS, not the app
>
> **The 144 slots below are `sb-bench`'s internal defaults, not any
> configuration switchblade actually runs under.** The runner is hermetic
> (`no_config: true`, temp `HOME`), so it uses `Tuning::default()` —
> `thumb 640×360`, `atlas 7680×4320` → 12×12 = **144 slots**. Both real
> configs are far larger, and neither is over capacity:
>
> | config | `thumb` | `atlas` | slots | visible at `zoom_min` | over? |
> |---|---|---|--:|--:|---|
> | harness default (measured below) | 640×360 | 7680×4320 | **144** | 484 | **YES, 3.4×** |
> | repo-root `switchblade.toml` | 768×432 | 16128×15984 | **777** | 484 | no |
> | `~/.config/switchblade.toml` | 640×320 | 19200×12800 | **1000** | 392 | no |
>
> Measured with `benchmarks/scenarios/zoom_out_capacity.toml`, which
> answers this question the way it should have been asked in the first
> place: the ratio is a property of **layout and config only**, so demo
> tiles settle it in 4 seconds with no disk, no decoders and no network.
> (The last row uses the user's real layout tuning too — `tile_width` 150,
> `gap` 2 — which is why its visible count differs.)
>
> **So there is no capacity ceiling in the app as it is actually run, and
> the "needs a design decision (mip tier / bigger atlas / capped zoom-out)"
> conclusion is withdrawn.** With 777–1000 slots the tier-1 walk budgets
> `slots() - 8` and requests *every* visible tile, so a slow zoomed-out
> fill is a **throughput** problem — §6's parked pool and the permanently
> engaged gen throttle — not a capacity one. What survives unchanged: the
> `atlas_full_drops = 0` result (§7's disproved runaway loop), and the fact
> that the *default* atlas cannot fill a zoomed-out screen, which is a
> defaults question and now the only open part.
>
> The lesson is the reusable one: **a hermetic harness measures the
> harness's defaults.** Any claim about a config-derived quantity has to be
> re-run with `--set` at the values that ship, or it is a statement about
> `Tuning::default()`.
>
> The rest of this section is left as originally written.

The first four runs zoomed out **four steps**. That was not what the user
does, and it mattered: they recorded `evictions=0`, i.e. the atlas never
even filled. Two further runs (`maxzoom1`, `maxzoom2`) zoom to the floor
(`zoom_min = 0.35`, twelve presses, clamped) on a 3,000-clip grid.

The instrument added for it is decisive:

```
atlas: visible_tiles_max=428 vs 144 slots  <-- OVER CAPACITY | full_drops=0 | evictions=0
```

**428 tiles on screen; 144 atlas slots; 136 tier-1 requests.** At
`zoom_min` a tile is ~84×47 px while an atlas slot is 640×360 — the app
stores a full-size thumb to draw it postage-stamp size, so the atlas holds
144 thumbs no matter how small the tiles get. Three consequences, all
measured:

1. **~292 of the 428 visible tiles are never requested at tier 1 at all.**
   `request_visible_thumbs` budgets its walk against `slots() - 8 = 136`,
   so the walk stops long before the screen does. Those tiles are not
   "slow" — they are outside the foreground policy entirely.
2. **They fall to the gen sweep, which is running one job wide.** Sampled
   throughout the hold at max zoom:
   ```
   queue: thumbs=0  gen_sweep=4231  inflight=1  gen_running=1
   ```
   The thumb queue is **empty** — tier 1 finished long ago. The backlog is
   4,231 and *grew* during the run (2,931 → 4,231) as ingest admitted more
   clips than one worker could sweep. Gen queue wait: **mean 167 s, max
   333 s**. At ~1 s per SMB job, a 292-tile screen fills at roughly one
   tile per second, from a queue that is getting longer.
3. **This is the "screen full of 'loading' that never populates".** It is
   not thrash and not a stall: `worker_utilisation` 0.36, thumb queue
   empty, pool parked. The user is watching a 428-tile screen being served
   by a single throttled worker.

**A hypothesis tested and disproved.** Before running this, the obvious
theory was a runaway regeneration loop: visible tiles exceed slots →
`alloc_slot` cannot evict in-zone statics → `drain_media` resets the clip
to `Thumb::None` "to stay retryable" → still visible → re-requested next
frame → forever. The `atlas_full_drops` counter was added specifically to
catch it. **It reads 0 in every run, at every zoom level.** The loop does
not occur, because the tier-1 budget (136) keeps requests *below* the slot
count (144), so a finished artifact always finds a free slot. The budget
that starves the screen is the same budget that prevents the loop.

Recording it because it is a natural theory that will be re-formed by the
next person reading `alloc_slot`, and the counter to settle it now exists.

## 8. Conclusions

1. **The zoom-out is causal for the UI freeze and belongs in the repro.** It
   doubles the tier-1 flood to the atlas bound (136 jobs / 3 workers), and
   against that contention the render thread blocked 901 ms — where both
   zoomed-in controls, run either side of it, saw zero frames over 16 ms.
   The video-thread freeze (1,011 ms read, 1,036 ms gap) is measured and
   real but appeared only in the coldest run, so its link to the zoom-out
   specifically is **unconfirmed** (§4, §5).
2. **The UI freeze is `clip_meta` on the render thread** (§3), the same
   mechanism 04 found, now reliably triggerable and worse. The storm does
   not add many calls; it makes each call ~5,700× more expensive.
3. **The scheduler is not thrashing** (§6). Utilisation 0.37–0.39. Both
   freezes are *filesystem* latency reaching a thread that must not block,
   not queue mismanagement. The fixes are therefore about **which thread
   touches the disk**, not about queue order.
4. **Tier 1 having no live throttle is a real structural gap** (§3.2) — it
   defeats the "user attention owns the CPU" invariant from the other
   direction: the flood *is* nominally attention work, so it outranks
   everything, including the storyboard sheet the user is waiting on
   (`anim` sits *below* `thumb` in the ladder).
5. ~~**At full zoom-out the binding constraint is atlas capacity, not
   scheduling**~~ — **WITHDRAWN, see §7's correction.** 144 slots was the
   harness's own default, not a shipped configuration; at the real 777–1000
   slots every visible tile *is* requested at tier 1. The binding constraint
   at full zoom-out is throughput (conclusions 3 and 6), and what remains of
   this finding is that the *default* atlas cannot fill a zoomed-out screen
   — a defaults question, not a design one.
6. **Throttling tier 1 is NOT the fix, and an earlier draft of this review
   was wrong to lead with it.** The pool is parked at 0.36 utilisation with
   an empty thumb queue; there is no concurrency to take away. The two real
   levers are (a) get blocking fs off the render thread, and (b) make the
   zoomed-out screen fillable at all — more slots, or smaller slots at low
   zoom. Throttling would only reduce link contention for (a), which
   fixing (a) directly makes moot.
7. **The crash still has no measured explanation** after three reviews.

## 9. Follow-up: the same scenario after the fixes (2026-07-24, same day)

`zoom_out_max.toml` re-run on the same library and link, at the **real**
atlas + layout config (`--set atlas_width=19200 --set atlas_height=12800
--set thumb_width=640 --set thumb_height=320 --set tile_width=150 --set
tile_height=150 --set gap=2`), against `2baf275`:

| | maxzoom1 | maxzoom2 | **after** |
|---|--:|--:|--:|
| worker utilisation | 0.35 | 0.36 | **0.92** |
| artifacts *made* (excl. hits) | 364 | 384 | **776** |
| made/s over full wall | 0.88 | 1.08 | **1.58** |
| tier-1 requests | 136 | 136 | **392** |
| visible tiles vs atlas slots | 428 vs 144 | 428 vs 144 | 358 vs **1000** |
| **worst frame (`tick_ms` max)** | 299 ms | **546 ms** | **4.0 ms** |
| **render-thread fs stalls** | 17 (max 298 ms) | 17 (max 544 ms) | **0** |
| `decode_read` max / >1s | 681 ms / 0 | 513 ms / 0 | 616 ms / 0 |

**Three changes are in flight at once here** (the config, the throttle
narrowing, the async meta read) and the link was slower this time
(`library_count` at 302 s vs 105–207 s), so read the rows by what could
possibly have moved them, not as one result:

- **`render_stalls` 17 → 0, worst frame 546 ms → 4.0 ms.** Only the meta /
  thumb-path work touches render-thread fs, so this is attributable and
  clean. §3's UI freeze — the headline of this review — **does not
  reproduce**, under the conditions that produced it reliably.
- **Utilisation 0.36 → 0.92.** Only the throttle change touches worker
  parking. §6's "the pool is idle while the user waits" is fixed: the pool
  now works instead of parking.
- **Tier-1 requests 136 → 392** is arithmetic from the slot count
  (`budget = slots() - 8`), per §7's correction.
- **Throughput 1.08 → 1.58 made/s is confounded** — more tier-1 work was
  admissible *and* the sweep ran wider *and* the link differed. Directionally
  right, not a clean 1.5× claim. (It is also a floor: 302 s of this run's
  490 s went to ingest.)
- **`decode_read` still shows a 616 ms block** with none over 1 s. §4's
  video-thread finding is neither reproduced at full severity nor closed.

## 10. Open work

Indexed in [TASKS.md](../../TASKS.md); carries forward from
[04 §7](04-network-storage-scheduler.md#7-open-work).

- ~~**Move `clip_meta` off the render thread**~~ — **shipped 2026-07-24**,
  same day. Two halves: the source `stat` was *eliminated* (the ingest
  thread's stat now carries `(size, mtime)` to `Clip.fp`, so the lookup
  reads only the local cache entry), and the remaining read moved to a
  dedicated `MetaService` thread — not the ffmpeg pool, whose workers sit
  in ~1 s jobs here. A spawn without its meta defers one frame instead of
  guessing an anchor, capped by `Tuning::meta_wait_ms` and counted by
  `meta_wait_expired`. Measured on `hover_then_select_handoff`: 20
  render-thread fs calls → 1, `render_stall meta 0×`. **Confirmed on this
  review's own scenario in §9**: 17 stalls and a 546 ms worst frame → 0
  stalls and a 4.0 ms worst frame. See
  [live-playback.md](../architecture/live-playback.md).
- ~~**Gate the handoff-dump `cached_thumb_path` behind `SB_HANDOFF_DUMP`**~~
  — **shipped 2026-07-24**: the argument is a closure now, evaluated only
  past the env-var and first-frame gates. The drag ghost's own
  `cached_thumb_path` moved to mouse-down at the same time (P1.7), so the
  `path` class is down to at most one call per press.
- ~~**Make the zoomed-out grid fillable**~~ — **mostly withdrawn** (§7's
  correction): at the real 777–1000 slots there is no capacity ceiling.
  What is left is narrow — **the internal DEFAULT atlas (144 slots) cannot
  fill a zoomed-out screen (484 tiles)**, which every `--no-config` run,
  every test and every unconfigured user gets. A defaults question, not the
  DESIGN.md-level mip-tier decision this was filed as.
- ~~**Widen the sweep when it is what the visible screen depends on**~~ —
  **shipped 2026-07-24.** `gen_live_cap`'s trigger now means "the user is
  watching this" (a modal is open, or any lane is still in its cold spawn)
  instead of "a tile has a lane", which was true whenever the grid had a
  selection. A settled grid preview no longer throttles the sweep. The
  cap's *value* is untouched, still pending [03 §2](03-slow-disk-scheduler.md)'s
  4K re-measurement. **Confirmed over SMB in §9**: utilisation 0.36 → 0.92.
- ~~Throttle the tier-1 flood~~ — **dropped.** An earlier draft proposed
  this as a grid-fill-vs-playback tradeoff needing a product call. The
  measurements do not support it: utilisation 0.36, thumb queue empty,
  `atlas_full_drops` 0. There is no flood to throttle at the zoom levels
  that hurt, and the real costs are elsewhere (§7, §3).
- **Re-run the storm scenario on a warm link** to close the §5 ordering
  confound on the `decode_read` comparison.
- **Repeat runs.** Everything here is n=1 per configuration against an
  intermittent fat tail.
- **Still unclosed from 03/04**: Tier B GPU/upload run, long-run soak with
  the process canary armed (the remaining candidate for the crash), 4K
  decode path.

---

Back to the [perf-review index](README.md) · [CLAUDE.md](../../CLAUDE.md)
