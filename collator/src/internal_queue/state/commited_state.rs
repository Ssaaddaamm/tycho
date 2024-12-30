use ahash::HashMapExt;
use anyhow::Result;
use everscale_types::models::{IntAddr, ShardIdent};
use tycho_block_util::queue::{QueueKey, QueuePartition};
use tycho_storage::model::StatKey;
use tycho_storage::Storage;
use tycho_util::FastHashMap;
use weedb::OwnedSnapshot;

use crate::internal_queue::state::state_iterator::{
    ShardIteratorWithRange, StateIterator, StateIteratorImpl,
};
use crate::internal_queue::types::{InternalMessageValue, QueueRange, QueueShardRange};
use crate::types::processed_upto::PartitionId;
// CONFIG

pub struct CommittedStateConfig {
    pub storage: Storage,
}

// FACTORY

impl<F, R, V> CommittedStateFactory<V> for F
where
    F: Fn() -> R,
    R: CommittedState<V>,
    V: InternalMessageValue,
{
    type CommittedState = R;

    fn create(&self) -> Self::CommittedState {
        self()
    }
}

pub struct CommittedStateImplFactory {
    pub storage: Storage,
}

impl CommittedStateImplFactory {
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }
}

impl<V: InternalMessageValue> CommittedStateFactory<V> for CommittedStateImplFactory {
    type CommittedState = CommittedStateStdImpl;

    fn create(&self) -> Self::CommittedState {
        CommittedStateStdImpl::new(self.storage.clone())
    }
}

pub trait CommittedStateFactory<V: InternalMessageValue> {
    type CommittedState: CommittedState<V>;

    fn create(&self) -> Self::CommittedState;
}

// TRAIT

pub trait CommittedState<V: InternalMessageValue>: Send + Sync {
    fn snapshot(&self) -> OwnedSnapshot;

    fn iterator(
        &self,
        snapshot: &OwnedSnapshot,
        receiver: ShardIdent,
        partition: QueuePartition,
        ranges: Vec<QueueShardRange>,
    ) -> Result<Box<dyn StateIterator<V>>>;

    fn delete(&self, partitions: &[QueuePartition], ranges: &[QueueShardRange]) -> Result<()>;
    fn load_statistics(
        &self,
        result: &mut FastHashMap<IntAddr, u64>,
        snapshot: &OwnedSnapshot,
        partition: QueuePartition,
        range: &Vec<QueueShardRange>,
    ) -> Result<()>;
}

// IMPLEMENTATION

pub struct CommittedStateStdImpl {
    storage: Storage,
}

impl CommittedStateStdImpl {
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }
}

impl<V: InternalMessageValue> CommittedState<V> for CommittedStateStdImpl {
    fn snapshot(&self) -> OwnedSnapshot {
        self.storage.internal_queue_storage().snapshot()
    }

    fn iterator(
        &self,
        snapshot: &OwnedSnapshot,
        receiver: ShardIdent,
        partition: QueuePartition,
        ranges: Vec<QueueShardRange>,
    ) -> Result<Box<dyn StateIterator<V>>> {
        let mut shard_iters_with_ranges = Vec::new();

        for range in ranges {
            let iter = self
                .storage
                .internal_queue_storage()
                .build_iterator_committed(snapshot);

            shard_iters_with_ranges.push((iter, range));
        }

        let iterator = StateIteratorImpl::new(partition, shard_iters_with_ranges, receiver)?;
        Ok(Box::new(iterator))
    }

    fn delete(&self, partitions: &[QueuePartition], ranges: &[QueueShardRange]) -> Result<()> {
        let mut queue_ranges = vec![];
        for partition in partitions {
            for range in ranges {
                queue_ranges.push(tycho_storage::model::QueueRange {
                    partition: *partition,
                    shard_ident: range.shard_ident,
                    from: range.from,
                    to: range.to,
                });
            }
        }
        self.storage.internal_queue_storage().delete(queue_ranges)
    }

    fn load_statistics(
        &self,
        result: &mut FastHashMap<IntAddr, u64>,
        snapshot: &OwnedSnapshot,
        partition: QueuePartition,
        ranges: &Vec<QueueShardRange>,
    ) -> Result<()> {
        for range in ranges {
            self.storage
                .internal_queue_storage()
                .collect_commited_stats_in_range(
                    &snapshot,
                    range.shard_ident,
                    partition,
                    range.from,
                    range.to,
                    result,
                )?;
        }

        Ok(())
    }
}
