//! `gaze-shard` — the shard bridge (spec §9, WP S1/S2).
//!
//! It talks to `f1r3node-rust` over its HTTP API and event stream directly
//! rather than through Embers' `firefly-client`, which submits over gRPC and
//! needs `protoc` and `tonic` at build time; the node's `/api/deploy`
//! accepts the same signed deploy (see [`deploy`] for the exact preimage).
//!
//! Every reply to a page carries its assurance rung: `("ok", rung, v)`.
//! `node` and `quorum` are implemented; `proof` needs node work package N1
//! and `replayed` the rspace adapter, and both answer
//! `("err", "unavailable", ...)` until then.

#![forbid(unsafe_code)]

pub mod bridge;
pub mod chain;
pub mod deploy;
pub mod expr;
pub mod history;
pub mod keys;
pub mod node;
pub mod pos;
pub mod site;
pub mod term;
pub mod txn;
pub mod wallet;

pub use bridge::{Bridge, DriveSource, EventHub, KeyPayer, Payer, Prompt, Rung, ShardConfig, ShardOut, ShardService};
pub use keys::{FileKeystore, Keystore, MemKeystore};
pub use node::{Node, NodeDialect};
pub use site::{SiteAddr, SiteManifest};
