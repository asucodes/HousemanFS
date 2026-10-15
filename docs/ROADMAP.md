# Roadmap

Ordered so that each phase proves the thing the next phase depends on. The sequencing is
deliberate: everything downstream — search, suggestions, the decision model — sits on top of the
accounting being correct, so the accounting gets built and verified first.

## Current status

Phase 0 is closed except the elevated validation; Phase 1 is built; Phase 2 judges with a
baseline engine. The walk scans real volumes and reconciles against the filesystem with a
decomposed residual — 72 tests passing, plus a CI workflow running fmt, clippy and the
suite on Windows.

Measured on `C:\`, October 2026 (NTFS, 302 GiB volume):

- **Elevated:** 1.31M files, unaccounted **7.13 GiB / 2.36%** — MFT (1.98 GiB), NTFS system
  files and directory overhead (496 MiB) all measured, not estimated.
- **Standard user:** unaccounted ~13.5%, dominated by 516 unreadable directories and refused
  filesystem-metadata queries. The gap is access, not accounting: every unknown byte is
  named as unknown.
- **Alternate streams:** enumerated per file on allocation size — 1,293 streams totalling
  352 KiB on the measured volume. Small there, which is itself a finding: streams are not
  where this volume's space went.
- **Independent cross-check:** WMI (`Win32_LogicalDisk`) agrees with the tool's volume
  figures exactly on capacity and within 9 clusters on free space — the free-space wobble
  is live-volume activity between the two queries, not disagreement. `fsutil` itself needs
  elevation and could not serve as the reference from this shell; WMI goes through a
  separate stack, which is what makes it independent.

Two platform facts are now recorded rather than discovered repeatedly: `FileStreamInfo`
fails on directory handles (`ERROR_HANDLE_EOF`), so directory streams are disclosed once
in the report instead of counted; and timestamps compare at full precision, after a
truncated clock made every fresh file read as future-dated.

**The 2% tolerance, stated plainly.** It is a judgment, not a measurement: an elevated
scan attributes all but ~2.4%, and the structurally unmeasurable remainder — directory
streams, reparse-point payloads, resident-vs-allocated rounding, quota effects — plausibly
fits inside 2% on a healthy NTFS volume. It stays provisional until an elevated scan
with streams lands at or under it; if that scan lands above, the tolerance moves with a
written reason, not silently.

Beyond the walk: the index (schema v3) stores per-entry stream occupancy, provenance and
timestamps, counts hardlink groups once in every aggregate, refreshes in place with
conservative deletion, and carries the scan record; `explain` shows the measurement
behind any indexed path; `ask` answers structured questions through a typed IR;
`export` publishes a versioned JSON snapshot. Judgment runs through five versioned
rules over a baseline engine, measured against a 16-case committed evaluation set
(`suggest`, `eval`). The read-only invariant is enforced by an audit test, and the walk
is covered by a synthetic fixture test containing no real filesystem data.

Still missing: an elevated re-scan with streams (needs an administrator shell:
`housemanfs scan C:/ --out report.txt` from an elevated prompt, then compare the
unaccounted ratio against the 2% tolerance); engine selection and calibration (D4) —
the rules read factors, not engines, so a learned model plugs in without touching them.

## Phase 0 — the accounting oracle

The smallest thing that can prove or kill the project.

**Deliverable.** Point the tool at a volume and account for every byte: files, directories,
cluster slack, hardlink groups, alternate data streams, and system metadata. Publish the
residual and decompose it.

**Exit criterion.** The attribution reconciles against an independent reference (`fsutil`)
within a stated tolerance, and every unexplained byte is either explained or explicitly
reported as unexplained.

**Status.** Met except the elevated validation. The independent reference turned out to be
WMI rather than `fsutil` — both need elevation for the filesystem-metadata queries, but
only the capacity/free figures needed an independent check, and WMI supplies those through
a separate stack: exact agreement on capacity, 9 clusters on free. What remains is one
elevated scan with streams against the 2% tolerance; the command and the comparison are
recorded in Current status so whoever runs it needs no context.

**Why first.** If the numbers are wrong, nothing built on top of them means anything. A decision
model on a wrong index produces confident wrong answers faster, which is worse than no answers.

## Phase 1 — index, ask, explain (v1)

**Deliverables.**
- A persistent, incrementally-updatable index with a versioned schema. **Done:** schema v3
  with per-entry provenance, stream occupancy and timestamps, hardlink-aware aggregates, a
  versioned JSON export — and `refresh`, which re-walks and folds in only what changed
  (added/removed/modified/moved), deleting solely under scanned parents and counting the
  rest as stale.
- Natural-language search over that index. The model translates a question into a query; queries
  only read. **Done deterministically:** `ask` compiles size, extension, name, age, sharing
  and stream clauses — plus a closed set of kind words — into the typed `AskQuery` IR the
  index executes as parameterized SQL. It is pattern matching and says so; a language-model
  frontend would compile into the same IR, which bounds what it can express.
- `explain` — for any figure, show the measurement behind it. **Done** for indexed paths:
  `housemanfs explain <path>` reports each figure with the query that produced it, plus age.
- A minimal interface. Read-only, and visibly so. **Done twice:** the finder renders the
  index in a terminal and cannot initiate an action, and the desktop client
  (`housemanfs-desktop`) offers folder picking, background scanning with progress and
  cancel, a residual dashboard, tables, an allocated-size treemap, structured search,
  evidence panels and gated suggestions — rendering the same index through the same
  shared conversions, deciding nothing.

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

**Status.** The boundary holds end to end: `hfs-model` owns state assembly, the
versioned question set, the engine trait, five versioned rules and a 16-case committed
evaluation set. The engine answering the factors is a labelled heuristic baseline, not the
learned decision model D4 describes — engine selection and calibration remain, and the
rules will not care when they land, because they read factors, not engines. `suggest`
judges the largest indexed files with evidence, rule versions and confidence on every
line; `eval` runs the set for humans and fails the command on mismatch.

**Exit criteria.** Suggestions are correct often enough to be useful, measured against a
committed evaluation set — and the evaluation set contains no real filesystem data.
**Met as far as a baseline can:** 16 of 16 cases pass, silence cases included. A learned
engine must beat this score on this set to replace the baseline — that is the bar,
written down before the model exists.

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
