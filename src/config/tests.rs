use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use super::*;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestMdmReader {
    reads: Arc<AtomicUsize>,
    content: String,
}

impl managed::MdmReader for TestMdmReader {
    fn read(&self) -> Result<Option<managed::MdmConfiguration>, ConfigError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        Ok(Some(managed::MdmConfiguration::new(
            "test MDM policy",
            self.content.clone(),
        )))
    }
}

struct TempTree {
    root: PathBuf,
}

impl TempTree {
    fn new() -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "qq-config-test-{}-{nanos}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("global")).unwrap();
        fs::create_dir_all(root.join("data")).unwrap();
        fs::create_dir_all(root.join("managed")).unwrap();
        fs::create_dir_all(root.join("work/.git")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.join("data"), fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self { root }
    }

    fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.root.join(relative)
    }

    fn write(&self, relative: impl AsRef<Path>, content: &str) -> PathBuf {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    fn loader(&self) -> ConfigLoader {
        ConfigLoader::new(ConfigPaths::new(
            self.path("global"),
            self.path("data"),
            self.path("managed"),
        ))
    }

    fn request(&self) -> LoadRequest {
        LoadRequest::new(self.path("work"))
            .with_overrides(RuntimeOverrides::new().with_model("openai/test-model"))
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn loads_target_syntax_and_splits_model_on_only_the_first_slash() {
    let tree = TempTree::new();
    let request = LoadRequest::new(tree.path("work")).with_explicit_content(
        r#"(
            version: 1,
            model: "openrouter/anthropic/claude-sonnet-4",
            providers: {
                "openrouter": Custom(
                    connection: (
                        base_url: "https://openrouter.ai/api/v1",
                        api: OpenAiChatCompletions,
                        auth: ApiKey(Env("OPENROUTER_API_KEY")),
                        headers: {
                            "HTTP-Referer": "https://qq.dev",
                            "X-Title": "qq",
                        },
                    ),
                    models: {
                        "anthropic/claude-sonnet-4": (
                            name: "Claude Sonnet 4",
                            reasoning: true,
                            input: [Text, Image],
                            context_window: 200000,
                            max_output_tokens: 64000,
                        ),
                    },
                ),
            },
        )"#,
    );

    let snapshot = tree.loader().load(&request).unwrap();

    assert_eq!(snapshot.model().provider(), "openrouter");
    assert_eq!(snapshot.model().model(), "anthropic/claude-sonnet-4");
    let provider = snapshot.providers().get("openrouter").unwrap();
    let model = provider.models().get("anthropic/claude-sonnet-4").unwrap();
    assert_eq!(model.name(), Some("Claude Sonnet 4"));
    assert!(model.reasoning());
    assert_eq!(model.input(), &[InputModality::Text, InputModality::Image]);
}

#[test]
fn applies_every_layer_in_documented_order() {
    let tree = TempTree::new();
    fs::create_dir_all(tree.path("work/child/deeper")).unwrap();
    tree.write(
        "global/config.ron",
        r#"(version: 1, organization: "global", max_output_tokens: 1)"#,
    );
    tree.write(
        "global/config.d/10-first.ron",
        r#"(version: 1, max_output_tokens: 2)"#,
    );
    tree.write(
        "global/config.d/20-second.ron",
        r#"(version: 1, max_output_tokens: 3)"#,
    );
    tree.write("work/qq.ron", r#"(version: 1, max_output_tokens: 4)"#);
    tree.write("work/child/qq.ron", r#"(version: 1, max_output_tokens: 5)"#);
    tree.write(
        "work/child/deeper/.qq/config.ron",
        r#"(version: 1, max_output_tokens: 6)"#,
    );
    let explicit = tree.write(
        "explicit.ron",
        r#"(version: 1, organization: "explicit", max_output_tokens: 7)"#,
    );
    tree.write(
        "managed/managed.ron",
        r#"(version: 1, organization: "managed", max_output_tokens: 10)"#,
    );

    let request = LoadRequest::new(tree.path("work/child/deeper"))
        .with_explicit_path(explicit)
        .with_explicit_content(r#"(version: 1, organization: "inline", max_output_tokens: 8)"#)
        .with_overrides(
            RuntimeOverrides::new()
                .with_organization("runtime")
                .with_model("openai/runtime")
                .with_max_output_tokens(9),
        );

    let snapshot = tree.loader().load(&request).unwrap();

    assert_eq!(snapshot.organization(), Some("managed"));
    assert_eq!(snapshot.max_output_tokens(), 10);
    assert_eq!(snapshot.model().as_str(), "openai/runtime");
    assert_eq!(
        snapshot.provenance().max_output_tokens().unwrap().kind(),
        SourceKind::Managed
    );
}

#[test]
fn mdm_is_read_once_and_applied_after_managed_files() {
    let tree = TempTree::new();
    tree.write(
        "managed/managed.ron",
        r#"(
            version: 1,
            organization: "managed-file",
            max_output_tokens: 10,
            policy: (allowed_providers: ["openai", "anthropic"]),
        )"#,
    );
    let reads = Arc::new(AtomicUsize::new(0));
    let loader = tree.loader().with_mdm_reader(Arc::new(TestMdmReader {
        reads: Arc::clone(&reads),
        content: r#"(
            version: 1,
            organization: "mdm",
            model: "anthropic/managed-model",
            max_output_tokens: 11,
            policy: (allowed_providers: ["anthropic"]),
        )"#
        .to_owned(),
    }));

    let snapshot = loader.load(&tree.request()).unwrap();

    assert_eq!(reads.load(Ordering::Relaxed), 1);
    assert_eq!(snapshot.organization(), Some("mdm"));
    assert_eq!(snapshot.model().as_str(), "anthropic/managed-model");
    assert_eq!(snapshot.max_output_tokens(), 11);
    assert_eq!(
        snapshot.provenance().organization().unwrap().kind(),
        SourceKind::Mdm
    );
    assert_eq!(
        snapshot.source_reports().last().unwrap().source().kind(),
        SourceKind::Mdm
    );
}

#[test]
fn mdm_content_is_bounded_and_cannot_embed_literal_secrets() {
    let tree = TempTree::new();
    let literal = tree.loader().with_mdm_reader(Arc::new(TestMdmReader {
        reads: Arc::new(AtomicUsize::new(0)),
        content: r#"(
            version: 1,
            providers: {"openai": OpenAi(api_key: Value("mdm-secret"))},
        )"#
        .to_owned(),
    }));
    let error = literal.load(&tree.request()).unwrap_err();
    assert!(matches!(
        error,
        ConfigError::LiteralSecretForbidden { ref origin }
            if origin.kind() == SourceKind::Mdm
    ));
    assert!(!format!("{error:?}").contains("mdm-secret"));

    let oversized = tree.loader().with_mdm_reader(Arc::new(TestMdmReader {
        reads: Arc::new(AtomicUsize::new(0)),
        content: "x".repeat(MAX_CONFIG_BYTES + 1),
    }));
    assert!(matches!(
        oversized.load(&tree.request()),
        Err(ConfigError::SourceTooLarge { origin, .. })
            if origin.kind() == SourceKind::Mdm
    ));
}

#[test]
fn fragment_and_root_to_current_order_are_observable() {
    let tree = TempTree::new();
    tree.write(
        "global/config.d/20-later.ron",
        r#"(version: 1, max_output_tokens: 20)"#,
    );
    tree.write(
        "global/config.d/10-earlier.ron",
        r#"(version: 1, max_output_tokens: 10)"#,
    );
    let global_only = tree.loader().load(&tree.request()).unwrap();
    assert_eq!(global_only.max_output_tokens(), 20);

    fs::create_dir_all(tree.path("work/child")).unwrap();
    tree.write("work/qq.ron", r#"(version: 1, max_output_tokens: 30)"#);
    tree.write(
        "work/child/qq.ron",
        r#"(version: 1, max_output_tokens: 40)"#,
    );
    let child_request = LoadRequest::new(tree.path("work/child"))
        .with_overrides(RuntimeOverrides::new().with_model("openai/test"));
    assert_eq!(
        tree.loader()
            .load(&child_request)
            .unwrap()
            .max_output_tokens(),
        40
    );

    let explicit = tree.write(
        "explicit-order.ron",
        r#"(version: 1, max_output_tokens: 50)"#,
    );
    assert_eq!(
        tree.loader()
            .load(&child_request.with_explicit_path(explicit))
            .unwrap()
            .max_output_tokens(),
        50
    );
}

#[test]
fn clear_and_remove_delete_inherited_values() {
    let tree = TempTree::new();
    tree.write(
        "global/config.ron",
        r#"(
            version: 1,
            organization: "inherited",
            providers: {
                "custom": Custom(
                    connection: (
                        base_url: "https://example.test",
                        api: OpenAiChatCompletions,
                        auth: NoAuth,
                    ),
                    models: {
                        "keep": (name: "Keep"),
                        "drop": (name: "Drop"),
                    },
                ),
            },
        )"#,
    );
    let request = tree.request().with_explicit_content(
        r#"(
            version: 1,
            organization: Clear,
            providers: {
                "anthropic": Remove,
                "custom": Custom(connection: Clear, models: {"drop": Remove}),
            },
        )"#,
    );

    let snapshot = tree.loader().load(&request).unwrap();

    assert_eq!(snapshot.organization(), None);
    assert!(!snapshot.providers().contains_key("anthropic"));
    let custom = snapshot.providers().get("custom").unwrap();
    assert_eq!(custom.connection(), None);
    let models = custom.models();
    assert!(models.contains_key("keep"));
    assert!(!models.contains_key("drop"));

    let literal_clear = tree
        .request()
        .with_explicit_content(r#"(version: 1, organization: "Clear")"#);
    assert_eq!(
        tree.loader().load(&literal_clear).unwrap().organization(),
        Some("Clear")
    );
}

#[test]
fn rejects_duplicate_struct_fields_and_map_keys() {
    let tree = TempTree::new();
    let duplicate_field = tree
        .request()
        .with_explicit_content(r#"(version: 1, max_output_tokens: 1, max_output_tokens: 2)"#);
    let duplicate_map = tree.request().with_explicit_content(
        r#"(
            version: 1,
            providers: {
                "x": Custom(),
                "x": Custom(),
            },
        )"#,
    );

    assert!(matches!(
        tree.loader().load(&duplicate_field),
        Err(ConfigError::Parse { .. })
    ));
    assert!(matches!(
        tree.loader().load(&duplicate_map),
        Err(ConfigError::Parse { .. })
    ));
}

#[test]
fn project_trust_gates_sensitive_changes_and_ignores_safe_edits() {
    let tree = TempTree::new();
    tree.write(
        "work/qq.ron",
        r#"(version: 1, model: "openai/project", max_output_tokens: 10)"#,
    );
    let request = LoadRequest::new(tree.path("work"));

    let first_pending =
        match tree.loader().load(&request).unwrap_err() {
            ConfigError::TrustRequired { pending, reports } => {
                assert!(reports.iter().any(|report| {
                    report.status() == SourceStatus::PartiallyAppliedPendingTrust
                }));
                pending
            }
            error => panic!("unexpected error: {error}"),
        };
    assert_eq!(first_pending.len(), 1);
    let granted = tree.loader().grant_pending_trust(&request).unwrap();
    assert_eq!(granted, first_pending);
    assert_eq!(
        tree.loader().load(&request).unwrap().max_output_tokens(),
        10
    );

    tree.write(
        "work/qq.ron",
        r#"(
            // A safe-only edit and formatting change preserve the trust digest.
            version: 1,
            model: "openai/project",
            max_output_tokens: 20,
        )"#,
    );
    assert_eq!(
        tree.loader().load(&request).unwrap().max_output_tokens(),
        20
    );

    tree.write(
        "work/qq.ron",
        r#"(version: 1, model: "openai/changed", max_output_tokens: 20)"#,
    );
    assert!(matches!(
        tree.loader().load(&request),
        Err(ConfigError::TrustRequired { .. })
    ));
}

