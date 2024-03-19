use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use exponential_backoff::Backoff;
use tokio::sync::{Notify, Semaphore};
use tycho_util::futures::JoinTask;
use tycho_util::time::now_sec;
use tycho_util::FastDashMap;

use crate::dht::DhtService;
use crate::network::{KnownPeerHandle, KnownPeersError, Network, PeerBannedError, WeakNetwork};
use crate::proto::dht;
use crate::types::{PeerId, PeerInfo};

pub struct PeerResolverBuilder {
    inner: PeerResolverConfig,
}

impl PeerResolverBuilder {
    /// Minimal time-to-live for the resolved peer info.
    ///
    /// Default: 600 seconds.
    pub fn with_min_ttl_sec(mut self, ttl_sec: u32) -> Self {
        self.inner.min_ttl_sec = ttl_sec;
        self
    }

    /// Time before the expiration when the peer info should be updated.
    ///
    /// Default: 1200 seconds.
    pub fn with_update_before_sec(mut self, update_before_sec: u32) -> Self {
        self.inner.update_before_sec = update_before_sec;
        self
    }

    /// Number of fast retries before switching to the stale retry interval.
    ///
    /// Default: 10.
    pub fn with_fast_retry_count(mut self, fast_retry_count: u32) -> Self {
        self.inner.fast_retry_count = fast_retry_count;
        self
    }

    /// Minimal interval between the fast retries.
    ///
    /// Default: 1 second.
    pub fn with_min_retry_interval(mut self, min_retry_interval: Duration) -> Self {
        self.inner.min_retry_interval = min_retry_interval;
        self
    }

    /// Maximal interval between the fast retries.
    ///
    /// Default: 120 seconds.
    pub fn with_max_retry_interval(mut self, max_retry_interval: Duration) -> Self {
        self.inner.max_retry_interval = max_retry_interval;
        self
    }

    /// Interval between the stale retries.
    ///
    /// Default: 600 seconds.
    pub fn with_stale_retry_interval(mut self, stale_retry_interval: Duration) -> Self {
        self.inner.stale_retry_interval = stale_retry_interval;
        self
    }

    pub fn build(self) -> PeerResolver {
        // PeerResolver {
        //     inner: Arc::new(PeerResolverInner {
        //         weak_network: (),
        //         dht_service: (),
        //         config: Default::default(),
        //         tasks: Default::default(),
        //         semaphore: (),
        //     }),
        // }
        todo!()
    }
}

struct PeerResolverConfig {
    max_parallel_resolve_requests: usize,
    min_ttl_sec: u32,
    update_before_sec: u32,
    fast_retry_count: u32,
    min_retry_interval: Duration,
    max_retry_interval: Duration,
    stale_retry_interval: Duration,
}

impl Default for PeerResolverConfig {
    fn default() -> Self {
        Self {
            max_parallel_resolve_requests: 100,
            min_ttl_sec: 600,
            update_before_sec: 1200,
            fast_retry_count: 10,
            min_retry_interval: Duration::from_secs(1),
            max_retry_interval: Duration::from_secs(120),
            stale_retry_interval: Duration::from_secs(600),
        }
    }
}

pub struct PeerResolver {
    inner: Arc<PeerResolverInner>,
}

impl PeerResolver {
    pub fn builder() -> PeerResolverBuilder {
        PeerResolverBuilder {
            inner: Default::default(),
        }
    }

    pub fn insert(&self, peer_id: &PeerId) -> PeerResolverHandle {
        use dashmap::mapref::entry::Entry;

        match self.inner.tasks.entry(*peer_id) {
            Entry::Vacant(entry) => {
                let handle = self.inner.make_resolver_handle(peer_id);
                entry.insert(Arc::downgrade(&handle.inner));
                handle
            }
            Entry::Occupied(mut entry) => match entry.get().upgrade() {
                Some(inner) => PeerResolverHandle {
                    inner: ManuallyDrop::new(inner),
                },
                None => {
                    let handle = self.inner.make_resolver_handle(peer_id);
                    entry.insert(Arc::downgrade(&handle.inner));
                    handle
                }
            },
        }
    }
}

struct PeerResolverInner {
    weak_network: WeakNetwork,
    dht_service: DhtService,
    config: PeerResolverConfig,
    tasks: FastDashMap<PeerId, Weak<PeerResolverHandleInner>>,
    semaphore: Semaphore,
}

