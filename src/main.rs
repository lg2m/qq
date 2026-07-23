#![forbid(unsafe_code)]

use std::{
    error::Error,
    io::{self, IsTerminal, Read},
    process::ExitCode,
    sync::Arc,
};

use qq_protocol::{RunCommand, RunEvent};

mod auth;
mod catalog;
mod cli;
mod client;
mod config;
mod models;
mod output;
mod providers;
mod runtime;
mod server;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let cli = cli::Cli::parse();
    let overrides = CliOverrides {
        model: cli.model,
        max_output_tokens: cli.max_output_tokens,
        organization: cli.organization,
    };

    match cli.command {
        Some(cli::Command::Ask { prompt }) => ask(prompt, &overrides).await?,
        Some(cli::Command::Serve { bind }) => serve(bind).await?,
        Some(cli::Command::Config { command }) => config_command(command, &overrides)?,
        Some(cli::Command::Auth { command }) => {
            run_blocking_command(move || auth_command(command)).await?
        }
        Some(cli::Command::Org { command }) => organization_command(command)?,
        Some(cli::Command::Trust) => trust_command(&overrides)?,
        None => interactive(&overrides).await?,
    }

    Ok(())
}

#[derive(Clone, Debug, Default)]
struct CliOverrides {
    model: Option<String>,
    max_output_tokens: Option<u32>,
    organization: Option<String>,
}

impl CliOverrides {
    fn load_request(&self) -> Result<config::LoadRequest, config::ConfigError> {
        let request = config::LoadRequest::from_current_process(self.max_output_tokens)?;
        let mut values = request.overrides().clone();
        if let Some(model) = &self.model {
            values = values.with_model(model.clone());
        }
        if let Some(organization) = &self.organization {
            values = values.with_organization(organization.clone());
        }
        Ok(request.with_overrides(values))
    }
}

async fn ask(prompt: String, overrides: &CliOverrides) -> Result<(), Box<dyn Error>> {
    let factory = runtime::RuntimeFactory::system()?;
    let load = overrides.load_request()?;
    let runtime = tokio::task::spawn_blocking(move || factory.runtime_for(&load)).await??;
    render_events(runtime.run(RunCommand::new(prompt))).await
}

async fn serve(bind: std::net::SocketAddr) -> Result<(), Box<dyn Error>> {
    let handler =
        Arc::new(runtime::RuntimeHandler::open(runtime::RuntimeFactory::system()?).await?);
    let options = server::ServerOptions::for_user()?.with_bind_address(bind);
    match server::start(handler, options).await? {
        server::StartOutcome::Existing(connection) => {
            println!("qq server already running at {}", connection.address());
        }
        server::StartOutcome::Started(server) => {
            println!("qq server listening at {}", server.connection().address());
            tokio::signal::ctrl_c().await?;
            server.shutdown().await?;
        }
    }
    Ok(())
}

async fn interactive(overrides: &CliOverrides) -> Result<(), Box<dyn Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other("interactive mode requires a terminal").into());
    }
    let factory = runtime::RuntimeFactory::system()?;
    let request = overrides.load_request()?;
    let config_factory = factory.clone();
    let (snapshot, tui) = tokio::task::spawn_blocking(move || {
        let snapshot = config_factory.load(&request)?;
        let tui = config::ConfigLoader::system()?.load_tui(request.cwd())?;
        Ok::<_, runtime::RuntimeBuildError>((snapshot, tui))
    })
    .await??;
    let mut embedded = None;
    let connection = if let Some(connection) = client::discover().await? {
        connection
    } else {
        let handler = Arc::new(runtime::RuntimeHandler::open(factory).await?);
        match server::start(handler, server::ServerOptions::for_user()?).await? {
            server::StartOutcome::Existing(connection) => connection,
            server::StartOutcome::Started(server) => {
                let connection = server.connection().clone();
                embedded = Some(server);
                connection
            }
        }
    };

    let workspace = std::fs::canonicalize(std::env::current_dir()?)?;
    let configured_model = qq_protocol::ModelSelection {
        model: Some(snapshot.model().as_str().to_owned()),
        max_output_tokens: Some(snapshot.max_output_tokens()),
        organization: snapshot.organization().map(str::to_owned),
    };
    let models = client::SessionClient::new(connection.clone())?
        .models(qq_protocol::ModelCatalogRequest {
            workspace: workspace.to_string_lossy().into_owned(),
            selection: configured_model.clone(),
        })
        .await?
        .into_iter()
        .map(|model| qq_tui::ModelOption {
            provider: model.provider,
            model: model.model,
            name: model.name,
            context_window: model.context_window,
            selection: model.selection,
        })
        .collect::<Vec<_>>();
    let model = models
        .iter()
        .any(|option| option.selection.model == configured_model.model)
        .then_some(configured_model);
    let tui_client = client::TuiClient::start(connection, workspace, model.clone())?;
    let result = qq_tui::run(
        tui_client,
        qq_tui::TuiOptions {
            settings: tui.settings().clone(),
            model: model.unwrap_or_default(),
            models,
        },
    )
    .await;

    if let Some(server) = embedded {
        server.shutdown().await?;
    }
    result.map_err(Into::into)
}

