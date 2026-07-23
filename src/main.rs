#![forbid(unsafe_code)]

use std::{
    error::Error,
    io::{self, IsTerminal, Read, Write},
    process::ExitCode,
    sync::Arc,
};

use async_stream::stream;
use futures_util::StreamExt;
use qq_protocol::{AskRequest, RunCommand, RunEvent, RunFailureKind};

mod auth;
mod cli;
mod client;
mod config;
mod output;
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
        Some(cli::Command::Auth { command }) => auth_command(command)?,
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

    fn ask_request(&self, prompt: String) -> Result<AskRequest, io::Error> {
        let mut request = AskRequest::new(prompt, std::env::current_dir()?);
        request.model.clone_from(&self.model);
        request.max_output_tokens = self.max_output_tokens;
        request.organization.clone_from(&self.organization);
        Ok(request)
    }
}

async fn ask(prompt: String, overrides: &CliOverrides) -> Result<(), Box<dyn Error>> {
    if let Some(connection) = client::discover().await? {
        let request = overrides.ask_request(prompt)?;
        let events = client::ask(&connection, request).await?;
        return render_client_events(events).await;
    }

    let factory = runtime::RuntimeFactory::system()?;
    let load = overrides.load_request()?;
    let runtime = tokio::task::spawn_blocking(move || factory.runtime_for(&load)).await??;
    render_events(runtime.run(RunCommand::new(prompt))).await
}

async fn serve(bind: std::net::SocketAddr) -> Result<(), Box<dyn Error>> {
    let handler = Arc::new(runtime::RuntimeHandler::new(
        runtime::RuntimeFactory::system()?,
    ));
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
    let mut embedded = None;
    let connection = if let Some(connection) = client::discover().await? {
        connection
    } else {
        let handler = Arc::new(runtime::RuntimeHandler::new(
            runtime::RuntimeFactory::system()?,
        ));
        match server::start(handler, server::ServerOptions::for_user()?).await? {
            server::StartOutcome::Existing(connection) => connection,
            server::StartOutcome::Started(server) => {
                let connection = server.connection().clone();
                embedded = Some(server);
                connection
            }
        }
    };

    eprintln!(
        "qq connected to {}. Enter a prompt, or /quit.",
        connection.address()
    );
    let session_id = new_session_id()?;
    loop {
        let Some(prompt) = read_prompt().await? else {
            break;
        };
        if matches!(prompt.as_str(), "/quit" | "/exit") {
            break;
        }
        if prompt.is_empty() {
            continue;
        }
        let mut request = overrides.ask_request(prompt)?;
        request.session_id = Some(session_id.clone());
        match client::ask(&connection, request).await {
            Ok(events) => {
                if let Err(error) = render_client_events(events).await {
                    eprintln!("error: {error}");
                }
            }
            Err(error) => eprintln!("error: {error}"),
        }
    }

    if let Some(server) = embedded {
        server.shutdown().await?;
    }
    Ok(())
}

fn new_session_id() -> Result<String, io::Error> {
    const RANDOM_BYTES: usize = 16;
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut random = [0_u8; RANDOM_BYTES];
    getrandom::fill(&mut random)
        .map_err(|_| io::Error::other("secure randomness is unavailable"))?;
    let mut id = String::with_capacity(RANDOM_BYTES * 2);
    for byte in random {
        id.push(HEX[(byte >> 4) as usize] as char);
        id.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(id)
}

async fn read_prompt() -> Result<Option<String>, io::Error> {
    tokio::task::spawn_blocking(|| {
        eprint!("> ");
        io::stderr().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            return Ok(None);
        }
        while matches!(line.as_bytes().last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        Ok(Some(line))
    })
    .await
    .map_err(io::Error::other)?
}

async fn render_client_events(mut events: client::RunEventStream) -> Result<(), Box<dyn Error>> {
    let converted = stream! {
        while let Some(event) = events.next().await {
            match event {
                Ok(event) => yield event,
                Err(error) => {
                    yield RunEvent::Failed {
                        kind: RunFailureKind::Server,
                        message: error.to_string(),
                    };
                    return;
                }
            }
        }
    };
    render_events(converted).await
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
        }
        cli::ConfigCommand::Check => {
            loader.load(&overrides.load_request()?)?;
            println!("configuration is valid");
        }
        cli::ConfigCommand::Show => {
            let snapshot = loader.load(&overrides.load_request()?)?;
            print_snapshot(&snapshot);
        }
        cli::ConfigCommand::Explain { field } => {
            let snapshot = loader.load(&overrides.load_request()?)?;
            let source = match field.as_str() {
                "organization" => snapshot.provenance().organization(),
                "model" => snapshot.provenance().model(),
                "max_output_tokens" => snapshot.provenance().max_output_tokens(),
                _ => field
                    .strip_prefix("provider.")
                    .and_then(|name| snapshot.provenance().provider(name)),
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

fn print_snapshot(snapshot: &config::ConfigSnapshot) {
    println!(
        "organization: {}",
        snapshot.organization().unwrap_or("<none>")
    );
    println!("model: {}", snapshot.model().as_str());
    println!("max_output_tokens: {}", snapshot.max_output_tokens());
    println!("providers:");
    for (name, provider) in snapshot.providers() {
        let kind = match provider {
            config::ProviderConfig::OpenAi { .. } => "OpenAi",
            config::ProviderConfig::OpenAiCodex { .. } => "OpenAiCodex",
            config::ProviderConfig::Anthropic { .. } => "Anthropic",
            config::ProviderConfig::Google { .. } => "Google",
            config::ProviderConfig::LiteLlm { .. } => "LiteLlm",
            config::ProviderConfig::AmazonBedrock { .. } => "AmazonBedrock",
            config::ProviderConfig::AmazonBedrockMantle { .. } => "AmazonBedrockMantle",
            config::ProviderConfig::Custom { .. } => "Custom",
        };
        println!("  {name}: {kind}");
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
            let backend = if arguments.provider == "openai-codex" {
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
        _ => None,
    }
}
