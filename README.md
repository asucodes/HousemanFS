# HousemanFS

A local, read-only filesystem index that tells you the truth about your storage, lets you ask
questions about it in plain language, and shows the evidence behind every number it reports.

> **Status: Phase 0 in progress.**
> The accounting model exists in `hfs-core`: file identity, extent ownership, and volume
> reconciliation with an explicit residual, all covered by tests. Nothing scans a real volume
> yet, and nothing writes to one. See [`docs/ROADMAP.md`](docs/ROADMAP.md).

---

## The problem

Disk tools are not short of features. They are short of honesty.

Every consumer disk tool lies in the same direction: optimistically. It will report 50 GB of
"duplicates" on a volume where those files already share the same physical storage, so deleting
them frees nothing. It will rank a sparse 100 GB VM image as your biggest file when it occupies
2 GB. It will estimate compression savings on logical size rather than allocated size. Then it
prints a big number and lets you believe it.

HousemanFS takes the opposite position. It reports what it can measure, explains how it measured
it, and says "there is nothing to reclaim here" when that is the true answer. That answer is
rare, and it is the point.

```
apparent duplicates       50 GB
actually reclaimable      12 GB
  38 GB already shares storage
```

## What it is

- A **read-only** index of a volume's metadata.
- A **finder**: ask about your disk in plain language, not in a file-manager dialog.
- A source of **suggestions with evidence** — every claim traceable to the measurement behind it.
- **Local.** Nothing about your files leaves your machine.

## What it is not

- **Not a disk cleaner.** It does not delete things. v1 cannot write to a scanned volume at all,
  and that is enforced by tests rather than by good intentions.
- **Not a file manager.** It does not replace Explorer and does not try to. It is the tool you
  reach for when Explorer has already failed you.
- **Not a black-box optimizer.** There is no opaque "uselessness score". Judgment comes from
  named, versioned rules you can read and switch off.
- **Not a backup tool.** It is never the only copy of anything.

## Design principles

1. **Read-only first.** The first release cannot mutate a scanned volume. Not "is careful
   about it" — cannot.
2. **The model proposes, code decides.** A local decision model produces per-factor
   probabilities; deterministic code combines them into a suggestion. The model has no path to
   the disk and cannot invoke an action.
3. **Every number carries its provenance.** A figure with no traceable measurement is a bug.
4. **Confidence gates output.** Below threshold, the tool stays silent rather than guessing.
5. **Accounting is extent-aware.** Hardlinks counted once, shared extents not double-counted,
   sparse and compressed files measured on allocated size.
6. **Local by default.** No telemetry, no phone-home, no cloud dependency.

## Architecture

HousemanFS is a Rust workspace. Platform-independent logic lives in pure crates that can be
tested anywhere; all operating-system interaction is confined to a platform crate, so that the
data model is not entangled with Windows APIs.

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the design and
[`docs/DECISIONS.md`](docs/DECISIONS.md) for the reasoning behind each choice.

## Building

Requires a Rust toolchain (install via [rustup](https://rustup.rs)):

```sh
cargo build
cargo test
```

The toolchain version is pinned in `rust-toolchain.toml`.

## Contributing

Contributions are welcome. Please read [`CONTRIBUTING.md`](CONTRIBUTING.md) first — it covers
commit conventions, the read-only invariant, and one rule that matters more here than in most
projects: **never commit real filesystem data.**

## License

MIT. See [`LICENSE`](LICENSE).
