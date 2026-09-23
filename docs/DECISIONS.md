# Decisions

Design decisions recorded with their reasoning, so they can be challenged rather than silently
inherited. If you think one is wrong, open an issue and argue it.

Format: context, decision, consequences.

---

## D1 — The first release is read-only

**Context.** Any tool that writes to a user's volume will eventually destroy data. An
open-source project gets roughly one such incident before it is finished, not because the code
was bad but because the trust premise broke.

**Decision.** v1 cannot write to a scanned volume. This is enforced by an automated test
asserting zero create, write, delete and set-info operations during a scan — not by review.

**Consequences.** The tool ships without an action story, which is a real product limitation.
In exchange, a bug is survivable. Actions arrive later, one reversibility class at a time, and
irreversible operations are never reachable from a bulk selection.

---

## D2 — Rust

**Context.** The tool parses structures that come off disk and are not under our control, and
it is heavily concurrent.

**Decision.** Rust, with `unsafe` confined to a single platform crate.

**Consequences.** Memory-safety failure modes are removed from exactly the code that handles
untrusted input. In exchange we accept a large build graph and dependency churn, mitigated by
keeping platform dependencies behind our own types.

---

## D3 — MIT license

**Decision.** MIT.

**Consequences.** Maximum ease of reuse and contribution. No explicit patent grant, which is a
real if minor difference from Apache-2.0.

---

## D4 — A local decision model, not a hosted one

**Context.** Jev-style "System One" models return calibrated probabilities for typed questions
without generating text. The original is proprietary and hosted only; open reproductions exist
but are young.

**Decision.** Use a local, open-weights decision model (Laya: Apache-2.0, 421M parameters,
runs on CPU). Keep it behind an abstraction so the engine can be replaced.

**Consequences.** File metadata never leaves the machine, which for a filesystem tool is the
decisive advantage — file names reveal a great deal. We accept weaker judgment than a frontier
model would give, and we must not depend on the current engine being the engine in a year's
time.

---

## D5 — The model proposes; code decides

**Decision.** The model returns per-factor probabilities. Deterministic, versioned, individually
switchable rules combine them. The model has no path to the disk and cannot invoke an action.

**Consequences.** Explainability is structural rather than aspirational: every factor is
inspectable and the combination rule is readable code. We give up the ability to let the model
"just handle it", which is a feature.

---

## D6 — Identity is the file ID, never the path

**Decision.** `(volume instance, file ID)`, with the volume instance derived from properties
that survive remounting. MFT reference numbers are not used.

**Consequences.** Renames and moves become free rather than invalidating every derived fact.
Costs some complexity on filesystems without stable IDs, where identity has to be approximated
and the limitation surfaced.

---

## D7 — Documented APIs only

**Decision.** No raw on-disk parsing, no undocumented control codes, no kernel driver.

**Consequences.** We give up some speed and some data that only raw parsing would expose.
In exchange the tool does not break silently across OS releases, which is not acceptable in
something people point at their only copy of a file.

---

## D8 — Extent-aware accounting

**Decision.** Hardlinks counted once, shared extents never double-counted, sparse and compressed
files measured on allocated size, alternate data streams counted explicitly. Where unique
ownership cannot be determined, report `unknown`.

**Consequences.** Reclaim estimates are smaller and sometimes less exciting than competitors'
numbers. They are also correct, which is the entire product thesis.

---

## D9 — Reclaim claims come from measured deltas

**Decision.** Bytes-freed figures are derived from a measured free-space delta, never from
arithmetic on file sizes.

**Consequences.** Estimates require actually measuring rather than summing, which is slower and
more work. It is the difference between a claim that survives scrutiny and one that does not.

---

## D10 — The index is derived data

**Decision.** Rebuildable, versioned schema from the first commit, never a source of truth.

**Consequences.** Corruption means rebuild rather than repair. Format changes require
migrations, because once anyone holds an index file it cannot be broken.

---

## D11 — The extension surface is a data contract, not a plugin ABI

**Decision.** Publish the index schema and a machine-readable export. No in-process plugins.

**Consequences.** Anyone can build on HousemanFS in any language without third-party code
running inside a process that can read the entire disk. We give up the ecosystem energy that a
plugin API generates, and we avoid maintaining an unstable ABI for nobody.

---

## D12 — No telemetry

**Decision.** No phone-home, no usage reporting, no cloud dependency.

**Consequences.** We cannot measure real-world behaviour and must rely on voluntary reports.
Given that the tool's input is a map of a user's private storage, this is the only defensible
default.
