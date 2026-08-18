//! Sends a real chat request to a local Ollama server and streams the response.
//!
//! This is the vertical slice end to end: kernel → registry → `ModelProvider` → Ollama →
//! a real model → a streamed response, run through nothing but public APIs.
//!
//! Requires a running Ollama instance (`ollama serve`) with at least one model pulled
//! (e.g. `ollama pull llama3.2`). If Ollama is not reachable, or has no models installed,
//! this prints an explanation and exits successfully — that is an environment problem, not
//! a failure of the kernel or the provider, so it must not look like one.
//!
//! ```text
//! cargo run -p aik-ollama --example chat
//! cargo run -p aik-ollama --example chat -- mistral "what is a kernel?"
//! ```

use std::io::Write as _;

use aik_api::execution::ExecutionContext;
use aik_api::model::{
    CompletionChunk, CompletionRequest, ContentPart, Message, ModelProvider, Role,
};
use aik_core::prelude::*;
use aik_ollama::OllamaComponent;
use futures::StreamExt;

#[tokio::main]
async fn main() -> Result<()> {
    // A kernel with exactly one component. Nothing here mentions HTTP or Ollama's wire
    // format — that is entirely inside `aik-ollama`.
    let kernel = Kernel::builder()
        .component(OllamaComponent::new())
        .build()?;
    kernel.start().await?;

    let outcome = chat(&kernel.context()).await;

    // Shut down cleanly regardless of how the chat attempt went.
    kernel.shutdown().await?;

    if let Err(error) = outcome {
        println!("Could not complete a chat request against Ollama: {error}");
        println!("Is `ollama serve` running, with a model pulled (e.g. `ollama pull llama3.2`)?");
        return Ok(());
    }

    Ok(())
}

async fn chat(ctx: &KernelContext) -> Result<()> {
    // Resolved by capability. Nothing here names `OllamaProvider`.
    let provider = ctx.service::<dyn ModelProvider>()?;

    let mut args = std::env::args().skip(1);
    let requested_model = args.next();
    let prompt = args
        .next()
        .unwrap_or_else(|| "Say hello in five words or fewer.".to_owned());

    let model = match requested_model {
        Some(model) => model,
        None => {
            let models = provider.models().await?;
            models
                .into_iter()
                .next()
                .ok_or_else(|| Error::other("no models are installed on this Ollama server"))?
                .id
                .to_string()
        }
    };

    println!("model:    {model}");
    println!("prompt:   {prompt}");
    print!("response: ");
    std::io::stdout().flush().ok();

    let request = CompletionRequest::new(model, vec![Message::text(Role::User, prompt)]);
    let mut chunks = provider.stream(request, &ExecutionContext::new()).await?;

    while let Some(chunk) = chunks.next().await {
        match chunk? {
            CompletionChunk::Delta(ContentPart::Text { text }) => {
                print!("{text}");
                std::io::stdout().flush().ok();
            }
            CompletionChunk::Delta(_) => {}
            CompletionChunk::ToolCall(_) => {}
            CompletionChunk::Done {
                finish_reason,
                usage,
            } => {
                println!("\n\n(finished: {finish_reason:?}, usage: {usage:?})");
            }
        }
    }

    Ok(())
}