async fn render_events(
    events: impl futures_core::Stream<Item = RunEvent>,
) -> Result<(), Box<dyn Error>> {
    let stdout = io::stdout();
    let mode = if stdout.is_terminal() {
        output::OutputMode::Terminal
    } else {
        output::OutputMode::Raw
    };
    let mut stdout = stdout.lock();
    output::render(events, &mut stdout, mode).await?;
    Ok(())
}

fn config_command(
    command: cli::ConfigCommand,
    overrides: &CliOverrides,
) -> Result<(), Box<dyn Error>> {
    let loader = config::ConfigLoader::system()?;
    match command {
        cli::ConfigCommand::Paths => {
            println!("global:  {}", loader.paths().global_dir().display());
            println!(
                "global TUI: {}",
                loader.paths().global_dir().join("tui.ron").display()
            );
            println!("data:    {}", loader.paths().data_dir().display());
            println!("managed: {}", loader.paths().managed_dir().display());
            println!(
                "organizations: {}",
                loader.paths().organizations_file().display()
            );
            println!(
                "organization cache: {}",
                loader.paths().organizations_cache_dir().display()
            );
        }
        cli::ConfigCommand::Sources => {
            let request = overrides.load_request()?;
            match loader.load(&request) {
                Ok(snapshot) => print_sources(snapshot.source_reports()),
                Err(config::ConfigError::TrustRequired { reports, pending }) => {
                    print_sources(&reports);
                    for item in pending {
                        println!("pending trust: {}", item.source());
                    }
                }
                Err(error) => return Err(error.into()),
            }
            print_tui_sources(loader.load_tui(request.cwd())?.source_reports());
        }
        cli::ConfigCommand::Check => {
            let request = overrides.load_request()?;
            loader.load(&request)?;
            loader.load_tui(request.cwd())?;
            println!("configuration is valid");
        }
        cli::ConfigCommand::Show => {
            let request = overrides.load_request()?;
            let snapshot = loader.load(&request)?;
            print_snapshot(&snapshot);
            print_tui_snapshot(&loader.load_tui(request.cwd())?);
        }
        cli::ConfigCommand::Explain { field } => {
            let request = overrides.load_request()?;
            let source = if field == "tui.layout" {
                Some(
                    loader
                        .load_tui(request.cwd())?
                        .provenance()
                        .layout()
                        .clone(),
                )
            } else if let Some(action) = field
                .strip_prefix("tui.bindings.")
                .and_then(parse_tui_action)
            {
                Some(
                    loader
                        .load_tui(request.cwd())?
                        .provenance()
                        .binding(action)
                        .clone(),
                )
            } else {
                let snapshot = loader.load(&request)?;
                match field.as_str() {
                    "organization" => snapshot.provenance().organization(),
                    "model" => snapshot.provenance().model(),
                    "max_output_tokens" => snapshot.provenance().max_output_tokens(),
                    _ => field
                        .strip_prefix("provider.")
                        .and_then(|name| snapshot.provenance().provider(name)),
                }
                .cloned()
            };
            let source =
                source.ok_or_else(|| format!("unknown or unset config field {field:?}"))?;
            println!("{field}: {source}");
        }
    }
    Ok(())
}

fn print_sources(reports: &[config::SourceReport]) {
    for report in reports {
        println!("{:?}\t{}", report.status(), report.source());
    }
}

fn print_tui_sources(reports: &[config::TuiSourceReport]) {
    for report in reports {
        println!("Applied\t{}", report.source());
    }
}

