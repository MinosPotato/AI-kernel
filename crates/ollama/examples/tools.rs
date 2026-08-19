//! Runs a complete tool-calling exchange against a local Ollama server.
//!
//! The counterpart to the `chat` example, one level up: the model is offered a tool, asks
//! for it, is given a result, and answers from it. Nothing here authorizes anything or runs
//! a real tool — this is the provider in isolation, so the "tool" is a constant. What it
//! proves is the part that only a real server can prove: that the wire format this crate
//! builds is the one Ollama accepts, in both directions.
//!
//! Requires a running Ollama instance (`ollama serve`) with a **tool-capable** model pulled.
//! `ollama list` does not say which those are; `/api/tags` reports a `tools` capability, and
//! a model without it will simply answer in prose instead of calling anything.
//!
//! ```text
//! cargo run -p aik-ollama --example tools
//! cargo run -p aik-ollama --example tools -- qwen3:8b
//! ```

use aik_api::execution::ExecutionContext;
use aik_api::model::{
    CompletionRequest, ContentPart, Message, ModelProvider, Role, ToolDefinition,
};
use aik_api::tool::ToolName;
use aik_core::prelude::*;
use aik_ollama::OllamaComponent;
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let kernel = Kernel::builder()
        .component(OllamaComponent::new())
        .build()?;
    kernel.start().await?;

    let outcome = exchange(&kernel.context()).await;
    kernel.shutdown().await?;

    if let Err(error) = outcome {
        println!("Could not complete a tool-calling exchange against Ollama: {error}");
        println!("Is `ollama serve` running, with a tool-capable model pulled?");
        return Ok(());
    }

    Ok(())
}

fn weather_tool() -> ToolDefinition {
    ToolDefinition::new(
        "get_weather",
        "Get the current weather for a city.",
        json!({
            "type": "object",
            "properties": { "city": { "type": "string", "description": "The city name" } },
            "required": ["city"],
        }),
    )
}

async fn exchange(ctx: &KernelContext) -> Result<()> {
    let provider = ctx.service::<dyn ModelProvider>()?;

    let model = match std::env::args().nth(1) {
        Some(model) => model,
        None => provider
            .models()
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| Error::other("no models are installed on this Ollama server"))?
            .id
            .to_string(),
    };

    println!("model: {model}");
    println!("tool:  get_weather(city)");

    // --- turn one: the model is offered a tool and asks for it ---------------------------

    let mut messages = vec![Message::text(
        Role::User,
        "What is the weather in Paris? Use the tool.",
    )];
    let mut request = CompletionRequest::new(model.clone(), messages.clone());
    request.tools.push(weather_tool());

    let first = provider.complete(request, &ExecutionContext::new()).await?;
    println!("\nturn 1 finished: {:?}", first.finish_reason);

    let calls: Vec<_> = first
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect();

    if calls.is_empty() {
        println!("the model answered without calling anything:");
        println!("  {}", text_of(&first.message));
        println!("\n(that is a model choice, not a provider failure — try a model whose");
        println!("`/api/tags` capabilities include `tools`.)");
        return Ok(());
    }

    for call in &calls {
        println!("  call {}: {} {}", call.call_id, call.name, call.arguments);
    }

    // --- turn two: the results go back, and the model answers from them -------------------

    messages.push(first.message);
    for call in &calls {
        // A real system runs the tool through `ToolRegistry::invoke` here, which is where
        // authorization and approval happen. A constant stands in for all of that.
        let output = if call.name == ToolName::new("get_weather") {
            json!({ "temperature_c": 19, "conditions": "light rain" })
        } else {
            json!({ "error": "no such tool" })
        };
        messages.push(Message {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                call_id: call.call_id.clone(),
                content: output,
                is_error: false,
            }],
            name: None,
        });
    }

    let mut request = CompletionRequest::new(model, messages);
    request.tools.push(weather_tool());
    let second = provider.complete(request, &ExecutionContext::new()).await?;

    println!("\nturn 2 finished: {:?}", second.finish_reason);
    println!("answer: {}", text_of(&second.message));

    Ok(())
}

fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}
