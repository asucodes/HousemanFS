# Roadmap

Ordered so that each phase proves the thing the next phase depends on. The sequencing is
deliberate: everything downstream — search, suggestions, the decision model — sits on top of the
accounting being correct, so the accounting gets built and verified first.

## Current status

Phase 0 is nearly closed; Phase 1 is half built. The walk scans real volumes and reconciles
against the filesystem with a decomposed residual — 45 tests passing, plus a CI workflow
running fmt, clippy and the suite on Windows.

Measured on `C:\`, October 2026 (NTFS, 302 GiB volume):

- **Elevated:** 1.31M files, unaccounted **7.13 GiB / 2.36%** — MFT (1.98 GiB), NTFS system
  files and directory overhead (496 MiB) all measured, not estimated.
- **Standard user:** unaccounted ~13.5%, dominated by 516 unreadable directories and refused
  filesystem-metadata queries. The gap is access, not accounting: every unknown byte is
  named as unknown.
- **Alternate streams:** enumerated per file on allocation size — 1,293 streams totalling
  352 KiB on the measured volume. Small there, which is itself a finding: streams are not
  where this volume's space went.

Two platform facts are now recorded rather than discovered repeatedly: `FileStreamInfo`
fails on directory handles (`ERROR_HANDLE_EOF`), so directory streams are disclosed once
in the report instead of counted; and the residual's 2% tolerance is still a stated
constant, not a calibrated value — closing Phase 0 means either meeting it elevated or
writing down why it is what it is.

Beyond the walk: the index (schema v2) stores per-entry stream occupancy and provenance,
counts hardlink groups once in every aggregate, and carries the scan record; `explain`
shows the measurement behind any indexed path, and `export` publishes a versioned JSON
snapshot. The read-only invariant is enforced by an audit test, and the walk is covered
by a synthetic fixture test containing no real filesystem data.

Still missing for Phase 0's exit: an elevated re-scan with streams (expected to land at
or under tolerance), and reconciliation against the independent reference (`fsutil`).
Still missing for Phase 1: incremental index updates, natural-language search, and the
`hfs-model` boundary those depend on.

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
- A persistent, incrementally-updatable index with a versioned schema. **Part done:** the
  index exists at schema v2 with per-entry provenance, stream occupancy and hardlink-aware
  aggregates, plus a versioned JSON export. Incremental updates are not done — every build
  is a full re-scan.
- Natural-language search over that index. The model translates a question into a query; queries
  only read. **Not done:** search today is literal substring matching. The `hfs-model`
  boundary it depends on does not exist yet.
- `explain` — for any figure, show the measurement behind it. **Done** for indexed paths:
  `housemanfs explain <path>` reports each figure with the query that produced it.
- A minimal interface. Read-only, and visibly so. **Done:** the finder renders the index
  and cannot initiate an action.

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

**Status.** The boundary is scaffolded ahead of the engine: `hfs-model` owns state
assembly, the versioned question set and the engine trait, with a stub engine returning
maximum uncertainty so the pipeline and its tests exist before any model does. Still
missing: engine selection (D4), the evaluation set, and the combination rules.

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
