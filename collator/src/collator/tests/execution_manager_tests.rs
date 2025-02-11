use std::marker::PhantomData;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use everscale_types::cell::{CellBuilder, HashBytes};
use everscale_types::dict::Dict;
use everscale_types::models::{
    BlockId, BlockIdShort, BlockchainConfig, CurrencyCollection, ExternalsProcessedUpto,
    ShardDescription, ShardIdent, ShardStateUnsplit, ValidatorInfo,
};
use tycho_block_util::queue::{QueueKey, QueuePartition};
use tycho_block_util::state::{MinRefMcStateTracker, ShardStateStuff};
use tycho_util::FastHashMap;

#[cfg(FALSE)]
use super::super::do_collate::tests::{build_stub_collation_data, fill_test_anchors_cache};
use super::super::types::{AnchorsCache, PrevData, WorkingState};
#[cfg(FALSE)]
use super::{
    GetNextMessageGroupContext, GetNextMessageGroupMode, InitIteratorMode, MessagesReader,
};
use crate::internal_queue::iterator::{IterItem, QueueIterator};
use crate::internal_queue::queue::ShortQueueDiff;
use crate::internal_queue::types::{
    DiffStatistics, EnqueuedMessage, InternalMessageValue, QueueDiffWithMessages, QueueFullDiff,
    QueueRange, QueueShardRange, QueueStatistics,
};
use crate::queue_adapter::MessageQueueAdapter;
use crate::test_utils::try_init_test_tracing;

#[derive(Default)]
struct QueueIteratorTestImpl<V: InternalMessageValue> {
    _phantom_data: PhantomData<V>,
}

#[allow(clippy::unimplemented)]
impl<V: InternalMessageValue> QueueIterator<V> for QueueIteratorTestImpl<V> {
    fn next(&mut self, _with_new: bool) -> Result<Option<IterItem<V>>> {
        Ok(None)
    }

    fn next_new(&mut self) -> Result<Option<IterItem<V>>> {
        unimplemented!()
    }

    fn process_new_messages(&mut self) -> Result<Option<IterItem<V>>> {
        unimplemented!()
    }

    fn has_new_messages_for_current_shard(&self) -> bool {
        unimplemented!()
    }

    fn current_position(&self) -> FastHashMap<ShardIdent, QueueKey> {
        unimplemented!()
    }

    fn set_new_messages_from_full_diff(&mut self, _full_diff: QueueFullDiff<V>) {
        unimplemented!()
    }

    fn extract_full_diff(&mut self) -> QueueFullDiff<V> {
        unimplemented!()
    }

    fn take_diff(&self) -> QueueDiffWithMessages<V> {
        unimplemented!()
    }

    fn commit(&mut self, _messages: Vec<(ShardIdent, QueueKey)>) -> Result<()> {
        Ok(())
    }

    fn add_message(&mut self, _message: V) -> Result<()> {
        unimplemented!()
    }
}

#[derive(Default)]
struct MessageQueueAdapterTestImpl<V: InternalMessageValue> {
    _phantom_data: PhantomData<V>,
}

#[async_trait]
#[allow(clippy::unimplemented)]
impl<V: InternalMessageValue + Default> MessageQueueAdapter<V> for MessageQueueAdapterTestImpl<V> {
    fn create_iterator(
        &self,
        _for_shard_id: ShardIdent,
        _partition: QueuePartition,
        _ranges: Vec<QueueShardRange>,
    ) -> Result<Box<dyn QueueIterator<V>>> {
        Ok(Box::new(QueueIteratorTestImpl::default()))
    }

    fn get_statistics(
        &self,
        _partition: QueuePartition,
        _ranges: Vec<QueueShardRange>,
    ) -> Result<QueueStatistics> {
        unimplemented!()
    }

    fn apply_diff(
        &self,
        _diff: QueueDiffWithMessages<V>,
        _block_id_short: BlockIdShort,
        _diff_hash: &HashBytes,
        _statistics: DiffStatistics,
        _max_message: QueueKey,
    ) -> Result<()> {
        unimplemented!()
    }

    fn commit_diff(&self, _mc_top_blocks: Vec<(BlockIdShort, bool)>) -> Result<()> {
        unimplemented!()
    }

    fn add_message_to_iterator(
        &self,
        _iterator: &mut dyn QueueIterator<V>,
        _message: V,
    ) -> Result<()> {
        unimplemented!()
    }

