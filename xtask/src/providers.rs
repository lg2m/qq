use std::{
    env, io,
    process::{Command as ProcessCommand, ExitCode},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use futures_util::StreamExt;
use qq_auth::CredentialStore;
use qq_provider::{
    BedrockAuth, EndpointSpec, HttpAuth, HttpProtocol, HttpProviderRecipe, Message, ModelRequest,
    Provider, ProviderCompiler, ProviderError, ProviderEvent, ProviderRecipe,
};
use serde_json::json;
use thiserror::Error;
use tokio::time::{Duration, Instant, timeout_at};

const LIVE_OPT_IN: &str = "QQ_LIVE_PROVIDER_TESTS";
const SMOKE_MARKER: &str = "QQ_PROVIDER_SMOKE_OK";
const FIRST_TOKEN_TIMEOUT: Duration = Duration::from_secs(20);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(45);

const CASES: &[CanaryCase] = &[
    CanaryCase {
        id: "openai-responses",
        provider: ProviderName::OpenAi,
        protocol: "responses",
        auth: "bearer-api-key",
        model: "gpt-4.1-mini",
        model_env: "QQ_CANARY_OPENAI_MODEL",
        recipe: RecipeKind::Http {
            endpoint: "https://api.openai.com/v1/responses",
            endpoint_kind: EndpointKind::Exact,
            protocol: HttpProtocol::OpenAiResponses,
            credential: CredentialKind::Environment("OPENAI_API_KEY"),
        },
    },
    CanaryCase {
        id: "openai-codex-responses",
        provider: ProviderName::OpenAiCodex,
        protocol: "responses",
        auth: "request-time-codex-oauth",
        model: "gpt-5.4-mini",
        model_env: "QQ_CANARY_OPENAI_CODEX_MODEL",
        recipe: RecipeKind::Http {
            endpoint: "https://chatgpt.com/backend-api/codex/responses",
            endpoint_kind: EndpointKind::Exact,
            protocol: HttpProtocol::OpenAiResponses,
            credential: CredentialKind::OpenAiCodex,
        },
    },
    CanaryCase {
        id: "anthropic-messages",
        provider: ProviderName::Anthropic,
        protocol: "messages",
        auth: "x-api-key",
        model: "claude-haiku-4-5",
        model_env: "QQ_CANARY_ANTHROPIC_MODEL",
        recipe: RecipeKind::Http {
            endpoint: "https://api.anthropic.com/v1/messages",
            endpoint_kind: EndpointKind::Exact,
            protocol: HttpProtocol::AnthropicMessages,
            credential: CredentialKind::Environment("ANTHROPIC_API_KEY"),
        },
    },
    CanaryCase {
        id: "google-generate-content",
        provider: ProviderName::Google,
        protocol: "generate-content",
        auth: "x-goog-api-key",
        model: "gemini-2.5-flash",
        model_env: "QQ_CANARY_GOOGLE_MODEL",
        recipe: RecipeKind::Http {
            endpoint: "https://generativelanguage.googleapis.com/v1beta",
            endpoint_kind: EndpointKind::Base,
            protocol: HttpProtocol::GoogleGenerateContent,
            credential: CredentialKind::Environment("GEMINI_API_KEY"),
        },
    },
    CanaryCase {
        id: "xai-responses",
        provider: ProviderName::XAi,
        protocol: "responses",
        auth: "request-time-bearer-via-qq-auth",
        model: "grok-4.5",
        model_env: "QQ_CANARY_XAI_RESPONSES_MODEL",
        recipe: RecipeKind::Http {
            endpoint: "https://api.x.ai/v1",
            endpoint_kind: EndpointKind::Base,
            protocol: HttpProtocol::OpenAiResponses,
            credential: CredentialKind::XAi,
        },
    },
    CanaryCase {
        id: "xai-chat-completions",
        provider: ProviderName::XAi,
        protocol: "chat-completions",
        auth: "request-time-bearer-via-qq-auth",
        model: "grok-4.3",
        model_env: "QQ_CANARY_XAI_CHAT_MODEL",
        recipe: RecipeKind::Http {
            endpoint: "https://api.x.ai/v1",
            endpoint_kind: EndpointKind::Base,
            protocol: HttpProtocol::OpenAiChatCompletions,
            credential: CredentialKind::XAi,
        },
    },
    CanaryCase {
        id: "amazon-bedrock-converse-stream",
        provider: ProviderName::AmazonBedrock,
        protocol: "converse-stream",
        auth: "default-aws-chain",
        model: "us.anthropic.claude-haiku-4-5-20251001-v1:0",
        model_env: "QQ_CANARY_BEDROCK_MODEL",
        recipe: RecipeKind::AmazonBedrock,
    },
    CanaryCase {
        id: "bedrock-mantle-responses",
        provider: ProviderName::BedrockMantle,
        protocol: "responses",
        auth: "default-aws-chain-sigv4",
        model: "openai.gpt-oss-120b",
        model_env: "QQ_CANARY_MANTLE_RESPONSES_MODEL",
        recipe: RecipeKind::BedrockMantle(HttpProtocol::OpenAiResponses),
    },
    CanaryCase {
        id: "bedrock-mantle-chat-completions",
        provider: ProviderName::BedrockMantle,
        protocol: "chat-completions",
        auth: "default-aws-chain-sigv4",
        model: "openai.gpt-oss-120b",
        model_env: "QQ_CANARY_MANTLE_CHAT_MODEL",
        recipe: RecipeKind::BedrockMantle(HttpProtocol::OpenAiChatCompletions),
    },
    CanaryCase {
        id: "bedrock-mantle-anthropic-messages",
        provider: ProviderName::BedrockMantle,
        protocol: "anthropic-messages",
        auth: "default-aws-chain-sigv4",
        model: "anthropic.claude-haiku-4-5-20251001-v1:0",
        model_env: "QQ_CANARY_MANTLE_ANTHROPIC_MODEL",
        recipe: RecipeKind::BedrockMantle(HttpProtocol::AnthropicMessages),
    },
];

#[derive(Debug, Parser)]
#[command(name = "cargo xtask", about = "Repository maintenance tasks for QQ")]
struct Cli {
    #[command(subcommand)]
    command: Task,
}

#[derive(Debug, Subcommand)]
enum Task {
    /// Run provider validation tasks.
    Providers(ProvidersArgs),
}

#[derive(Debug, Args)]
struct ProvidersArgs {
    #[command(subcommand)]
    command: ProvidersCommand,
}

#[derive(Debug, Subcommand)]
enum ProvidersCommand {
    /// Run an offline or credentialed provider check.
    Check(CheckArgs),
}

#[derive(Debug, Args)]
struct CheckArgs {
    #[command(subcommand)]
    mode: CheckMode,
}

#[derive(Debug, Subcommand)]
enum CheckMode {
    /// Run the deterministic provider package gate.
    Offline,
    /// Run bounded, single-attempt live provider canaries.
    Live(LiveArgs),
}

#[derive(Debug, Args)]
struct LiveArgs {
    /// Provider deployment to check. May be repeated.
    #[arg(long, value_enum)]
    provider: Vec<ProviderName>,
    /// Check every row in the executable canary matrix.
    #[arg(long)]
    all: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ProviderName {
    #[value(name = "openai")]
    OpenAi,
    #[value(name = "openai-codex")]
    OpenAiCodex,
    Anthropic,
    Google,
    #[value(name = "xai")]
    XAi,
    #[value(name = "amazon-bedrock")]
    AmazonBedrock,
    #[value(name = "bedrock-mantle")]
    BedrockMantle,
}

impl ProviderName {
    const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::OpenAiCodex => "openai-codex",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
            Self::XAi => "xai",
            Self::AmazonBedrock => "amazon-bedrock",
            Self::BedrockMantle => "bedrock-mantle",
        }
    }
}

