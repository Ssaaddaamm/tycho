use std::fmt::Debug;

use anyhow::Result;

use super::cache_persistent::PersistentCacheService;
use super::queue::MessageQueueImpl;
use super::storage::StorageService;

pub trait PersistentStateService: Debug + Sized {
    fn new() -> Result<Self>;
}

// This part of the code contains logic of working with persistent state.
//
// We use partials just to separate the codebase on smaller and easier maintainable parts.
impl<CH, ST, DB> MessageQueueImpl<CH, ST, DB>
where
    CH: PersistentCacheService,
    ST: PersistentStateService,
    DB: StorageService,
{
    fn _some_internal_method_for_persistent_state(&mut self) -> Result<()> {
        todo!()
    }
    pub(super) fn _some_module_internal_method_for_persistent_state(&mut self) -> Result<()> {
        todo!()
    }
}

// STUBS

#[derive(Debug)]
pub struct PersistentStateServiceStubImpl {}
impl PersistentStateService for PersistentStateServiceStubImpl {
    fn new() -> Result<Self> {
        Ok(Self {})
    }
}
