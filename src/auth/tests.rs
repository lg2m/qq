use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::{Read as _, Write as _},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

static TEST_DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum FakeMode {
    #[default]
    Available,
    Unavailable,
    Failure,
}

#[derive(Default)]
struct FakeKeyring {
    mode: Mutex<FakeMode>,
    values: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl FakeKeyring {
    fn set_mode(&self, mode: FakeMode) {
        *self.mode.lock().unwrap() = mode;
    }

    fn erase(&self, name: &str) {
        self.values.lock().unwrap().remove(name);
    }

    fn value(&self, name: &str) -> Option<Vec<u8>> {
        self.values.lock().unwrap().get(name).cloned()
    }

    fn check_mode(&self) -> Result<(), KeyringError> {
        match *self.mode.lock().unwrap() {
            FakeMode::Available => Ok(()),
            FakeMode::Unavailable => Err(KeyringError::Unavailable),
            FakeMode::Failure => Err(KeyringError::Failure),
        }
    }
}

impl KeyringBackend for FakeKeyring {
    fn get(&self, name: &str) -> Result<Vec<u8>, KeyringError> {
        self.check_mode()?;
        self.value(name).ok_or(KeyringError::Missing)
    }

    fn set(&self, name: &str, secret: &[u8]) -> Result<(), KeyringError> {
        self.check_mode()?;
        self.values
            .lock()
            .unwrap()
            .insert(name.to_owned(), secret.to_vec());
        Ok(())
    }