#[test]
fn literal_secret_scope_and_debug_output_are_safe() {
    let tree = TempTree::new();
    tree.write(
        "global/config.ron",
        r#"(
            version: 1,
            providers: {"openai": OpenAi(api_key: Value("global-secret"))},
        )"#,
    );
    let snapshot = tree.loader().load(&tree.request()).unwrap();
    let debug = format!("{snapshot:?}");
    assert!(!debug.contains("global-secret"));
    assert!(debug.contains("<redacted>"));

    tree.write(
        "work/qq.ron",
        r#"(
            version: 1,
            providers: {"openai": OpenAi(api_key: Value("project-secret"))},
        )"#,
    );
    assert!(matches!(
        tree.loader().load(&tree.request()),
        Err(ConfigError::LiteralSecretForbidden { .. })
    ));

    fs::remove_file(tree.path("work/qq.ron")).unwrap();
    tree.write(
        "managed/managed.ron",
        r#"(
            version: 1,
            providers: {"openai": OpenAi(api_key: Value("managed-secret"))},
        )"#,
    );
    assert!(matches!(
        tree.loader().load(&tree.request()),
        Err(ConfigError::LiteralSecretForbidden { .. })
    ));
}

#[test]
fn managed_policy_is_monotonic_and_violations_are_errors() {
    let tree = TempTree::new();
    tree.write(
        "managed/managed.ron",
        r#"(
            version: 1,
            policy: (
                allowed_providers: ["openai", "anthropic"],
                max_output_tokens: 100,
            ),
        )"#,
    );
    tree.write(
        "managed/managed.d/10-restrict.ron",
        r#"(
            version: 1,
            policy: (
                allowed_providers: ["openai"],
                denied_providers: ["anthropic"],
                max_output_tokens: 50,
            ),
        )"#,
    );
    let request = LoadRequest::new(tree.path("work")).with_overrides(
        RuntimeOverrides::new()
            .with_model("openai/test")
            .with_max_output_tokens(51),
    );

    assert!(matches!(
        tree.loader().load(&request),
        Err(ConfigError::PolicyViolation {
            rule: "max_output_tokens",
            ..
        })
    ));
}

