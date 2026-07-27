use std::{hint::black_box, time::Instant};

use futures_util::{StreamExt, stream};
use qq_core::Runtime;
use qq_protocol::RunCommand;
use qq_provider::{ContentBlock, ModelRequest, Provider, ProviderEvent, ProviderStream};

const DEFAULT_ITERATIONS: u64 = 1_000;

struct ReadToolProvider;

impl Provider for ReadToolProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let has_result = request
            .messages()
            .iter()
            .flat_map(|message| message.content())
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
        if has_result {
            Box::pin(stream::iter([Ok(ProviderEvent::Completed { usage: None })]))
        } else {
            Box::pin(stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: "benchmark-call".to_owned(),
                    name: "read_file".to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: "benchmark-call".to_owned(),
                    json: r#"{"path":"input.txt"}"#.to_owned(),
                }),
                Ok(ProviderEvent::ToolCallCompleted {
                    id: "benchmark-call".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }
}

fn main() {
    let iterations = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    let directory = tempfile::tempdir().expect("temporary workspace must be created");
    std::fs::write(directory.path().join("input.txt"), "benchmark\n")
        .expect("benchmark input must be written");
    let runtime =
        Runtime::new(ReadToolProvider, "benchmark-model", 64).expect("runtime must be configured");
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Tokio runtime must initialize");

    for _ in 0..10 {
        tokio.block_on(run_once(&runtime, directory.path().to_owned()));
    }
    let started = Instant::now();
    for _ in 0..iterations {
        tokio.block_on(run_once(&runtime, directory.path().to_owned()));
    }
    let nanos_per_iteration = started.elapsed().as_nanos() / u128::from(iterations);
    println!("read_tool_loop: {nanos_per_iteration} ns/iteration ({iterations} iterations)");
}

async fn run_once(runtime: &Runtime, workspace: std::path::PathBuf) {
    let events = runtime
        .run_in_workspace(RunCommand::new("read the input"), workspace)
        .collect::<Vec<_>>()
        .await;
    black_box(events);
}
