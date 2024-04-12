use std::sync::{Arc, Mutex};

use bytes::{Buf, Bytes};
use tokio::sync::broadcast;
use tycho_network::proto::dht::{rpc, NodeResponse, Value, ValueResponseRaw};
use tycho_network::{Response, Service, ServiceRequest};
use tycho_storage::{BlockConnection, KeyBlocksDirection, Storage};
use tycho_util::futures::BoxFutureOrNoop;

use crate::proto;

pub struct OverlayServer(Arc<OverlayServerInner>);

impl OverlayServer {
    pub fn new(storage: Arc<Storage>) -> Arc<Self> {
        Arc::new(Self(Arc::new(OverlayServerInner { storage })))
    }
}

impl Service<ServiceRequest> for OverlayServer {
    type QueryResponse = Response;
    type OnQueryFuture = BoxFutureOrNoop<Option<Self::QueryResponse>>;
    type OnMessageFuture = futures_util::future::Ready<()>;
    type OnDatagramFuture = futures_util::future::Ready<()>;

    #[tracing::instrument(
        level = "debug",
        name = "on_overlay_server_query",
        skip_all,
        fields(peer_id = %req.metadata.peer_id, addr = %req.metadata.remote_address)
    )]
    fn on_query(&self, req: ServiceRequest) -> Self::OnQueryFuture {
        let (constructor, body) = match self.0.try_handle_prefix(&req) {
            Ok(rest) => rest,
            Err(e) => {
                tracing::debug!("failed to deserialize query: {e}");
                return BoxFutureOrNoop::Noop;
            }
        };

        tycho_network::match_tl_request!(body, tag = constructor, {
            proto::overlay::rpc::GetNextKeyBlockIds as req => {
                BoxFutureOrNoop::future({
                    tracing::debug!(blockId = %req.block, max_size = req.max_size, "getNextKeyBlockIds");

                    let inner = self.0.clone();

                    async move {
                        let res = inner.handle_get_next_key_block_ids(req);
                        Some(Response::from_tl(res))
                    }
                })
            },
            proto::overlay::rpc::GetBlockFull as req => {
                BoxFutureOrNoop::future({
                    tracing::debug!(blockId = %req.block, "getBlockFull");

                    let inner = self.0.clone();

                    async move {
                        let res = inner.handle_get_block_full(req).await;
                        Some(Response::from_tl(res))
                    }
                })
            },
            proto::overlay::rpc::GetNextBlockFull as req => {
                BoxFutureOrNoop::future({
                    tracing::debug!(prevBlockId = %req.prev_block, "getNextBlockFull");

                    let inner = self.0.clone();

                    async move {
                        let res = inner.handle_get_next_block_full(req).await;
                        Some(Response::from_tl(res))
                    }
                })
            },
        }, e => {
            tracing::debug!("failed to deserialize query: {e}");
            BoxFutureOrNoop::Noop
        })
    }

    #[inline]
    fn on_message(&self, _req: ServiceRequest) -> Self::OnMessageFuture {
        futures_util::future::ready(())
    }

    #[inline]
    fn on_datagram(&self, _req: ServiceRequest) -> Self::OnDatagramFuture {
        futures_util::future::ready(())
    }
}

struct OverlayServerInner {
    storage: Arc<Storage>,
}

impl OverlayServerInner {
    fn storage(&self) -> &Storage {
        self.storage.as_ref()
    }

