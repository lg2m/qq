use std::{hint::black_box, time::Instant};

use qq_provider::{
    EndpointSpec, HttpAuth, HttpProtocol, HttpProviderRecipe, ProviderCompiler, ProviderRecipe,
};

const DEFAULT_ITERATIONS: u64 = 25_000;

fn main() {
    let iterations = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    let compiler = ProviderCompiler::new().expect("HTTP clients must initialize");

    for _ in 0..1_000 {
        black_box(
            compiler
                .compile(openai_recipe())
                .expect("recipe must compile"),
        );
    }

    let started = Instant::now();
    for _ in 0..iterations {
        black_box(
            compiler
                .compile(openai_recipe())
                .expect("recipe must compile"),
        );
    }
    let elapsed = started.elapsed();
    let nanos_per_iteration = elapsed.as_nanos() / u128::from(iterations);

    println!(
        "provider_recipe_compile: {nanos_per_iteration} ns/iteration ({iterations} iterations)"
    );
}

fn openai_recipe() -> ProviderRecipe {
    ProviderRecipe::http(HttpProviderRecipe::new(
        EndpointSpec::exact("https://api.openai.com/v1/responses", false),
        HttpProtocol::OpenAiResponses,
        HttpAuth::ApiKey("benchmark-key".to_owned()),
    ))
}
