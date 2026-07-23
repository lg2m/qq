//! Request-time HTTP authorization.

use std::{
    sync::{Arc, LazyLock},
    time::{Duration, SystemTime},
};

use aws_credential_types::{
    Credentials,
    provider::{self, ProvideCredentials, SharedCredentialsProvider, future as credential_future},
};
use aws_sigv4::{
    http_request::{SignableBody, SignableRequest, SigningParams, SigningSettings, sign},
    sign::v4,
};
use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, watch};

use crate::{ProviderError, ProviderErrorKind};

const MANTLE_SIGNING_NAME: &str = "bedrock-mantle";
const CREDENTIAL_LOAD_TIMEOUT: Duration = Duration::from_secs(5);
const CREDENTIAL_PROVIDER_DEADLINE: Duration = Duration::from_secs(30);
const CREDENTIAL_LOAD_CONCURRENCY: usize = 2;
const DEFAULT_CREDENTIAL_LIFETIME: Duration = Duration::from_secs(15 * 60);
const CREDENTIAL_REFRESH_BUFFER: Duration = Duration::from_secs(10);
const CREDENTIAL_FAILURE_CACHE_TTL: Duration = Duration::from_secs(1);
const CREDENTIAL_LOAD_FAILURE_MESSAGE: &str = "AWS credentials could not be loaded";
const CREDENTIAL_LOAD_TIMEOUT_MESSAGE: &str = "AWS credential loading timed out";
const CREDENTIAL_LOAD_TASK_FAILURE_MESSAGE: &str =
    "AWS credential loading worker stopped unexpectedly";
const CREDENTIAL_LOAD_CAPACITY_MESSAGE: &str = "AWS credential loading capacity is exhausted";
const CREDENTIAL_PROVIDER_DEADLINE_MESSAGE: &str =
    "AWS credential provider exceeded its cooperative deadline";
const CREDENTIAL_LOAD_RUNTIME_FAILURE_MESSAGE: &str =
    "AWS credential loading runtime could not be initialized";

static CREDENTIAL_LOAD_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(CREDENTIAL_LOAD_CONCURRENCY)));

#[derive(Clone, Default)]
pub(crate) struct RequestAuthorizer {
    sigv4: Option<Arc<SigV4Authorizer>>,
}

impl RequestAuthorizer {
    pub(crate) fn bedrock_mantle_sigv4(
        region: impl Into<Arc<str>>,
        credentials: AwsCredentialLease,
    ) -> Self {
        Self {
            sigv4: Some(Arc::new(SigV4Authorizer::new(region, credentials))),
        }
    }

    #[cfg(test)]
    pub(crate) fn bedrock_mantle_sigv4_with_clock(
        region: impl Into<Arc<str>>,
        provider: SharedCredentialsProvider,
        clock: fn() -> SystemTime,
    ) -> Self {
        let mut credentials = AwsCredentialLease::new_for_test(provider);
        credentials.clock = clock;
        credentials.load_permits = Arc::new(Semaphore::new(CREDENTIAL_LOAD_CONCURRENCY));
        let mut authorizer = SigV4Authorizer::new(region, credentials);
        authorizer.clock = clock;
        Self {
            sigv4: Some(Arc::new(authorizer)),
        }
    }

    pub(crate) async fn authorize(
        &self,
        request: &mut reqwest::Request,
    ) -> Result<(), ProviderError> {
        match &self.sigv4 {
            Some(authorizer) => authorizer.sign(request).await,
            None => Ok(()),
        }
    }
}

struct CachedCredentials {
    credentials: Credentials,
    refresh_after: SystemTime,
}

struct CachedCredentialFailure {
    retry_after: SystemTime,
}

enum CredentialCacheEntry {
    Credentials(CachedCredentials),
    Failure(CachedCredentialFailure),
}

#[derive(Clone)]
enum CredentialLoadOutcome {
    Credentials(Credentials),
    ProviderFailure,
    ProviderDeadlineExceeded,
    RuntimeConstructionFailure,
    JoinFailure,
}

#[derive(Clone)]
struct InFlightCredentialLoad {
    id: u64,
    result: watch::Receiver<Option<CredentialLoadOutcome>>,
}

#[derive(Default)]
struct CredentialLoadState {
    cached: Option<CredentialCacheEntry>,
    in_flight: Option<InFlightCredentialLoad>,
    next_id: u64,
}

