use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use crate::{
    impl_enum_try_into, method_to_async_task_closure,
    types::{
        ext_types::{BlockHandle, BlockIdExt},
        BlockStuff, ShardStateStuff,
    },
    utils::{
        async_queued_dispatcher::{AsyncQueuedDispatcher, STANDART_DISPATCHER_QUEUE_BUFFER_SIZE},
        task_descr::TaskResponseReceiver,
    },
};

// BUILDER

pub trait StateNodeAdapterBuilder<T>
where
    T: StateNodeAdapter,
{
    fn new() -> Self;
    fn build(self, listener: Arc<dyn StateNodeEventListener>) -> T;
}

pub struct StateNodeAdapterBuilderStdImpl<T> {
    _marker_adapter: std::marker::PhantomData<T>,
}

impl<T> StateNodeAdapterBuilder<T> for StateNodeAdapterBuilderStdImpl<T>
where
    T: StateNodeAdapter,
{
    fn new() -> Self {
        Self {
            _marker_adapter: std::marker::PhantomData,
        }
    }
    fn build(self, listener: Arc<dyn StateNodeEventListener>) -> T {
        T::create(listener)
    }
}

// EVENTS EMITTER AMD LISTENER

#[async_trait]
pub trait StateNodeEventEmitter {
    /// When new masterchain block received from blockchain
    async fn on_mc_block_event(&self, mc_block_id: BlockIdExt);
}

#[async_trait]
pub trait StateNodeEventListener: Send + Sync {
    /// Process new received masterchain block from blockchain
    async fn on_mc_block(&self, mc_block_id: BlockIdExt) -> Result<()>;
}

// ADAPTER

#[async_trait]
pub trait StateNodeAdapter: Send + Sync + 'static {
    fn create(listener: Arc<dyn StateNodeEventListener>) -> Self;
    async fn get_last_applied_mc_block_id(&self) -> Result<BlockIdExt>;
    async fn request_state(
        &self,
        block_id: BlockIdExt,
    ) -> Result<StateNodeTaskResponseReceiver<Arc<ShardStateStuff>>>;
    async fn get_block(&self, block_id: BlockIdExt) -> Result<Option<Arc<BlockStuff>>>;
    async fn request_block(
        &self,
        block_id: BlockIdExt,
    ) -> Result<StateNodeTaskResponseReceiver<Option<Arc<BlockStuff>>>>;
    async fn accept_block(&mut self, block: BlockStuff) -> Result<Arc<BlockHandle>>;
}

pub struct StateNodeAdapterStdImpl {
    dispatcher: Arc<AsyncQueuedDispatcher<StateNodeProcessor, StateNodeTaskResult>>,
    listener: Arc<dyn StateNodeEventListener>,
}

#[async_trait]
impl StateNodeAdapter for StateNodeAdapterStdImpl {
    async fn get_last_applied_mc_block_id(&self) -> Result<BlockIdExt> {
        self.dispatcher
            .execute_task(method_to_async_task_closure!(get_last_applied_mc_block_id,))
            .await
            .and_then(|res| res.try_into())
    }
    fn create(listener: Arc<dyn StateNodeEventListener>) -> Self {
        let processor = StateNodeProcessor {
            listener: listener.clone(),
        };
        let dispatcher =
            AsyncQueuedDispatcher::create(processor, STANDART_DISPATCHER_QUEUE_BUFFER_SIZE);
        Self {
            dispatcher: Arc::new(dispatcher),
            listener,
        }
    }
    async fn request_state(
        &self,
        block_id: BlockIdExt,
    ) -> Result<StateNodeTaskResponseReceiver<Arc<ShardStateStuff>>> {
        let receiver = self
            .dispatcher
            .enqueue_task_with_responder(method_to_async_task_closure!(get_state, block_id))
            .await?;

        Ok(StateNodeTaskResponseReceiver::create(receiver))
    }
    async fn get_block(&self, block_id: BlockIdExt) -> Result<Option<Arc<BlockStuff>>> {
        self.dispatcher
            .execute_task(method_to_async_task_closure!(get_block, block_id))
            .await
            .and_then(|res| res.try_into())
    }
    async fn request_block(
        &self,
        block_id: BlockIdExt,
    ) -> Result<StateNodeTaskResponseReceiver<Option<Arc<BlockStuff>>>> {
        let receiver = self
            .dispatcher
            .enqueue_task_with_responder(method_to_async_task_closure!(get_block, block_id))
            .await?;

        Ok(StateNodeTaskResponseReceiver::create(receiver))
    }
    async fn accept_block(&mut self, block: BlockStuff) -> Result<Arc<BlockHandle>> {
        self.dispatcher
            .execute_task(method_to_async_task_closure!(accept_block, block))
            .await
            .and_then(|res| res.try_into())
    }
}

// ADAPTER PROCESSOR

struct StateNodeProcessor {
    listener: Arc<dyn StateNodeEventListener>,
}

pub enum StateNodeTaskResult {
    Void,
    BlockId(BlockIdExt),
    ShardState(Arc<ShardStateStuff>),
    Block(Option<Arc<BlockStuff>>),
    BlockHandle(Arc<BlockHandle>),
}

impl_enum_try_into!(StateNodeTaskResult, BlockId, BlockIdExt);
impl_enum_try_into!(StateNodeTaskResult, ShardState, Arc<ShardStateStuff>);
impl_enum_try_into!(StateNodeTaskResult, Block, Option<Arc<BlockStuff>>);
impl_enum_try_into!(StateNodeTaskResult, BlockHandle, Arc<BlockHandle>);

type StateNodeTaskResponseReceiver<T> = TaskResponseReceiver<StateNodeTaskResult, T>;

impl StateNodeProcessor {
    async fn get_last_applied_mc_block_id(&self) -> Result<StateNodeTaskResult> {
        todo!()
    }
    async fn get_state(&self, block_id: BlockIdExt) -> Result<StateNodeTaskResult> {
        todo!()
    }
    async fn get_block(&self, block_id: BlockIdExt) -> Result<StateNodeTaskResult> {
        todo!()
    }
    async fn accept_block(&mut self, block: BlockStuff) -> Result<StateNodeTaskResult> {
        todo!()
    }
}