#[test]
fn require_https_and_custom_provider_policy_are_enforced() {
    let tree = TempTree::new();
    tree.write(
        "global/config.ron",
        r#"(
            version: 1,
            providers: {
                "custom": Custom(connection: (
                    base_url: "http://localhost:8080",
                    api: OpenAiChatCompletions,
                    auth: NoAuth,
                )),
            },
        )"#,
    );
    tree.write(
        "managed/managed.ron",
        r#"(
            version: 1,
            policy: (require_https: true, allow_custom_providers: true),
        )"#,
    );
    let request = LoadRequest::new(tree.path("work"))
        .with_overrides(RuntimeOverrides::new().with_model("custom/model"));

    assert!(matches!(
        tree.loader().load(&request),
        Err(ConfigError::PolicyViolation {
            rule: "require_https",
            ..
        })
    ));
}

#[test]
fn custom_provider_policy_classifies_litellm_as_a_custom_endpoint() {
    let tree = TempTree::new();
    tree.write(
        "global/config.ron",
        r#"(
            version: 1,
            providers: {
                "gateway": LiteLlm(connection: (
                    base_url: "https://gateway.example.test/v1",
                    api: OpenAiChatCompletions,
                    auth: NoAuth,
                )),
            },
        )"#,
    );
    tree.write(
        "managed/managed.ron",
        r#"(version: 1, policy: (allow_custom_providers: false))"#,
    );
    tree.write(
        "managed/managed.d/99-cannot-relax.ron",
        r#"(version: 1, policy: (allow_custom_providers: true))"#,
    );
    let request = LoadRequest::new(tree.path("work"))
        .with_overrides(RuntimeOverrides::new().with_model("gateway/model"));

    assert!(matches!(
        tree.loader().load(&request),
        Err(ConfigError::PolicyViolation {
            rule: "allow_custom_providers",
            ..
        })
    ));
}