struct AwsCredentialLeaseInner {
    provider: SharedCredentialsProvider,
    state: Mutex<CredentialLoadState>,
}

/// The only owner of a raw AWS credential provider after configuration loading.
#[derive(Clone)]
pub(crate) struct AwsCredentialLease {
    inner: Arc<AwsCredentialLeaseInner>,
    clock: fn() -> SystemTime,
    load_timeout: Duration,
    provider_deadline: Duration,
    failure_cache_ttl: Duration,
    load_permits: Arc<Semaphore>,
}

impl std::fmt::Debug for AwsCredentialLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AwsCredentialLease")
            .finish_non_exhaustive()
    }
}

impl AwsCredentialLease {
    pub(crate) fn new_primed(
        provider: SharedCredentialsProvider,
        credentials: Credentials,
    ) -> Self {
        let now = SystemTime::now();
        Self {
            inner: Arc::new(AwsCredentialLeaseInner {
                provider,
                state: Mutex::new(CredentialLoadState {
                    cached: Some(CredentialCacheEntry::Credentials(cached_credentials(
                        credentials,
                        now,
                    ))),
                    in_flight: None,
                    next_id: 0,
                }),
            }),
            clock: SystemTime::now,
            load_timeout: CREDENTIAL_LOAD_TIMEOUT,
            provider_deadline: CREDENTIAL_PROVIDER_DEADLINE,
            failure_cache_ttl: CREDENTIAL_FAILURE_CACHE_TTL,
            load_permits: Arc::clone(&CREDENTIAL_LOAD_PERMITS),
        }
    }

    #[cfg(test)]
    fn new_for_test(provider: SharedCredentialsProvider) -> Self {
        Self {
            inner: Arc::new(AwsCredentialLeaseInner {
                provider,
                state: Mutex::new(CredentialLoadState::default()),
            }),
            clock: SystemTime::now,
            load_timeout: CREDENTIAL_LOAD_TIMEOUT,
            provider_deadline: CREDENTIAL_PROVIDER_DEADLINE,
            failure_cache_ttl: CREDENTIAL_FAILURE_CACHE_TTL,
            load_permits: Arc::new(Semaphore::new(CREDENTIAL_LOAD_CONCURRENCY)),
        }
    }

    pub(crate) async fn credentials(&self) -> Result<Credentials, AwsCredentialLeaseError> {
        match tokio::time::timeout(self.load_timeout, self.credentials_without_timeout()).await {
            Ok(result) => result,
            Err(_) => Err(AwsCredentialLeaseError::CallerTimedOut),
        }
    }

    async fn credentials_without_timeout(&self) -> Result<Credentials, AwsCredentialLeaseError> {
        let (id, result) = {
            let mut state = self.inner.state.lock().await;
            let now = (self.clock)();
            match state.cached.as_ref() {
                Some(CredentialCacheEntry::Credentials(cached)) if cached.refresh_after > now => {
                    return Ok(cached.credentials.clone());
                }
                Some(CredentialCacheEntry::Failure(cached)) if cached.retry_after > now => {
                    return Err(AwsCredentialLeaseError::ProviderFailure);
                }
                Some(CredentialCacheEntry::Credentials(_) | CredentialCacheEntry::Failure(_))
                | None => {}
            }
            state.cached = None;

            if let Some(in_flight) = state.in_flight.as_ref() {
                (in_flight.id, in_flight.result.clone())
            } else {
                let permit = match Arc::clone(&self.load_permits).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => return Err(AwsCredentialLeaseError::CapacityUnavailable),
                };
                let id = state.next_id;
                state.next_id = state.next_id.wrapping_add(1);
                let (sender, result) = watch::channel(None);
                state.in_flight = Some(InFlightCredentialLoad {
                    id,
                    result: result.clone(),
                });
                self.spawn_credential_load(id, permit, sender);
                (id, result)
            }
        };

