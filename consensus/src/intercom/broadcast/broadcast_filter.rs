use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use tycho_network::PeerId;
use tycho_util::FastDashMap;

use crate::dag::Verifier;
use crate::intercom::dto::{BroadcastResponse, PeerState};
use crate::intercom::PeerSchedule;
use crate::models::{Digest, Location, NodeCount, Point, PointId, Round};

use super::dto::ConsensusEvent;

#[derive(Clone)]
pub struct BroadcastFilter(Arc<BroadcastFilterInner>);

impl BroadcastFilter {
    pub fn new(
        local_id: Arc<String>,
        peer_schedule: Arc<PeerSchedule>,
        output: mpsc::UnboundedSender<ConsensusEvent>,
    ) -> Self {
        let this = Self(Arc::new(BroadcastFilterInner {
            local_id,
            last_by_peer: Default::default(),
            by_round: Default::default(),
            current_dag_round: Default::default(), // will advance with other peers
            peer_schedule,
            output,
        }));
        let listener = this.clone();
        tokio::spawn(listener.clean_cache());
        this
    }

    pub fn add(&self, point: Arc<Point>) -> BroadcastResponse {
        self.0.add(point)
    }

    pub fn advance_round(&self, new_round: &Round) {
        self.0.advance_round(new_round)
    }

    async fn clean_cache(self) {
        let mut rx = self.0.peer_schedule.updates();
        match rx.recv().await {
            Ok((peer_id, PeerState::Unknown)) => {
                self.0.last_by_peer.remove(&peer_id);
            }
            Ok(_) => {}
            Err(err @ RecvError::Lagged(_)) => {
                tracing::error!("peer schedule updates {err}");
            }
            Err(err @ RecvError::Closed) => {
                panic!("peer schedule updates {err}");
            }
        }
    }
}

struct BroadcastFilterInner {
    local_id: Arc<String>,
    // defend from spam from future rounds:
    // should keep rounds greater than current dag round
    last_by_peer: FastDashMap<PeerId, Round>,
    // very much like DAG structure, but without dependency check;
    // just to determine reliably that consensus advanced without current node
    by_round: FastDashMap<
        Round,
        (
            NodeCount,
            BTreeMap<PeerId, BTreeMap<Digest, ConsensusEvent>>,
        ),
    >,
    current_dag_round: AtomicU32,
    peer_schedule: Arc<PeerSchedule>,
    output: mpsc::UnboundedSender<ConsensusEvent>,
}

impl BroadcastFilterInner {
    // TODO logic is doubtful because of contradiction in requirements:
    //  * we must determine the latest consensus round reliably:
    //    the current approach is to collect 1/3+1 points at the same future round
    //    => we should collect as much points as possible
    //  * we must defend the DAG and current cache from spam from future rounds,
    //    => we should discard points from the far future