    fn try_handle_prefix<'a>(&self, req: &'a ServiceRequest) -> anyhow::Result<(u32, &'a [u8])> {
        let mut body = req.as_ref();
        anyhow::ensure!(body.len() >= 4, tl_proto::TlError::UnexpectedEof);

        let mut constructor = std::convert::identity(body).get_u32_le();

        Ok((constructor, body))
    }

    fn handle_get_next_key_block_ids(
        &self,
        req: proto::overlay::rpc::GetNextKeyBlockIds,
    ) -> proto::overlay::Response<proto::overlay::KeyBlockIds> {
        const NEXT_KEY_BLOCKS_LIMIT: usize = 8;

        let block_handle_storage = self.storage().block_handle_storage();

        let limit = std::cmp::min(req.max_size as usize, NEXT_KEY_BLOCKS_LIMIT);

        let get_next_key_block_ids = || {
            let start_block_id = &req.block;
            if !start_block_id.shard.is_masterchain() {
                return Err(OverlayServerError::BlockNotFromMasterChain.into());
            }

            let mut iterator = block_handle_storage
                .key_blocks_iterator(KeyBlocksDirection::ForwardFrom(start_block_id.seqno))
                .take(limit)
                .peekable();

            if let Some(Ok(id)) = iterator.peek() {
                if id.root_hash != start_block_id.root_hash {
                    return Err(OverlayServerError::InvalidRootHash.into());
                }
                if id.file_hash != start_block_id.file_hash {
                    return Err(OverlayServerError::InvalidFileHash.into());
                }
            }

            let mut ids = Vec::with_capacity(limit);
            while let Some(id) = iterator.next().transpose()? {
                ids.push(id);
                if ids.len() >= limit {
                    break;
                }
            }

            Ok::<_, anyhow::Error>(ids)
        };

        match get_next_key_block_ids() {
            Ok(ids) => {
                let incomplete = ids.len() < limit;
                proto::overlay::Response::Ok(proto::overlay::KeyBlockIds {
                    blocks: ids,
                    incomplete,
                })
            }
            Err(e) => {
                tracing::warn!("get_next_key_block_ids failed: {e:?}");
                proto::overlay::Response::Err
            }
        }
    }

    async fn handle_get_block_full(
        &self,
        req: proto::overlay::rpc::GetBlockFull,
    ) -> proto::overlay::Response<proto::overlay::BlockFull> {
        let block_handle_storage = self.storage().block_handle_storage();
        let block_storage = self.storage().block_storage();

        let get_block_full = || async {
            let mut is_link = false;
            let block = match block_handle_storage.load_handle(&req.block)? {
                Some(handle)
                    if handle.meta().has_data() && handle.has_proof_or_link(&mut is_link) =>
                {
                    let block = block_storage.load_block_data_raw(&handle).await?;
                    let proof = block_storage.load_block_proof_raw(&handle, is_link).await?;

                    proto::overlay::BlockFull::Found {
                        block_id: req.block,
                        proof: proof.into(),
                        block: block.into(),
                        is_link,
                    }
                }
                _ => proto::overlay::BlockFull::Empty,
            };

            Ok::<_, anyhow::Error>(block)
        };

        match get_block_full().await {
            Ok(block_full) => proto::overlay::Response::Ok(block_full),
            Err(e) => {
                tracing::warn!("get_block_full failed: {e:?}");
                proto::overlay::Response::Err
            }
        }
    }

    async fn handle_get_next_block_full(
        &self,
        req: proto::overlay::rpc::GetNextBlockFull,
    ) -> proto::overlay::Response<proto::overlay::BlockFull> {
        let block_handle_storage = self.storage().block_handle_storage();
        let block_connection_storage = self.storage().block_connection_storage();
        let block_storage = self.storage().block_storage();

        let get_next_block_full = || async {
            let next_block_id = match block_handle_storage.load_handle(&req.prev_block)? {
                Some(handle) if handle.meta().has_next1() => block_connection_storage
                    .load_connection(&req.prev_block, BlockConnection::Next1)?,
                _ => return Ok(proto::overlay::BlockFull::Empty),
            };

            let mut is_link = false;
            let block = match block_handle_storage.load_handle(&next_block_id)? {
                Some(handle)
                    if handle.meta().has_data() && handle.has_proof_or_link(&mut is_link) =>
                {
                    let block = block_storage.load_block_data_raw(&handle).await?;
                    let proof = block_storage.load_block_proof_raw(&handle, is_link).await?;

                    proto::overlay::BlockFull::Found {
                        block_id: next_block_id,
                        proof: proof.into(),
                        block: block.into(),
                        is_link,
                    }
                }
                _ => proto::overlay::BlockFull::Empty,
            };

            Ok::<_, anyhow::Error>(block)
        };

        match get_next_block_full().await {
            Ok(block_full) => proto::overlay::Response::Ok(block_full),
            Err(e) => {
                tracing::warn!("get_next_block_full failed: {e:?}");
                proto::overlay::Response::Err
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum OverlayServerError {
    #[error("Block is not from masterchain")]
    BlockNotFromMasterChain,
    #[error("Invalid root hash")]
    InvalidRootHash,
    #[error("Invalid file hash")]
    InvalidFileHash,
}