        self.await_credential_load(id, result).await
    }

    fn spawn_credential_load(
        &self,
        id: u64,
        permit: OwnedSemaphorePermit,
        result: watch::Sender<Option<CredentialLoadOutcome>>,
    ) {
        let inner = Arc::clone(&self.inner);
        let provider = inner.provider.clone();
        let clock = self.clock;
        let provider_deadline = self.provider_deadline;
        let failure_cache_ttl = self.failure_cache_ttl;
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(_) => return CredentialLoadOutcome::RuntimeConstructionFailure,
            };
            runtime.block_on(async move {
                // The deadline preempts cooperative futures only. A synchronous poll keeps this
                // worker and permit occupied, bounding damage to the global permit count.
                match tokio::time::timeout(provider_deadline, async move {
                    provider.provide_credentials().await
                })
                .await
                {
                    Ok(Ok(credentials)) => CredentialLoadOutcome::Credentials(credentials),
                    Ok(Err(_)) => CredentialLoadOutcome::ProviderFailure,
                    Err(_) => CredentialLoadOutcome::ProviderDeadlineExceeded,
                }
            })
        });
        std::mem::drop(tokio::spawn(async move {
            let outcome = match task.await {
                Ok(outcome) => outcome,
                Err(_) => CredentialLoadOutcome::JoinFailure,
            };
            let completed_at = clock();
            let cached = match &outcome {
                CredentialLoadOutcome::Credentials(credentials) => {
                    let expires = credentials.expiry().unwrap_or_else(|| {
                        completed_at
                            .checked_add(DEFAULT_CREDENTIAL_LIFETIME)
                            .unwrap_or(completed_at)
                    });
                    let refresh_after = expires
                        .checked_sub(CREDENTIAL_REFRESH_BUFFER)
                        .unwrap_or(expires);
                    Some(CredentialCacheEntry::Credentials(CachedCredentials {
                        credentials: credentials.clone(),
                        refresh_after,
                    }))
                }
                CredentialLoadOutcome::ProviderFailure => {
                    Some(CredentialCacheEntry::Failure(CachedCredentialFailure {
                        retry_after: completed_at
                            .checked_add(failure_cache_ttl)
                            .unwrap_or(completed_at),
                    }))
                }
                CredentialLoadOutcome::ProviderDeadlineExceeded
                | CredentialLoadOutcome::RuntimeConstructionFailure
                | CredentialLoadOutcome::JoinFailure => None,
            };

            let mut state = inner.state.lock().await;
            if state.in_flight.as_ref().is_some_and(|load| load.id == id) {
                state.in_flight = None;
                state.cached = cached;
            }
            let _ignored = result.send(Some(outcome));
            drop(state);
        }));
    }

    async fn await_credential_load(
        &self,
        id: u64,
        mut result: watch::Receiver<Option<CredentialLoadOutcome>>,
    ) -> Result<Credentials, AwsCredentialLeaseError> {
        loop {
            let outcome = result.borrow().clone();
            if let Some(outcome) = outcome {
                return match outcome {
                    CredentialLoadOutcome::Credentials(credentials) => Ok(credentials),
                    CredentialLoadOutcome::ProviderFailure => {
                        Err(AwsCredentialLeaseError::ProviderFailure)
                    }
                    CredentialLoadOutcome::ProviderDeadlineExceeded => {
                        Err(AwsCredentialLeaseError::ProviderDeadlineExceeded)
                    }
                    CredentialLoadOutcome::RuntimeConstructionFailure => {
                        Err(AwsCredentialLeaseError::RuntimeConstructionFailure)
                    }
                    CredentialLoadOutcome::JoinFailure => {
                        Err(AwsCredentialLeaseError::WorkerFailed)
                    }
                };
            }
            if result.changed().await.is_err() {
                let mut state = self.inner.state.lock().await;
                if state.in_flight.as_ref().is_some_and(|load| load.id == id) {
                    state.in_flight = None;
                }
                return Err(AwsCredentialLeaseError::WorkerFailed);
            }
        }
    }
}