    /// returns Vec of points to insert into DAG if consensus round is determined reliably
    fn add(&self, point: Arc<Point>) -> BroadcastResponse {
        let local_id = &self.local_id;
        // dag @r+0 accepts broadcasts of [r-1; r+1] rounds;
        // * points older than r-1 are rejected, but are sent to DAG for validation
        //   as they may be used by some point as a dependency
        // * newer broadcasts are enqueued until 1/3+1 points per round collected
        let dag_round = Round(self.current_dag_round.load(Ordering::Acquire));
        // for any node @ r+0, its DAG always contains [r-DAG_DEPTH-N; r+1] rounds, where N>=0
        let PointId {
            location: Location { round, author },
            digest,
        } = point.id();

        tracing::info!(
            "{local_id} @ {dag_round:?} filter <= bcaster {author:.4?} @ {round:?} : received"
        );

        // conceal raw point, do not use it
        let point = match Verifier::verify(&point, &self.peer_schedule) {
            Ok(()) => ConsensusEvent::Verified(point),
            Err(dag_point) => {
                tracing::error!(
                    "{local_id} @ {dag_round:?} filter <= bcaster {author:.4?} @ {round:?} : \
                     invalid {point:.4?}"
                );
                ConsensusEvent::Invalid(dag_point)
            }
        };
        if round <= dag_round {
            let response = if matches!(point, ConsensusEvent::Invalid(_)) {
                BroadcastResponse::Rejected
            } else if round >= dag_round.prev() {
                BroadcastResponse::Accepted // we will sign, maybe
            } else {
                tracing::error!(
                    "{local_id} @ {dag_round:?} filter <= bcaster {author:.4?} @ {round:?} : \
                    Rejected as too old round"
                );
                // too old, current node will not sign, but some point may include it
                BroadcastResponse::Rejected
            };
            _ = self.output.send(point);
            return response;
        } // else: either consensus moved forward without us,
          // or we shouldn't accept the point yet, or this is spam

        let mut outdated_peer_round = None;
        if *self
            .last_by_peer
            .entry(author)
            .and_modify(|next| {
                if *next < round {
                    if *next >= dag_round {
                        outdated_peer_round = Some(*next);
                    }
                    *next = round
                }
            })
            .or_insert(round)
            > round
        {
            // equivocations are handled by DAG;
            // node must not send broadcasts out-of order;
            // TODO we should ban a peer that broadcasts its rounds out of order,
            //   though we cannot prove this decision for other nodes
            tracing::error!(
                "{local_id} @ {dag_round:?} filter <= bcaster {author:.4?} @ {round:?} : \
                 Rejected as out of order by round"
            );
            return BroadcastResponse::Rejected;
        };
        if let Some(to_delete) = outdated_peer_round {
            // unfortunately, removals will occur every time node lags behind consensus
            self.by_round.entry(to_delete).and_modify(|(_, authors)| {
                // luckily no need to shrink a BTreeMap
                // TODO ban the author, if we detect equivocation now; we won't be able to prove it
                //   if some signatures are invalid (it's another reason for a local ban)
                authors.remove(&author);
            });
        }
        match self.by_round.entry(round).or_try_insert_with(|| {
            // how many nodes should send broadcasts
            NodeCount::try_from(self.peer_schedule.peers_for(&round).len())
                .map(|node_count| (node_count, Default::default()))
        }) {
            // will not accept broadcasts from not initialized validator set
            Err(_) => return BroadcastResponse::TryLater,
            Ok(mut entry) => {
                let (node_count, ref mut same_round) = entry.value_mut();
                same_round.entry(author).or_default().insert(digest, point);
                if same_round.len() < node_count.reliable_minority() {
                    tracing::info!(
                        "{local_id} @ {dag_round:?} filter <= bcaster {author:.4?} @ {round:?} : \
                        round is not determined yet",
                    );
                    return BroadcastResponse::TryLater; // round is not yet determined
                };
            }
        }

        self.advance_round(&round);
        BroadcastResponse::Accepted
    }

    // drop everything up to the new round (inclusive), channelling cached points
    fn advance_round(&self, new_round: &Round) {
        let Ok(old) =
            self.current_dag_round
                .fetch_update(Ordering::Release, Ordering::Relaxed, |old| {
                    Some(new_round.0).filter(|new| old < *new)
                })
        else {
            return;
        };
        // if dag advanced more than by +1 round, include our potential witness points
        // TODO it would be great to drain all contents up to the new round for performance,
        //   (no need to download discarded data) but only top 2 of them are truly necessary;
        //   looks like DashMap doesn't fit well
        let mut data = if old < new_round.0 {
            self.by_round.remove(&new_round.prev())
        } else {
            None
        }
        .into_iter()
        .chain(self.by_round.remove(&new_round));

        while let Some((round, (_, by_author))) = data.next() {
            _ = self.output.send(ConsensusEvent::Forward(round));
            for (_, points) in by_author {
                for (_, point) in points {
                    _ = self.output.send(point);
                }
            }
        }
        // clear older rounds TODO: shrink to fit
        self.by_round.retain(|round, _| round > new_round);
    }
}
