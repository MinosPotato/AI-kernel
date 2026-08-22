//! The one handle every memory tool holds, and the only thing that is filled in late.
//!
//! Tools are handed to the registry's component before the kernel is built; the store is
//! published by a component during `init`. Something has to bridge those two moments, and
//! this is deliberately the smallest thing that can: one slot, written exactly once, by
//! [`MemoryToolsComponent`](super::MemoryToolsComponent), read by every tool it handed out.
//!
//! It is a *binding*, not a service locator. A tool cannot ask it for anything but the store
//! it was bound to, cannot rebind it, and cannot reach the kernel registry through it — so
//! there is no path from a tool to a capability nobody deliberately gave it.

use std::sync::{Arc, OnceLock};

use aik_api::memory::MemoryStore;
use aik_core::clock::{SharedClock, Timestamp};
use aik_core::{Error, Result};

/// What a bound tool set actually got.
struct Bound {
    store: Arc<dyn MemoryStore>,
    clock: SharedClock,
}

impl std::fmt::Debug for Bound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bound").field("clock", &self.clock).finish()
    }
}

/// The store and clock a set of memory tools share, filled in during component `init`.
#[derive(Debug, Default)]
pub(crate) struct MemoryToolBinding {
    bound: OnceLock<Bound>,
}

impl MemoryToolBinding {
    /// Creates an unbound binding. Every tool built from it refuses to run until it is
    /// bound.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Binds the tools to a store and a clock.
    ///
    /// Binding twice is a wiring error rather than a silent replacement: two components
    /// pointing one tool set at two different stores is a mistake worth failing startup
    /// over, and quietly keeping the first would leave half the wiring meaning something
    /// nobody wrote.
    pub(crate) fn bind(&self, store: Arc<dyn MemoryStore>, clock: SharedClock) -> Result<()> {
        self.bound
            .set(Bound { store, clock })
            .map_err(|_| Error::Lifecycle("memory tools are already bound to a store".to_owned()))
    }

    /// The bound store, or an explanation of the missing wiring.
    ///
    /// Failing closed here is what makes a forgotten
    /// [`MemoryToolsComponent`](super::MemoryToolsComponent) a refused tool call rather than
    /// a panic or, worse, a silently absent memory.
    pub(crate) fn store(&self) -> Result<&Arc<dyn MemoryStore>> {
        Ok(&self.bound()?.store)
    }

    /// The bound clock.
    pub(crate) fn clock(&self) -> Result<&SharedClock> {
        Ok(&self.bound()?.clock)
    }

    /// The current time according to the bound clock.
    pub(crate) fn now(&self) -> Result<Timestamp> {
        Ok(self.clock()?.now())
    }

    fn bound(&self) -> Result<&Bound> {
        self.bound.get().ok_or_else(|| {
            Error::Lifecycle(
                "memory tools are not bound to a store; add `MemoryToolsComponent` to the kernel"
                    .to_owned(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::ErrorKind;
    use aik_core::clock::SystemClock;

    use crate::InMemoryMemoryStore;

    fn store() -> Arc<dyn MemoryStore> {
        Arc::new(InMemoryMemoryStore::new())
    }

    #[test]
    fn an_unbound_binding_refuses_rather_than_panicking() {
        let binding = MemoryToolBinding::new();
        let error = match binding.store() {
            Ok(_) => panic!("nothing is bound"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), ErrorKind::Lifecycle);
        assert!(binding.now().is_err());
    }

    #[test]
    fn binding_twice_is_a_wiring_error() {
        let binding = MemoryToolBinding::new();
        let clock: SharedClock = Arc::new(SystemClock);
        binding.bind(store(), clock.clone()).expect("first bind");
        let error = binding.bind(store(), clock).expect_err("second bind");
        assert_eq!(error.kind(), ErrorKind::Lifecycle);
    }
}
