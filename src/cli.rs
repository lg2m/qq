//! Command-line parsing and dispatch.

use std::net::SocketAddr;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "qq", version, about = "Build and run AI agents")]
pub struct Cli {
    /// Override the configured provider/model route.
    #[arg(long, global = true, value_name = "PROVIDER/MODEL")]
    pub model: Option<String>,

    /// Override the maximum number of generated tokens.
    #[arg(long, global = true)]
    pub max_output_tokens: Option<u32>,

    /// Select an enrolled organization.
    #[arg(long, global = true)]
    pub organization: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    pub fn parse() -> Self {
        <Self as Parser>::parse()
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Send one prompt and stream the response to stdout.
    Ask {
        /// Prompt to send to the model.
        prompt: String,
    },

    /// Run the user-scoped QQ server in the foreground.
    Serve {
        /// Loopback address to bind. Port 0 selects an available port.
        #[arg(long, default_value = "127.0.0.1:0")]
        bind: SocketAddr,
    },

    /// Inspect and validate effective configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },

    /// Store and inspect provider credentials.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },

    /// Enroll and manage organization configuration manifests.
    Org {
        #[command(subcommand)]
        command: OrgCommand,
    },

    /// Trust the sensitive operations in current project configuration.
    Trust,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print configuration and state paths.
    Paths,
    /// Print loaded sources in precedence order.
    Sources,
    /// Validate the effective configuration.
    Check,
    /// Print the redacted effective configuration.
    Show,
    /// Explain the source of one effective field.
    Explain {
        /// Field name: model, organization, max_output_tokens, or provider.NAME.
        field: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Prompt for and store a provider API credential.
    Login(LoginArgs),
    /// Store a named credential read from the terminal or stdin.
    Set(SetCredentialArgs),
    /// List stored credential metadata.
    List,
    /// Show nonsecret metadata for one credential.
    Status { name: String },
    /// Remove a stored credential.
    Logout { name: String },
}

#[derive(Debug, Subcommand)]
pub enum OrgCommand {
    /// Fetch and cache an organization's HTTPS RON manifest.
    Enroll {
        /// Local organization name used by --organization.
        name: String,
        /// HTTPS URL of the RON configuration manifest.
        manifest_url: String,
    },
    /// List enrolled organizations without fetching the network.
    List,
    /// Select the default organization.
    Use { name: String },
    /// Refresh one manifest while retaining the last known good copy on failure.
    Refresh { name: String },
    /// Remove an enrollment and its cached manifest.
    Remove { name: String },
}

#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Built-in provider ID, such as openai or anthropic.
    pub provider: String,
    /// Credential profile name.
    #[arg(long, default_value = "default")]
    pub profile: String,
    /// Authenticate xAI with OAuth instead of prompting for an API key.
    #[arg(long)]
    pub oauth: bool,
    /// Allow an explicit user-only plaintext file if the OS keyring is unavailable.
    #[arg(long)]
    pub allow_file: bool,
}

#[derive(Debug, Args)]
pub struct SetCredentialArgs {
    /// Portable Stored(...) reference name.
    pub name: String,
    /// Optional provider/credential kind shown by auth status.
    #[arg(long)]
    pub kind: Option<String>,
    /// Bind use of this credential to one normalized provider endpoint.
    #[arg(long)]
    pub endpoint: Option<String>,
    /// Allow an explicit user-only plaintext file if the OS keyring is unavailable.
    #[arg(long)]
    pub allow_file: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ask_command_and_global_overrides() {
        let cli = Cli::try_parse_from([
            "qq",
            "ask",
            "hello",
            "--model",
            "openai/gpt-test",
            "--max-output-tokens",
            "123",
        ])
        .unwrap();

        assert_eq!(cli.model.as_deref(), Some("openai/gpt-test"));
        assert_eq!(cli.max_output_tokens, Some(123));
        assert!(matches!(
            cli.command,
            Some(Command::Ask { prompt }) if prompt == "hello"
        ));
    }

    #[test]
    fn parses_bare_interactive_mode_and_server() {
        assert!(Cli::try_parse_from(["qq"]).unwrap().command.is_none());
        assert!(matches!(
            Cli::try_parse_from(["qq", "serve"]).unwrap().command,
            Some(Command::Serve { bind }) if bind == "127.0.0.1:0".parse().unwrap()
        ));
    }

    #[test]
    fn parses_config_and_auth_commands() {
        assert!(matches!(
            Cli::try_parse_from(["qq", "config", "explain", "model"])
                .unwrap()
                .command,
            Some(Command::Config {
                command: ConfigCommand::Explain { field }
            }) if field == "model"
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "qq",
                "auth",
                "login",
                "xai",
                "--oauth",
                "--allow-file"
            ])
                .unwrap()
                .command,
            Some(Command::Auth {
                command: AuthCommand::Login(LoginArgs {
                    provider,
                    oauth: true,
                    allow_file: true,
                    ..
                })
            }) if provider == "xai"
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "qq",
                "org",
                "enroll",
                "acme",
                "https://config.example.test/acme.ron"
            ])
            .unwrap()
            .command,
            Some(Command::Org {
                command: OrgCommand::Enroll { name, manifest_url }
            }) if name == "acme" && manifest_url == "https://config.example.test/acme.ron"
        ));
    }
}