    fn commit_messages_to_iterator(
        &self,
        _iterator: &mut dyn QueueIterator<V>,
        _messages: &[(ShardIdent, QueueKey)],
    ) -> Result<()> {
        unimplemented!()
    }

    fn clear_uncommitted_state(&self) -> Result<()> {
        unimplemented!()
    }

    fn trim_diffs(&self, _source_shard: &ShardIdent, _inclusive_until: &QueueKey) -> Result<()> {
        unimplemented!()
    }

    fn get_diffs(
        &self,
        _blocks: FastHashMap<ShardIdent, u32>,
    ) -> Vec<(ShardIdent, ShortQueueDiff)> {
        unimplemented!()
    }

    fn get_diffs_count_by_shard(&self, _shard_ident: &ShardIdent) -> usize {
        unimplemented!()
    }
}

#[cfg(FALSE)]
const DEFAULT_BLOCK_LIMITS: BlockLimits = BlockLimits {
    bytes: BlockParamLimits {
        underload: 131072,
        soft_limit: 524288,
        hard_limit: 1048576,
    },
    gas: BlockParamLimits {
        underload: 900000,
        soft_limit: 1200000,
        hard_limit: 20_000_000,
    },
    lt_delta: BlockParamLimits {
        underload: 1000,
        soft_limit: 5000,
        hard_limit: 10000,
    },
};

#[cfg(FALSE)]
pub(crate) fn build_stub_collation_data(
    next_block_id: BlockIdShort,
    anchors_cache: &AnchorsCache,
    start_lt: u64,
) -> BlockCollationData {
    BlockCollationDataBuilder::new(
        next_block_id,
        HashBytes::ZERO,
        1,
        anchors_cache
            .last_imported_anchor()
            .map(|a| a.ct)
            .unwrap_or_default(),
        Default::default(),
        HashBytes::ZERO,
        GlobalVersion {
            version: 50,
            capabilities: supported_capabilities(),
        },
        None,
    )
    .build(start_lt, DEFAULT_BLOCK_LIMITS)
}

#[cfg(FALSE)]
fn gen_stub_working_state(
    next_block_id_short: BlockIdShort,
    prev_block_info: (BlockIdShort, (u64, u64)),
    mc_block_info: (BlockIdShort, (u64, u64), (u64, u64)),
    top_shard_block_info: (BlockIdShort, (u64, u64)),
) -> Box<WorkingState> {
    let msgs_buffer = MessagesBuffer::new(next_block_id_short.shard, 3, 2);

    let prev_block_id = BlockId {
        shard: prev_block_info.0.shard,
        seqno: prev_block_info.0.seqno,
        root_hash: HashBytes::default(),
        file_hash: HashBytes::default(),
    };

    let tracker = MinRefMcStateTracker::new();
    let shard_state = ShardStateUnsplit {
        shard_ident: prev_block_id.shard,
        seqno: prev_block_id.seqno,
        gen_lt: prev_block_info.1 .1,
        ..Default::default()
    };
    let shard_state_root = CellBuilder::build_from(&shard_state).unwrap();
    let prev_state_stuff = ShardStateStuff::from_state_and_root(
        &prev_block_id,
        Box::new(shard_state),
        shard_state_root,
        &tracker,
    )
    .unwrap();
    let (prev_shard_data, usage_tree) =
        PrevData::build(vec![prev_state_stuff], vec![HashBytes::default()]).unwrap();

    let shard_descr = ShardDescription {
        seqno: top_shard_block_info.0.seqno,
        reg_mc_seqno: 1,
        start_lt: top_shard_block_info.1 .0,
        end_lt: top_shard_block_info.1 .1,
        root_hash: HashBytes::default(),
        file_hash: HashBytes::default(),
        before_split: false,
        before_merge: false,
        want_split: false,
        nx_cc_updated: false,
        want_merge: false,
        next_catchain_seqno: 1,
        ext_processed_to_anchor_id: 1,
        top_sc_block_updated: true,
        min_ref_mc_seqno: 1,
        gen_utime: 6,
        split_merge_at: None,
        fees_collected: CurrencyCollection::default(),
        funds_created: CurrencyCollection::default(),
        copyleft_rewards: Dict::default(),
        proof_chain: None,
    };
    let shards_info = [(ShardIdent::new_full(0), (&shard_descr).into())];

    let working_state = WorkingState {
        next_block_id_short,
        mc_data: Arc::new(McData {
            global_id: 0,
            block_id: BlockId {
                shard: mc_block_info.0.shard,
                seqno: mc_block_info.0.seqno,
                root_hash: HashBytes::default(),
                file_hash: HashBytes::default(),
            },
            prev_key_block_seqno: 0,
            gen_lt: mc_block_info.1 .1,
            gen_chain_time: 6944,
            libraries: Dict::default(),
            total_validator_fees: CurrencyCollection::default(),
            global_balance: CurrencyCollection::default(),
            shards: shards_info.into_iter().collect(),
            config: BlockchainConfig::new_empty(HashBytes([0x55; 32])),
            validator_info: ValidatorInfo {
                validator_list_hash_short: 0,
                catchain_seqno: 1,
                nx_cc_updated: false,
            },
            consensus_info: Default::default(),
            processed_upto: ProcessedUptoInfoStuff {
                internals: [
                    (ShardIdent::new_full(-1), InternalsProcessedUptoStuff {
                        processed_to_msg: (mc_block_info.2 .1, HashBytes::default()).into(),
                        read_to_msg: (mc_block_info.2 .1, HashBytes::default()).into(),
                    }),
                    (ShardIdent::new_full(0), InternalsProcessedUptoStuff {
                        processed_to_msg: (top_shard_block_info.1 .1, HashBytes::default()).into(),
                        read_to_msg: (top_shard_block_info.1 .1, HashBytes::default()).into(),
                    }),
                ]
                .into(),
                externals: Some(ExternalsProcessedUpto {
                    processed_to: (4, 4),
                    read_to: (4, 4),
                }),
                processed_offset: 0,
            },
            top_processed_to_anchor: 0,
            ref_mc_state_handle: prev_shard_data.ref_mc_state_handle().clone(),
            shards_processed_to: Default::default(),
        }),
        collation_config: Arc::new(Default::default()),
        wu_used_from_last_anchor: 0,
        prev_shard_data: Some(prev_shard_data),
        usage_tree: Some(usage_tree),
        has_unprocessed_messages: Some(true),
        msgs_buffer,
    };

    Box::new(working_state)
}

