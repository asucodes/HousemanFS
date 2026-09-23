# Architecture

This document describes the intended shape of HousemanFS. It is a design, not a description of
working code — see [`ROADMAP.md`](ROADMAP.md) for what actually exists.

## Guiding constraint

The tool must be able to explain any number it reports. Everything below follows from that: it
is why the model cannot act, why accounting is extent-aware, and why every stored fact carries
provenance.

## Layering

The workspace is split so that platform-independent logic never touches an operating system.
This is the seam that makes the whole thing testable, and it is worth protecting.

```
hfs-core      pure data model — identity, accounting, claims. No I/O, no platform code.
hfs-index     persistence and queries (SQLite). No platform code.
hfs-model     the decision-model boundary: state assembly, typed questions, calibration.
hfs-win       the ONLY crate that talks to Windows. All FFI and unsafe lives here.
hfs-cli       the user-facing entry point.
hfs-tui       the minimal interface. Renders; decides nothing.
```

Rules that keep this honest:

- `hfs-core`, `hfs-index` and `hfs-model` contain no operating-system code and are testable on
  any platform.
- `unsafe` is confined to `hfs-win`. Any exception needs a written justification.
- The interface layer opens no files and holds no volume handles. It renders values it is
  given, and cannot initiate an action.

## The read path

1. **Enumerate.** Walk the volume's metadata. On NTFS this uses documented volume-level APIs to
   read directory entries without opening each file.
2. **Measure.** For each entry, record logical size, allocated size, timestamps, attributes,
   link count, alternate data streams, and reparse state — each tagged with which API produced
   it.
3. **Account.** Aggregate to directories and to the volume, then compute the residual: the
   difference between what the filesystem says is used and what we managed to attribute.
4. **Reconcile.** Compare against an independent reference (`fsutil`). If the residual is
   larger than expected and cannot be explained, that is a finding to surface, not a bug to
   hide.

The residual is a first-class output. A tool that silently reports an unexplained 40 GB gap is
not trustworthy; one that says *"11 GB unaccounted: 6 GB estimated system metadata, 5 GB across
1,284 paths we were denied access to"* is.

## Identity

Files are identified by **file ID, never by path**.

Paths are not stable — renames and moves are among the most common operations, and a
path-keyed index loses every derived fact about a file the moment it moves. Keying on the file
ID makes a move free.

Identity is `(volume instance, file ID)`, where the volume instance is derived from properties
that survive remounting. MFT reference numbers are **not** used, because they are recycled and
their layout shifts.

## Accounting model

This is the part that has to be right, and the part almost nobody gets right.

- **Hard links** are counted once, as a group. Counting each link separately is the classic
  2× overcount bug.
- **Shared extents** (deduplicated or block-cloned data) must not be double-counted. Deleting
  one of several files that share storage frees nothing, and a reclaim estimate that ignores
  this is wrong in the optimistic direction — the exact failure this project exists to fix.
- **Sparse and compressed files** are measured on allocated size, not logical size.
- **Alternate data streams** are counted separately and explicitly. They are invisible to
  directory listings and are a common source of unexplained space.
- **Cluster slack** is accounted for using the volume's actual cluster size.

Where unique ownership of an extent cannot be determined, the estimate is reported as
**unknown**, never as an optimistic guess.

## The decision-model boundary

HousemanFS uses a local decision model rather than a chat model. The distinction matters:

- A decision model takes **state** plus **typed questions** and returns calibrated
  probabilities with confidence. It generates no text.
- Because the answer must be one of the supplied options, it cannot return a file that does
  not exist or an action that was not defined.
- It runs locally, so no metadata leaves the machine.

The boundary is strict:

```
metadata  ->  typed questions  ->  calibrated probabilities  ->  YOUR RULES  ->  suggestion
                                                                    ^
                                                    the model has no path past here
```

The model produces **factors**. Deterministic code combines them using rules that are readable,
versioned and individually switchable. A suggestion is only emitted when confidence clears a
threshold; otherwise the tool says nothing.

This is what makes "AI-powered" compatible with "explainable": every factor is inspectable, and
the combination rule is code you can read and test.

Note that there is no prompt here. The equivalent of a prompt is the question set plus the state
schema — both versioned artifacts, which is strictly better than a string.

## Persistence

An index is derived data: rebuildable, never a source of truth. Consequences:

- Corrupt index → rebuild, never attempt repair.
- The schema is versioned from the first commit. Once anyone holds an index file, it cannot be
  broken.
- Queries are parameterised from a typed intermediate representation. No raw SQL crosses an API
  boundary.

## Deliberately absent

Each of these was considered and excluded for a reason, not for lack of time:

- **Raw on-disk format parsing.** Documented APIs only. Undocumented structures break silently
  across OS releases, which is unacceptable in a tool people point at their only copy of
  something.
- **Kernel drivers.** Signing, crash risk, supply-chain exposure, and no requirement that
  user-mode APIs cannot meet.
- **Real-time monitoring.** Change journals are bounded and lossy. Reconciliation is the
  correctness mechanism; streaming is a hint.
- **Plugin ABI.** The extension surface is the versioned data contract, so that anyone can
  build on HousemanFS in any language without third-party code running inside a tool that can
  read your entire disk.