impl PeerResolverInner {
    fn make_resolver_handle(self: &Arc<Self>, peer_id: &PeerId) -> PeerResolverHandle {
        let handle = match self.weak_network.upgrade() {
            Some(handle) => handle.known_peers().make_handle(peer_id, false),
            None => {
                return PeerResolverHandle::new(
                    JoinTask::new(futures_util::future::ready(())),
                    Arc::new(PeerResolverHandleData::new(peer_id, None)),
                    self,
                );
            }
        };
        let next_update_at = handle
            .as_ref()
            .map(|handle| self.compute_update_at(&handle.peer_info()));

        let data = Arc::new(PeerResolverHandleData::new(peer_id, handle));

        PeerResolverHandle::new(
            JoinTask::new(self.clone().run_task(data.clone(), next_update_at)),
            data,
            self,
        )
    }

    async fn run_task(
        self: Arc<Self>,
        data: Arc<PeerResolverHandleData>,
        mut next_update_at: Option<u32>,
    ) {
        tracing::trace!(peer_id = %data.peer_id, "peer resolver task started");

        // TODO: Select between the loop body and `KnownPeers` update event.
        loop {
            // Wait if needed.
            if let Some(update_at) = next_update_at {
                let update_at = std::time::UNIX_EPOCH + Duration::from_secs(update_at as u64);
                let now = std::time::SystemTime::now();
                if let Ok(remaining) = update_at.duration_since(now) {
                    tokio::time::sleep(remaining).await;
                }
            }

            // Start resolving peer.
            match self.resolve_peer(&data).await {
                Some((network, peer_info)) => {
                    let mut handle = data.handle.lock().unwrap();

                    let peer_info_guard;
                    let peer_info = match &*handle {
                        // TODO: Force write into known peers to keep the handle in it?
                        Some(handle) => match handle.update_peer_info(&peer_info) {
                            Ok(()) => peer_info.as_ref(),
                            Err(KnownPeersError::OutdatedInfo) => {
                                peer_info_guard = handle.peer_info();
                                peer_info_guard.as_ref()
                            }
                            // TODO: Allow resuming task after ban?
                            Err(KnownPeersError::PeerBanned(PeerBannedError)) => break,
                        },
                        None => match network
                            .known_peers()
                            .insert_allow_outdated(peer_info, false)
                        {
                            Ok(new_handle) => {
                                peer_info_guard = handle.insert(new_handle).peer_info();
                                data.mark_resolved();
                                peer_info_guard.as_ref()
                            }
                            // TODO: Allow resuming task after ban?
                            Err(PeerBannedError) => break,
                        },
                    };

                    next_update_at = Some(self.compute_update_at(peer_info));
                }
                None => break,
            }
        }

        tracing::trace!(peer_id = %data.peer_id, "peer resolver task finished");
    }

    /// Returns a verified peer info with the strong reference to the network.
    /// Or `None` if network no longer exists.
    async fn resolve_peer(
        &self,
        data: &PeerResolverHandleData,
    ) -> Option<(Network, Arc<PeerInfo>)> {
        struct Iter<'a> {
            backoff: Option<exponential_backoff::Iter<'a>>,
            data: &'a PeerResolverHandleData,
            stale_retry_interval: &'a Duration,
        }

        impl Iterator for Iter<'_> {
            type Item = Duration;

            fn next(&mut self) -> Option<Self::Item> {
                Some(loop {
                    match self.backoff.as_mut() {
                        // Get next duration from the backoff iterator.
                        Some(backoff) => match backoff.next() {
                            // Use it for the first attempts.
                            Some(duration) => break duration,
                            // Set `is_stale` flag on last attempt and continue wih only
                            // the `stale_retry_interval` for all subsequent iterations.
                            None => {
                                self.data.set_stale(true);
                                self.backoff = None;
                            }
                        },
                        // Use `stale_retry_interval` after the max retry count is reached.
                        None => break *self.stale_retry_interval,
                    }
                })
            }
        }

        let backoff = Backoff::new(
            self.config.fast_retry_count,
            self.config.min_retry_interval,
            Some(self.config.max_retry_interval),
        );
        let mut iter = Iter {
            backoff: Some(backoff.iter()),
            data,
            stale_retry_interval: &self.config.stale_retry_interval,
        };

        // "Fast" path
        let mut attempts = 0usize;
        loop {
            attempts += 1;
            let is_stale = attempts > self.config.fast_retry_count as usize;

            // NOTE: Acquire network ref only during the operation.
            {
                let network = self.weak_network.upgrade()?;
                let dht_client = self.dht_service.make_client(network.clone());

                let res = {
                    let _permit = self.semaphore.acquire().await.unwrap();
                    dht_client
                        .entry(dht::PeerValueKeyName::NodeInfo)
                        .find_value::<PeerInfo>(&data.peer_id)
                        .await
                };

                let now = now_sec();
                match res {
                    // TODO: Should we move signature check into the `spawn_blocking`?
                    Ok(peer_info) if peer_info.id == data.peer_id && peer_info.is_valid(now) => {
                        return Some((network, Arc::new(peer_info)));
                    }
                    Ok(_) => {
                        tracing::trace!(
                            peer_id = %data.peer_id,
                            attempts,
                            is_stale,
                            "received an invalid peer info",
                        );
                    }
                    Err(e) => {
                        tracing::trace!(
                            peer_id = %data.peer_id,
                            attempts,
                            is_stale,
                            "failed to resolve a peer info: {e:?}",
                        );
                    }
                }
            }

            let interval = iter.next().expect("retries iterator must be infinite");
            tokio::time::sleep(interval).await;
        }
    }

    fn compute_update_at(&self, peer_info: &PeerInfo) -> u32 {
        let real_ttl = peer_info
            .expires_at
            .saturating_sub(self.config.update_before_sec)
            .saturating_sub(peer_info.created_at);

        let adjusted_ttl = std::cmp::max(real_ttl, self.config.min_ttl_sec);
        peer_info.created_at.saturating_add(adjusted_ttl)
    }
}

