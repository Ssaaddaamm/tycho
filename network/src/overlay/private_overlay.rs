use std::borrow::Borrow;
use std::collections::hash_map;
use std::sync::Arc;

use anyhow::Result;
use bytes::{Bytes, BytesMut};
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use rand::seq::SliceRandom;
use rand::Rng;
use tokio::sync::broadcast;
use tycho_util::futures::BoxFutureOrNoop;
use tycho_util::{FastHashMap, FastHashSet};

use crate::network::{KnownPeerHandle, Network};
use crate::overlay::OverlayId;
use crate::proto::overlay::rpc;
use crate::types::{BoxService, PeerId, Request, Response, Service, ServiceExt, ServiceRequest};
use crate::util::NetworkExt;

pub struct PrivateOverlayBuilder {
    overlay_id: OverlayId,
    entries: FastHashSet<PeerId>,
    entry_events_channel_size: usize,
    resolve_peers: bool,
}

impl PrivateOverlayBuilder {
    /// Whether to resolve peer info in the background.
    ///
    /// Default: `false`.
    pub fn resolve_peers(mut self, resolve_peers: bool) -> Self {
        self.resolve_peers = resolve_peers;
        self
    }

    pub fn with_entries<I>(mut self, allowed_peers: I) -> Self
    where
        I: IntoIterator,
        I::Item: Borrow<PeerId>,
    {
        self.entries
            .extend(allowed_peers.into_iter().map(|p| *p.borrow()));
        self
    }

    /// The capacity of entries set events.
    ///
    /// Default: 100.
    pub fn with_entry_events_channel_size(mut self, entry_events_channel_size: usize) -> Self {
        self.entry_events_channel_size = entry_events_channel_size;
        self
    }

    pub fn build<S>(self, service: S) -> PrivateOverlay
    where
        S: Send + Sync + 'static,
        S: Service<ServiceRequest, QueryResponse = Response>,
    {
        let request_prefix = tl_proto::serialize(rpc::Prefix {
            overlay_id: self.overlay_id.as_bytes(),
        });

        let mut entries = PrivateOverlayEntries {
            peer_id_to_index: Default::default(),
            data: Default::default(),
            resolved_peers: Default::default(),
            events_tx: broadcast::channel(self.entry_events_channel_size).0,
        };
        for peer_id in self.entries {
            entries.insert(&peer_id, None);
        }

        PrivateOverlay {
            inner: Arc::new(Inner {
                overlay_id: self.overlay_id,
                entries: RwLock::new(entries),
                service: service.boxed(),
                resolve_peers: self.resolve_peers,
                request_prefix: request_prefix.into_boxed_slice(),
            }),
        }
    }
}

#[derive(Clone)]
pub struct PrivateOverlay {
    inner: Arc<Inner>,
}

impl PrivateOverlay {
    pub fn builder(overlay_id: OverlayId) -> PrivateOverlayBuilder {
        PrivateOverlayBuilder {
            overlay_id,
            entries: Default::default(),
            entry_events_channel_size: 100,
            resolve_peers: false,
        }
    }

    #[inline]
    pub fn overlay_id(&self) -> &OverlayId {
        &self.inner.overlay_id
    }

    pub async fn query(
        &self,
        network: &Network,
        peer_id: &PeerId,
        mut request: Request,
    ) -> Result<Response> {
        self.prepend_prefix_to_body(&mut request.body);
        network.query(peer_id, request).await
    }

    pub async fn send(
        &self,
        network: &Network,
        peer_id: &PeerId,
        mut request: Request,
    ) -> Result<()> {
        self.prepend_prefix_to_body(&mut request.body);
        network.send(peer_id, request).await
    }

    pub fn write_entries(&self) -> PrivateOverlayEntriesWriteGuard<'_> {
        PrivateOverlayEntriesWriteGuard {
            entries: self.inner.entries.write(),
        }
    }

    pub fn read_entries(&self) -> PrivateOverlayEntriesReadGuard<'_> {
        PrivateOverlayEntriesReadGuard {
            entries: self.inner.entries.read(),
        }
    }

    pub(crate) fn handle_query(&self, req: ServiceRequest) -> BoxFutureOrNoop<Option<Response>> {
        if self.inner.entries.read().contains(&req.metadata.peer_id) {
            BoxFutureOrNoop::future(self.inner.service.on_query(req))
        } else {
            BoxFutureOrNoop::Noop
        }
    }

    pub(crate) fn handle_message(&self, req: ServiceRequest) -> BoxFutureOrNoop<()> {
        if self.inner.entries.read().contains(&req.metadata.peer_id) {
            BoxFutureOrNoop::future(self.inner.service.on_message(req))
        } else {
            BoxFutureOrNoop::Noop
        }
    }

    pub(crate) fn should_resolve_peers(&self) -> bool {
        self.inner.resolve_peers
    }

    fn prepend_prefix_to_body(&self, body: &mut Bytes) {
        // TODO: reduce allocations
        let mut res = BytesMut::with_capacity(self.inner.request_prefix.len() + body.len());
        res.extend_from_slice(&self.inner.request_prefix);
        res.extend_from_slice(body);
        *body = res.freeze();
    }
}

