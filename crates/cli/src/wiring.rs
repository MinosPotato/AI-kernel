//! Assembling a kernel, from a terminal run's resolved [`Settings`].
//!
//! One function, and it delegates. The assembly itself lives in
//! [`aik_runtime::wiring`] because there are now two frontends that need it and only one
//! description of a deployment that either may use — see that module for what it decides and,
//! more importantly, what it is not allowed to decide.
//!
//! What stays here is the translation: a terminal run's [`Settings`] carry a prompt, a
//! verbosity and a place to write measurements, none of which change how the system is put
//! together. Stripping them off is the whole of this module.

use aik_api::model::ModelId;
use aik_core::prelude::*;

pub use aik_runtime::wiring::{Assembled, POLICY_SECTION};

use crate::settings::Settings;

/// Builds every component the frontend owns *except* the model provider.
///
/// Split out so the same wiring can be started against a stub provider in tests: the model
/// is the one collaborator that needs a server.
pub fn builder(
    settings: &Settings,
    model: ModelId,
) -> Result<(KernelBuilder, std::sync::Arc<aik_approval::ApprovalBroker>)> {
    aik_runtime::wiring::builder(&settings.runtime, model)
}

/// Builds the frontend's kernel, with the Ollama provider as its model source.
pub fn assemble(settings: &Settings, model: ModelId) -> Result<Assembled> {
    aik_runtime::wiring::assemble(&settings.runtime, model)
}

/// Starts a throwaway kernel holding only the model provider, to ask it what it serves.
pub async fn first_available_model(settings: &Settings) -> Result<ModelId> {
    aik_runtime::wiring::first_available_model(&settings.runtime).await
}
