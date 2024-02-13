use std::future::Future;

use anyhow::Result;
use bytes::Bytes;

use crate::network::{Network, Peer};
use crate::types::{PeerEvent, PeerId, Request, Response};

pub trait NetworkExt {
    fn query(
        &self,
        peer_id: &PeerId,
        request: Request<Bytes>,
    ) -> impl Future<Output = Result<Response<Bytes>>> + Send;

    fn send(
        &self,
        peer_id: &PeerId,
        request: Request<Bytes>,
    ) -> impl Future<Output = Result<()>> + Send;
}

impl NetworkExt for Network {
    async fn query(&self, peer_id: &PeerId, request: Request<Bytes>) -> Result<Response<Bytes>> {
        on_connected_peer(self, Peer::rpc, peer_id, request).await
    }

    async fn send(&self, peer_id: &PeerId, request: Request<Bytes>) -> Result<()> {
        on_connected_peer(self, Peer::send_message, peer_id, request).await
    }
}

async fn on_connected_peer<T, F>(
    network: &Network,
    f: F,
    peer_id: &PeerId,
    request: Request<Bytes>,
) -> Result<T>
where
    for<'a> F: PeerTask<'a, T>,
{
    use tokio::sync::broadcast::error::RecvError;

    let mut peer_events = network.subscribe()?;

    // Interact if already connected
    if let Some(peer) = network.peer(peer_id) {
        return f.call(&peer, request).await;
    }

    match network.known_peers().get(peer_id) {
        // Initiate a connection of it is a known peer
        Some(peer_info) => {
            network
                .connect_with_peer_id(peer_info.address, peer_id)
                .await?;
        }
        // Error otherwise
        None => anyhow::bail!("trying to interact with an unknown peer: {peer_id}"),
    }

    loop {
        match peer_events.recv().await {
            Ok(PeerEvent::NewPeer(new_peer_id)) if &new_peer_id == peer_id => {
                if let Some(peer) = network.peer(peer_id) {
                    return f.call(&peer, request).await;
                }
            }
            Ok(_) => {}
            Err(RecvError::Closed) => anyhow::bail!("network subscription closed"),
            Err(RecvError::Lagged(_)) => {
                peer_events = peer_events.resubscribe();

                if let Some(peer) = network.peer(peer_id) {
                    return f.call(&peer, request).await;
                }
            }
        }

        anyhow::ensure!(
            network.known_peers().contains(peer_id),
            "waiting for a connection to an unknown peer: {peer_id}",
        );
    }
}

trait PeerTask<'a, T> {
    type Output: Future<Output = Result<T>> + 'a;

    fn call(self, peer: &'a Peer, request: Request<Bytes>) -> Self::Output;
}

impl<'a, T, F, Fut> PeerTask<'a, T> for F
where
    F: FnOnce(&'a Peer, Request<Bytes>) -> Fut,
    Fut: Future<Output = Result<T>> + 'a,
{
    type Output = Fut;

    #[inline]
    fn call(self, peer: &'a Peer, request: Request<Bytes>) -> Fut {
        self(peer, request)
    }
}