fn cached_credentials(credentials: Credentials, now: SystemTime) -> CachedCredentials {
    let expires = credentials
        .expiry()
        .unwrap_or_else(|| now.checked_add(DEFAULT_CREDENTIAL_LIFETIME).unwrap_or(now));
    let refresh_after = expires
        .checked_sub(CREDENTIAL_REFRESH_BUFFER)
        .unwrap_or(expires);
    CachedCredentials {
        credentials,
        refresh_after,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AwsCredentialLeaseError {
    ProviderFailure,
    CallerTimedOut,
    CapacityUnavailable,
    ProviderDeadlineExceeded,
    RuntimeConstructionFailure,
    WorkerFailed,
}

impl AwsCredentialLeaseError {
    fn message(self) -> &'static str {
        match self {
            Self::ProviderFailure => CREDENTIAL_LOAD_FAILURE_MESSAGE,
            Self::CallerTimedOut => CREDENTIAL_LOAD_TIMEOUT_MESSAGE,
            Self::CapacityUnavailable => CREDENTIAL_LOAD_CAPACITY_MESSAGE,
            Self::ProviderDeadlineExceeded => CREDENTIAL_PROVIDER_DEADLINE_MESSAGE,
            Self::RuntimeConstructionFailure => CREDENTIAL_LOAD_RUNTIME_FAILURE_MESSAGE,
            Self::WorkerFailed => CREDENTIAL_LOAD_TASK_FAILURE_MESSAGE,
        }
    }

    fn to_provider_error(self) -> ProviderError {
        match self {
            Self::ProviderFailure => ProviderError::ResponseFailed {
                kind: ProviderErrorKind::Authentication,
                message: self.message().to_owned(),
            },
            Self::CallerTimedOut
            | Self::CapacityUnavailable
            | Self::ProviderDeadlineExceeded
            | Self::RuntimeConstructionFailure
            | Self::WorkerFailed => ProviderError::Transport(self.message().to_owned()),
        }
    }

    fn to_aws_error(self) -> provider::error::CredentialsError {
        provider::error::CredentialsError::provider_error(std::io::Error::other(self.message()))
    }
}

impl ProvideCredentials for AwsCredentialLease {
    fn provide_credentials<'a>(&'a self) -> credential_future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        credential_future::ProvideCredentials::new(async move {
            self.credentials()
                .await
                .map_err(AwsCredentialLeaseError::to_aws_error)
        })
    }
}

struct SigV4Authorizer {
    region: Arc<str>,
    credentials: AwsCredentialLease,
    clock: fn() -> SystemTime,
}

impl SigV4Authorizer {
    fn new(region: impl Into<Arc<str>>, credentials: AwsCredentialLease) -> Self {
        Self {
            region: region.into(),
            credentials,
            clock: SystemTime::now,
        }
    }

    async fn sign(&self, request: &mut reqwest::Request) -> Result<(), ProviderError> {
        let credentials = self
            .credentials
            .credentials()
            .await
            .map_err(AwsCredentialLeaseError::to_provider_error)?;
        let identity = credentials.into();
        let mut headers = Vec::with_capacity(request.headers().len());
        for (name, value) in request.headers() {
            let value = value.to_str().map_err(|_| {
                ProviderError::Configuration(
                    "Amazon Bedrock Mantle request contains a non-text header".to_owned(),
                )
            })?;
            headers.push((name.as_str(), value));
        }
        let body = request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .ok_or_else(|| {
                ProviderError::Configuration(
                    "Amazon Bedrock Mantle requests must have a buffered body".to_owned(),
                )
            })?;
        let signable = SignableRequest::new(
            request.method().as_str(),
            request.url().as_str(),
            headers.into_iter(),
            SignableBody::Bytes(body),
        )
        .map_err(|error| {
            ProviderError::Configuration(format!(
                "could not prepare Amazon Bedrock Mantle request for signing: {error}"
            ))
        })?;
        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.region)
            .name(MANTLE_SIGNING_NAME)
            .time((self.clock)())
            .settings(SigningSettings::default())
            .build()
            .map_err(|error| {
                ProviderError::Configuration(format!(
                    "could not configure Amazon Bedrock Mantle request signing: {error}"
                ))
            })?;
        let params = SigningParams::from(params);
        let (instructions, _) = sign(signable, &params)
            .map_err(|error| {
                ProviderError::Configuration(format!(
                    "could not sign Amazon Bedrock Mantle request: {error}"
                ))
            })?
            .into_parts();
        let (headers, query) = instructions.into_parts();
        if !query.is_empty() {
            return Err(ProviderError::Configuration(
                "Amazon Bedrock Mantle signer unexpectedly produced query parameters".to_owned(),
            ));
        }
        for header in headers {
            let name = HeaderName::from_bytes(header.name().as_bytes()).map_err(|_| {
                ProviderError::Configuration(
                    "Amazon Bedrock Mantle signer produced an invalid header name".to_owned(),
                )
            })?;
            let mut value = HeaderValue::from_str(header.value()).map_err(|_| {
                ProviderError::Configuration(
                    "Amazon Bedrock Mantle signer produced an invalid header value".to_owned(),
                )
            })?;
            value.set_sensitive(
                header.sensitive()
                    || name == AUTHORIZATION
                    || name.as_str() == "x-amz-security-token",
            );
            request.headers_mut().insert(name, value);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        hint::black_box,
        sync::{
            Condvar, Mutex as StdMutex,
            atomic::{AtomicUsize, Ordering},
        },
        thread::{self, ThreadId},
        time::Instant,
    };