impl std::fmt::Debug for PrivateOverlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivateOverlay")
            .field("overlay_id", &self.inner.overlay_id)
            .finish()
    }
}

struct Inner {
    overlay_id: OverlayId,
    entries: RwLock<PrivateOverlayEntries>,
    service: BoxService<ServiceRequest, Response>,
    resolve_peers: bool,
    request_prefix: Box<[u8]>,
}

// NOTE: `#[derive(Default)]` is missing to prevent construction outside the
// crate.
pub struct PrivateOverlayEntries {
    peer_id_to_index: FastHashMap<PeerId, PrivateOverlayEntryIndices>,
    data: Vec<PeerId>,
    resolved_peers: Vec<KnownPeerHandle>,
    events_tx: broadcast::Sender<PrivateOverlayEntriesEvent>,
}

impl PrivateOverlayEntries {
    /// Subscribes to the set updates.
    pub fn subscribe(&self) -> broadcast::Receiver<PrivateOverlayEntriesEvent> {
        self.events_tx.subscribe()
    }

    /// Returns an iterator over the entry ids.
    ///
    /// The order is not random, but is not defined.
    pub fn iter(&self) -> std::slice::Iter<'_, PeerId> {
        self.data.iter()
    }

    /// Returns an iterator over the entries.
    ///
    /// The order is not random, but is not defined.
    pub fn iter_with_resolved(&self) -> PrivateOverlayEntriesIter<'_> {
        PrivateOverlayEntriesIter {
            inner: self.peer_id_to_index.iter(),
            entries: self,
        }
    }

    /// Returns one random peer, or `None` if set is empty.
    pub fn choose<R>(&self, rng: &mut R) -> Option<&PeerId>
    where
        R: Rng + ?Sized,
    {
        self.data.choose(rng)
    }

    /// Returns one random resolved peer handle, or `None` if no resolved peers
    /// are present.
    pub fn choose_resolved<R>(&self, rng: &mut R) -> Option<&KnownPeerHandle>
    where
        R: Rng + ?Sized,
    {
        self.resolved_peers.choose(rng)
    }

    /// Chooses `n` entries from the set, without repetition,
    /// and in random order.
    pub fn choose_multiple<R>(
        &self,
        rng: &mut R,
        n: usize,
    ) -> rand::seq::SliceChooseIter<'_, [PeerId], PeerId>
    where
        R: Rng + ?Sized,
    {
        self.data.choose_multiple(rng, n)
    }

    /// Chooses `n` resolved entries from the set, without repetition,
    /// and in random order.
    pub fn choose_multiple_resolved<R>(
        &self,
        rng: &mut R,
        n: usize,
    ) -> rand::seq::SliceChooseIter<'_, [KnownPeerHandle], KnownPeerHandle>
    where
        R: Rng + ?Sized,
    {
        self.resolved_peers.choose_multiple(rng, n)
    }

    /// Clears the set, removing all entries.
    pub fn clear(&mut self) {
        self.peer_id_to_index.clear();
        self.data.clear();
        self.resolved_peers.clear();
    }

    /// Returns `true` if the set contains no elements.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Returns the number of elements in the set, also referred to as its 'length'.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Returns true if the set contains the specified peer id.
    pub fn contains(&self, peer_id: &PeerId) -> bool {
        self.peer_id_to_index.contains_key(peer_id)
    }

    /// Returns the resolved peer handle for the specified peer id, if it exists.
    pub fn get_resolved(&self, peer_id: &PeerId) -> Option<&KnownPeerHandle> {
        self.peer_id_to_index.get(peer_id).and_then(|link| {
            link.resolved_data_index
                .map(|index| &self.resolved_peers[index])
        })
    }

    /// Adds a peer id to the set.
    ///
    /// Returns whether the value was newly inserted.
    pub fn insert(&mut self, peer_id: &PeerId, handle: Option<KnownPeerHandle>) -> bool {
        match self.peer_id_to_index.entry(*peer_id) {
            // No entry for the peer_id, insert a new one
            hash_map::Entry::Vacant(entry) => {
                entry.insert(PrivateOverlayEntryIndices {
                    data_index: self.data.len(),
                    resolved_data_index: handle.is_some().then_some(self.resolved_peers.len()),
                });

                self.data.push(*peer_id);
                if let Some(handle) = handle {
                    self.resolved_peers.push(handle);
                }

                _ = self
                    .events_tx
                    .send(PrivateOverlayEntriesEvent::Added(*peer_id));

                true
            }
            // Entry for the peer_id exists, do nothing
            hash_map::Entry::Occupied(mut entry) => {
                if let Some(handle) = handle {
                    match entry.get().resolved_data_index {
                        // Replace an existing resolved handle
                        Some(index) => self.resolved_peers[index] = handle,
                        // Insert a new resolved handle
                        None => {
                            entry.get_mut().resolved_data_index = Some(self.resolved_peers.len());
                            self.resolved_peers.push(handle);
                        }
                    }

                    _ = self
                        .events_tx
                        .send(PrivateOverlayEntriesEvent::Resolved(*peer_id));
                }
                false
            }
        }
    }

    /// Updates the resolved peer handle for the specified peer id.
    ///
    /// Returns whether the value was updated.
    pub fn set_resolved(&mut self, handle: KnownPeerHandle) -> bool {
        // NOTE: using a temp variable to prevent holding arcswap guard for too long
        let peer_id = handle.peer_info().id;

        match self.peer_id_to_index.get_mut(&peer_id) {
            Some(link) => {
                match link.resolved_data_index {
                    // Replace an existing resolved handle
                    Some(index) => self.resolved_peers[index] = handle,
                    // Insert a new resolved handle
                    None => {
                        link.resolved_data_index = Some(self.resolved_peers.len());
                        self.resolved_peers.push(handle);
                    }
                }

                _ = self
                    .events_tx
                    .send(PrivateOverlayEntriesEvent::Resolved(peer_id));

                true
            }
            None => false,
        }
    }

    /// Removes a value from the set.
    ///
    /// Returns whether the value was present in the set.
    pub fn remove(&mut self, peer_id: &PeerId) -> bool {
        let Some(link) = self.peer_id_to_index.remove(peer_id) else {
            return false;
        };

        // Remove the entry from the data vector
        self.data.swap_remove(link.data_index);
        self.fix_data_index(link.data_index);

        if let Some(index) = link.resolved_data_index {
            // Remove the entry from the resolved peers vector
            self.resolved_peers.swap_remove(index);
            self.fix_resolved_data_index(index);
        }

        _ = self
            .events_tx
            .send(PrivateOverlayEntriesEvent::Removed(*peer_id));

        true
    }

    fn fix_data_index(&mut self, index: usize) {
        if index < self.data.len() {
            let entry = self
                .peer_id_to_index
                .get_mut(&self.data[index])
                .expect("inconsistent data state");
            entry.data_index = index;
        }
    }

    fn fix_resolved_data_index(&mut self, index: usize) {
        if index < self.resolved_peers.len() {
            let entry = self
                .peer_id_to_index
                .get_mut(&self.resolved_peers[index].peer_info().id)
                .expect("inconsistent resolved data state");
            entry.resolved_data_index = Some(index);
        }
    }
}