fn print_snapshot(snapshot: &config::ConfigSnapshot) {
    println!(
        "organization: {}",
        snapshot.organization().unwrap_or("<none>")
    );
    println!("model: {}", snapshot.model().as_str());
    println!("max_output_tokens: {}", snapshot.max_output_tokens());
    println!("providers:");
    for (name, provider) in snapshot.providers() {
        let kind = match provider.kind() {
            config::ProviderKind::OpenAi => "OpenAi",
            config::ProviderKind::OpenAiCodex => "OpenAiCodex",
            config::ProviderKind::Anthropic => "Anthropic",
            config::ProviderKind::Google => "Google",
            config::ProviderKind::XAi => "XAi",
            config::ProviderKind::LiteLlm => "LiteLlm",
            config::ProviderKind::AmazonBedrock => "AmazonBedrock",
            config::ProviderKind::AmazonBedrockMantle => "AmazonBedrockMantle",
            config::ProviderKind::Custom => "Custom",
        };
        println!("  {name}: {kind}");
    }
}

fn print_tui_snapshot(snapshot: &config::TuiConfigSnapshot) {
    println!("tui:");
    println!("  layout: {:?}", snapshot.settings().initial_layout());
    println!("  bindings:");
    for (action, bindings) in snapshot.settings().bindings() {
        let labels: Vec<_> = bindings.iter().map(ToString::to_string).collect();
        println!("    {}: {}", tui_action_name(*action), labels.join(", "));
    }
}

fn parse_tui_action(value: &str) -> Option<qq_tui::Action> {
    match value {
        "select_threadline" => Some(qq_tui::Action::SelectThreadline),
        "select_fold_focus" => Some(qq_tui::Action::SelectFoldFocus),
        "next_layout" => Some(qq_tui::Action::NextLayout),
        "previous_layout" => Some(qq_tui::Action::PreviousLayout),
        "toggle_navigator" => Some(qq_tui::Action::ToggleNavigator),
        "create_root_session" => Some(qq_tui::Action::CreateRootSession),
        "create_child_session" => Some(qq_tui::Action::CreateChildSession),
        "cancel_run" => Some(qq_tui::Action::CancelRun),
        _ => None,
    }
}

fn tui_action_name(action: qq_tui::Action) -> &'static str {
    match action {
        qq_tui::Action::SelectThreadline => "select_threadline",
        qq_tui::Action::SelectFoldFocus => "select_fold_focus",
        qq_tui::Action::NextLayout => "next_layout",
        qq_tui::Action::PreviousLayout => "previous_layout",
        qq_tui::Action::ToggleNavigator => "toggle_navigator",
        qq_tui::Action::CreateRootSession => "create_root_session",
        qq_tui::Action::CreateChildSession => "create_child_session",
        qq_tui::Action::CancelRun => "cancel_run",
    }
}

fn trust_command(overrides: &CliOverrides) -> Result<(), Box<dyn Error>> {
    let loader = config::ConfigLoader::system()?;
    let pending = loader.grant_pending_trust(&overrides.load_request()?)?;
    if pending.is_empty() {
        println!("no project configuration requires trust");
    } else {
        for item in pending {
            println!("trusted {}", item.source());
        }
    }
    Ok(())
}

