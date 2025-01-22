use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use everscale_types::cell::HashBytes;
use everscale_types::models::{MsgsExecutionParams, ShardIdent};
use tycho_block_util::queue::{QueueKey, QueuePartitionIdx};
use tycho_util::FastHashSet;

use self::externals_reader::*;
use self::internals_reader::*;
use self::new_messages::*;
pub(super) use self::reader_state::*;
use super::messages_buffer::{DisplayMessageGroup, MessageGroup, MessagesBufferLimits};
use super::types::{AnchorsCache, MsgsExecutionParamsExtension};
use crate::collator::messages_buffer::DebugMessageGroup;
use crate::internal_queue::queue::ShortQueueDiff;
use crate::internal_queue::types::{
    EnqueuedMessage, PartitionRouter, QueueDiffWithMessages, QueueStatistics,
};
use crate::queue_adapter::MessageQueueAdapter;
use crate::tracing_targets;
use crate::types::processed_upto::{BlockSeqno, Lt};
use crate::types::{DebugIter, IntAdrExt, ProcessedTo};

mod externals_reader;
mod internals_reader;
mod new_messages;
mod reader_state;

pub(super) struct FinalizedMessagesReader {
    pub has_unprocessed_messages: bool,
    pub reader_state: ReaderState,
    pub anchors_cache: AnchorsCache,
    pub queue_diff_with_msgs: QueueDiffWithMessages<EnqueuedMessage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GetNextMessageGroupMode {
    Continue,
    Refill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessagesReaderStage {
    FinishPreviousExternals,
    ExistingAndExternals,
    FinishCurrentExternals,
    ExternalsAndNew,
}

pub(super) struct MessagesReader {
    for_shard_id: ShardIdent,

    msgs_exec_params: Arc<MsgsExecutionParams>,

    metrics: MessagesReaderMetrics,

    new_messages: NewMessagesState<EnqueuedMessage>,

    externals_reader: ExternalsReader,
    internals_partition_readers: BTreeMap<QueuePartitionIdx, InternalsParitionReader>,

    readers_stages: BTreeMap<QueuePartitionIdx, MessagesReaderStage>,
}

#[derive(Default)]
pub(super) struct MessagesReaderContext {
    pub for_shard_id: ShardIdent,
    pub block_seqno: BlockSeqno,
    pub next_chain_time: u64,
    pub msgs_exec_params: Arc<MsgsExecutionParams>,
    pub mc_state_gen_lt: Lt,
    pub prev_state_gen_lt: Lt,
    pub mc_top_shards_end_lts: Vec<(ShardIdent, Lt)>,
    pub reader_state: ReaderState,
    pub anchors_cache: AnchorsCache,
}

impl MessagesReader {
    pub fn new(
        cx: MessagesReaderContext,
        mq_adapter: Arc<dyn MessageQueueAdapter<EnqueuedMessage>>,
    ) -> Result<Self> {
        metrics::gauge!("tycho_do_collate_msgs_exec_params_buffer_limit")
            .set(cx.msgs_exec_params.buffer_limit as f64);
        metrics::gauge!("tycho_do_collate_msgs_exec_params_group_limit")
            .set(cx.msgs_exec_params.group_limit as f64);
        metrics::gauge!("tycho_do_collate_msgs_exec_params_group_vert_size")
            .set(cx.msgs_exec_params.group_vert_size as f64);

        // group limits by msgs kinds
        let msgs_buffer_max_count = cx.msgs_exec_params.buffer_limit as usize;
        let group_vert_size = (cx.msgs_exec_params.group_vert_size as usize).max(1);
        let group_limit = cx.msgs_exec_params.group_limit as usize;

        let mut internals_buffer_limits_by_partitions =
            BTreeMap::<QueuePartitionIdx, MessagesBufferLimits>::new();
        let mut externals_buffer_limits_by_partitions =
            BTreeMap::<QueuePartitionIdx, MessagesBufferLimits>::new();

        // TODO: msgs-v3: should create partitions 1+ only when exist in current processed_upto

        let slots_fractions = cx.msgs_exec_params.group_slots_fractions()?;

        // internals: normal partition 0: 80% of `group_limit`, but min 1
        let par_0_slots_fraction = slots_fractions.get(&0).cloned().unwrap() as usize;
        internals_buffer_limits_by_partitions.insert(0, MessagesBufferLimits {
            max_count: msgs_buffer_max_count,
            slots_count: group_limit
                .saturating_mul(par_0_slots_fraction)
                .saturating_div(100)
                .max(1),
            slot_vert_size: group_vert_size,
        });
        // externals: normal partition 0: 100%, but min 2, vert size +1
        externals_buffer_limits_by_partitions.insert(0, MessagesBufferLimits {
            max_count: msgs_buffer_max_count,
            slots_count: group_limit.saturating_mul(100).saturating_div(100).max(2),
            slot_vert_size: group_vert_size + 1,
        });

        // internals: low-priority partition 1: 10%, but min 1
        let par_1_slots_fraction = slots_fractions.get(&1).cloned().unwrap() as usize;
        internals_buffer_limits_by_partitions.insert(1, MessagesBufferLimits {
            max_count: msgs_buffer_max_count,
            slots_count: group_limit
                .saturating_mul(par_1_slots_fraction)
                .saturating_div(100)
                .max(1),
            slot_vert_size: group_vert_size,
        });
        // externals: low-priority partition 1: equal to internals, vert size +1
        {
            let int_buffer_limits = internals_buffer_limits_by_partitions.get(&1).unwrap();
            externals_buffer_limits_by_partitions.insert(1, MessagesBufferLimits {
                max_count: msgs_buffer_max_count,
                slots_count: int_buffer_limits.slots_count,
                slot_vert_size: int_buffer_limits.slot_vert_size + 1,
            });
        }

        // TODO: msgs-v3: remove if we do not need this field
        let _msg_group_max_limits = MessagesBufferLimits {
            max_count: msgs_buffer_max_count,
            slots_count: externals_buffer_limits_by_partitions
                .values()
                .map(|l| l.slots_count)
                .sum(),
            slot_vert_size: group_vert_size,
        };

        // create externals reader
        let externals_reader = ExternalsReader::new(
            cx.for_shard_id,
            cx.block_seqno,
            cx.next_chain_time,
            cx.msgs_exec_params.clone(),
            externals_buffer_limits_by_partitions.clone(),
            cx.anchors_cache,
            cx.reader_state.externals,
        );

        let mut res = Self {
            for_shard_id: cx.for_shard_id,

            msgs_exec_params: cx.msgs_exec_params.clone(),

            metrics: Default::default(),

            new_messages: NewMessagesState::new(cx.for_shard_id),

            externals_reader,
            internals_partition_readers: Default::default(),

            readers_stages: Default::default(),
        };

        // define the initial reader stage
        let all_read_externals_collected = res.externals_reader.all_ranges_read_and_collected();
        let initial_reader_stage = match all_read_externals_collected {
            true => MessagesReaderStage::ExistingAndExternals,
            false => MessagesReaderStage::FinishPreviousExternals,
        };

        // create internals readers by partitions
        let mut partition_reader_states = cx.reader_state.internals.partitions;

        // normal partition 0
        let target_limits = internals_buffer_limits_by_partitions.remove(&0).unwrap();
        let max_limits = {
            let ext_limits = externals_buffer_limits_by_partitions.remove(&0).unwrap();
            MessagesBufferLimits {
                max_count: msgs_buffer_max_count,
                slots_count: ext_limits.slots_count,
                slot_vert_size: target_limits.slot_vert_size,
            }
        };
        let par_reader_state = partition_reader_states.remove(&0).unwrap_or_default();
        let par_reader = InternalsParitionReader::new(
            InternalsPartitionReaderContext {
                partition_id: 0,
                for_shard_id: cx.for_shard_id,
                block_seqno: cx.block_seqno,
                target_limits,
                max_limits,
                msgs_exec_params: cx.msgs_exec_params.clone(),
                mc_state_gen_lt: cx.mc_state_gen_lt,
                prev_state_gen_lt: cx.prev_state_gen_lt,
                mc_top_shards_end_lts: cx.mc_top_shards_end_lts.clone(),
                reader_state: par_reader_state,
            },
            mq_adapter.clone(),
        )?;
        res.internals_partition_readers.insert(0, par_reader);
        res.readers_stages.insert(0, initial_reader_stage);

        // low-priority partition 1
        let target_limits = internals_buffer_limits_by_partitions.remove(&1).unwrap();
        let max_limits = {
            let ext_limits = externals_buffer_limits_by_partitions.remove(&1).unwrap();
            MessagesBufferLimits {
                max_count: msgs_buffer_max_count,
                slots_count: ext_limits.slots_count,
                slot_vert_size: target_limits.slot_vert_size,
            }
        };
        let par_reader_state = partition_reader_states.remove(&1).unwrap_or_default();
        let par_reader = InternalsParitionReader::new(
            InternalsPartitionReaderContext {
                partition_id: 1,
                for_shard_id: cx.for_shard_id,
                block_seqno: cx.block_seqno,
                target_limits,
                max_limits,
                msgs_exec_params: cx.msgs_exec_params.clone(),
                mc_state_gen_lt: cx.mc_state_gen_lt,
                prev_state_gen_lt: cx.prev_state_gen_lt,
                mc_top_shards_end_lts: cx.mc_top_shards_end_lts,
                reader_state: par_reader_state,
            },
            mq_adapter,
        )?;
        res.internals_partition_readers.insert(1, par_reader);
        res.readers_stages.insert(1, initial_reader_stage);

        // get full statistics from partition 1 and init partition router in new messages state
        let par_1_all_ranges_msgs_stats = res
            .internals_partition_readers
            .get(&1)
            .unwrap()
            .range_readers()
            .values()
            .filter_map(|r| r.reader_state.msgs_stats.as_ref());
        res.new_messages
            .init_partition_router(1, par_1_all_ranges_msgs_stats);

        tracing::debug!(target: tracing_targets::COLLATOR,
            readers_stages = ?res.readers_stages,
            externals_all_ranges_read_and_collected = res.externals_reader.all_ranges_read_and_collected(),
            internals_all_read_existing_messages_collected = ?DebugIter(res
                .internals_partition_readers
                .iter()
                .map(|(par_id, par)| (par_id, par.all_read_existing_messages_collected()))),
            "messages reader created",
        );

        Ok(res)
    }

    pub fn reset_read_state(&mut self) {
        // reset metrics
        self.metrics = Default::default();

        // define the initial reader stage
        let all_read_externals_collected = self.externals_reader.all_ranges_read_and_collected();
        let initial_reader_stage = match all_read_externals_collected {
            true => MessagesReaderStage::ExistingAndExternals,
            false => MessagesReaderStage::FinishPreviousExternals,
        };

        // reset internals reader stages
        for (_, par_reader_stage) in self.readers_stages.iter_mut() {
            *par_reader_stage = initial_reader_stage;
        }

        tracing::debug!(target: tracing_targets::COLLATOR,
            readers_stages = ?self.readers_stages,
            externals_all_ranges_read_and_collected = self.externals_reader.all_ranges_read_and_collected(),
            internals_all_read_existing_messages_collected = ?DebugIter(self
                .internals_partition_readers
                .iter()
                .map(|(par_id, par)| (par_id, par.all_read_existing_messages_collected()))),
            "messages reader state was reset",
        );
    }

    pub fn check_has_pending_internals_in_iterators(&mut self) -> Result<bool> {
        for (_, par_reader) in self.internals_partition_readers.iter_mut() {
            if par_reader.check_has_pending_internals_in_iterators()? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn drop_internals_next_range_readers(&mut self) {
        for (_, par_reader) in self.internals_partition_readers.iter_mut() {
            par_reader.drop_next_range_reader();
        }
    }

    fn get_min_internals_processed_to_by_shards(&self) -> ProcessedTo {
        let mut min_internals_processed_to = ProcessedTo::default();

        for par_reader in self.internals_partition_readers.values() {
            for (shard_id, key) in &par_reader.reader_state().processed_to {
                min_internals_processed_to
                    .entry(*shard_id)
                    .and_modify(|min_key| *min_key = std::cmp::min(*min_key, *key))
                    .or_insert(*key);
            }
        }

        min_internals_processed_to
    }

    pub fn finalize(
        mut self,
        current_next_lt: u64,
        diffs: Vec<(ShardIdent, ShortQueueDiff)>,
    ) -> Result<FinalizedMessagesReader> {
        let mut has_unprocessed_messages = self.has_messages_in_buffers()
            || self.has_pending_new_messages()
            || self.has_pending_externals_in_cache();

        // aggregated messages stats from all ranges
        // we need it to detect target ratition for new messages from queue diff
        let mut aggregated_stats = QueueStatistics::default();

        // collect internals partition readers states
        let mut internals_reader_state = InternalsReaderState::default();
        for (par_id, par_reader) in self.internals_partition_readers.iter_mut() {
            // collect aggregated messages stats
            for range_reader in par_reader.range_readers().values() {
                if range_reader.fully_read && range_reader.reader_state.buffer.msgs_count() == 0 {
                    continue;
                }
                if let Some(msgs_stats) = &range_reader.reader_state.msgs_stats {
                    aggregated_stats.append(msgs_stats);
                }
            }

            // check pending internals in iterators
            if !has_unprocessed_messages {
                has_unprocessed_messages = par_reader.check_has_pending_internals_in_iterators()?;
            }

            // handle last new messages range reader
            if let Ok((_, last_int_range_reader)) = par_reader.get_last_range_reader() {
                if last_int_range_reader.kind == InternalsRangeReaderKind::NewMessages {
                    // if skip offset in new messages reader and last externals range reader are same
                    // then we can drop processed offset both in internals and externals readers
                    let last_ext_range_reader = self
                        .externals_reader
                        .get_last_range_reader()?
                        .1
                        .reader_state()
                        .get_state_by_partition(*par_id)?;

                    if last_int_range_reader.reader_state.skip_offset
                        == last_ext_range_reader.skip_offset
                    {
                        par_reader.drop_processing_offset(true)?;
                        self.externals_reader
                            .drop_processing_offset(*par_id, true)?;
                    }
                }
            }
        }

        // build queue diff
        let min_internals_processed_to = self.get_min_internals_processed_to_by_shards();
        let mut queue_diff_with_msgs = self
            .new_messages
            .into_queue_diff_with_messages(min_internals_processed_to);

        // get current queue diff messages stats and merge with aggregated stats
        let queue_diff_msgs_stats = (&queue_diff_with_msgs, self.for_shard_id).into();
        aggregated_stats.append_diff_statistics(&queue_diff_msgs_stats);

        // reset queue diff partition router
        // according to actual aggregated stats
        let moved_from_par_0_accounts = Self::reset_partition_router_by_stats(
            &self.msgs_exec_params,
            &mut queue_diff_with_msgs.partition_router,
            aggregated_stats,
            self.for_shard_id,
            diffs,
        )?;

        // remove moved accounts from partition 0 buffer
        let par_reader = self.internals_partition_readers.get_mut(&0).unwrap();
        if let Ok(last_int_range_reader) = par_reader.get_last_range_reader_mut() {
            if last_int_range_reader.kind == InternalsRangeReaderKind::NewMessages {
                last_int_range_reader
                    .reader_state
                    .buffer
                    .remove_messages_by_accounts(&moved_from_par_0_accounts);
            }
        }

        // collect internals reader state
        for (par_id, par_reader) in self.internals_partition_readers {
            internals_reader_state
                .partitions
                .insert(par_id, par_reader.finalize(current_next_lt)?);
        }

        // collect externals reader state
        let FinalizedExternalsReader {
            externals_reader_state,
            anchors_cache,
        } = self.externals_reader.finalize()?;

        let reader_state = ReaderState {
            externals: externals_reader_state,
            internals: internals_reader_state,
        };

        Ok(FinalizedMessagesReader {
            has_unprocessed_messages,
            reader_state,
            anchors_cache,
            queue_diff_with_msgs,
        })
    }

    pub fn reset_partition_router_by_stats(
        msgs_exec_params: &MsgsExecutionParams,
        partition_router: &mut PartitionRouter,
        aggregated_stats: QueueStatistics,
        for_shard_id: ShardIdent,
        top_block_diffs: Vec<(ShardIdent, ShortQueueDiff)>,
    ) -> Result<FastHashSet<HashBytes>> {
        let par_0_msgs_count_limit = msgs_exec_params.par_0_int_msgs_count_limit as u64;
        let mut moved_from_par_0_accounts = FastHashSet::default();

        for (dest_int_address, msgs_count) in aggregated_stats {
            let existing_partition = partition_router.get_partition(None, &dest_int_address);
            if existing_partition != 0 {
                continue;
            }

            if for_shard_id.contains_address(&dest_int_address) {
                tracing::trace!(target: tracing_targets::COLLATOR,
                    "check address {} for partition 0 because it is in current shard",
                    dest_int_address,
                );

                // if we have account for current shard then check if we need to move it to partition 1
                // if we have less than limit then keep it in partition 0
                if msgs_count > par_0_msgs_count_limit {
                    tracing::trace!(target: tracing_targets::COLLATOR,
                        "move address {} to partition 1 because it has {} messages",
                        dest_int_address, msgs_count,
                    );
                    partition_router.insert_dst(&dest_int_address, 1)?;
                    moved_from_par_0_accounts.insert(dest_int_address.get_address());
                }
            } else {
                tracing::trace!(target: tracing_targets::COLLATOR,
                    "reset partition router for address {} because it is not in current shard",
                    dest_int_address,
                );
                // if we have account for another shard then take info from that shard
                let acc_shard_diff_info = top_block_diffs
                    .iter()
                    .find(|(shard_id, _)| shard_id.contains_address(&dest_int_address))
                    .map(|(_, diff)| diff);

                // try to get remote partition from diff
                let total_msgs = match acc_shard_diff_info {
                    // if we do not have diff then use aggregated stats
                    None => {
                        tracing::trace!(target: tracing_targets::COLLATOR,
                            "use aggregated stats for address {} because we do not have diff",
                            dest_int_address,
                        );
                        msgs_count
                    }
                    Some(diff) => {
                        tracing::trace!(target: tracing_targets::COLLATOR,
                            "use diff for address {} because we have diff",
                            dest_int_address,
                        );
                        // getting remote shard partition from diff
                        let remote_shard_partition =
                            diff.router().get_partition(None, &dest_int_address);

                        tracing::trace!(target: tracing_targets::COLLATOR,
                            "remote shard partition for address {} is {}",
                            dest_int_address, remote_shard_partition,
                        );

                        if remote_shard_partition != 0 {
                            tracing::trace!(target: tracing_targets::COLLATOR,
                                "move address {} to partition {} because it has partition {} in diff",
                                dest_int_address, remote_shard_partition, remote_shard_partition,
                            );
                            partition_router
                                .insert_dst(&dest_int_address, remote_shard_partition)?;
                            continue;
                        }

                        // if remote partition == 0 then we need to check statistics
                        let remote_msgs_count = match diff.statistics().partition(0) {
                            None => {
                                tracing::trace!(target: tracing_targets::COLLATOR,
                                    "use aggregated stats for address {} because we do not have partition 0 stats in diff",
                                    dest_int_address,
                                );
                                0
                            }
                            Some(partition) => {
                                tracing::trace!(target: tracing_targets::COLLATOR,
                                    "use partition 0 stats for address {} because we have partition 0 stats in diff",
                                    dest_int_address,
                                );
                                partition.get(&dest_int_address).copied().unwrap_or(0)
                            }
                        };

                        msgs_count + remote_msgs_count
                    }
                };

                tracing::trace!(target: tracing_targets::COLLATOR,
                    "total messages for address {} is {}",
                    dest_int_address, total_msgs,
                );
                if total_msgs > par_0_msgs_count_limit {
                    tracing::trace!(target: tracing_targets::COLLATOR,
                        "move address {} to partition 1 because it has {} messages",
                        dest_int_address, total_msgs,
                    );
                    partition_router.insert_dst(&dest_int_address, 1)?;
                    moved_from_par_0_accounts.insert(dest_int_address.get_address());
                }
            }
        }

        Ok(moved_from_par_0_accounts)
    }

    pub fn last_read_to_anchor_chain_time(&self) -> Option<u64> {
        self.externals_reader.last_read_to_anchor_chain_time()
    }

    pub fn metrics(&self) -> &MessagesReaderMetrics {
        &self.metrics
    }

    pub fn add_new_messages(&mut self, messages: impl IntoIterator<Item = Arc<EnqueuedMessage>>) {
        self.new_messages.add_messages(messages);
    }

    pub fn count_messages_in_buffers(&self) -> usize {
        self.count_internals_in_buffers() + self.count_externals_in_buffers()
    }

    pub fn has_messages_in_buffers(&self) -> bool {
        self.has_internals_in_buffers() || self.has_externals_in_buffers()
    }

    pub fn count_internals_in_buffers(&self) -> usize {
        self.internals_partition_readers
            .values()
            .map(|v| v.count_messages_in_buffers())
            .sum()
    }

    pub fn has_internals_in_buffers(&self) -> bool {
        self.internals_partition_readers
            .values()
            .any(|v| v.has_messages_in_buffers())
    }

    pub fn has_not_fully_read_internals_ranges(&self) -> bool {
        self.internals_partition_readers
            .values()
            .any(|v| !v.all_ranges_fully_read)
    }

    pub fn has_pending_new_messages(&self) -> bool {
        self.new_messages.has_pending_messages()
    }

    pub fn count_externals_in_buffers(&self) -> usize {
        self.externals_reader.count_messages_in_buffers()
    }

    pub fn has_externals_in_buffers(&self) -> bool {
        self.externals_reader.has_messages_in_buffers()
    }

    pub fn has_not_fully_read_externals_ranges(&self) -> bool {
        self.externals_reader.has_not_fully_read_ranges()
    }

    pub fn can_read_and_collect_more_messages(&self) -> bool {
        self.has_not_fully_read_externals_ranges()
            || self.has_not_fully_read_internals_ranges()
            || self.has_pending_new_messages()
            || self.has_messages_in_buffers()
    }

    pub fn has_pending_externals_in_cache(&self) -> bool {
        self.externals_reader.has_pending_externals()
    }

    pub fn check_has_non_zero_processed_offset(&self) -> bool {
        let check_internals = self
            .internals_partition_readers
            .values()
            .any(|par_reader| par_reader.has_non_zero_processed_offset());
        if check_internals {
            return check_internals;
        }

        // NOTE: in current implementation processed_offset syncronized in internals and externals readers
        self.externals_reader.has_non_zero_processed_offset()
    }

    pub fn check_need_refill(&self) -> bool {
        if self.has_messages_in_buffers() {
            return false;
        }

        // check if hash non zero processed offset
        self.check_has_non_zero_processed_offset()
    }

    pub fn refill_buffers_upto_offsets(&mut self) -> Result<()> {
        tracing::debug!(target: tracing_targets::COLLATOR,
            internals_processed_offsets = ?DebugIter(self.internals_partition_readers
                .iter()
                .map(|(par_id, par_r)| {
                    (
                        par_id,
                        par_r.get_last_range_reader()
                            .map(|(_, r)| r.reader_state.processed_offset)
                            .unwrap_or_default(),
                    )
                })),
            externals_processed_offset = ?self.externals_reader.get_last_range_reader_offsets_by_partitions(),
            "start: refill messages buffer and skip groups upto",
        );

        loop {
            let msg_group = self.get_next_message_group(
                GetNextMessageGroupMode::Refill,
                0, // can pass 0 because new messages reader was not initialized in this case
            )?;
            if msg_group.is_none() {
                // on restart from a new genesis we will not be able to refill buffer with externals
                // so we stop refilling when there is no more groups in buffer
                break;
            }
        }

        // next time we should read next message group like we did not make refill before
        // so we need to reset flags and states that control the read flow
        self.reset_read_state();

        tracing::debug!(target: tracing_targets::COLLATOR,
            "finished: refill messages buffer and skip groups upto",
        );

        Ok(())
    }

    #[tracing::instrument(skip_all)]
    pub fn get_next_message_group(
        &mut self,
        read_mode: GetNextMessageGroupMode,
        current_next_lt: u64,
    ) -> Result<Option<MessageGroup>> {
        // we collect separate messages groups by partitions them merge them into one
        let mut msg_groups = BTreeMap::<QueuePartitionIdx, MessageGroup>::new();

        // TODO: msgs-v3: try to read all in parallel

        // collect separate metrics by partitions
        let mut metrics_by_partitions = BTreeMap::<QueuePartitionIdx, MessagesReaderMetrics>::new();

        // count how many times prev processed offset reached in readers
        let mut prev_processed_offset_reached_count = 0;

        // check if we have FinishExternals stage in any partition
        let mut has_finish_externals_stage = false;

        //--------------------
        // read internals
        for (par_id, par_reader_stage) in self.readers_stages.iter_mut() {
            let par_reader = self
                .internals_partition_readers
                .get_mut(par_id)
                .context("reader for partition should exist")?;

            // check if we have FinishExternals stage in any partition
            if matches!(
                par_reader_stage,
                MessagesReaderStage::FinishPreviousExternals
                    | MessagesReaderStage::FinishCurrentExternals
            ) {
                has_finish_externals_stage = true;
            }

            // on refill read only until the last range processed offset reached
            if read_mode == GetNextMessageGroupMode::Refill
                && par_reader.last_range_offset_reached()
            {
                prev_processed_offset_reached_count += 1;
                continue;
            }

            // collect separate metrics by partitions
            let metrics_of_partition = metrics_by_partitions.entry(*par_id).or_default();

            match par_reader_stage {
                MessagesReaderStage::ExistingAndExternals => {
                    let metrics = par_reader.read_existing_messages_into_buffers(read_mode)?;
                    metrics_of_partition.append(metrics);
                }
                MessagesReaderStage::FinishPreviousExternals
                | MessagesReaderStage::FinishCurrentExternals => {
                    // do not read internals when finishing to collect externals
                }
                MessagesReaderStage::ExternalsAndNew => {
                    let read_new_messages_res = par_reader
                        .read_new_messages_into_buffers(&mut self.new_messages, current_next_lt)?;

                    metrics_of_partition.append(read_new_messages_res.metrics);
                }
            }
        }

        //--------------------
        // read externals
        'read_externals: {
            // do not read more externals on FinishExternals stage in any partition
            if has_finish_externals_stage {
                break 'read_externals;
            }

            // on refill read only until the last range processed offset reached
            if read_mode == GetNextMessageGroupMode::Refill
                && prev_processed_offset_reached_count == self.internals_partition_readers.len()
            {
                break 'read_externals;
            }

            let metrics = self
                .externals_reader
                .read_into_buffers(read_mode, self.new_messages.partition_router());
            tracing::debug!(target: tracing_targets::COLLATOR,
                "external messages read: ext={}",
                metrics.read_ext_msgs_count,
            );
            self.metrics.append(metrics);
        }

        let labels = [("workchain", self.for_shard_id.workchain().to_string())];
        metrics::gauge!("tycho_do_collate_msgs_exec_buffer_messages_count", &labels)
            .set(self.count_messages_in_buffers() as f64);

        //----------
        // collect messages after reading
        let mut partitions_readers = BTreeMap::new();

        for (par_id, par_reader_stage) in self.readers_stages.iter_mut() {
            // extract partition reader from state to use partition 0 buffer
            // to check for account skip on collecting messages from partition 1
            let mut par_reader = self
                .internals_partition_readers
                .remove(par_id)
                .context("reader for partition should exist")?;

            // on refill collect only until the last range processed offset reached
            if read_mode == GetNextMessageGroupMode::Refill
                && par_reader.last_range_offset_reached()
            {
                partitions_readers.insert(*par_id, par_reader);
                continue;
            }

            // collect separate metrics by partitions
            let metrics_of_partition = metrics_by_partitions.entry(*par_id).or_default();

            // collect existing internals, externals and new internals
            let has_pending_new_messages_for_partition = self
                .new_messages
                .has_pending_messages_from_partition(*par_id);
            let CollectMessageForPartitionResult {
                metrics,
                msg_group,
                collected_queue_msgs_keys,
            } = Self::collect_messages_for_partition(
                par_reader_stage,
                &mut par_reader,
                &mut self.externals_reader,
                has_pending_new_messages_for_partition,
                &partitions_readers,
                &msg_groups,
            )?;

            msg_groups.insert(*par_id, msg_group);
            metrics_of_partition.append(metrics);

            // remove collected new messages
            self.new_messages
                .remove_collected_messages(&collected_queue_msgs_keys);

            partitions_readers.insert(*par_id, par_reader);
        }
        // return partition readers to state
        self.internals_partition_readers = partitions_readers;

        // aggregate metrics from partitions
        for (par_id, metrics) in metrics_by_partitions {
            tracing::debug!(target: tracing_targets::COLLATOR,
                "messages read from partition {}: existing={}, ext={}, new={}",
                par_id,
                metrics.read_int_msgs_from_iterator_count,
                metrics.read_ext_msgs_count,
                metrics.read_new_msgs_count,
            );
            self.metrics.append(metrics);
        }

        tracing::debug!(target: tracing_targets::COLLATOR,
            int_curr_processed_offset = ?DebugIter(self
                .internals_partition_readers.iter()
                .map(|(par_id, par)| (par_id, par.reader_state().curr_processed_offset))),
            ext_curr_processed_offset = ?DebugIter(self
                .externals_reader.reader_state()
                .by_partitions.iter()
                .map(|(par_id, par)| (par_id, par.curr_processed_offset))),
            int_msgs_count_in_buffers = ?DebugIter(self
                .internals_partition_readers.iter()
                .map(|(par_id, par)| (par_id, par.count_messages_in_buffers()))),
            ext_msgs_count_in_buffers = ?self.externals_reader.count_messages_in_buffers_by_partitions(),
            "collected message groups by partitions: {:?}",
            DebugIter(msg_groups.iter().map(|(par_id, g)| (*par_id, DisplayMessageGroup(g)))),
        );

        // aggregate message group
        self.metrics.add_to_message_groups_timer.start();
        let msg_group = msg_groups
            .into_iter()
            .fold(MessageGroup::default(), |acc, (_, next)| acc.add(next));
        self.metrics.add_to_message_groups_timer.stop();

        // check if prev processed offset reached
        // in all internals partition readers
        let all_prev_processed_offset_reached =
            prev_processed_offset_reached_count == self.internals_partition_readers.len();

        tracing::debug!(target: tracing_targets::COLLATOR,
            has_pending_new_messages = self.has_pending_new_messages(),
            has_pending_externals_in_cache = self.has_pending_externals_in_cache(),
            has_not_fully_read_internals_ranges = self.has_not_fully_read_internals_ranges(),
            ?read_mode,
            all_prev_processed_offset_reached,
            add_to_message_groups_total_elapsed_ms = self.metrics.add_to_message_groups_timer.total_elapsed.as_millis(),
            "aggregated collected message group: {:?}",
            DebugMessageGroup(&msg_group),
        );

        // retun None when messages group is empty
        if msg_group.len() == 0
            // and we reached previous processed offset on refill
            && ((read_mode == GetNextMessageGroupMode::Refill && all_prev_processed_offset_reached)
                // or we do not have messages in buffers and no pending new messages and all ranges fully read
                // so we cannot read more messages into buffers and then collect them
                || !self.can_read_and_collect_more_messages()
            )
        {
            Ok(None)
        } else {
            Ok(Some(msg_group))
        }
    }

    fn collect_messages_for_partition(
        par_reader_stage: &mut MessagesReaderStage,
        par_reader: &mut InternalsParitionReader,
        externals_reader: &mut ExternalsReader,
        has_pending_new_messages_for_partition: bool,
        prev_partitions_readers: &BTreeMap<QueuePartitionIdx, InternalsParitionReader>,
        prev_msg_groups: &BTreeMap<QueuePartitionIdx, MessageGroup>,
    ) -> Result<CollectMessageForPartitionResult> {
        let mut res = CollectMessageForPartitionResult::default();

        // update processed offset anyway
        par_reader.increment_curr_processed_offset();
        externals_reader.increment_curr_processed_offset(&par_reader.partition_id)?;

        // remember if all internals or externals were collected before to reduce spam in logs further
        let mut all_internals_collected_before = false;
        let all_read_externals_collected_before;

        // collect existing internals
        if *par_reader_stage == MessagesReaderStage::ExistingAndExternals {
            all_internals_collected_before = par_reader.all_read_existing_messages_collected();

            let CollectInternalsResult { metrics, .. } = par_reader.collect_messages(
                par_reader_stage,
                &mut res.msg_group,
                prev_partitions_readers,
                prev_msg_groups,
            )?;

            res.metrics.append(metrics);
        }

        // collect externals
        {
            all_read_externals_collected_before = !externals_reader.has_messages_in_buffers();

            let CollectExternalsResult { metrics } = externals_reader.collect_messages(
                par_reader.partition_id,
                &mut res.msg_group,
                prev_partitions_readers,
                prev_msg_groups,
            )?;
            res.metrics.append(metrics);
        }

        // collect new internals
        if *par_reader_stage == MessagesReaderStage::ExternalsAndNew {
            all_internals_collected_before =
                par_reader.all_new_messages_collected(has_pending_new_messages_for_partition);

            let CollectInternalsResult {
                metrics,
                mut collected_queue_msgs_keys,
            } = par_reader.collect_messages(
                par_reader_stage,
                &mut res.msg_group,
                prev_partitions_readers,
                prev_msg_groups,
            )?;
            res.metrics.append(metrics);
            res.collected_queue_msgs_keys
                .append(&mut collected_queue_msgs_keys);

            // set skip offset to current offset
            // because we will not save collected new messages to the queue
            par_reader.set_skip_offset_to_current()?;
        }

        // switch to the next reader stage if required

        // if all read externals collected
        let all_read_externals_collected = !externals_reader.has_messages_in_buffers();
        if all_read_externals_collected {
            // finalize externals read state
            {
                // drop all ranges except the last one
                externals_reader.retain_only_last_range_reader()?;
                // update reader state for each partitions
                let par_ids = externals_reader.get_partition_ids();
                for par_id in par_ids {
                    // mark all read messages processed
                    externals_reader.set_processed_to_current_position(par_id)?;
                    // set skip offset to current offset
                    externals_reader.set_skip_offset_to_current(par_id)?;
                }
                // we can move "from" boundary to current position
                // because all messages up to current position processed
                externals_reader.set_from_to_current_position_in_last_range_reader()?;
                // drop last read to anchor chain time when no pending externals in cache
                if !externals_reader.has_pending_externals() {
                    externals_reader.drop_last_read_to_anchor_chain_time();
                }
            }

            // log only first time
            if !all_read_externals_collected_before {
                tracing::debug!(target: tracing_targets::COLLATOR,
                    has_pending_externals = externals_reader.has_pending_externals(),
                    ext_reader_states = ?externals_reader.reader_state().by_partitions,
                    last_range_reader_state = ?externals_reader.get_last_range_reader().map(|(seqno, r)| (seqno, DebugExternalsRangeReaderState(r.reader_state()))),
                    "all read externals collected",
                );
            }
        }

        let partition_id = par_reader.partition_id;
        let update_reader_stage = |curr: &mut MessagesReaderStage, new| {
            let old = *curr;
            *curr = new;
            tracing::debug!(target: tracing_targets::COLLATOR,
                partition_id,
                ?old,
                ?new,
                "messages partition reader stage updated",
            );
        };

        // if all read externals collected from the previous block collation
        // then we can switch to the "read existing internals stage"
        if all_read_externals_collected
            && *par_reader_stage == MessagesReaderStage::FinishPreviousExternals
        {
            // switch to the "read existing internals stage" stage
            update_reader_stage(par_reader_stage, MessagesReaderStage::ExistingAndExternals);
        }

        // if all existing internals collected
        // then we should collect all already read externals without reading more from cache
        // and only after that we can finalize existing internals read state
        if *par_reader_stage == MessagesReaderStage::ExistingAndExternals
            && par_reader.all_read_existing_messages_collected()
        {
            // log only first time
            if !all_internals_collected_before {
                tracing::debug!(target: tracing_targets::COLLATOR,
                    partition_id = par_reader.partition_id,
                    int_processed_to = ?par_reader.reader_state().processed_to,
                    int_curr_processed_offset = par_reader.reader_state().curr_processed_offset,
                    last_range_reader_state = ?par_reader.get_last_range_reader().map(|(seqno, r)| (seqno, DebugInternalsRangeReaderState(&r.reader_state))),
                    "all read existing internals collected from partition",
                );
            }

            // switch to the "collect only already read externals" stage
            update_reader_stage(
                par_reader_stage,
                MessagesReaderStage::FinishCurrentExternals,
            );
        }

        // if all read externals collected from current block collation
        // then we can finalize existing internals read state
        // and switch to the "new messages processing" stage
        if all_read_externals_collected
            && *par_reader_stage == MessagesReaderStage::FinishCurrentExternals
        {
            // finalize existing intenals read state
            // drop all ranges except the last one
            par_reader.retain_only_last_range_reader()?;
            // mark all read messages processed
            par_reader.set_processed_to_current_position()?;
            // drop processing offset for existing internals
            par_reader.drop_processing_offset(true)?;
            // and drop processing offset for externals
            externals_reader.drop_processing_offset(par_reader.partition_id, true)?;

            // switch to the "new messages processing" stage
            // if all existing messages read (last range reader was created in current block)
            let (last_seqno, _) = par_reader.get_last_range_reader()?;
            if last_seqno == &par_reader.block_seqno {
                update_reader_stage(par_reader_stage, MessagesReaderStage::ExternalsAndNew);
            } else {
                // otherwise return to the reading of existing messages
                update_reader_stage(par_reader_stage, MessagesReaderStage::ExistingAndExternals);
            }
        }

        // if all new messages collected
        // finalize new messages read state
        if *par_reader_stage == MessagesReaderStage::ExternalsAndNew
            && par_reader.all_new_messages_collected(has_pending_new_messages_for_partition)
        {
            // mark all read messages processed
            par_reader.set_processed_to_current_position()?;

            // if all read externals collected
            // drop processed offset both for externals and new message
            if all_read_externals_collected {
                par_reader.drop_processing_offset(true)?;
                externals_reader.drop_processing_offset(par_reader.partition_id, true)?;
            }

            // log only first time
            if !all_internals_collected_before {
                tracing::debug!(target: tracing_targets::COLLATOR,
                    partition_id = par_reader.partition_id,
                    int_processed_to = ?par_reader.reader_state().processed_to,
                    int_curr_processed_offset = par_reader.reader_state().curr_processed_offset,
                    last_range_reader_state = ?par_reader.get_last_range_reader().map(|(seqno, r)| (seqno, DebugInternalsRangeReaderState(&r.reader_state))),
                    "all new internals collected from partition",
                );
            }
        }

        Ok(res)
    }
}

#[derive(Default)]
struct CollectMessageForPartitionResult {
    metrics: MessagesReaderMetrics,
    msg_group: MessageGroup,
    collected_queue_msgs_keys: Vec<QueueKey>,
}

#[derive(Default)]
pub struct MetricsTimer {
    timer: Option<std::time::Instant>,
    pub total_elapsed: Duration,
}
impl std::fmt::Debug for MetricsTimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.total_elapsed)
    }
}
impl MetricsTimer {
    pub fn start(&mut self) {
        self.timer = Some(std::time::Instant::now());
    }
    pub fn stop(&mut self) -> Duration {
        match self.timer.take() {
            Some(timer) => {
                let elapsed = timer.elapsed();
                self.total_elapsed += elapsed;
                elapsed
            }
            None => Duration::default(),
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct MessagesReaderMetrics {
    /// sum total time of initializations of internal messages iterators
    pub init_iterator_timer: MetricsTimer,

    /// sum total time of reading existing internal messages
    pub read_existing_messages_timer: MetricsTimer,
    /// sum total time of reading new internal messages
    pub read_new_messages_timer: MetricsTimer,
    /// sum total time of reading external messages
    pub read_ext_messages_timer: MetricsTimer,
    /// sum total time of adding messages to buffers
    pub add_to_message_groups_timer: MetricsTimer,

    /// num of existing internal messages read
    pub read_int_msgs_from_iterator_count: u64,
    /// num of external messages read
    pub read_ext_msgs_count: u64,
    /// num of new internal messages read
    pub read_new_msgs_count: u64,

    pub add_to_msgs_groups_ops_count: u64,
}

impl MessagesReaderMetrics {
    fn append(&mut self, other: Self) {
        self.init_iterator_timer.total_elapsed += other.init_iterator_timer.total_elapsed;

        self.read_existing_messages_timer.total_elapsed +=
            other.read_existing_messages_timer.total_elapsed;
        self.read_new_messages_timer.total_elapsed += other.read_new_messages_timer.total_elapsed;
        self.read_ext_messages_timer.total_elapsed += other.read_ext_messages_timer.total_elapsed;
        self.add_to_message_groups_timer.total_elapsed +=
            other.add_to_message_groups_timer.total_elapsed;

        self.read_int_msgs_from_iterator_count += other.read_int_msgs_from_iterator_count;
        self.read_ext_msgs_count += other.read_ext_msgs_count;
        self.read_new_msgs_count += other.read_new_msgs_count;

        self.add_to_msgs_groups_ops_count = self
            .add_to_msgs_groups_ops_count
            .saturating_add(other.add_to_msgs_groups_ops_count);
    }
}