#[derive(Clone)]
#[repr(transparent)]
pub struct PeerResolverHandle {
    inner: ManuallyDrop<Arc<PeerResolverHandleInner>>,
}

impl PeerResolverHandle {
    fn new(
        task: JoinTask<()>,
        data: Arc<PeerResolverHandleData>,
        resolver: &Arc<PeerResolverInner>,
    ) -> Self {
        Self {
            inner: ManuallyDrop::new(Arc::new(PeerResolverHandleInner {
                task,
                data,
                resolver: Arc::downgrade(resolver),
            })),
        }
    }

    pub fn load_handle(&self) -> Option<KnownPeerHandle> {
        self.inner.data.handle.lock().unwrap().clone()
    }

    pub fn is_stale(&self) -> bool {
        self.inner.data.flags.load(Ordering::Acquire) & STALE_FLAG != 0
    }

    pub fn is_resolved(&self) -> bool {
        self.inner.data.flags.load(Ordering::Acquire) & RESOLVED_FLAG != 0
    }

    pub async fn wait_resolved(&self) -> KnownPeerHandle {
        loop {
            let resolved = self.inner.data.notify_resolved.notified();
            if let Some(load_handle) = self.load_handle() {
                break load_handle;
            }
            resolved.await;
        }
    }
}

impl Drop for PeerResolverHandle {
    fn drop(&mut self) {
        // SAFETY: inner value is dropped only once
        let inner = unsafe { ManuallyDrop::take(&mut self.inner) };

        // Remove this entry from the resolver if it was the last strong reference.
        if let Some(inner) = Arc::into_inner(inner) {
            // NOTE: At this point an `Arc` was dropped, so the `Weak` in the resolver
            // addresses only the remaining references.

            if let Some(resolver) = inner.resolver.upgrade() {
                resolver
                    .tasks
                    .remove_if(&inner.data.peer_id, |_, value| value.strong_count() == 0);
            }
        }
    }
}

struct PeerResolverHandleInner {
    task: JoinTask<()>,
    data: Arc<PeerResolverHandleData>,
    resolver: Weak<PeerResolverInner>,
}

struct PeerResolverHandleData {
    peer_id: PeerId,
    handle: Mutex<Option<KnownPeerHandle>>,
    flags: AtomicU32,
    notify_resolved: Notify,
}

impl PeerResolverHandleData {
    fn new(peer_id: &PeerId, handle: Option<KnownPeerHandle>) -> Self {
        let flags = AtomicU32::new(if handle.is_some() { RESOLVED_FLAG } else { 0 });

        Self {
            peer_id: *peer_id,
            handle: Mutex::new(handle),
            flags,
            notify_resolved: Notify::new(),
        }
    }

    fn mark_resolved(&self) {
        self.flags.fetch_or(RESOLVED_FLAG, Ordering::Release);
        self.notify_resolved.notify_waiters();
    }

    fn is_resolved(&self) -> bool {
        self.flags.load(Ordering::Acquire) & RESOLVED_FLAG != 0
    }

    fn set_stale(&self, stale: bool) {
        if stale {
            self.flags.fetch_or(STALE_FLAG, Ordering::Release);
        } else {
            self.flags.fetch_and(!STALE_FLAG, Ordering::Release);
        }
    }

    fn is_stale(&self) -> bool {
        self.flags.load(Ordering::Acquire) & STALE_FLAG != 0
    }
}

const STALE_FLAG: u32 = 0b1;
const RESOLVED_FLAG: u32 = 0b10;
