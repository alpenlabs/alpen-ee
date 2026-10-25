use std::error;

use alpen_primitives::WithdrawalIntent;
use strata_acct_types::Hash;

/// Trait for engine payloads that can be serialized and provide block metadata.
pub trait EnginePayload: Sized + Clone {
    type Error: error::Error + Send + Sync + 'static;

    /// Returns the block number of this payload.
    fn blocknum(&self) -> u64;
    /// Returns the block hash of this payload.
    fn blockhash(&self) -> Hash;
    /// Returns the execution state root of this payload.
    fn state_root(&self) -> Hash;
    /// Returns the withdrawal intents included in this payload.
    fn withdrawal_intents(&self) -> &[WithdrawalIntent];

    /// Serializes this payload to bytes.
    fn to_bytes(&self) -> Result<Vec<u8>, Self::Error>;
    /// Deserializes a payload from bytes.
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::Error>;
}
