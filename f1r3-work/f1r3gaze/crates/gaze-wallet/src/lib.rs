//! `gaze-wallet` — the browser's wallet. The agent driving the browser pays
//! for work on the node: every deploy the bridge makes is signed by the
//! active wallet ([`Wallets`] is the bridge's [`gaze_shard::Payer`]).
//!
//! * [`address`]: F1R3Cap addresses, as the Embers SDK derives them.
//! * [`file`]: wallet files interchangeable with F1R3Sky.
//! * [`contract`]: what the wallet will sign for a transfer, checked.
//! * [`embers`]: balances, history and transfers through Embers.
//! * [`wallets`]: the profile's wallets and the active one.

#![forbid(unsafe_code)]

pub mod address;
pub mod contract;
pub mod embers;
pub mod file;
pub mod wallets;

pub use address::Address;
pub use contract::Limits;
pub use embers::{Embers, Transfer, WalletState};
pub use wallets::{Entry, Wallets};
