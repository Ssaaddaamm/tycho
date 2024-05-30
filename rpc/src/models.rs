use everscale_types::cell::HashBytes;
use serde::{Deserialize, Serialize};
use tycho_util::serde_helpers;

// NOTE: All fields must be serialized in `camelCase`.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenTimings {
    #[serde(with = "serde_helpers::string")]
    pub gen_lt: u64,
    pub gen_utime: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastTransactionId {
    pub lt: u64,
    pub hash: HashBytes,
}

impl Serialize for LastTransactionId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct LastTransactionId<'a> {
            is_exact: bool,
            lt: u64,
            hash: &'a HashBytes,
        }

        LastTransactionId {
            is_exact: true,
            lt: self.lt,
            hash: &self.hash,
        }
        .serialize(serializer)
    }
}

// TODO: Add `#[serde(rename_all = "camelCase")]` to this struct when all
// compatibility issues are resolved (it was ommited for this struct by mistake).
#[derive(Default, Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct StateTimings {
    pub last_mc_block_seqno: u32,
    pub last_shard_client_mc_block_seqno: u32,
    pub last_mc_utime: u32,
    pub mc_time_diff: i64,
    pub shard_client_time_diff: i64,
    pub smallest_known_lt: Option<u64>,
}
