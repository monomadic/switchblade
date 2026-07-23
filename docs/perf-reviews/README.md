# Performance reviews

Numbered chronologically (`01-`, `02-`, …), one file per review. These are
**reflective documents** — what was investigated, what was measured, what was
decided, and the durable facts that came out of it. They are *not* todo
lists: any still-open work a review produces is indexed in
[TASKS.md](../../TASKS.md), which links back here for the full problem
statements.

| # | Review | Date |
|---|---|---|
| 01 | [Live-video pipeline](01-live-video-pipeline.md) — decode/scale bottleneck, hw scaling, pacing invariants, deferred NV12 | 2026-07-10 |
| 02 | [Performance & efficiency review](02-efficiency-review.md) — scheduling, residency, proportionality (P0/P1/P2 queue); includes the 2026-07-20 render-thread stall addendum | 2026-07-16 |

The next review takes the next number. Keep the numbers stable — code
comments and other docs cite these files by path.

Measurement-driven **investigation reports** (bench bundles + analysis of a
specific symptom) live in [benchmarks/reports/](../../benchmarks/reports/),
alongside the scenarios and raw data they reference.
