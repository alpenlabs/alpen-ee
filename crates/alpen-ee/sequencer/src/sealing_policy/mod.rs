//! Generic block accumulation and sealing policy framework.
//!
//! An [`AccumulationPolicy`] defines:
//! - The type of data collected per block ([`AccumulationPolicy::BlockData`])
//! - The type of accumulated value ([`AccumulationPolicy::AccumulatedValue`])
//! - How to accumulate block data ([`AccumulationPolicy::accumulate`])
//!
//! A [`SealingPolicy`] determines when to seal based on the accumulated state.
//!
//! These traits are used by the batch builder (and will be used by the chunk
//! builder) with different policy implementations.
//!
//! # Built-in policies
//!
//! | Module | Seals when… |
//! |--------|-------------|
//! | [`max_value_policy`] | A summed per-block value (block count, gas, …) would exceed a limit |
//! | [`or_policy`] | Any of the composed policies triggers (see [`crate::or_sealing!`]) |
//! | [`rotation_policy`] | The group's last block consumed a predicate rotation |

pub mod block_count_data_provider;
pub mod max_value_policy;
pub mod or_policy;
mod policy;
pub mod rotation_policy;

pub use policy::{AccumulationPolicy, Accumulator, BlockDataProvider, SealingPolicy};