#[cfg(FALSE)]
#[test]
fn test_refill_msgs_buffer_with_only_externals() {
    try_init_test_tracing(tracing_subscriber::filter::LevelFilter::TRACE);

    let mc_shard_id = ShardIdent::new_full(-1);
    let shard_id = ShardIdent::new_full(0);

    let mut anchors_cache = AnchorsCache::default();
    fill_test_anchors_cache(&mut anchors_cache, shard_id);

    let mc_block_info = (
        BlockIdShort {
            shard: shard_id,
            seqno: 1,
        },
        (3001, 3020),
        (0, 0),
    );
    let top_shard_block_info = (
        BlockIdShort {
            shard: shard_id,
            seqno: 2,
        },
        (2001, 2041),
    );

    // let prev_block_info = top_shard_block_info;
    // let prev_block_upto = ((0, 0), (1001, 1052));
    // let next_block_info = (
    //     BlockIdShort {
    //         shard: shard_id,
    //         seqno: 3,
    //     },
    //     (4001, 4073),
    // );

    let prev_block_info = (
        BlockIdShort {
            shard: shard_id,
            seqno: 3,
        },
        (4001, 4073),
    );
    let prev_block_upto = ((3001, 3020), (2001, 2041));
    let next_block_info = (
        BlockIdShort {
            shard: shard_id,
            seqno: 4,
        },
        (5001, 5035),
    );

    let mut collation_data = build_stub_collation_data(next_block_info.0, &anchors_cache, 0);
    let working_state = gen_stub_working_state(
        next_block_info.0,
        prev_block_info,
        mc_block_info,
        top_shard_block_info,
    );

    let WorkingState {
        mut msgs_buffer, ..
    } = *working_state;

    let mq_adapter: Arc<dyn MessageQueueAdapter<EnqueuedMessage>> =
        Arc::new(MessageQueueAdapterTestImpl::default());
    let mut mq_iterator_adapter = QueueIteratorAdapter::new(
        shard_id,
        mq_adapter,
        msgs_buffer.current_iterator_positions.take().unwrap(),
        0,
        0,
    );

    // ===================================
    // Set ProcessedUpto like we read and processed all internals,
    // read externals but have not processed them all
    collation_data.processed_upto = ProcessedUptoInfoStuff {
        internals: [
            (ShardIdent::new_full(-1), InternalsProcessedUptoStuff {
                processed_to_msg: (prev_block_upto.0 .1, HashBytes::default()).into(),
                read_to_msg: (prev_block_upto.0 .1, HashBytes::default()).into(),
            }),
            (ShardIdent::new_full(0), InternalsProcessedUptoStuff {
                processed_to_msg: (prev_block_upto.1 .1, HashBytes::default()).into(),
                read_to_msg: (prev_block_upto.1 .1, HashBytes::default()).into(),
            }),
        ]
        .into(),
        externals: Some(ExternalsProcessedUpto {
            processed_to: (4, 4),
            read_to: (16, 3),
        }),
        processed_offset: 2,
    };

    let prev_processed_offset = collation_data.processed_upto.processed_offset;
    assert!(!msgs_buffer.has_pending_messages());
    assert!(prev_processed_offset > 0);

    let mc_top_shards_end_lts = vec![];
    mq_iterator_adapter
        .try_init_next_range_iterator(
            &mut collation_data.processed_upto,
            mc_top_shards_end_lts.iter().copied(),
            InitIteratorMode::OmitNextRange,
        )
        .unwrap();

    let mut messages_reader = MessagesReader::new(shard_id, 20, mc_top_shards_end_lts);

    while msgs_buffer.message_groups_offset() < prev_processed_offset {
        let msg_group = messages_reader
            .get_next_message_group(
                GetNextMessageGroupContext {
                    next_chain_time: collation_data.get_gen_chain_time(),
                    max_new_message_key_to_current_shard: QueueKey::MIN,
                    mode: GetNextMessageGroupMode::Refill,
                },
                &mut collation_data.processed_upto,
                &mut msgs_buffer,
                &mut anchors_cache,
                &mut mq_iterator_adapter,
            )
            .unwrap();
        if msg_group.is_none() {
            break;
        }
    }

    println!(
        "after refill collation_data.processed_upto = {:?}",
        collation_data.processed_upto
    );

    assert_eq!(msgs_buffer.message_groups_offset(), prev_processed_offset);
    assert_eq!(
        collation_data.processed_upto.processed_offset,
        prev_processed_offset
    );

    let (last_imported_anchor_id, _) = anchors_cache.get_last_imported_anchor_id_and_ct().unwrap();
    assert_eq!(last_imported_anchor_id, 40);

    assert_eq!(msgs_buffer.current_ext_reader_position.unwrap(), (16, 3));
    let processed_upto_externals = collation_data.processed_upto.externals.as_ref().unwrap();
    assert_eq!(processed_upto_externals.processed_to, (4, 4));
    assert_eq!(processed_upto_externals.read_to, (16, 3));

    assert_eq!(msgs_buffer.message_groups.int_messages_count(), 0);
    assert_eq!(msgs_buffer.message_groups.ext_messages_count(), 1);

    let processed_upto_internals = &collation_data.processed_upto.internals;
    let mc_upto_int = processed_upto_internals.get(&mc_shard_id).unwrap();
    assert_eq!(
        mc_upto_int.processed_to_msg,
        (prev_block_upto.0 .1, HashBytes::default()).into()
    );
    let sc_upto_int = processed_upto_internals.get(&shard_id).unwrap();
    assert_eq!(
        sc_upto_int.read_to_msg,
        (prev_block_upto.1 .1, HashBytes::default()).into()
    );

    // ===================================
    // And finish reading all messages
    loop {
        let msg_group = messages_reader
            .get_next_message_group(
                GetNextMessageGroupContext {
                    next_chain_time: collation_data.get_gen_chain_time(),
                    max_new_message_key_to_current_shard: QueueKey::MIN,
                    mode: GetNextMessageGroupMode::Continue,
                },
                &mut collation_data.processed_upto,
                &mut msgs_buffer,
                &mut anchors_cache,
                &mut mq_iterator_adapter,
            )
            .unwrap();
        if msg_group.is_none() {
            break;
        }
    }

    println!(
        "after reading finished collation_data.processed_upto = {:?}",
        collation_data.processed_upto
    );

    assert_eq!(msgs_buffer.message_groups_offset(), 0);
    assert_eq!(collation_data.processed_upto.processed_offset, 0);

    assert_eq!(msgs_buffer.current_ext_reader_position.unwrap(), (40, 0));
    let processed_upto_externals = collation_data.processed_upto.externals.as_ref().unwrap();
    assert_eq!(processed_upto_externals.processed_to, (40, 0));
    assert_eq!(processed_upto_externals.read_to, (40, 0));

    assert_eq!(msgs_buffer.message_groups.int_messages_count(), 0);
    assert_eq!(msgs_buffer.message_groups.ext_messages_count(), 0);
}