    fn remove(&self, name: &str) -> Result<(), KeyringError> {
        self.check_mode()?;
        self.values
            .lock()
            .unwrap()
            .remove(name)
            .map(|_| ())
            .ok_or(KeyringError::Missing)
    }
}

struct FakeCodexTokenClient {
    exchanges: Mutex<Vec<(String, String, String)>>,
    refreshes: Mutex<Vec<String>>,
    exchanged: codex::ExchangedTokens,
    refreshed: codex::RefreshedTokens,
}

impl FakeCodexTokenClient {
    fn new(exchanged: codex::ExchangedTokens, refreshed: codex::RefreshedTokens) -> Self {
        Self {
            exchanges: Mutex::new(Vec::new()),
            refreshes: Mutex::new(Vec::new()),
            exchanged,
            refreshed,
        }
    }
}

impl codex::CodexTokenClient for FakeCodexTokenClient {
    fn exchange(
        &self,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<codex::ExchangedTokens, codex::CodexAuthError> {
        self.exchanges.lock().unwrap().push((
            code.to_owned(),
            redirect_uri.to_owned(),
            code_verifier.to_owned(),
        ));
        Ok(self.exchanged.clone())
    }

    fn refresh(
        &self,
        refresh_token: &str,
    ) -> Result<codex::RefreshedTokens, codex::CodexAuthError> {
        self.refreshes
            .lock()
            .unwrap()
            .push(refresh_token.to_owned());
        Ok(self.refreshed.clone())
    }
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        loop {
            let counter = TEST_DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("qq-auth-test-{}-{counter}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("failed to create test directory: {error}"),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn test_store() -> (CredentialStore, Arc<FakeKeyring>, TestDirectory) {
    let directory = TestDirectory::new();
    let keyring = Arc::new(FakeKeyring::default());
    let store =
        CredentialStore::with_backend(CredentialPaths::new(directory.path()), keyring.clone());
    (store, keyring, directory)
}

fn jwt(payload: serde_json::Value) -> String {
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
    format!("e30.{payload}.signature")
}

fn callback(port: u16, query: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET /auth/callback?{query} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn write_private(path: &Path, bytes: impl AsRef<[u8]>) {
    fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}

#[test]
fn secret_debug_and_display_are_redacted() {
    let secret = Secret::from_secret_bytes(b"do-not-print".to_vec());

    assert_eq!(format!("{secret:?}"), "<redacted>");
    assert_eq!(format!("{secret}"), "<redacted>");
    assert_eq!(secret.expose_secret_bytes(), b"do-not-print");
    assert_eq!(secret.expose_secret_str().unwrap(), "do-not-print");
}

#[test]
fn literal_resolution_preserves_whitespace() {
    let (store, _, _directory) = test_store();
    let reference: SecretRef = ron::from_str(r#"Value("  secret value  ")"#).unwrap();

    let secret = store.resolve(&reference).unwrap();

    assert_eq!(secret.expose_secret_str().unwrap(), "  secret value  ");
}

#[test]
fn environment_errors_are_distinct_and_values_are_not_trimmed() {
    let missing = environment_secret("QQ_TEST_MISSING", None).unwrap_err();
    let empty = environment_secret("QQ_TEST_EMPTY", Some(OsString::from(""))).unwrap_err();
    let present = environment_secret("QQ_TEST_PRESENT", Some(OsString::from(" value\n"))).unwrap();

    assert!(matches!(missing, AuthError::EnvironmentMissing { .. }));
    assert!(matches!(empty, AuthError::EnvironmentEmpty { .. }));
    assert_eq!(present.expose_secret_str().unwrap(), " value\n");
}

#[test]
fn public_environment_resolution_reports_a_missing_value() {
    let (store, _keyring, _directory) = test_store();
    let reference = SecretRef::Env("QQ_TEST_ENV_THAT_DOES_NOT_EXIST_90D1".to_owned());

    assert!(matches!(
        store.resolve(&reference).unwrap_err(),
        AuthError::EnvironmentMissing { .. }
    ));
}

#[cfg(unix)]
#[test]
fn non_unicode_environment_value_is_distinct() {
    use std::os::unix::ffi::OsStringExt;

    let error = environment_secret("QQ_TEST_NON_UNICODE", Some(OsString::from_vec(vec![0xff])))
        .unwrap_err();

    assert!(matches!(error, AuthError::EnvironmentNotUnicode { .. }));
}

#[test]
fn credential_names_are_strictly_validated() {
    for valid in [
        "a",
        "openai",
        "provider/openai",
        "provider/openai.api-key_2",
    ] {
        assert!(validate_credential_name(valid).is_ok(), "{valid}");
    }

    let too_long = format!("a{}", "b".repeat(MAX_CREDENTIAL_NAME_LEN));
    for invalid in [
        "",
        "Openai",
        "1openai",
        "open ai",
        "openai/",
        "openai//key",
        "openai/../key",
        "openai..key",
        &too_long,
    ] {
        assert!(validate_credential_name(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn keyring_success_records_and_resolves_the_backend() {
    let (store, keyring, _directory) = test_store();

    let backend = store.set("openai", "  key  ", false).unwrap();

    assert_eq!(backend, CredentialBackend::Keyring);
    assert_eq!(keyring.value("openai").unwrap(), b"  key  ");
    assert_eq!(
        store
            .resolve(&SecretRef::Stored("openai".to_owned()))
            .unwrap()
            .expose_secret_str()
            .unwrap(),
        "  key  "
    );
    assert_eq!(
        store.status("openai").unwrap().unwrap().backend,
        CredentialBackend::Keyring
    );
}

#[test]
fn unavailable_keyring_never_silently_falls_back() {
    let (store, keyring, _directory) = test_store();
    keyring.set_mode(FakeMode::Unavailable);

    let error = store.set("openai", "secret", false).unwrap_err();

    assert!(matches!(error, AuthError::FileFallbackNotAllowed { .. }));
    assert!(!store.paths().fallback_file().exists());
    assert!(!store.paths().index_file().exists());
}

#[test]
fn keyring_failures_do_not_trigger_an_allowed_fallback() {
    let (store, keyring, _directory) = test_store();
    keyring.set_mode(FakeMode::Failure);

    let error = store.set("openai", "secret", true).unwrap_err();

    assert!(matches!(error, AuthError::KeyringFailure { .. }));
    assert!(!store.paths().fallback_file().exists());
}

#[cfg(unix)]
#[test]
fn explicit_file_fallback_is_private_and_resolvable() {
    use std::os::unix::fs::PermissionsExt;

    let (store, keyring, _directory) = test_store();
    keyring.set_mode(FakeMode::Unavailable);

    let backend = store.set("openai", " file secret ", true).unwrap();

    assert_eq!(backend, CredentialBackend::File);
    assert_eq!(
        store
            .resolve(&SecretRef::Stored("openai".to_owned()))
            .unwrap()
            .expose_secret_str()
            .unwrap(),
        " file secret "
    );
    assert_eq!(
        fs::metadata(store.paths().data_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for path in [
        store.paths().fallback_file(),
        store.paths().index_file(),
        store.paths().lock_file(),
    ] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(store.remove("openai").unwrap());
    assert!(store.status("openai").unwrap().is_none());
    assert!(matches!(
        store
            .resolve(&SecretRef::Stored("openai".to_owned()))
            .unwrap_err(),
        AuthError::StoredCredentialNotRegistered { .. }
    ));
}

#[test]
fn endpoint_binding_is_normalized_and_enforced() {
    let (store, _keyring, _directory) = test_store();
    store
        .set_with_metadata(
            "openai",
            "secret",
            false,
            Some("openai"),
            Some("HTTPS://API.Example.TEST:443/v1/"),
        )
        .unwrap();
    let reference = SecretRef::Stored("openai".to_owned());

    assert_eq!(
        store.status("openai").unwrap().unwrap().endpoint.as_deref(),
        Some("https://api.example.test/v1")
    );
    assert!(matches!(
        store.resolve(&reference).unwrap_err(),
        AuthError::EndpointRequired { .. }
    ));
    assert_eq!(
        store
            .resolve_with_endpoint(&reference, Some("https://api.example.test/v1"))
            .unwrap()
            .expose_secret_str()
            .unwrap(),
        "secret"
    );
    assert!(matches!(
        store
            .resolve_with_endpoint(&reference, Some("https://other.example.test/v1"))
            .unwrap_err(),
        AuthError::EndpointMismatch { .. }
    ));
}

#[test]
fn list_status_and_remove_use_index_metadata() {
    let (store, keyring, _directory) = test_store();
    store
        .set_with_metadata("zeta", "z", false, Some("custom"), None)
        .unwrap();
    store.set("alpha", "a", false).unwrap();

    let list = store.list().unwrap();
    assert_eq!(
        list.iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "zeta"]
    );
    assert_eq!(list[1].kind.as_deref(), Some("custom"));
    assert!(store.is_registered("alpha").unwrap());

    assert!(store.remove("alpha").unwrap());
    assert!(!store.remove("alpha").unwrap());
    assert!(keyring.value("alpha").is_none());
    assert!(store.status("alpha").unwrap().is_none());
}

#[test]
fn missing_keyring_delete_is_idempotent_but_failures_are_not_swallowed() {
    let (store, keyring, _directory) = test_store();
    store.set("missing", "secret", false).unwrap();
    keyring.erase("missing");
    assert!(store.remove("missing").unwrap());

    store.set("failing", "secret", false).unwrap();
    keyring.set_mode(FakeMode::Failure);
    assert!(matches!(
        store.remove("failing").unwrap_err(),
        AuthError::KeyringFailure { .. }
    ));
    keyring.set_mode(FakeMode::Available);
    assert!(store.status("failing").unwrap().is_some());
}

#[test]
fn provider_resolution_does_not_fall_back_past_a_registered_record() {
    let (store, keyring, _directory) = test_store();
    store.set("openai", "secret", false).unwrap();
    keyring.erase("openai");

    let error = resolve_provider_credential(
        &store,
        None,
        "openai",
        "QQ_TEST_ENV_THAT_DOES_NOT_EXIST_4F2D",
        None,
    )
    .unwrap_err();

    assert!(matches!(error, AuthError::StoredCredentialMissing { .. }));
}

#[test]
fn provider_resolution_prefers_an_explicit_reference() {
    let (store, _keyring, _directory) = test_store();
    let explicit: SecretRef = ron::from_str(r#"Value("explicit")"#).unwrap();

    let secret = resolve_provider_credential(
        &store,
        Some(&explicit),
        "not/a/valid/../stored/name",
        "QQ_TEST_ENV_THAT_DOES_NOT_EXIST_56AA",
        None,
    )
    .unwrap();

    assert_eq!(secret.expose_secret_str().unwrap(), "explicit");
}

#[test]
fn codex_login_uses_pkce_rejects_wrong_state_and_stores_tokens() {
    let (mut store, _keyring, _directory) = test_store();
    let id_token = jwt(serde_json::json!({
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "workspace-test-id",
            "chatgpt_account_is_fedramp": true
        }
    }));
    let client = Arc::new(FakeCodexTokenClient::new(
        codex::ExchangedTokens {
            id_token,
            access_token: "access-token".to_owned(),
            refresh_token: "refresh-token".to_owned(),
        },
        codex::RefreshedTokens::default(),
    ));
    store.codex_client = client.clone();
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let login =
        CodexLogin::start_for_test(0, "known-state", verifier, Duration::from_secs(5)).unwrap();
    let authorization = reqwest::Url::parse(login.authorization_url()).unwrap();
    let parameters = authorization
        .query_pairs()
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect::<BTreeMap<_, _>>();
    let redirect = reqwest::Url::parse(&parameters["redirect_uri"]).unwrap();
    let port = redirect.port().unwrap();

    assert_eq!(
        authorization.as_str().split('?').next().unwrap(),
        "https://auth.openai.com/oauth/authorize"
    );
    assert_eq!(parameters["response_type"], "code");
    assert_eq!(parameters["client_id"], codex::CLIENT_ID);
    assert_eq!(
        parameters["scope"],
        "openid profile email offline_access api.connectors.read api.connectors.invoke"
    );
    assert_eq!(
        parameters["code_challenge"],
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    assert_eq!(parameters["code_challenge_method"], "S256");
    assert_eq!(parameters["id_token_add_organizations"], "true");
    assert_eq!(parameters["codex_cli_simplified_flow"], "true");
    assert_eq!(parameters["state"], "known-state");
    assert_eq!(parameters["originator"], "qq");

    let completion_store = store.clone();
    let completion = thread::spawn(move || login.complete(&completion_store, "default", false));
    let mismatch = callback(port, "code=ignored&state=wrong-state");
    assert!(mismatch.starts_with("HTTP/1.1 400"));
    let success = callback(port, "code=authorization-code&state=known-state");
    assert!(success.starts_with("HTTP/1.1 200"));
    assert_eq!(
        completion.join().unwrap().unwrap(),
        CredentialBackend::Keyring
    );

    assert_eq!(
        client.exchanges.lock().unwrap().as_slice(),
        [(
            "authorization-code".to_owned(),
            parameters["redirect_uri"].clone(),
            verifier.to_owned()
        )]
    );
    let credential = store.resolve_codex("default").unwrap();
    assert_eq!(
        credential.access_token().expose_secret_str().unwrap(),
        "access-token"
    );
    assert_eq!(credential.account_id(), "workspace-test-id");
    assert!(credential.is_fedramp());
    let metadata = store.status("openai-codex/default").unwrap().unwrap();
    assert_eq!(metadata.kind.as_deref(), Some("openai-codex"));
    assert_eq!(metadata.endpoint.as_deref(), Some("https://chatgpt.com"));
}

#[test]
fn codex_resolution_refreshes_an_expired_access_token_once() {
    let (mut store, _keyring, _directory) = test_store();
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3_600;
    let refreshed_access_token = jwt(serde_json::json!({"exp": expires_at}));
    let client = Arc::new(FakeCodexTokenClient::new(
        codex::ExchangedTokens {
            id_token: String::new(),
            access_token: String::new(),
            refresh_token: String::new(),
        },
        codex::RefreshedTokens {
            id_token: None,
            access_token: Some(refreshed_access_token.clone()),
            refresh_token: Some("rotated-refresh-token".to_owned()),
        },
    ));
    store.codex_client = client.clone();
    let expired_access_token = jwt(serde_json::json!({"exp": 1}));
    let stored = serde_json::json!({
        "version": 1,
        "id_token": jwt(serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "workspace-test-id",
                "chatgpt_account_is_fedramp": false
            }
        })),
        "access_token": expired_access_token,
        "refresh_token": "original-refresh-token",
        "account_id": "workspace-test-id",
        "is_fedramp": false,
        "refreshed_at": 0
    });
    store
        .set_with_metadata(
            "openai-codex/work",
            serde_json::to_vec(&stored).unwrap(),
            false,
            Some("openai-codex"),
            Some("https://chatgpt.com"),
        )
        .unwrap();

    let first = store.resolve_codex("work").unwrap();
    let second = store.resolve_codex("work").unwrap();

    assert_eq!(
        first.access_token().expose_secret_str().unwrap(),
        refreshed_access_token
    );
    assert_eq!(
        second.access_token().expose_secret_str().unwrap(),
        refreshed_access_token
    );
    assert_eq!(first.account_id(), "workspace-test-id");
    assert_eq!(
        client.refreshes.lock().unwrap().as_slice(),
        ["original-refresh-token"]
    );
}

#[cfg(unix)]
#[test]
fn codex_refresh_preserves_an_explicit_private_file_backend() {
    use std::os::unix::fs::PermissionsExt;

    let (mut store, keyring, _directory) = test_store();
    keyring.set_mode(FakeMode::Unavailable);
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3_600;
    let refreshed_access_token = jwt(serde_json::json!({"exp": expires_at}));
    let client = Arc::new(FakeCodexTokenClient::new(
        codex::ExchangedTokens {
            id_token: String::new(),
            access_token: String::new(),
            refresh_token: String::new(),
        },
        codex::RefreshedTokens {
            id_token: None,
            access_token: Some(refreshed_access_token.clone()),
            refresh_token: Some("rotated-refresh-token".to_owned()),
        },
    ));
    store.codex_client = client;
    let stored = serde_json::json!({
        "version": 1,
        "id_token": jwt(serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "workspace-test-id",
                "chatgpt_account_is_fedramp": false
            }
        })),
        "access_token": jwt(serde_json::json!({"exp": 1})),
        "refresh_token": "original-refresh-token",
        "account_id": "workspace-test-id",
        "is_fedramp": false,
        "refreshed_at": 0
    });
    store
        .set_with_metadata(
            "openai-codex/file",
            serde_json::to_vec(&stored).unwrap(),
            true,
            Some("openai-codex"),
            Some("https://chatgpt.com"),
        )
        .unwrap();

    let resolved = store.resolve_codex("file").unwrap();

    assert_eq!(
        resolved.access_token().expose_secret_str().unwrap(),
        refreshed_access_token
    );
    assert_eq!(
        store.status("openai-codex/file").unwrap().unwrap().backend,
        CredentialBackend::File
    );
    assert_eq!(
        fs::metadata(store.paths().codex_lock_file())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn corrupt_unknown_and_unsupported_index_state_is_rejected() {
    let (store, _keyring, _directory) = test_store();
    assert!(store.list().unwrap().is_empty());

    write_private(store.paths().index_file(), "not ron");
    assert!(matches!(
        store.list().unwrap_err(),
        AuthError::CorruptState { .. }
    ));

    write_private(
        store.paths().index_file(),
        "(version: 1, records: [], unknown: true)",
    );
    assert!(matches!(
        store.list().unwrap_err(),
        AuthError::CorruptState { .. }
    ));

    write_private(store.paths().index_file(), "(version: 2, records: [])");
    assert!(matches!(
        store.list().unwrap_err(),
        AuthError::UnsupportedStateVersion { version: 2, .. }
    ));
}

#[test]
fn duplicate_names_in_both_state_files_are_rejected() {
    let (store, _keyring, _directory) = test_store();
    assert!(store.list().unwrap().is_empty());
    write_private(
        store.paths().index_file(),
        r#"(
            version: 1,
            records: [
                (name: "openai", backend: File),
                (name: "openai", backend: File),
            ],
        )"#,
    );
    assert!(matches!(
        store.list().unwrap_err(),
        AuthError::DuplicateCredentialName { .. }
    ));

    write_private(
        store.paths().index_file(),
        r#"(version: 1, records: [(name: "openai", backend: File)])"#,
    );
    write_private(
        store.paths().fallback_file(),
        r#"(
            version: 1,
            records: [
                (name: "openai", secret: [1]),
                (name: "openai", secret: [2]),
            ],
        )"#,
    );
    assert!(matches!(
        store
            .resolve(&SecretRef::Stored("openai".to_owned()))
            .unwrap_err(),
        AuthError::DuplicateCredentialName { .. }
    ));
}

#[test]
fn oversized_state_is_rejected_before_parsing() {
    let (store, _keyring, _directory) = test_store();
    assert!(store.list().unwrap().is_empty());
    write_private(store.paths().index_file(), vec![b'x'; MAX_STATE_BYTES + 1]);

    assert!(matches!(
        store.list().unwrap_err(),
        AuthError::StateTooLarge { .. }
    ));
}

#[cfg(unix)]
#[test]
fn symlinked_state_and_data_directories_are_rejected() {
    use std::os::unix::fs::symlink;

    let (store, _keyring, directory) = test_store();
    assert!(store.list().unwrap().is_empty());
    let target = directory.path().join("target.ron");
    write_private(&target, "(version: 1, records: [])");
    symlink(&target, store.paths().index_file()).unwrap();
    assert!(matches!(
        store.list().unwrap_err(),
        AuthError::SymlinkPath { .. }
    ));

    let outer = TestDirectory::new();
    let actual = outer.path().join("actual");
    let linked = outer.path().join("linked");
    fs::create_dir(&actual).unwrap();
    symlink(&actual, &linked).unwrap();
    let linked_store = CredentialStore::with_backend(
        CredentialPaths::new(&linked),
        Arc::new(FakeKeyring::default()),
    );
    assert!(matches!(
        linked_store.list().unwrap_err(),
        AuthError::SymlinkPath { .. }
    ));
}

#[cfg(unix)]
#[test]
fn insecure_existing_state_permissions_are_rejected() {
    use std::os::unix::fs::PermissionsExt;

    let (store, _keyring, _directory) = test_store();
    assert!(store.list().unwrap().is_empty());
    write_private(store.paths().index_file(), "(version: 1, records: [])");
    fs::set_permissions(
        store.paths().index_file(),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();

    assert!(matches!(
        store.list().unwrap_err(),
        AuthError::InsecurePermissions { .. }
    ));
}

#[cfg(unix)]
#[test]
fn file_lock_serializes_concurrent_updates() {
    let (store, keyring, _directory) = test_store();
    keyring.set_mode(FakeMode::Unavailable);
    let barrier = Arc::new(Barrier::new(9));
    let mut threads = Vec::new();

    for index in 0..8 {
        let store = store.clone();
        let barrier = barrier.clone();
        threads.push(thread::spawn(move || {
            barrier.wait();
            store
                .set(
                    &format!("provider/key-{index}"),
                    format!("secret-{index}"),
                    true,
                )
                .unwrap();
        }));
    }
    barrier.wait();
    for thread in threads {
        thread.join().unwrap();
    }

    let list = store.list().unwrap();
    assert_eq!(list.len(), 8);
    for index in 0..8 {
        let secret = store
            .resolve(&SecretRef::Stored(format!("provider/key-{index}")))
            .unwrap();
        assert_eq!(
            secret.expose_secret_str().unwrap(),
            format!("secret-{index}")
        );
    }
}
