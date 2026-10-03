mod account_state;
mod batch;
mod exec_block;
mod olblockid;

pub use account_state::{DBAccountStateAtEpoch, DBEeAccountState};
pub use batch::{
    DBBatch, DBBatchId, DBBatchStatus, DBBatchWithStatus, DBChunk, DBChunkId, DBChunkStatus,
    DBChunkWithStatus, DBL1DaBlockRef, DBTxidPair,
};
pub use exec_block::DBExecBlockRecord;
#[cfg(feature = "migration")]
pub use exec_block::{package_from_sled_era, package_to_sled_era};
pub use olblockid::DBOLBlockId;

/// Serde for a list of 32-byte hashes as a list of hex strings.
///
/// `hex::serde` covers one array; a `Vec<[u8; 32]>` would otherwise reflect
/// as a list of lists of integers.
pub(crate) mod hex_list {
    use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};

    pub(crate) fn serialize<S: Serializer>(items: &[[u8; 32]], ser: S) -> Result<S::Ok, S::Error> {
        let strings: Vec<String> = items.iter().map(hex::encode).collect();
        strings.serialize(ser)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<[u8; 32]>, D::Error> {
        let strings: Vec<String> = Vec::deserialize(de)?;
        strings
            .into_iter()
            .map(|text| {
                let bytes = hex::decode(&text).map_err(D::Error::custom)?;
                <[u8; 32]>::try_from(bytes).map_err(|bytes| {
                    D::Error::custom(format!("expected 32 bytes, got {}", bytes.len()))
                })
            })
            .collect()
    }
}
