//! The AI operating layer, in one dependency.
//!
//! This crate is a facade. It contains no logic of its own; it re-exports the kernel and
//! the subsystem contracts so that downstream code depends on `aik` rather than on each
//! crate separately.
//!
//! ```
//! use aik::prelude::*;
//! ```
//!
//! * [`kernel`] — [`aik_core`], the kernel: lifecycle, events, registry, tasks, config,
//!   plugins. This is the part meant to stay still.
//! * [`api`] — [`aik_api`], contracts for models, tools, memory, agents, permissions,
//!   scheduling and platform integration. Provisional, and behind the `api` feature.
//!
//! Depend on `aik-core` directly, or turn off default features, if you want the kernel
//! without the contracts.

pub use aik_core as kernel;

#[cfg(feature = "api")]
pub use aik_api as api;

pub use aik_core::{
    Component, ComponentContext, ComponentDescriptor, ComponentId, Config, Error, Event, Kernel,
    KernelContext, Plugin, Registry, Result, Tasks,
};

/// Everything needed to write a component, a plugin or a subsystem implementation.
pub mod prelude {
    pub use aik_core::prelude::*;

    #[cfg(feature = "api")]
    pub use aik_api::prelude::*;
}