#[test]
fn malformed_unknown_and_wrong_version_documents_are_rejected() {
    let tree = TempTree::new();
    for content in [
        "this is not ron",
        r#"(version: 1, mystery: true)"#,
        r#"(version: 2)"#,
        r#"(model: "openai/test")"#,
    ] {
        let request = tree.request().with_explicit_content(content);
        assert!(tree.loader().load(&request).is_err(), "accepted {content}");
    }

    let policy = tree
        .request()
        .with_explicit_content(r#"(version: 1, policy: (require_https: true))"#);
    assert!(matches!(
        tree.loader().load(&policy),
        Err(ConfigError::PolicyOutsideManaged { .. })
    ));
}

#[test]
fn explicit_missing_file_is_fatal() {
    let tree = TempTree::new();
    let request = tree
        .request()
        .with_explicit_path(tree.path("does-not-exist.ron"));

    assert!(matches!(
        tree.loader().load(&request),
        Err(ConfigError::ExplicitConfigMissing { .. })
    ));
}

#[cfg(unix)]
#[test]
fn rejects_symlink_sources() {
    use std::os::unix::fs::symlink;

    let tree = TempTree::new();
    let target = tree.write("target.ron", r#"(version: 1)"#);
    let link = tree.path("link.ron");
    symlink(target, &link).unwrap();
    let request = tree.request().with_explicit_path(link);

    assert!(matches!(
        tree.loader().load(&request),
        Err(ConfigError::SymlinkSource { .. })
    ));
}
