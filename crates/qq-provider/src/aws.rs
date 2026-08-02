//! Shared AWS implementation: configuration loading, region rules, and the
//! static AWS HTTP client. Consumed by the Bedrock and Mantle adapters.

use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

#[allow(deprecated)]
use aws_config::profile::profile_file::ProfileFiles;
use aws_config::{
    BehaviorVersion, Region, SdkConfig,
    default_provider::credentials::DefaultCredentialsChain,
    environment::region::EnvironmentVariableRegionProvider,
    imds::region::ImdsRegionProvider,
    meta::region::{ProvideRegion, RegionProviderChain},
    profile::{ProfileFileCredentialsProvider, ProfileFileRegionProvider},
    provider_config::ProviderConfig,
    retry::RetryConfig,
};
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_smithy_http_client::{Builder as SmithyHttpClientBuilder, tls};
use aws_smithy_runtime_api::client::http::SharedHttpClient;
use tokio::sync::Semaphore;

use crate::{
    ProviderError, ProviderErrorKind, credentials::SecretLiteral, request_auth::AwsCredentialLease,
};

const AWS_CONFIG_LOAD_TIMEOUT: Duration = Duration::from_secs(5);
const AWS_CONFIG_BUILD_CONCURRENCY: usize = 2;

static AWS_CONFIG_BUILD_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(AWS_CONFIG_BUILD_CONCURRENCY)));
static DIRECT_AWS_HTTP_CLIENT: LazyLock<SharedHttpClient> = LazyLock::new(|| {
    // This client stays proxy-disabled unless proxy configuration is explicitly installed.
    SmithyHttpClientBuilder::new()
        .tls_provider(tls::Provider::Rustls(
            tls::rustls_provider::CryptoMode::AwsLc,
        ))
        .build_https()
});

/// Authentication used by Amazon Bedrock Runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BedrockAuth {
    /// Uses the standard AWS credential and region provider chains.
    DefaultChain,
    /// Uses one named AWS profile.
    Profile(String),
    /// Uses an Amazon Bedrock API key as an HTTP bearer token.
    ApiKey(SecretLiteral),
}

pub(crate) fn validate_configuration(
    auth: &BedrockAuth,
    region: Option<&str>,
) -> Result<(), ProviderError> {
    match auth {
        BedrockAuth::Profile(profile) if invalid_configuration_value(profile) => {
            return Err(ProviderError::Configuration(
                "AWS profile name must not be empty or contain control characters".to_owned(),
            ));
        }
        BedrockAuth::ApiKey(secret) if invalid_configuration_value(secret.expose_secret()) => {
            return Err(ProviderError::Configuration(
                "Amazon Bedrock API key must not be empty or contain control characters".to_owned(),
            ));
        }
        BedrockAuth::DefaultChain | BedrockAuth::Profile(_) | BedrockAuth::ApiKey(_) => {}
    }

    if region.is_some_and(|region| !valid_region_label(region)) {
        return Err(ProviderError::Configuration(
            "AWS region must be a valid DNS label".to_owned(),
        ));
    }

    Ok(())
}

fn invalid_configuration_value(value: &str) -> bool {
    value.trim().is_empty() || value.chars().any(char::is_control)
}

pub(crate) fn valid_region_label(region: &str) -> bool {
    !region.is_empty()
        && region.len() <= 63
        && region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && region
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && region
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

pub(crate) async fn load_aws_config(
    auth: &BedrockAuth,
    region: Option<&str>,
) -> Result<LoadedAwsConfig, AwsConfigLoadError> {
    load_aws_config_with_profile_files(auth, region, None, Arc::clone(&AWS_CONFIG_BUILD_PERMITS))
        .await
}

#[allow(deprecated)]
async fn load_aws_config_with_profile_files(
    auth: &BedrockAuth,
    region: Option<&str>,
    profile_files: Option<ProfileFiles>,
    permits: Arc<Semaphore>,
) -> Result<LoadedAwsConfig, AwsConfigLoadError> {
    let auth = auth.clone();
    let region = region.map(str::to_owned);
    run_bounded_aws_config_build(permits, AWS_CONFIG_LOAD_TIMEOUT, move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| AwsConfigLoadError::RuntimeConstructionFailed)?;
        runtime.block_on(build_aws_config(&auth, region.as_deref(), profile_files))
    })
    .await
}

