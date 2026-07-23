use std::{hint::black_box, time::Instant};

use qq_provider::{
    EndpointSpec, HttpAuth, HttpProtocol, HttpProviderRecipe, ModelRequest, Provider,
    ProviderCompiler, ProviderRecipe, bedrock::BedrockAuth,
};

const DEFAULT_ITERATIONS: u64 = 25_000;

fn main() {
    let iterations = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    let compiler = ProviderCompiler::new().expect("HTTP clients must initialize");

    for (name, recipe) in [
        (
            "provider_recipe_compile",
            openai_recipe as fn() -> ProviderRecipe,
        ),
        ("google_recipe_compile", google_recipe),
        ("mantle_recipe_compile", mantle_recipe),
    ] {
        benchmark(&compiler, iterations, name, recipe);
    }
    benchmark_mantle_warm_dispatch(&compiler, iterations);
}

fn benchmark(
    compiler: &ProviderCompiler,
    iterations: u64,
    name: &str,
    recipe: fn() -> ProviderRecipe,
) {
    for _ in 0..1_000 {
        black_box(compiler.compile(recipe()).expect("recipe must compile"));
    }
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(compiler.compile(recipe()).expect("recipe must compile"));
    }
    let elapsed = started.elapsed();
    let nanos_per_iteration = elapsed.as_nanos() / u128::from(iterations);

    println!("{name}: {nanos_per_iteration} ns/iteration ({iterations} iterations)");
}

fn openai_recipe() -> ProviderRecipe {
    ProviderRecipe::http(HttpProviderRecipe::new(
        EndpointSpec::exact("https://api.openai.com/v1/responses", false),
        HttpProtocol::OpenAiResponses,
        HttpAuth::ApiKey("benchmark-key".to_owned()),
    ))
}

fn google_recipe() -> ProviderRecipe {
    ProviderRecipe::http(HttpProviderRecipe::new(
        EndpointSpec::base("https://generativelanguage.googleapis.com/v1beta", false),
        HttpProtocol::GoogleGenerateContent,
        HttpAuth::ApiKey("benchmark-key".to_owned()),
    ))
}

fn mantle_recipe() -> ProviderRecipe {
    ProviderRecipe::amazon_bedrock_mantle(
        Some("us-east-1".to_owned()),
        HttpProtocol::OpenAiResponses,
        BedrockAuth::DefaultChain,
    )
}

fn benchmark_mantle_warm_dispatch(compiler: &ProviderCompiler, iterations: u64) {
    let provider = compiler
        .compile(ProviderRecipe::amazon_bedrock_mantle(
            Some("us-east-1".to_owned()),
            HttpProtocol::OpenAiResponses,
            BedrockAuth::ApiKey("benchmark-key".to_owned()),
        ))
        .expect("Mantle recipe must compile");
    let request = ModelRequest::new("benchmark-model", Vec::new(), 64);

    for _ in 0..1_000 {
        drop(black_box(provider.stream(request.clone())));
    }
    let started = Instant::now();
    for _ in 0..iterations {
        drop(black_box(provider.stream(request.clone())));
    }
    let nanos_per_iteration = started.elapsed().as_nanos() / u128::from(iterations);

    println!("mantle_warm_dispatch: {nanos_per_iteration} ns/iteration ({iterations} iterations)");
}
