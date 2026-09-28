# F1R3Gaze

A browser whose only execution mechanism is f1r3lang on a native RSpace.
This repository implements the portable core of the *F1R3Gaze Implementation
Specification* v0.1 (`publications/f1r3gaze`), on top of CampF1R3 with the
upstream work packages U1, U3, U4 and U6 applied.

## Building

The workspace expects a CampF1R3 checkout beside it, on the
`gaze/k1g-apertures` branch (the patch in `patches/` applied to
`campf1r3@b46e16c`):

```
../campf1r3      CampF1R3 with patches/0001 applied
./               this workspace
cargo test --offline
```

Rust 1.91, edition 2024. No external crates: every dependency is CampF1R3 or
the standard library, and every crate carries `#![forbid(unsafe_code)]`.

## What is here

| crate | spec | contents |
| --- | --- | --- |
| `gaze-graded` | §10 | the four integer semirings (Boolean, Viterbi, tropical, R≥0), xoshiro256**, argmax / argmin / proportional sampling, the ψ ⊕ ε fairness combinator, ceilings |
| `gaze-knf` | §5 | the `.knf` container, the manifest as a K1G map, program and grant hashes, `integrity` strings, conventional capability names |
| `gaze-dom-core` | §7 | the DOM protocol engine over a `DomBackend` trait; names, attenuation, the verb set, fragments with `ref` markers, frame-deferred writes, event dispatch with capture, bubble, `stop`, `prevent`, `once`; `mem::MemDom`, an in-memory reference backend with a small HTML parser and selector engine |
| `gaze-exec` | §6, §8 | `TabExec`: grounding of imports, apertures, the `doc`, `log`, `clock` and `rand` capabilities, dead channels for denied imports, the five-step frame loop, the page meter, the `.gzlog` recorder and replay |

## Upstream changes to CampF1R3 (in `patches/`)

- **U1 ground data.** Booleans, 64-bit integers, strings, bytes, lists, tuples
  and maps through lexer, parser (level `K1G`), AST, normaliser, encoding
  (tags `0x09`–`0x0F`), decoding, substitution, JSON, printer and matcher.
  Maps are ordered by key encoding; duplicate literal keys are rejected.
- **U3** `Machine::with_observer`, and `Observer::observe_aperture`.
- **U4** `bind_aperture`, `take_outbound`, `inject`.
- **U6** the `Keyed` minter.
- A job whose cut an observer refuses is put back at the head of the queue,
  so an exhausted budget stops progress without losing work.

Three defects in the existing code were found and fixed on the way:

1. Substitution reversed the arguments of a polyadic send (`rebuild.rs`).
2. The matcher bound a name pattern to the raw quote `@P` instead of applying
   `@*x = x`, so a received name could not be sent on. The rewrite was missing
   at three sites: the `*y` process pattern, the datum-to-name goal, and
   `Chan::quote`.
3. A quoted pattern `@{*y}` could not match a concrete (unforgeable) name.

## Tests

CampF1R3: 181 (the original 170 plus 11). This workspace: 24, including the
conformance suite in `crates/gaze-exec/tests/conformance.rs`:

- the lamp of the specification's appendix toggles end to end, including two
  clicks in one frame;
- a recorded session replays with identical commit hashes, through the log's
  byte encoding;
- the same seed gives the same run; a different seed gives different names and
  the same document;
- denied capabilities are dead channels;
- a component handed an attenuated name cannot read outside its subtree;
- clock subscriptions and timers;
- a runaway page is bounded per frame and keeps its work.

## Deviations from the specification, for review

- **Ground values** are two node variants, `Lit` and `Coll` (a map's items
  alternate key and value), not one `Ground(Lit)`.
- **Authority of names.** A name obtained through another name inherits its
  root, so a page can navigate its own document (`parent`, `closest`). A new
  verb, `attenuate`, mints a name rooted at its own node; that is the name a page
  hands to a component to confine it. The specification's literal rule (every
  name rooted at itself) would make `parent` useless.
- **Commit hash.** The hash recorded per frame is of the committed document's
  serialisation, not of the write batch.

## Not yet implemented

- U2 guards (`k1ndl1ng-eval`) and U5 (the store's resolver seam): K2 and
  graded pages are refused at load with a message naming the work package.
  `gaze-graded` is complete and waiting for the seam.
- `decide` listeners and their synchronous drain.
- The native side: `gaze-dom-blitz` (`RhoDocument`, `RhoEventHandler`),
  `gaze-broker`, `gaze-net`, `gaze-store`, `gaze-shard`, `gaze-blob`,
  `gaze-shell`, `gaze-devtools`, `gaze-reach`, `gaze-legacy`, `gaze-gateway`,
  and the `f1r3c` toolchain CLI.
- Matching of map patterns with holes in keys, and list rest patterns.