async fn run_bounded_aws_config_build<T>(
    permits: Arc<Semaphore>,
    timeout: Duration,
    build: impl FnOnce() -> Result<T, AwsConfigLoadError> + Send + 'static,
) -> Result<T, AwsConfigLoadError>
where
    T: Send + 'static,
{
    let permit = permits
        .try_acquire_owned()
        .map_err(|_| AwsConfigLoadError::CapacityUnavailable)?;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        build()
    });

    match tokio::time::timeout(timeout, task).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(AwsConfigLoadError::JoinFailed),
        Err(_) => Err(AwsConfigLoadError::TimedOut),
    }
}

#[allow(deprecated)]
async fn build_aws_config(
    auth: &BedrockAuth,
    region: Option<&str>,
    profile_files: Option<ProfileFiles>,
) -> Result<LoadedAwsConfig, AwsConfigLoadError> {
    let provider_config = ProviderConfig::without_region()
        .with_http_client((*DIRECT_AWS_HTTP_CLIENT).clone())
        .with_retry_config(RetryConfig::disabled())
        .with_behavior_version(Some(BehaviorVersion::latest()));

    let resolved_region = match region {
        Some(region) => Some(Region::new(region.to_owned())),
        None => match auth {
            BedrockAuth::Profile(profile) => {
                let mut builder = ProfileFileRegionProvider::builder()
                    .configure(&provider_config)
                    .profile_name(profile);
                if let Some(profile_files) = profile_files.clone() {
                    builder = builder.profile_files(profile_files);
                }
                ProvideRegion::region(&builder.build()).await
            }
            BedrockAuth::DefaultChain | BedrockAuth::ApiKey(_) => {
                let mut profile = ProfileFileRegionProvider::builder().configure(&provider_config);
                if let Some(profile_files) = profile_files.clone() {
                    profile = profile.profile_files(profile_files);
                }
                RegionProviderChain::first_try(EnvironmentVariableRegionProvider::new())
                    .or_else(profile.build())
                    .or_else(
                        ImdsRegionProvider::builder()
                            .configure(&provider_config)
                            .build(),
                    )
                    .region()
                    .await
            }
        },
    };
    let provider_config = provider_config.with_region(resolved_region.clone());

    let credentials = match auth {
        BedrockAuth::DefaultChain => {
            let credentials = DefaultCredentialsChain::builder()
                .configure(provider_config)
                .region(resolved_region.clone())
                .build()
                .await;
            Some(SharedCredentialsProvider::new(credentials))
        }
        BedrockAuth::Profile(profile) => {
            let mut credentials = ProfileFileCredentialsProvider::builder()
                .configure(&provider_config)
                .profile_name(profile);
            if let Some(profile_files) = profile_files {
                credentials = credentials.profile_files(profile_files);
            }
            Some(SharedCredentialsProvider::new(credentials.build()))
        }
        BedrockAuth::ApiKey(_) => None,
    };

    let credentials = if let Some(credentials) = credentials {
        let initial = match credentials.provide_credentials().await {
            Ok(credentials) => credentials,
            Err(_) => return Err(AwsConfigLoadError::CredentialsUnavailable),
        };
        let credentials = AwsCredentialLease::new_primed(credentials, initial);
        Some(credentials)
    } else {
        None
    };

    let mut sdk_config = SdkConfig::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(resolved_region)
        .retry_config(RetryConfig::disabled())
        .http_client((*DIRECT_AWS_HTTP_CLIENT).clone());
    if let Some(credentials) = &credentials {
        sdk_config =
            sdk_config.credentials_provider(SharedCredentialsProvider::new(credentials.clone()));
    }

    Ok(LoadedAwsConfig {
        sdk_config: sdk_config.build(),
        credentials,
    })
}