fn auth_command(command: cli::AuthCommand) -> Result<(), Box<dyn Error>> {
    let store = auth::CredentialStore::system()?;
    match command {
        cli::AuthCommand::Login(arguments) => {
            let name = format!("{}/{}", arguments.provider, arguments.profile);
            let backend = if arguments.oauth && arguments.provider != "xai" {
                return Err(format!(
                    "OAuth login is not supported for provider {:?}",
                    arguments.provider
                )
                .into());
            } else if arguments.provider == "openai-codex" {
                auth::validate_credential_name(&name)?;
                let login = auth::CodexLogin::start()?;
                eprintln!(
                    "Open this URL to sign in with OpenAI Codex:\n{}",
                    login.authorization_url()
                );
                if webbrowser::open(login.authorization_url()).is_err() {
                    eprintln!("The browser could not be opened automatically.");
                }
                login.complete(&store, &arguments.profile, arguments.allow_file)?
            } else if arguments.provider == "xai" && arguments.oauth {
                auth::validate_credential_name(&name)?;
                let login = auth::XaiLogin::start(&store)?;
                eprintln!(
                    "Open this URL to sign in with xAI:\n{}\n\nEnter code: {}",
                    login.verification_url(),
                    login.user_code()
                );
                if webbrowser::open(login.verification_url()).is_err() {
                    eprintln!("The browser could not be opened automatically.");
                }
                login.complete(&store, &arguments.profile, arguments.allow_file)?
            } else {
                let secret = read_secret(&format!("{} API key: ", arguments.provider))?;
                store.set_with_metadata(
                    &name,
                    secret.expose_secret_bytes(),
                    arguments.allow_file,
                    Some(&arguments.provider),
                    built_in_endpoint(&arguments.provider),
                )?
            };
            println!("stored {name} in {backend}");
        }
        cli::AuthCommand::Set(arguments) => {
            let secret = read_secret("Credential: ")?;
            let backend = store.set_with_metadata(
                &arguments.name,
                secret.expose_secret_bytes(),
                arguments.allow_file,
                arguments.kind.as_deref(),
                arguments.endpoint.as_deref(),
            )?;
            println!("stored {} in {backend}", arguments.name);
        }
        cli::AuthCommand::List => {
            for item in store.list()? {
                println!(
                    "{}\t{}\t{}",
                    item.name,
                    item.backend,
                    item.kind.as_deref().unwrap_or("-")
                );
            }
        }
        cli::AuthCommand::Status { name } => match store.status(&name)? {
            Some(item) => {
                println!("name: {}", item.name);
                println!("backend: {}", item.backend);
                println!("kind: {}", item.kind.as_deref().unwrap_or("<none>"));
                println!(
                    "endpoint: {}",
                    item.endpoint.as_deref().unwrap_or("<unbound>")
                );
            }
            None => return Err(format!("credential {name:?} is not stored").into()),
        },
        cli::AuthCommand::Logout { name } => {
            if store.remove(&name)? {
                println!("removed {name}");
            } else {
                println!("credential {name} was not stored");
            }
        }
    }
    Ok(())
}

fn organization_command(command: cli::OrgCommand) -> Result<(), Box<dyn Error>> {
    let loader = config::ConfigLoader::system()?;
    match command {
        cli::OrgCommand::Enroll { name, manifest_url } => {
            let enrollment = loader.enroll_organization(&name, &manifest_url)?;
            println!(
                "enrolled {} from {}{}",
                enrollment.name(),
                enrollment.manifest_url(),
                if enrollment.selected() {
                    " (selected)"
                } else {
                    ""
                }
            );
        }
        cli::OrgCommand::List => {
            for enrollment in loader.organizations()? {
                println!(
                    "{}{}\t{}",
                    if enrollment.selected() { "* " } else { "  " },
                    enrollment.name(),
                    enrollment.manifest_url()
                );
            }
        }
        cli::OrgCommand::Use { name } => {
            loader.select_organization(&name)?;
            println!("selected {name}");
        }
        cli::OrgCommand::Refresh { name } => {
            let enrollment = loader.refresh_organization(&name)?;
            println!("refreshed {}", enrollment.name());
        }
        cli::OrgCommand::Remove { name } => {
            if loader.remove_organization(&name)? {
                println!("removed {name}");
            } else {
                println!("organization {name} was not enrolled");
            }
        }
    }
    Ok(())
}

fn read_secret(prompt: &str) -> Result<auth::Secret, Box<dyn Error>> {
    let value = if io::stdin().is_terminal() {
        rpassword::prompt_password(prompt)?
    } else {
        let mut value = String::new();
        io::stdin().take(64 * 1024).read_to_string(&mut value)?;
        if value.ends_with('\n') {
            value.pop();
            if value.ends_with('\r') {
                value.pop();
            }
        }
        value
    };
    if value.is_empty() {
        return Err("credential must not be empty".into());
    }
    Ok(auth::Secret::from_secret_bytes(value.into_bytes()))
}

fn built_in_endpoint(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some("https://api.openai.com"),
        "openai-codex" => Some("https://chatgpt.com"),
        "anthropic" => Some("https://api.anthropic.com"),
        "google" => Some("https://generativelanguage.googleapis.com"),
        "xai" => Some("https://api.x.ai"),
        _ => None,
    }
}

async fn run_blocking_command(
    command: impl FnOnce() -> Result<(), Box<dyn Error>> + Send + 'static,
) -> Result<(), Box<dyn Error>> {
    let result =
        tokio::task::spawn_blocking(move || command().map_err(|error| error.to_string())).await?;
    result.map_err(|error| io::Error::other(error).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blocking_command_can_drop_its_http_runtime() {
        run_blocking_command(|| {
            let client = reqwest::blocking::Client::builder().build()?;
            drop(client);
            Ok(())
        })
        .await
        .unwrap();
    }
}