#[derive(Clone, Copy)]
enum EndpointKind {
    Base,
    Exact,
}

#[derive(Clone, Copy)]
enum CredentialKind {
    Environment(&'static str),
    OpenAiCodex,
    XAi,
}

#[derive(Clone, Copy)]
enum RecipeKind {
    Http {
        endpoint: &'static str,
        endpoint_kind: EndpointKind,
        protocol: HttpProtocol,
        credential: CredentialKind,
    },
    AmazonBedrock,
    BedrockMantle(HttpProtocol),
}

struct CanaryCase {
    id: &'static str,
    provider: ProviderName,
    protocol: &'static str,
    auth: &'static str,
    model: &'static str,
    model_env: &'static str,
    recipe: RecipeKind,
}

struct ProbeResult {
    outcome: &'static str,
    marker: bool,
    event_count: u64,
    output_bytes: usize,
    first_token_ms: Option<u64>,
    total_ms: u64,
    error_kind: Option<String>,
}

impl ProbeResult {
    fn failure(
        started: Instant,
        marker: bool,
        event_count: u64,
        output_bytes: usize,
        first_token_ms: Option<u64>,
        error_kind: impl Into<String>,
    ) -> Self {
        Self {
            outcome: "fail",
            marker,
            event_count,
            output_bytes,
            first_token_ms,
            total_ms: elapsed_millis(started),
            error_kind: Some(error_kind.into()),
        }
    }
}

enum SetupError {
    Skip(&'static str),
    Infrastructure(&'static str),
}

#[derive(Debug, Error)]
enum XtaskError {
    #[error("choose one or more --provider values or --all, but not both")]
    InvalidSelection,
    #[error("live provider checks require {LIVE_OPT_IN}=1")]
    LiveOptInRequired,
    #[error("failed to launch the offline provider gate")]
    OfflineLaunch(#[source] io::Error),
    #[error("offline provider gate failed with status {0:?}")]
    OfflineFailed(Option<i32>),
    #[error("provider compiler construction failed")]
    Compiler(#[source] ProviderError),
    #[error("one or more live provider checks did not pass")]
    LiveFailed,
}

pub async fn run() -> ExitCode {
    match try_run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn try_run(cli: Cli) -> Result<(), XtaskError> {
    match cli.command {
        Task::Providers(args) => match args.command {
            ProvidersCommand::Check(args) => match args.mode {
                CheckMode::Offline => run_offline(),
                CheckMode::Live(args) => run_live(args).await,
            },
        },
    }
}

fn run_offline() -> Result<(), XtaskError> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = ProcessCommand::new(cargo)
        .args(["test", "-p", "qq-provider"])
        .status()
        .map_err(XtaskError::OfflineLaunch)?;
    if !status.success() {
        return Err(XtaskError::OfflineFailed(status.code()));
    }
    Ok(())
}

async fn run_live(args: LiveArgs) -> Result<(), XtaskError> {
    if env::var(LIVE_OPT_IN).ok().as_deref() != Some("1") {
        return Err(XtaskError::LiveOptInRequired);
    }
    let selected = selected_cases(&args)?;
    let compiler = ProviderCompiler::new().map_err(XtaskError::Compiler)?;
    let needs_store = selected.iter().any(|case| {
        matches!(
            case.recipe,
            RecipeKind::Http {
                credential: CredentialKind::OpenAiCodex | CredentialKind::XAi,
                ..
            }
        )
    });
    let store = needs_store.then(CredentialStore::system).transpose();
    let commit = git_commit();
    let timestamp = unix_timestamp();
    let mut all_passed = true;

    for case in selected {
        let recipe = match case.recipe(store.as_ref().ok().and_then(Option::as_ref)) {
            Ok(recipe) => recipe,
            Err(SetupError::Skip(reason)) => {
                print_setup_result(case, timestamp, &commit, "skip", reason);
                all_passed = false;
                continue;
            }
            Err(SetupError::Infrastructure(reason)) => {
                print_setup_result(case, timestamp, &commit, "infrastructure-error", reason);
                all_passed = false;
                continue;
            }
        };
        let provider = match compiler.compile_for_canary(recipe) {
            Ok(provider) => provider,
            Err(error) => {
                print_setup_result(
                    case,
                    timestamp,
                    &commit,
                    "fail",
                    provider_error_kind(&error),
                );
                all_passed = false;
                continue;
            }
        };
        let result = probe(provider, case.model()).await;
        print_probe_result(case, timestamp, &commit, &result);
        all_passed &= result.outcome == "pass";
    }

    if all_passed {
        Ok(())
    } else {
        Err(XtaskError::LiveFailed)
    }
}

fn selected_cases(args: &LiveArgs) -> Result<Vec<&'static CanaryCase>, XtaskError> {
    if args.all == !args.provider.is_empty() {
        return Err(XtaskError::InvalidSelection);
    }
    if args.all {
        return Ok(CASES.iter().collect());
    }
    Ok(CASES
        .iter()
        .filter(|case| args.provider.contains(&case.provider))
        .collect())
}

impl CanaryCase {
    fn model(&self) -> String {
        env::var(self.model_env)
            .ok()
            .filter(|model| !model.trim().is_empty())
            .unwrap_or_else(|| self.model.to_owned())
    }

    fn recipe(&self, store: Option<&CredentialStore>) -> Result<ProviderRecipe, SetupError> {
        match self.recipe {
            RecipeKind::Http {
                endpoint,
                endpoint_kind,
                protocol,
                credential,
            } => {
                let auth = match credential {
                    CredentialKind::Environment(name) => {
                        let value = env::var(name)
                            .ok()
                            .filter(|value| !value.trim().is_empty())
                            .ok_or(SetupError::Skip("credential-unavailable"))?;
                        HttpAuth::ApiKey(value.into())
                    }
                    CredentialKind::OpenAiCodex => {
                        let store = store
                            .ok_or(SetupError::Infrastructure("credential-store-unavailable"))?;
                        let present = store
                            .status("openai-codex/default")
                            .map_err(|_| SetupError::Infrastructure("credential-check-failed"))?
                            .is_some();
                        if !present {
                            return Err(SetupError::Skip("credential-unavailable"));
                        }
                        HttpAuth::RequestTimeCodex(store.codex_request_credentials("default"))
                    }
                    CredentialKind::XAi => {
                        let store = store
                            .ok_or(SetupError::Infrastructure("credential-store-unavailable"))?;
                        let environment_present = env::var("XAI_API_KEY")
                            .ok()
                            .is_some_and(|value| !value.trim().is_empty());
                        if !environment_present {
                            let stored_present = store
                                .status("xai/default")
                                .map_err(|_| SetupError::Infrastructure("credential-check-failed"))?
                                .is_some();
                            if !stored_present {
                                return Err(SetupError::Skip("credential-unavailable"));
                            }
                        }
                        HttpAuth::RequestTimeBearer(store.xai_request_credentials("default", None))
                    }
                };
                let endpoint = match endpoint_kind {
                    EndpointKind::Base => EndpointSpec::base(endpoint, false),
                    EndpointKind::Exact => EndpointSpec::exact(endpoint, false),
                };
                Ok(ProviderRecipe::http(HttpProviderRecipe::new(
                    endpoint, protocol, auth,
                )))
            }
            RecipeKind::AmazonBedrock => Ok(ProviderRecipe::amazon_bedrock(
                canary_region(),
                BedrockAuth::DefaultChain,
            )),
            RecipeKind::BedrockMantle(protocol) => Ok(ProviderRecipe::amazon_bedrock_mantle(
                canary_region(),
                protocol,
                BedrockAuth::DefaultChain,
            )),
        }
    }
}

async fn probe(provider: Arc<dyn Provider>, model: String) -> ProbeResult {
    let started = Instant::now();
    let first_token_deadline = started + FIRST_TOKEN_TIMEOUT;
    let total_deadline = started + TOTAL_TIMEOUT;
    let mut stream = provider.stream(ModelRequest::new(
        model,
        vec![Message::user(format!("Reply only with {SMOKE_MARKER}"))],
        32,
    ));
    let mut output = String::new();
    let mut output_bytes = 0_usize;
    let mut event_count = 0_u64;
    let mut completed = 0_u8;
    let mut first_token_ms = None;

    loop {
        let deadline = if first_token_ms.is_some() {
            total_deadline
        } else {
            first_token_deadline
        };
        let event = match timeout_at(deadline, stream.next()).await {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(_) => {
                let kind = if first_token_ms.is_some() {
                    "total-timeout"
                } else {
                    "first-token-timeout"
                };
                return ProbeResult::failure(
                    started,
                    output.contains(SMOKE_MARKER),
                    event_count,
                    output_bytes,
                    first_token_ms,
                    kind,
                );
            }
        };
        event_count = event_count.saturating_add(1);
        match event {
            Ok(ProviderEvent::OutputTextDelta { text }) => {
                if !text.is_empty() && first_token_ms.is_none() {
                    first_token_ms = Some(elapsed_millis(started));
                }
                output_bytes = output_bytes.saturating_add(text.len());
                output.push_str(&text);
            }
            Ok(ProviderEvent::Completed { .. }) => {
                completed = completed.saturating_add(1);
            }
            Ok(_) => {}
            Err(error) => {
                return ProbeResult::failure(
                    started,
                    output.contains(SMOKE_MARKER),
                    event_count,
                    output_bytes,
                    first_token_ms,
                    provider_error_kind(&error),
                );
            }
        }
    }

    let marker = output.contains(SMOKE_MARKER);
    let error_kind = if first_token_ms.is_none() {
        Some("missing-output-text".to_owned())
    } else if completed != 1 {
        Some("invalid-terminal-count".to_owned())
    } else if !marker {
        Some("smoke-marker-missing".to_owned())
    } else {
        None
    };
    ProbeResult {
        outcome: if error_kind.is_some() { "fail" } else { "pass" },
        marker,
        event_count,
        output_bytes,
        first_token_ms,
        total_ms: elapsed_millis(started),
        error_kind,
    }
}

fn print_setup_result(
    case: &CanaryCase,
    timestamp: u64,
    commit: &str,
    outcome: &str,
    reason: &str,
) {
    println!(
        "{}",
        json!({
            "timestamp_unix": timestamp,
            "commit": commit,
            "case": case.id,
            "deployment": case.provider.as_str(),
            "protocol": case.protocol,
            "authentication": case.auth,
            "region": canary_region(),
            "model": case.model(),
            "outcome": outcome,
            "reason": reason,
        })
    );
}

fn print_probe_result(case: &CanaryCase, timestamp: u64, commit: &str, result: &ProbeResult) {
    println!(
        "{}",
        json!({
            "timestamp_unix": timestamp,
            "commit": commit,
            "case": case.id,
            "deployment": case.provider.as_str(),
            "protocol": case.protocol,
            "authentication": case.auth,
            "region": canary_region(),
            "model": case.model(),
            "outcome": result.outcome,
            "marker": result.marker,
            "event_count": result.event_count,
            "output_bytes": result.output_bytes,
            "first_token_ms": result.first_token_ms,
            "total_ms": result.total_ms,
            "error_kind": result.error_kind,
        })
    );
}

fn provider_error_kind(error: &ProviderError) -> &'static str {
    match error.kind() {
        qq_provider::ProviderErrorKind::Configuration => "configuration",
        qq_provider::ProviderErrorKind::Authentication => "authentication",
        qq_provider::ProviderErrorKind::RateLimited => "rate-limited",
        qq_provider::ProviderErrorKind::InvalidRequest => "invalid-request",
        qq_provider::ProviderErrorKind::Unavailable => "unavailable",
        qq_provider::ProviderErrorKind::Transport => "transport",
        qq_provider::ProviderErrorKind::Api => "api",
        qq_provider::ProviderErrorKind::Response => "response",
        qq_provider::ProviderErrorKind::Protocol => "protocol",
    }
}

fn canary_region() -> Option<String> {
    env::var("QQ_CANARY_AWS_REGION")
        .ok()
        .filter(|region| !region.trim().is_empty())
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn git_commit() -> String {
    ProcessCommand::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|commit| commit.trim().to_owned())
        .filter(|commit| !commit.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn parses_the_documented_provider_commands() {
        assert!(Cli::try_parse_from(["cargo xtask", "providers", "check", "offline"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "cargo xtask",
                "providers",
                "check",
                "live",
                "--provider",
                "google",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["cargo xtask", "providers", "check", "live", "--all",]).is_ok()
        );
    }

    #[test]
    fn executable_matrix_has_unique_rows_and_every_http_protocol() {
        let ids = CASES.iter().map(|case| case.id).collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), CASES.len());

        let protocols = CASES
            .iter()
            .filter_map(|case| match case.recipe {
                RecipeKind::Http { protocol, .. } => Some(protocol),
                RecipeKind::AmazonBedrock | RecipeKind::BedrockMantle(_) => None,
            })
            .collect::<Vec<_>>();
        for protocol in [
            HttpProtocol::OpenAiResponses,
            HttpProtocol::OpenAiChatCompletions,
            HttpProtocol::AnthropicMessages,
            HttpProtocol::GoogleGenerateContent,
        ] {
            assert!(protocols.contains(&protocol));
        }
    }

    #[test]
    fn selection_requires_exactly_one_selection_mode() {
        assert!(
            selected_cases(&LiveArgs {
                provider: Vec::new(),
                all: false,
            })
            .is_err()
        );
        assert!(
            selected_cases(&LiveArgs {
                provider: vec![ProviderName::Google],
                all: true,
            })
            .is_err()
        );
        let google = selected_cases(&LiveArgs {
            provider: vec![ProviderName::Google],
            all: false,
        })
        .unwrap();
        assert_eq!(google.len(), 1);
        assert_eq!(google[0].id, "google-generate-content");
    }
}