    use aws_credential_types::provider::{self, future};
    use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
    use tokio::sync::{Notify, Semaphore};

    use super::*;

    #[tokio::test]
    async fn sigv4_signs_the_buffered_body_and_caches_credentials() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = CountingCredentials {
            calls: Arc::clone(&calls),
            credentials: Credentials::new(
                "AKIDEXAMPLE",
                "test-secret-access-key",
                Some("test-session-token".to_owned()),
                None,
                "test",
            ),
        };
        let authorizer = RequestAuthorizer::bedrock_mantle_sigv4_with_clock(
            "us-east-1",
            SharedCredentialsProvider::new(provider),
            fixed_time,
        );
        let client = reqwest::Client::new();
        let mut first = client
            .post("https://bedrock-mantle.us-east-1.api.aws/v1/responses")
            .header(CONTENT_TYPE, "application/json")
            .body(br#"{"input":"first"}"#.to_vec())
            .build()
            .unwrap();
        let mut second = client
            .post("https://bedrock-mantle.us-east-1.api.aws/v1/responses")
            .header(CONTENT_TYPE, "application/json")
            .body(br#"{"input":"second"}"#.to_vec())
            .build()
            .unwrap();

        authorizer.authorize(&mut first).await.unwrap();
        authorizer.authorize(&mut second).await.unwrap();

        let first_authorization = first.headers()[AUTHORIZATION].to_str().unwrap();
        let second_authorization = second.headers()[AUTHORIZATION].to_str().unwrap();
        assert!(first_authorization.contains("/us-east-1/bedrock-mantle/aws4_request"));
        assert_ne!(first_authorization, second_authorization);
        assert_eq!(
            first.headers()["x-amz-security-token"],
            "test-session-token"
        );
        assert!(first.headers()[AUTHORIZATION].is_sensitive());
        assert!(first.headers()["x-amz-security-token"].is_sensitive());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_credential_load() {
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Semaphore::new(0));
        let started = Arc::new(Notify::new());
        let authorizer = Arc::new(test_lease(GatedCredentials {
            calls: Arc::clone(&calls),
            release: Arc::clone(&release),
            started: Arc::clone(&started),
            credentials: Credentials::new(
                "AKIDEXAMPLE",
                "test-secret-access-key",
                None,
                None,
                "test",
            ),
        }));
        let tasks = (0..8)
            .map(|_| {
                let authorizer = Arc::clone(&authorizer);
                tokio::spawn(async move { authorizer.credentials().await })
            })
            .collect::<Vec<_>>();
        started.notified().await;
        release.add_permits(1);

        for task in tasks {
            task.await.unwrap().unwrap();
        }

        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn timed_out_load_continues_and_reuses_the_same_eventual_result() {
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Semaphore::new(0));
        let started = Arc::new(Notify::new());
        let mut authorizer = test_lease(GatedCredentials {
            calls: Arc::clone(&calls),
            release: Arc::clone(&release),
            started: Arc::clone(&started),
            credentials: Credentials::new("FALLBACKKEY", "fallback-secret", None, None, "test"),
        });
        authorizer.load_timeout = Duration::ZERO;
        let authorizer = Arc::new(authorizer);

        let error = authorizer.credentials().await.unwrap_err();
        assert_eq!(error, AwsCredentialLeaseError::CallerTimedOut);
        started.notified().await;
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        release.add_permits(1);
        wait_for_credential_load(&authorizer).await;
        let first = authorizer.credentials().await.unwrap();
        let cached = authorizer.credentials().await.unwrap();
        assert_eq!(first.access_key_id(), "FALLBACKKEY");
        assert_eq!(cached.access_key_id(), "FALLBACKKEY");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn repeated_timeouts_do_not_start_duplicate_credential_loads() {
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Semaphore::new(0));
        let started = Arc::new(Notify::new());
        let mut authorizer = test_lease(GatedCredentials {
            calls: Arc::clone(&calls),
            release: Arc::clone(&release),
            started: Arc::clone(&started),
            credentials: Credentials::new("EVENTUALKEY", "eventual-secret", None, None, "test"),
        });
        authorizer.load_timeout = Duration::ZERO;
        authorizer.failure_cache_ttl = Duration::ZERO;
        let authorizer = Arc::new(authorizer);

        for _ in 0..3 {
            let error = authorizer.credentials().await.unwrap_err();
            assert_eq!(error, AwsCredentialLeaseError::CallerTimedOut);
        }
        started.notified().await;
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        release.add_permits(1);
        wait_for_credential_load(&authorizer).await;
        assert_eq!(
            authorizer.credentials().await.unwrap().access_key_id(),
            "EVENTUALKEY"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn cancelling_a_waiter_does_not_cancel_the_credential_load() {
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Semaphore::new(0));
        let started = Arc::new(Notify::new());
        let authorizer = Arc::new(test_lease(GatedCredentials {
            calls: Arc::clone(&calls),
            release: Arc::clone(&release),
            started: Arc::clone(&started),
            credentials: Credentials::new("EVENTUALKEY", "eventual-secret", None, None, "test"),
        }));
        let waiter = {
            let authorizer = Arc::clone(&authorizer);
            tokio::spawn(async move { authorizer.credentials().await })
        };
        started.notified().await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());

        release.add_permits(1);
        assert_eq!(
            authorizer.credentials().await.unwrap().access_key_id(),
            "EVENTUALKEY"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn caller_timeout_cannot_release_a_synchronously_blocked_worker_slot() {
        let permits = Arc::new(Semaphore::new(1));
        let started = Arc::new(Notify::new());
        let gate = Arc::new((StdMutex::new(false), Condvar::new()));
        let mut lease = test_lease(SynchronouslyBlockingCredentials {
            started: Arc::clone(&started),
            gate: Arc::clone(&gate),
        });
        lease.load_timeout = Duration::ZERO;
        lease.provider_deadline = Duration::ZERO;
        lease.load_permits = Arc::clone(&permits);
        let lease = Arc::new(lease);

        assert_eq!(
            lease.credentials().await.unwrap_err(),
            AwsCredentialLeaseError::CallerTimedOut
        );
        started.notified().await;
        assert_eq!(permits.available_permits(), 0);

        let excess_calls = Arc::new(AtomicUsize::new(0));
        let mut excess = test_lease(CountingCredentials {
            calls: Arc::clone(&excess_calls),
            credentials: Credentials::new("EXCESSKEY", "excess-secret", None, None, "test"),
        });
        excess.load_permits = Arc::clone(&permits);
        assert_eq!(
            excess.credentials().await.unwrap_err(),
            AwsCredentialLeaseError::CapacityUnavailable
        );
        assert_eq!(excess_calls.load(Ordering::Relaxed), 0);

        let (released, wake) = &*gate;
        *released.lock().unwrap() = true;
        wake.notify_all();
        wait_for_credential_load(&lease).await;
        assert_eq!(permits.available_permits(), 1);
        assert_eq!(
            lease.credentials().await.unwrap().access_key_id(),
            "BLOCKINGKEY"
        );
    }

    #[tokio::test]
    async fn global_capacity_rejects_excess_credential_loads_immediately() {
        let permits = Arc::new(Semaphore::new(1));
        let first_calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Semaphore::new(0));
        let started = Arc::new(Notify::new());
        let mut first = test_lease(GatedCredentials {
            calls: Arc::clone(&first_calls),
            release: Arc::clone(&release),
            started: Arc::clone(&started),
            credentials: Credentials::new("FIRSTKEY", "first-secret", None, None, "test"),
        });
        first.load_permits = Arc::clone(&permits);
        let first = Arc::new(first);
        let first_waiter = {
            let first = Arc::clone(&first);
            tokio::spawn(async move { first.credentials().await })
        };
        started.notified().await;

        let excess_calls = Arc::new(AtomicUsize::new(0));
        let mut excess = test_lease(CountingCredentials {
            calls: Arc::clone(&excess_calls),
            credentials: Credentials::new("EXCESSKEY", "excess-secret", None, None, "test"),
        });
        excess.load_permits = Arc::clone(&permits);
        let error = excess.credentials().await.unwrap_err();

        assert_eq!(error, AwsCredentialLeaseError::CapacityUnavailable);
        assert_eq!(
            error.to_provider_error().to_string(),
            format!("provider request failed: {CREDENTIAL_LOAD_CAPACITY_MESSAGE}")
        );
        assert_eq!(excess_calls.load(Ordering::Relaxed), 0);
        release.add_permits(1);
        first_waiter.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn cooperative_provider_deadline_releases_capacity_and_returns_transport() {
        let permits = Arc::new(Semaphore::new(1));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut authorizer = test_lease(PendingCredentials {
            calls: Arc::clone(&calls),
        });
        authorizer.load_permits = Arc::clone(&permits);
        authorizer.provider_deadline = Duration::ZERO;
        let error = authorizer.credentials().await.unwrap_err();

        assert_eq!(error, AwsCredentialLeaseError::ProviderDeadlineExceeded);
        assert_eq!(
            error.to_provider_error().to_string(),
            format!("provider request failed: {CREDENTIAL_PROVIDER_DEADLINE_MESSAGE}")
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(permits.available_permits(), 1);

        let replacement_calls = Arc::new(AtomicUsize::new(0));
        let mut replacement = test_lease(CountingCredentials {
            calls: Arc::clone(&replacement_calls),
            credentials: Credentials::new(
                "REPLACEMENTKEY",
                "replacement-secret",
                None,
                None,
                "test",
            ),
        });
        replacement.load_permits = permits;
        assert_eq!(
            replacement.credentials().await.unwrap().access_key_id(),
            "REPLACEMENTKEY"
        );
        assert_eq!(replacement_calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn credential_provider_is_polled_on_a_blocking_thread() {
        let caller_thread = thread::current().id();
        let provider_thread = Arc::new(StdMutex::new(None));
        let authorizer = test_lease(ThreadRecordingCredentials {
            provider_thread: Arc::clone(&provider_thread),
        });

        authorizer.credentials().await.unwrap();

        assert_ne!(provider_thread.lock().unwrap().unwrap(), caller_thread);
    }

    #[tokio::test]
    async fn provider_panic_returns_a_sanitized_join_failure() {
        let authorizer = test_lease(PanickingCredentials);
        let error = authorizer.credentials().await.unwrap_err();

        assert_eq!(error, AwsCredentialLeaseError::WorkerFailed);
        assert_eq!(
            error.to_provider_error().to_string(),
            format!("provider request failed: {CREDENTIAL_LOAD_TASK_FAILURE_MESSAGE}")
        );
        assert!(
            !error
                .to_provider_error()
                .to_string()
                .contains("SUPER_SECRET")
        );
    }

    #[tokio::test]
    async fn concurrent_failures_share_one_load_and_negative_cache() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut authorizer = test_lease(FailingCredentials {
            calls: Arc::clone(&calls),
        });
        authorizer.clock = fixed_time;
        let authorizer = Arc::new(authorizer);
        let tasks = (0..8)
            .map(|_| {
                let authorizer = Arc::clone(&authorizer);
                tokio::spawn(async move { authorizer.credentials().await })
            })
            .collect::<Vec<_>>();

        for task in tasks {
            let error = task.await.unwrap().unwrap_err();
            assert_eq!(error, AwsCredentialLeaseError::ProviderFailure);
            let error = error.to_provider_error();
            assert_eq!(
                error.to_string(),
                format!("provider response failed: {CREDENTIAL_LOAD_FAILURE_MESSAGE}")
            );
            assert!(!error.to_string().contains("credential helper failed"));
            assert!(!error.to_string().contains("SUPER_SECRET"));
        }
        authorizer.credentials().await.unwrap_err();

        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn retries_credential_failures_after_the_negative_cache_expires() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut authorizer = test_lease(FailingCredentials {
            calls: Arc::clone(&calls),
        });
        authorizer.clock = fixed_time;
        authorizer.failure_cache_ttl = Duration::ZERO;

        authorizer.credentials().await.unwrap_err();
        authorizer.credentials().await.unwrap_err();

        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    #[ignore = "focused timing harness"]
    async fn ready_credential_acquisition_and_signing_timing_harness() {
        const ITERATIONS: u32 = 25_000;

        let calls = Arc::new(AtomicUsize::new(0));
        let authorizer = RequestAuthorizer::bedrock_mantle_sigv4_with_clock(
            "us-east-1",
            SharedCredentialsProvider::new(CountingCredentials {
                calls: Arc::clone(&calls),
                credentials: Credentials::new(
                    "AKIDEXAMPLE",
                    "test-secret-access-key",
                    None,
                    None,
                    "test",
                ),
            }),
            fixed_time,
        );
        let client = reqwest::Client::new();
        let mut warmup = client
            .post("https://bedrock-mantle.us-east-1.api.aws/v1/responses")
            .header(CONTENT_TYPE, "application/json")
            .body(br#"{"input":"warmup"}"#.to_vec())
            .build()
            .unwrap();
        authorizer.authorize(&mut warmup).await.unwrap();

        let started = Instant::now();
        for _ in 0..ITERATIONS {
            let mut request = client
                .post("https://bedrock-mantle.us-east-1.api.aws/v1/responses")
                .header(CONTENT_TYPE, "application/json")
                .body(br#"{"input":"benchmark"}"#.to_vec())
                .build()
                .unwrap();
            authorizer.authorize(&mut request).await.unwrap();
            black_box(&request);
        }
        let elapsed = started.elapsed();

        println!(
            "mantle_ready_credential_sign: {} ns/iteration ({ITERATIONS} iterations)",
            elapsed.as_nanos() / u128::from(ITERATIONS)
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    fn fixed_time() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_225_600)
    }

    fn test_lease(provider: impl ProvideCredentials + 'static) -> AwsCredentialLease {
        AwsCredentialLease::new_for_test(SharedCredentialsProvider::new(provider))
    }

    async fn wait_for_credential_load(authorizer: &AwsCredentialLease) {
        loop {
            if authorizer.inner.state.lock().await.in_flight.is_none() {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    #[derive(Debug)]
    struct CountingCredentials {
        calls: Arc<AtomicUsize>,
        credentials: Credentials,
    }

    impl ProvideCredentials for CountingCredentials {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            self.calls.fetch_add(1, Ordering::Relaxed);
            future::ProvideCredentials::ready(provider::Result::Ok(self.credentials.clone()))
        }
    }

    #[derive(Debug)]
    struct PendingCredentials {
        calls: Arc<AtomicUsize>,
    }

    impl ProvideCredentials for PendingCredentials {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            self.calls.fetch_add(1, Ordering::Relaxed);
            future::ProvideCredentials::new(std::future::pending())
        }
    }

    #[derive(Debug)]
    struct SynchronouslyBlockingCredentials {
        started: Arc<Notify>,
        gate: Arc<(StdMutex<bool>, Condvar)>,
    }

    impl ProvideCredentials for SynchronouslyBlockingCredentials {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            self.started.notify_one();
            let (released, wake) = &*self.gate;
            let mut released = released.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            future::ProvideCredentials::ready(Ok(Credentials::new(
                "BLOCKINGKEY",
                "blocking-secret",
                None,
                None,
                "test",
            )))
        }
    }

    #[derive(Debug)]
    struct ThreadRecordingCredentials {
        provider_thread: Arc<StdMutex<Option<ThreadId>>>,
    }

    impl ProvideCredentials for ThreadRecordingCredentials {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            *self.provider_thread.lock().unwrap() = Some(thread::current().id());
            future::ProvideCredentials::ready(Ok(Credentials::new(
                "THREADKEY",
                "thread-secret",
                None,
                None,
                "test",
            )))
        }
    }

    #[derive(Debug)]
    struct PanickingCredentials;

    impl ProvideCredentials for PanickingCredentials {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            panic!("provider panic with SUPER_SECRET")
        }
    }

    #[derive(Debug)]
    struct GatedCredentials {
        calls: Arc<AtomicUsize>,
        release: Arc<Semaphore>,
        started: Arc<Notify>,
        credentials: Credentials,
    }

    #[derive(Debug)]
    struct FailingCredentials {
        calls: Arc<AtomicUsize>,
    }

    impl ProvideCredentials for FailingCredentials {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            let calls = Arc::clone(&self.calls);
            future::ProvideCredentials::new(async move {
                calls.fetch_add(1, Ordering::Relaxed);
                Err(provider::error::CredentialsError::not_loaded(
                    std::io::Error::other(
                        "credential helper failed with SUPER_SECRET\nand controls",
                    ),
                ))
            })
        }
    }

    impl ProvideCredentials for GatedCredentials {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            let calls = Arc::clone(&self.calls);
            let release = Arc::clone(&self.release);
            let started = Arc::clone(&self.started);
            let credentials = self.credentials.clone();
            future::ProvideCredentials::new(async move {
                calls.fetch_add(1, Ordering::Relaxed);
                started.notify_one();
                release.acquire_owned().await.unwrap().forget();
                Ok(credentials)
            })
        }
    }
}