#[derive(Clone)]
pub struct PrivateOverlayEntriesIter<'a> {
    inner: hash_map::Iter<'a, PeerId, PrivateOverlayEntryIndices>,
    entries: &'a PrivateOverlayEntries,
}

impl<'a> Iterator for PrivateOverlayEntriesIter<'a> {
    type Item = (&'a PeerId, Option<&'a KnownPeerHandle>);

    fn next(&mut self) -> Option<Self::Item> {
        let (peer_id, link) = self.inner.next()?;
        let handle = link
            .resolved_data_index
            .map(|index| &self.entries.resolved_peers[index]);

        Some((peer_id, handle))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

struct PrivateOverlayEntryIndices {
    data_index: usize,
    resolved_data_index: Option<usize>,
}

pub struct PrivateOverlayEntriesWriteGuard<'a> {
    entries: RwLockWriteGuard<'a, PrivateOverlayEntries>,
}

impl std::ops::Deref for PrivateOverlayEntriesWriteGuard<'_> {
    type Target = PrivateOverlayEntries;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl std::ops::DerefMut for PrivateOverlayEntriesWriteGuard<'_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entries
    }
}

pub struct PrivateOverlayEntriesReadGuard<'a> {
    entries: RwLockReadGuard<'a, PrivateOverlayEntries>,
}

