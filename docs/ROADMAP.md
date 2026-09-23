# Roadmap

Ordered so that each phase proves the thing the next phase depends on. The sequencing is
deliberate: everything downstream — search, suggestions, the decision model — sits on top of the
accounting being correct, so the accounting gets built and verified first.

## Current status

Phase 0 has started. The toolchain is installed and pinned (Rust 1.98.1, GNU target). `hfs-core`
holds the accounting model: file identity, extent ownership, and volume-level reconciliation
with an explicit residual — 17 tests, all passing.

Nothing scans a real volume yet. That is the next step, and it is the point where the model
stops being a claim and becomes a measurement.

## Phase 0 — the accounting oracle

The smallest thing that can prove or kill the project.

**Deliverable.** Point the tool at a volume and account for every byte: files, directories,
cluster slack, hardlink groups, alternate data streams, and system metadata. Publish the
residual and decompose it.

**Exit criterion.** The attribution reconciles against an independent reference (`fsutil`)
within a stated tolerance, and every unexplained byte is either explained or explicitly
reported as unexplained.

**Why first.** If the numbers are wrong, nothing built on top of them means anything. A decision
model on a wrong index produces confident wrong answers faster, which is worse than no answers.

## Phase 1 — index, ask, explain (v1)

**Deliverables.**
- A persistent, incrementally-updatable index with a versioned schema.
- Natural-language search over that index. The model translates a question into a query; queries
  only read.
- `explain` — for any figure, show the measurement behind it.
- A minimal interface. Read-only, and visibly so.

**Exit criteria.** A user can point it at a real drive and get a correct, inspectable answer to
"where did my space go", including an honest reclaim figure.

**Explicitly not in this phase.** Any write. Any plugin system. Any ecosystem surface beyond a
documented export format.

## Phase 2 — judgment and suggestions

**Deliverables.**
- Per-file judgment through the local decision model: categorization, staleness signals,
  compressibility.
- Suggestions assembled from those factors by versioned rules, each one inspectable.
- Confidence gating: below threshold, the tool says nothing.

**Exit criteria.** Suggestions are correct often enough to be useful, measured against a
committed evaluation set — and the evaluation set contains no real filesystem data.

## Phase 3 — reversible actions

Only after Phase 2 has been measured rather than assumed.

**Deliverables.** Actions in increasing order of consequence: compression, archive-move,
quarantine. Each with preflight re-verification against the live volume immediately before
execution, and an undo path.

**Explicitly not in this phase.** Irreversible deletion.

## Phase 4 — history and durability

**Deliverables.** Diff over time, scheduled read-only scans, and reporting suitable for
auditing. This is where a sustainable funding model could plausibly exist, if one is wanted.

## Not planned

- Replacing the system file manager. That is a different and much larger project.
- Real-time monitoring as a correctness mechanism. Change journals are lossy; reconciliation is
  the mechanism.
- Any opaque scoring. Judgment comes from named, versioned, switchable rules.
- Telemetry.
