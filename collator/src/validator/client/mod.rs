pub mod retry;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tycho_network::{Network, PeerId, PrivateOverlay, Request};

use crate::tracing_targets;
use crate::validator::network::dto::{SignaturesQueryRequest, SignaturesQueryResponse};

pub struct ValidatorClient {
    pub network: Network,
    pub private_overlay: PrivateOverlay,
    peer_id: PeerId,
}

impl ValidatorClient {
    pub fn new(network: Network, private_overlay: PrivateOverlay, peer_id: PeerId) -> Self {
        ValidatorClient {
            network,
            private_overlay,
            peer_id,
        }
    }

    pub async fn request_signatures(
        &self,
        payload: SignaturesQueryRequest,
        timeout_duration: Duration,
    ) -> Result<SignaturesQueryResponse> {
        tracing::trace!(target: tracing_targets::VALIDATOR, "Requesting signatures from validator");
        let payload = Request::from_tl(payload);
        match tokio::time::timeout(
            timeout_duration,
            self.private_overlay
                .query(&self.network, &self.peer_id, payload),
        )
        .await
        {
            Ok(Ok(response)) => response
                .parse_tl::<SignaturesQueryResponse>()
                .map_err(Into::into),
            Ok(Err(e)) => Err(anyhow!("Network error: {}", e)),
            Err(elapsed) => Err(anyhow!("Timeout during request. Elapsed: {:?}", elapsed)),
        }
    }
}