impl std::ops::Deref for PrivateOverlayEntriesReadGuard<'_> {
    type Target = PrivateOverlayEntries;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivateOverlayEntriesEvent {
    /// A new entry was inserted.
    Added(PeerId),
    /// An exising entry received a [`KnownPeerHandle`].
    Resolved(PeerId),
    /// An existing entry was removed.
    Removed(PeerId),
}

#[cfg(test)]
mod tests {
    use crate::network::KnownPeers;
    use crate::util::make_peer_info_stub;

    use super::*;

    #[test]
    fn entries_container_is_set() {
        let mut entries = PrivateOverlayEntries {
            peer_id_to_index: Default::default(),
            data: Default::default(),
            resolved_peers: Default::default(),
            events_tx: broadcast::channel(100).0,
        };
        assert!(entries.is_empty());
        assert_eq!(entries.len(), 0);

        let peer_id = rand::random();
        assert!(entries.insert(&peer_id, None));

        assert!(!entries.is_empty());
        assert_eq!(entries.len(), 1);

        assert!(!entries.insert(&peer_id, None));
        assert_eq!(entries.len(), 1);

        entries.clear();
        assert!(entries.is_empty());
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn remove_from_entries_container() {
        let (events_tx, mut events_rx) = broadcast::channel(100);

        let mut entries = PrivateOverlayEntries {
            peer_id_to_index: Default::default(),
            data: Default::default(),
            resolved_peers: Default::default(),
            events_tx,
        };

        let peer_ids = std::array::from_fn::<PeerId, 10, _>(|_| rand::random());
        for (i, peer_id) in peer_ids.iter().enumerate() {
            assert!(entries.insert(peer_id, None));
            assert_eq!(entries.len(), i + 1);
            assert_eq!(entries.data.len(), i + 1);
            assert_eq!(
                events_rx.try_recv().unwrap(),
                PrivateOverlayEntriesEvent::Added(*peer_id)
            );
        }

        for peer_id in &peer_ids {
            assert!(entries.remove(peer_id));
            assert_eq!(
                events_rx.try_recv().unwrap(),
                PrivateOverlayEntriesEvent::Removed(*peer_id)
            );

            assert!(!entries.data.contains(peer_id));
            for (index, data) in entries.data.iter().enumerate() {
                assert_eq!(entries.peer_id_to_index[data].data_index, index);
            }
        }

        assert!(entries.is_empty());

        assert!(!entries.remove(&rand::random()));
        assert!(events_rx.try_recv().is_err());
    }

    #[test]
    fn resolved_peers_in_entries_container() {
        let known_peers = KnownPeers::new();

        let (events_tx, mut events_rx) = broadcast::channel(100);

        let mut entries = PrivateOverlayEntries {
            peer_id_to_index: Default::default(),
            data: Default::default(),
            resolved_peers: Default::default(),
            events_tx,
        };

        let peer_info = std::array::from_fn::<_, 10, _>(|_| make_peer_info_stub(rand::random()));
        for (i, peer_info) in peer_info.iter().enumerate() {
            let handle =
                (i % 2 == 0).then(|| known_peers.insert(peer_info.clone(), false).unwrap());

            assert!(entries.insert(&peer_info.id, handle));
            assert_eq!(entries.len(), i + 1);
            assert_eq!(entries.data.len(), i + 1);
            assert_eq!(entries.resolved_peers.len(), (i / 2) + 1);

            assert_eq!(
                events_rx.try_recv().unwrap(),
                PrivateOverlayEntriesEvent::Added(peer_info.id)
            );
        }
        assert_eq!(entries.resolved_peers.len(), 5);

        // Add resolved info for the 1st item
        assert!(entries.set_resolved(known_peers.insert(peer_info[1].clone(), false).unwrap()));
        assert_eq!(entries.len(), 10);
        assert_eq!(entries.data.len(), 10);
        assert_eq!(entries.resolved_peers.len(), 6);
        assert_eq!(
            events_rx.try_recv().unwrap(),
            PrivateOverlayEntriesEvent::Resolved(peer_info[1].id)
        );

        // Try to set handle for the non-existing item
        assert!(!entries.set_resolved(
            known_peers
                .insert(make_peer_info_stub(rand::random()), false)
                .unwrap()
        ));
        assert!(events_rx.try_recv().is_err());

        // Final state check
        assert_eq!(entries.len(), 10);
        assert_eq!(entries.data.len(), 10);
        assert_eq!(entries.resolved_peers.len(), 6);
    }
}