#[derive(Debug)]
pub(crate) struct LoadedAwsConfig {
    pub(crate) sdk_config: SdkConfig,
    pub(crate) credentials: Option<AwsCredentialLease>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AwsConfigLoadError {
    TimedOut,
    CapacityUnavailable,
    JoinFailed,
    RuntimeConstructionFailed,
    CredentialsUnavailable,
}

impl AwsConfigLoadError {
    pub(crate) fn to_provider_error(self) -> ProviderError {
        let message = match self {
            Self::TimedOut => "AWS configuration and region resolution timed out",
            Self::CapacityUnavailable => "AWS configuration worker capacity is exhausted",
            Self::JoinFailed => "AWS configuration worker stopped unexpectedly",
            Self::RuntimeConstructionFailed => {
                "AWS configuration worker runtime could not be initialized"
            }
            Self::CredentialsUnavailable => {
                return ProviderError::ResponseFailed {
                    kind: ProviderErrorKind::Authentication,
                    message: "Amazon Bedrock could not load AWS credentials".to_owned(),
                };
            }
        };
        ProviderError::Transport(message.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    };

    #[allow(deprecated)]
    use aws_config::profile::profile_file::{ProfileFileKind, ProfileFiles};
    use aws_credential_types::provider::ProvideCredentials;

    use super::*;

    #[tokio::test]
    async fn aws_configuration_capacity_is_fail_fast() {
        let permits = Arc::new(Semaphore::new(0));
        let ran = Arc::new(AtomicBool::new(false));
        let ran_in_build = Arc::clone(&ran);
        let error = run_bounded_aws_config_build(permits, Duration::from_secs(1), move || {
            ran_in_build.store(true, Ordering::Relaxed);
            Ok(())
        })
        .await
        .unwrap_err();

        assert_eq!(error, AwsConfigLoadError::CapacityUnavailable);
        assert!(!ran.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn timed_out_aws_build_retains_capacity_and_rejects_excess_work() {
        let permits = Arc::new(Semaphore::new(1));
        let (release, released) = mpsc::channel();
        let error = run_bounded_aws_config_build(Arc::clone(&permits), Duration::ZERO, move || {
            released.recv().unwrap();
            Ok(())
        })
        .await
        .unwrap_err();
        assert_eq!(error, AwsConfigLoadError::TimedOut);
        assert_eq!(permits.available_permits(), 0);

        let ran = Arc::new(AtomicBool::new(false));
        let ran_in_build = Arc::clone(&ran);
        let excess =
            run_bounded_aws_config_build(Arc::clone(&permits), Duration::from_secs(1), move || {
                ran_in_build.store(true, Ordering::Relaxed);
                Ok(())
            })
            .await
            .unwrap_err();
        assert_eq!(excess, AwsConfigLoadError::CapacityUnavailable);
        assert!(!ran.load(Ordering::Relaxed));

        release.send(()).unwrap();
        let permit = Arc::clone(&permits).acquire_owned().await.unwrap();
        assert_eq!(permits.available_permits(), 0);
        drop(permit);
        assert_eq!(permits.available_permits(), 1);
    }

    #[tokio::test]
    async fn aws_build_join_and_runtime_errors_are_typed_and_sanitized() {
        let join_error = run_bounded_aws_config_build(
            Arc::new(Semaphore::new(1)),
            Duration::from_secs(1),
            || -> Result<(), AwsConfigLoadError> { panic!("blocking builder stopped") },
        )
        .await
        .unwrap_err();

        assert_eq!(join_error, AwsConfigLoadError::JoinFailed);
        for error in [
            AwsConfigLoadError::TimedOut,
            AwsConfigLoadError::CapacityUnavailable,
            AwsConfigLoadError::JoinFailed,
            AwsConfigLoadError::RuntimeConstructionFailed,
        ] {
            let provider_error = error.to_provider_error();
            assert_eq!(provider_error.kind(), ProviderErrorKind::Transport);
            assert!(
                !provider_error
                    .to_string()
                    .contains("blocking builder stopped")
            );
        }
        assert_eq!(
            AwsConfigLoadError::CredentialsUnavailable
                .to_provider_error()
                .kind(),
            ProviderErrorKind::Authentication
        );
    }

    #[tokio::test]
    async fn named_profiles_control_region_and_credentials_unless_region_is_explicit() {
        let permits = Arc::new(Semaphore::new(AWS_CONFIG_BUILD_CONCURRENCY));
        #[allow(deprecated)]
        let profile_files = ProfileFiles::builder()
            .with_contents(
                ProfileFileKind::Config,
                "[default]\nregion = us-east-1\n[profile selected]\nregion = us-west-2\n",
            )
            .with_contents(
                ProfileFileKind::Credentials,
                "[default]\naws_access_key_id = DEFAULTKEY\naws_secret_access_key = default-secret\n\
                 [selected]\naws_access_key_id = SELECTEDKEY\naws_secret_access_key = selected-secret\n",
            )
            .build();
        let config = load_aws_config_with_profile_files(
            &BedrockAuth::Profile("selected".to_owned()),
            None,
            Some(profile_files.clone()),
            Arc::clone(&permits),
        )
        .await
        .unwrap();
        let credentials = config
            .credentials
            .as_ref()
            .unwrap()
            .provide_credentials()
            .await
            .unwrap();
        let sdk_credentials = config
            .sdk_config
            .credentials_provider()
            .unwrap()
            .provide_credentials()
            .await
            .unwrap();
        let explicit = load_aws_config_with_profile_files(
            &BedrockAuth::Profile("selected".to_owned()),
            Some("eu-central-1"),
            Some(profile_files),
            permits,
        )
        .await
        .unwrap();

        assert_eq!(credentials.access_key_id(), "SELECTEDKEY");
        assert_eq!(sdk_credentials.access_key_id(), "SELECTEDKEY");
        assert_eq!(
            config.sdk_config.region().map(Region::as_ref),
            Some("us-west-2")
        );
        assert_eq!(
            explicit.sdk_config.region().map(Region::as_ref),
            Some("eu-central-1")
        );
        assert!(config.sdk_config.http_client().is_some());
        assert!(explicit.sdk_config.http_client().is_some());
    }

    #[tokio::test]
    async fn profile_endpoint_overrides_do_not_change_bedrock_routing() {
        #[allow(deprecated)]
        let profile_files = ProfileFiles::builder()
            .with_contents(
                ProfileFileKind::Config,
                "[profile selected]\nregion = us-east-1\nendpoint_url = http://127.0.0.1:9\n",
            )
            .with_contents(
                ProfileFileKind::Credentials,
                "[selected]\naws_access_key_id = SELECTEDKEY\naws_secret_access_key = selected-secret\n",
            )
            .build();

        let config = load_aws_config_with_profile_files(
            &BedrockAuth::Profile("selected".to_owned()),
            None,
            Some(profile_files),
            Arc::new(Semaphore::new(AWS_CONFIG_BUILD_CONCURRENCY)),
        )
        .await
        .unwrap();

        assert_eq!(config.sdk_config.endpoint_url(), None);
    }

    #[tokio::test]
    async fn credential_process_profiles_are_rejected_without_running_a_process() {
        #[allow(deprecated)]
        let profile_files = ProfileFiles::builder()
            .with_contents(
                ProfileFileKind::Config,
                "[profile selected]\nregion = us-east-1\ncredential_process = this-command-must-not-run\n",
            )
            .build();

        let error = load_aws_config_with_profile_files(
            &BedrockAuth::Profile("selected".to_owned()),
            Some("us-east-1"),
            Some(profile_files),
            Arc::new(Semaphore::new(AWS_CONFIG_BUILD_CONCURRENCY)),
        )
        .await
        .unwrap_err();

        assert_eq!(error, AwsConfigLoadError::CredentialsUnavailable);
        assert_eq!(
            error.to_provider_error().kind(),
            ProviderErrorKind::Authentication
        );
    }

    #[tokio::test]
    async fn api_key_configuration_has_no_credential_lease() {
        let config = load_aws_config_with_profile_files(
            &BedrockAuth::ApiKey("test-key".into()),
            Some("us-east-1"),
            None,
            Arc::new(Semaphore::new(AWS_CONFIG_BUILD_CONCURRENCY)),
        )
        .await
        .unwrap();

        assert!(config.credentials.is_none());
        assert!(config.sdk_config.credentials_provider().is_none());
    }
}
