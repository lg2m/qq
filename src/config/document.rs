use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    marker::PhantomData,
};

use ron::{Options, extensions::Extensions};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, EnumAccess, MapAccess, VariantAccess, Visitor},
};
use sha2::{Digest, Sha256};

use super::{
    AwsAuth, BedrockAuth, ConfigError, ConfigKey, ConfigProvenance, ConfigSnapshot, Connection,
    DEFAULT_MAX_OUTPUT_TOKENS, EffectivePolicy, InputModality, ModelMetadata, ModelRoute,
    ProviderApi, ProviderConfig, RuntimeOverrides, SecretRef, SourceIdentity, SourceKind,
    SourceReport,
};

pub(super) fn deserialize_unique_btree_map<'de, D, K, V>(
    deserializer: D,
) -> Result<BTreeMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    K: Deserialize<'de> + Ord + fmt::Debug,
    V: Deserialize<'de>,
{
    struct UniqueMapVisitor<K, V>(PhantomData<(K, V)>);

    impl<'de, K, V> Visitor<'de> for UniqueMapVisitor<K, V>
    where
        K: Deserialize<'de> + Ord + fmt::Debug,
        V: Deserialize<'de>,
    {
        type Value = BTreeMap<K, V>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map without duplicate keys")
        }

        fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = access.next_entry()? {
                if values.contains_key(&key) {
                    return Err(de::Error::custom(format_args!("duplicate map key {key:?}")));
                }
                values.insert(key, value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_map(UniqueMapVisitor(PhantomData))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
struct UniqueMap<K: Ord, V>(BTreeMap<K, V>);

impl<'de, K, V> Deserialize<'de> for UniqueMap<K, V>
where
    K: Deserialize<'de> + Ord + fmt::Debug,
    V: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_unique_btree_map(deserializer).map(Self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum ClearMarker {
    Clear,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Field<T> {
    #[default]
    Missing,
    Set(T),
    Clear,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum StringField {
    #[default]
    Missing,
    Set(String),
    Clear,
}

impl StringField {
    fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }

    fn is_present(&self) -> bool {
        !self.is_missing()
    }
}

impl Serialize for StringField {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Set(value) => serializer.serialize_newtype_variant("Patch", 0, "Set", value),
            Self::Clear => serializer.serialize_unit_variant("Patch", 1, "Clear"),
            Self::Missing => serializer.serialize_unit(),
        }
    }
}

impl<'de> Deserialize<'de> for StringField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StringVisitor;

        impl<'de> Visitor<'de> for StringVisitor {
            type Value = StringField;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a quoted string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StringField::Set(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StringField::Set(value))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StringField::Clear)
            }

            fn visit_enum<A>(self, data: A) -> Result<Self::Value, A::Error>
            where
                A: EnumAccess<'de>,
            {
                let (variant, access) = data.variant::<String>()?;
                if variant != "Clear" {
                    return Err(de::Error::unknown_variant(&variant, &["Clear"]));
                }
                access.unit_variant()?;
                Ok(StringField::Clear)
            }
        }

        deserializer.deserialize_any(StringVisitor)
    }
}

impl<T> Field<T> {
    fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }

    fn is_present(&self) -> bool {
        !self.is_missing()
    }
}

impl<T: Serialize> Serialize for Field<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Set(value) => serializer.serialize_newtype_variant("Patch", 0, "Set", value),
            Self::Clear => serializer.serialize_unit_variant("Patch", 1, "Clear"),
            Self::Missing => serializer.serialize_unit(),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Field<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Present<T> {
            Clear(ClearMarker),
            Set(T),
        }

        match Present::deserialize(deserializer)? {
            Present::Clear(ClearMarker::Clear) => Ok(Self::Clear),
            Present::Set(value) => Ok(Self::Set(value)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum RemoveMarker {
    Remove,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
enum ModelEntryPatch {
    Set(ModelPatch),
    Remove(RemoveMarker),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ModelPatch {
    #[serde(skip_serializing_if = "StringField::is_missing")]
    name: StringField,
    #[serde(skip_serializing_if = "Field::is_missing")]
    reasoning: Field<bool>,
    #[serde(skip_serializing_if = "Field::is_missing")]
    input: Field<Vec<InputModality>>,
    #[serde(skip_serializing_if = "Field::is_missing")]
    context_window: Field<u32>,
    #[serde(skip_serializing_if = "Field::is_missing")]
    max_output_tokens: Field<u32>,
    #[serde(skip_serializing_if = "Field::is_missing")]
    pricing: Field<qq_protocol::ModelPricing>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum ProviderEntryPatch {
    OpenAi {
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        api_key: Field<SecretRef>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        models: Field<UniqueMap<String, ModelEntryPatch>>,
    },
    OpenAiCodex {
        #[serde(default, skip_serializing_if = "StringField::is_missing")]
        profile: StringField,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        models: Field<UniqueMap<String, ModelEntryPatch>>,
    },
    Anthropic {
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        api_key: Field<SecretRef>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        models: Field<UniqueMap<String, ModelEntryPatch>>,
    },
    Google {
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        api_key: Field<SecretRef>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        models: Field<UniqueMap<String, ModelEntryPatch>>,
    },
    LiteLlm {
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        connection: Field<Connection>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        models: Field<UniqueMap<String, ModelEntryPatch>>,
    },
    AmazonBedrock {
        #[serde(default, skip_serializing_if = "StringField::is_missing")]
        region: StringField,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        auth: Field<BedrockAuth>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        models: Field<UniqueMap<String, ModelEntryPatch>>,
    },
    AmazonBedrockMantle {
        #[serde(default, skip_serializing_if = "StringField::is_missing")]
        region: StringField,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        api: Field<ProviderApi>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        auth: Field<BedrockAuth>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        models: Field<UniqueMap<String, ModelEntryPatch>>,
    },
    Custom {
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        connection: Field<Connection>,
        #[serde(default, skip_serializing_if = "Field::is_missing")]
        models: Field<UniqueMap<String, ModelEntryPatch>>,
    },
    Remove,
}

impl ProviderEntryPatch {
    pub(super) fn contains_literal_secret(&self) -> bool {
        match self {
            Self::OpenAi { api_key, .. }
            | Self::Anthropic { api_key, .. }
            | Self::Google { api_key, .. } => {
                matches!(api_key, Field::Set(SecretRef::Value(_)))
            }
            Self::LiteLlm { connection, .. } | Self::Custom { connection, .. } => {
                matches!(connection, Field::Set(value) if value.contains_literal_secret())
            }
            Self::AmazonBedrock { auth, .. } | Self::AmazonBedrockMantle { auth, .. } => {
                matches!(auth, Field::Set(value) if value.contains_literal_secret())
            }
            Self::OpenAiCodex { .. } | Self::Remove => false,
        }
    }

    fn references_local_credential(&self) -> bool {
        match self {
            Self::OpenAi { api_key, .. }
            | Self::Anthropic { api_key, .. }
            | Self::Google { api_key, .. } => matches!(api_key, Field::Set(_)),
            Self::OpenAiCodex { profile, .. } => matches!(profile, StringField::Set(_)),
            Self::LiteLlm { connection, .. } | Self::Custom { connection, .. } => {
                matches!(
                    connection,
                    Field::Set(value) if value.references_local_credential()
                )
            }
            Self::AmazonBedrock { auth, .. } | Self::AmazonBedrockMantle { auth, .. } => {
                matches!(auth, Field::Set(value) if value.references_local_credential())
            }
            Self::Remove => false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PolicyPatch {
    allowed_providers: Option<Vec<String>>,
    denied_providers: Option<Vec<String>>,
    max_output_tokens: Option<u32>,
    require_https: Option<bool>,
    allow_custom_providers: Option<bool>,
    allow_literal_secrets: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Document {
    version: u32,
    #[serde(default, skip_serializing_if = "StringField::is_missing")]
    organization: StringField,
    #[serde(default, skip_serializing_if = "StringField::is_missing")]
    model: StringField,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    max_output_tokens: Field<u32>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    providers: Field<UniqueMap<String, ProviderEntryPatch>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    policy: Option<PolicyPatch>,
}

impl Document {
    pub(super) fn parse(content: &str, origin: &SourceIdentity) -> Result<Self, ConfigError> {
        let options = Options::default().with_default_extension(Extensions::IMPLICIT_SOME);
        let document: Self = options
            .from_str(content)
            .map_err(|error| ConfigError::Parse {
                origin: origin.clone(),
                message: error.to_string(),
            })?;
        if document.version != 1 {
            return Err(ConfigError::UnsupportedVersion {
                origin: origin.clone(),
                version: document.version,
            });
        }
        document.validate(origin)?;
        Ok(document)
    }

    fn validate(&self, origin: &SourceIdentity) -> Result<(), ConfigError> {
        if self.policy.is_some() && !matches!(origin.kind(), SourceKind::Managed | SourceKind::Mdm)
        {
            return Err(ConfigError::PolicyOutsideManaged {
                origin: origin.clone(),
            });
        }
        if self.contains_literal_secret()
            && !matches!(
                origin.kind(),
                SourceKind::Global | SourceKind::Explicit | SourceKind::Inline
            )
        {
            return Err(ConfigError::LiteralSecretForbidden {
                origin: origin.clone(),
            });
        }
        if origin.kind() == SourceKind::Remote && self.references_local_credential() {
            return Err(ConfigError::RemoteCredentialReferenceForbidden {
                origin: origin.clone(),
            });
        }
        if let Some(policy) = &self.policy {
            validate_policy_names(policy, origin)?;
        }
        Ok(())
    }

    pub(super) fn has_sensitive_operations(&self) -> bool {
        self.organization.is_present() || self.model.is_present() || self.providers.is_present()
    }

    pub(super) fn sensitive_digest(&self) -> Result<Option<String>, ConfigError> {
        if !self.has_sensitive_operations() {
            return Ok(None);
        }

        #[derive(Serialize)]
        struct SensitiveProjection<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            organization: Option<&'a StringField>,
            #[serde(skip_serializing_if = "Option::is_none")]
            model: Option<&'a StringField>,
            #[serde(skip_serializing_if = "Option::is_none")]
            providers: Option<&'a Field<UniqueMap<String, ProviderEntryPatch>>>,
        }

        fn present<T>(field: &Field<T>) -> Option<&Field<T>> {
            field.is_present().then_some(field)
        }

        let projection = SensitiveProjection {
            organization: self.organization.is_present().then_some(&self.organization),
            model: self.model.is_present().then_some(&self.model),
            providers: present(&self.providers),
        };
        let canonical =
            serde_json::to_vec(&projection).map_err(|error| ConfigError::StateSerialization {
                message: error.to_string(),
            })?;
        let digest = Sha256::digest(canonical);
        let mut encoded = String::with_capacity(64);
        for byte in digest {
            use fmt::Write as _;
            write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Ok(Some(encoded))
    }

    pub(super) fn touched(&self) -> Vec<ConfigKey> {
        let mut touched = Vec::new();
        if self.organization.is_present() {
            touched.push(ConfigKey::Organization);
        }
        if self.model.is_present() {
            touched.push(ConfigKey::Model);
        }
        if self.max_output_tokens.is_present() {
            touched.push(ConfigKey::MaxOutputTokens);
        }
        if self.providers.is_present() {
            touched.push(ConfigKey::Providers);
            if let Field::Set(providers) = &self.providers {
                touched.extend(providers.0.keys().cloned().map(ConfigKey::Provider));
            }
        }
        if self.policy.is_some() {
            touched.push(ConfigKey::Policy);
        }
        touched
    }

    pub(super) fn contains_literal_secret(&self) -> bool {
        match &self.providers {
            Field::Set(providers) => providers
                .0
                .values()
                .any(ProviderEntryPatch::contains_literal_secret),
            Field::Missing | Field::Clear => false,
        }
    }

    fn references_local_credential(&self) -> bool {
        match &self.providers {
            Field::Set(providers) => providers
                .0
                .values()
                .any(ProviderEntryPatch::references_local_credential),
            Field::Missing | Field::Clear => false,
        }
    }

    pub(super) fn apply_organization(&self, organization: &mut Option<String>) -> bool {
        apply_optional_string(&self.organization, organization)
    }

    pub(super) fn matches_organization(&self, name: &str) -> bool {
        matches!(&self.organization, StringField::Set(value) if value == name)
    }
}

fn validate_policy_names(policy: &PolicyPatch, origin: &SourceIdentity) -> Result<(), ConfigError> {
    for (field, values) in [
        ("allowed_providers", policy.allowed_providers.as_ref()),
        ("denied_providers", policy.denied_providers.as_ref()),
    ] {
        let Some(values) = values else {
            continue;
        };
        let mut unique = BTreeSet::new();
        for value in values {
            if value.is_empty() {
                return Err(ConfigError::Parse {
                    origin: origin.clone(),
                    message: format!("policy field {field} contains an empty provider name"),
                });
            }
            if !unique.insert(value) {
                return Err(ConfigError::Parse {
                    origin: origin.clone(),
                    message: format!("policy field {field} contains duplicate value {value:?}"),
                });
            }
        }
    }
    Ok(())
}

pub(super) struct MergeState {
    organization: Option<String>,
    model: Option<String>,
    max_output_tokens: u32,
    providers: BTreeMap<String, ProviderConfig>,
    policy: EffectivePolicy,
    provenance: ConfigProvenance,
}

impl MergeState {
    pub(super) fn compiled() -> (Self, SourceReport) {
        let source = SourceIdentity::virtual_source(SourceKind::Compiled, "compiled defaults");
        let providers = BTreeMap::from([
            (
                "anthropic".to_owned(),
                ProviderConfig::Anthropic {
                    api_key: None,
                    models: crate::models::builtin_models("anthropic"),
                },
            ),
            (
                "bedrock".to_owned(),
                ProviderConfig::AmazonBedrock {
                    region: None,
                    auth: BedrockAuth::Aws(AwsAuth::DefaultChain),
                    models: crate::models::builtin_models("bedrock"),
                },
            ),
            (
                "bedrock-mantle".to_owned(),
                ProviderConfig::AmazonBedrockMantle {
                    region: None,
                    api: ProviderApi::OpenAiResponses,
                    auth: BedrockAuth::Aws(AwsAuth::DefaultChain),
                    models: crate::models::builtin_models("bedrock-mantle"),
                },
            ),
            (
                "google".to_owned(),
                ProviderConfig::Google {
                    api_key: None,
                    models: crate::models::builtin_models("google"),
                },
            ),
            (
                "openai".to_owned(),
                ProviderConfig::OpenAi {
                    api_key: None,
                    models: crate::models::builtin_models("openai"),
                },
            ),
            (
                "openai-codex".to_owned(),
                ProviderConfig::OpenAiCodex {
                    profile: None,
                    models: crate::models::builtin_models("openai-codex"),
                },
            ),
        ]);
        let provenance = ConfigProvenance {
            max_output_tokens: Some(source.clone()),
            providers: providers
                .keys()
                .cloned()
                .map(|name| (name, source.clone()))
                .collect(),
            ..ConfigProvenance::default()
        };
        let report = SourceReport::new(
            source,
            super::SourceStatus::Applied,
            vec![ConfigKey::MaxOutputTokens, ConfigKey::Providers],
        );
        (
            Self {
                organization: None,
                model: None,
                max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
                providers,
                policy: EffectivePolicy::default(),
                provenance,
            },
            report,
        )
    }

    pub(super) fn apply_document(
        &mut self,
        document: &Document,
        source: &SourceIdentity,
        sensitive: bool,
    ) {
        apply_default(
            &document.max_output_tokens,
            &mut self.max_output_tokens,
            DEFAULT_MAX_OUTPUT_TOKENS,
        );
        if document.max_output_tokens.is_present() {
            self.provenance.max_output_tokens = Some(source.clone());
        }
        if !sensitive {
            return;
        }

        if apply_optional_string(&document.organization, &mut self.organization) {
            self.provenance.organization = Some(source.clone());
        }
        if apply_optional_string(&document.model, &mut self.model) {
            self.provenance.model = Some(source.clone());
        }
        self.apply_providers(&document.providers, source);
        if let Some(policy) = &document.policy {
            self.compose_policy(policy);
        }
    }

    pub(super) fn apply_runtime(
        &mut self,
        overrides: &RuntimeOverrides,
        source: &SourceIdentity,
    ) -> Vec<ConfigKey> {
        let mut touched = Vec::new();
        if let Some(organization) = &overrides.organization {
            self.organization = Some(organization.clone());
            self.provenance.organization = Some(source.clone());
            touched.push(ConfigKey::Organization);
        }
        if let Some(model) = &overrides.model {
            self.model = Some(model.clone());
            self.provenance.model = Some(source.clone());
            touched.push(ConfigKey::Model);
        }
        if let Some(max_output_tokens) = overrides.max_output_tokens {
            self.max_output_tokens = max_output_tokens;
            self.provenance.max_output_tokens = Some(source.clone());
            touched.push(ConfigKey::MaxOutputTokens);
        }
        touched
    }

    fn apply_providers(
        &mut self,
        patch: &Field<UniqueMap<String, ProviderEntryPatch>>,
        source: &SourceIdentity,
    ) {
        match patch {
            Field::Missing => {}
            Field::Clear => {
                for name in self.providers.keys() {
                    self.provenance
                        .providers
                        .insert(name.clone(), source.clone());
                }
                self.providers.clear();
            }
            Field::Set(patches) => {
                for (name, patch) in &patches.0 {
                    self.apply_provider(name, patch);
                    self.provenance
                        .providers
                        .insert(name.clone(), source.clone());
                }
            }
        }
    }

    fn apply_provider(&mut self, name: &str, patch: &ProviderEntryPatch) {
        match patch {
            ProviderEntryPatch::Remove => {
                self.providers.remove(name);
            }
            ProviderEntryPatch::OpenAi { api_key, models } => {
                let provider = self.providers.entry(name.to_owned()).or_insert_with(|| {
                    ProviderConfig::OpenAi {
                        api_key: None,
                        models: BTreeMap::new(),
                    }
                });
                if !matches!(provider, ProviderConfig::OpenAi { .. }) {
                    *provider = ProviderConfig::OpenAi {
                        api_key: None,
                        models: BTreeMap::new(),
                    };
                }
                if let ProviderConfig::OpenAi {
                    api_key: current,
                    models: current_models,
                } = provider
                {
                    apply_optional(api_key, current);
                    apply_models(models, current_models);
                }
            }
            ProviderEntryPatch::OpenAiCodex { profile, models } => {
                let provider = self.providers.entry(name.to_owned()).or_insert_with(|| {
                    ProviderConfig::OpenAiCodex {
                        profile: None,
                        models: BTreeMap::new(),
                    }
                });
                if !matches!(provider, ProviderConfig::OpenAiCodex { .. }) {
                    *provider = ProviderConfig::OpenAiCodex {
                        profile: None,
                        models: BTreeMap::new(),
                    };
                }
                if let ProviderConfig::OpenAiCodex {
                    profile: current,
                    models: current_models,
                } = provider
                {
                    apply_optional_string(profile, current);
                    apply_models(models, current_models);
                }
            }
            ProviderEntryPatch::Anthropic { api_key, models } => {
                let provider = self.providers.entry(name.to_owned()).or_insert_with(|| {
                    ProviderConfig::Anthropic {
                        api_key: None,
                        models: BTreeMap::new(),
                    }
                });
                if !matches!(provider, ProviderConfig::Anthropic { .. }) {
                    *provider = ProviderConfig::Anthropic {
                        api_key: None,
                        models: BTreeMap::new(),
                    };
                }
                if let ProviderConfig::Anthropic {
                    api_key: current,
                    models: current_models,
                } = provider
                {
                    apply_optional(api_key, current);
                    apply_models(models, current_models);
                }
            }
            ProviderEntryPatch::Google { api_key, models } => {
                let provider = self.providers.entry(name.to_owned()).or_insert_with(|| {
                    ProviderConfig::Google {
                        api_key: None,
                        models: BTreeMap::new(),
                    }
                });
                if !matches!(provider, ProviderConfig::Google { .. }) {
                    *provider = ProviderConfig::Google {
                        api_key: None,
                        models: BTreeMap::new(),
                    };
                }
                if let ProviderConfig::Google {
                    api_key: current,
                    models: current_models,
                } = provider
                {
                    apply_optional(api_key, current);
                    apply_models(models, current_models);
                }
            }
            ProviderEntryPatch::LiteLlm { connection, models } => {
                let provider = self.providers.entry(name.to_owned()).or_insert_with(|| {
                    ProviderConfig::LiteLlm {
                        connection: None,
                        models: BTreeMap::new(),
                    }
                });
                if !matches!(provider, ProviderConfig::LiteLlm { .. }) {
                    *provider = ProviderConfig::LiteLlm {
                        connection: None,
                        models: BTreeMap::new(),
                    };
                }
                if let ProviderConfig::LiteLlm {
                    connection: current,
                    models: current_models,
                } = provider
                {
                    // Connection is deliberately atomic rather than field-merged.
                    apply_optional(connection, current);
                    apply_models(models, current_models);
                }
            }
            ProviderEntryPatch::AmazonBedrock {
                region,
                auth,
                models,
            } => {
                let provider = self.providers.entry(name.to_owned()).or_insert_with(|| {
                    ProviderConfig::AmazonBedrock {
                        region: None,
                        auth: BedrockAuth::Aws(AwsAuth::DefaultChain),
                        models: BTreeMap::new(),
                    }
                });
                if !matches!(provider, ProviderConfig::AmazonBedrock { .. }) {
                    *provider = ProviderConfig::AmazonBedrock {
                        region: None,
                        auth: BedrockAuth::Aws(AwsAuth::DefaultChain),
                        models: BTreeMap::new(),
                    };
                }
                if let ProviderConfig::AmazonBedrock {
                    region: current_region,
                    auth: current_auth,
                    models: current_models,
                } = provider
                {
                    apply_optional_string(region, current_region);
                    apply_default(auth, current_auth, BedrockAuth::Aws(AwsAuth::DefaultChain));
                    apply_models(models, current_models);
                }
            }
            ProviderEntryPatch::AmazonBedrockMantle {
                region,
                api,
                auth,
                models,
            } => {
                let provider = self.providers.entry(name.to_owned()).or_insert_with(|| {
                    ProviderConfig::AmazonBedrockMantle {
                        region: None,
                        api: ProviderApi::OpenAiResponses,
                        auth: BedrockAuth::Aws(AwsAuth::DefaultChain),
                        models: BTreeMap::new(),
                    }
                });
                if !matches!(provider, ProviderConfig::AmazonBedrockMantle { .. }) {
                    *provider = ProviderConfig::AmazonBedrockMantle {
                        region: None,
                        api: ProviderApi::OpenAiResponses,
                        auth: BedrockAuth::Aws(AwsAuth::DefaultChain),
                        models: BTreeMap::new(),
                    };
                }
                if let ProviderConfig::AmazonBedrockMantle {
                    region: current_region,
                    api: current_api,
                    auth: current_auth,
                    models: current_models,
                } = provider
                {
                    apply_optional_string(region, current_region);
                    apply_default(api, current_api, ProviderApi::OpenAiResponses);
                    apply_default(auth, current_auth, BedrockAuth::Aws(AwsAuth::DefaultChain));
                    apply_models(models, current_models);
                }
            }
            ProviderEntryPatch::Custom { connection, models } => {
                let provider = self.providers.entry(name.to_owned()).or_insert_with(|| {
                    ProviderConfig::Custom {
                        connection: None,
                        models: BTreeMap::new(),
                    }
                });
                if !matches!(provider, ProviderConfig::Custom { .. }) {
                    *provider = ProviderConfig::Custom {
                        connection: None,
                        models: BTreeMap::new(),
                    };
                }
                if let ProviderConfig::Custom {
                    connection: current,
                    models: current_models,
                } = provider
                {
                    // Connection is deliberately atomic rather than field-merged.
                    apply_optional(connection, current);
                    apply_models(models, current_models);
                }
            }
        }
    }

    fn compose_policy(&mut self, patch: &PolicyPatch) {
        if let Some(incoming) = &patch.allowed_providers {
            let incoming: BTreeSet<_> = incoming.iter().cloned().collect();
            let combined = match &self.policy.allowed_providers {
                Some(current) => current
                    .iter()
                    .filter(|name| incoming.contains(*name))
                    .cloned()
                    .collect(),
                None => incoming.into_iter().collect(),
            };
            self.policy.allowed_providers = Some(combined);
        }
        if let Some(incoming) = &patch.denied_providers {
            let mut combined: BTreeSet<_> = self.policy.denied_providers.iter().cloned().collect();
            combined.extend(incoming.iter().cloned());
            self.policy.denied_providers = combined.into_iter().collect();
        }
        if let Some(incoming) = patch.max_output_tokens {
            self.policy.max_output_tokens = Some(
                self.policy
                    .max_output_tokens
                    .map_or(incoming, |current| current.min(incoming)),
            );
        }
        if let Some(incoming) = patch.require_https {
            self.policy.require_https |= incoming;
        }
        if let Some(incoming) = patch.allow_custom_providers {
            self.policy.allow_custom_providers &= incoming;
        }
        if let Some(incoming) = patch.allow_literal_secrets {
            self.policy.allow_literal_secrets &= incoming;
        }
    }

    pub(super) fn finish(self, reports: Vec<SourceReport>) -> Result<ConfigSnapshot, ConfigError> {
        let model = ModelRoute::parse(self.model.ok_or(ConfigError::ModelRequired)?)?;
        if !self.providers.contains_key(model.provider()) {
            return Err(ConfigError::UnknownProvider(model.provider().to_owned()));
        }
        enforce_policy(
            &self.policy,
            &model,
            self.max_output_tokens,
            &self.providers,
        )?;
        Ok(ConfigSnapshot {
            organization: self.organization,
            model,
            max_output_tokens: self.max_output_tokens,
            providers: self.providers,
            policy: self.policy,
            reports,
            provenance: self.provenance,
        })
    }
}

fn apply_optional<T: Clone>(field: &Field<T>, current: &mut Option<T>) -> bool {
    match field {
        Field::Missing => false,
        Field::Set(value) => {
            *current = Some(value.clone());
            true
        }
        Field::Clear => {
            *current = None;
            true
        }
    }
}

fn apply_optional_string(field: &StringField, current: &mut Option<String>) -> bool {
    match field {
        StringField::Missing => false,
        StringField::Set(value) => {
            *current = Some(value.clone());
            true
        }
        StringField::Clear => {
            *current = None;
            true
        }
    }
}

fn apply_default<T: Clone>(field: &Field<T>, current: &mut T, default: T) -> bool {
    match field {
        Field::Missing => false,
        Field::Set(value) => {
            *current = value.clone();
            true
        }
        Field::Clear => {
            *current = default;
            true
        }
    }
}

fn apply_models(
    patch: &Field<UniqueMap<String, ModelEntryPatch>>,
    models: &mut BTreeMap<String, ModelMetadata>,
) {
    match patch {
        Field::Missing => {}
        Field::Clear => models.clear(),
        Field::Set(patches) => {
            for (name, patch) in &patches.0 {
                match patch {
                    ModelEntryPatch::Remove(RemoveMarker::Remove) => {
                        models.remove(name);
                    }
                    ModelEntryPatch::Set(patch) => {
                        apply_model_patch(models.entry(name.clone()).or_default(), patch);
                    }
                }
            }
        }
    }
}

fn apply_model_patch(model: &mut ModelMetadata, patch: &ModelPatch) {
    apply_optional_string(&patch.name, &mut model.name);
    apply_default(&patch.reasoning, &mut model.reasoning, false);
    apply_default(&patch.input, &mut model.input, Vec::new());
    apply_optional(&patch.context_window, &mut model.context_window);
    apply_optional(&patch.max_output_tokens, &mut model.max_output_tokens);
    apply_optional(&patch.pricing, &mut model.pricing);
}

fn enforce_policy(
    policy: &EffectivePolicy,
    model: &ModelRoute,
    max_output_tokens: u32,
    providers: &BTreeMap<String, ProviderConfig>,
) -> Result<(), ConfigError> {
    if let Some(allowed) = &policy.allowed_providers
        && !allowed.iter().any(|provider| provider == model.provider())
    {
        return Err(policy_violation(
            "allowed_providers",
            format!("provider {:?} is not allowed", model.provider()),
        ));
    }
    if policy
        .denied_providers
        .iter()
        .any(|provider| provider == model.provider())
    {
        return Err(policy_violation(
            "denied_providers",
            format!("provider {:?} is denied", model.provider()),
        ));
    }
    if let Some(limit) = policy.max_output_tokens
        && max_output_tokens > limit
    {
        return Err(policy_violation(
            "max_output_tokens",
            format!("configured value {max_output_tokens} exceeds {limit}"),
        ));
    }
    if !policy.allow_custom_providers
        && providers.values().any(ProviderConfig::uses_custom_endpoint)
    {
        return Err(policy_violation(
            "allow_custom_providers",
            "a custom or LiteLLM provider is configured".to_owned(),
        ));
    }
    if !policy.allow_literal_secrets
        && providers
            .values()
            .any(ProviderConfig::contains_literal_secret)
    {
        return Err(policy_violation(
            "allow_literal_secrets",
            "a literal secret or static header value is configured".to_owned(),
        ));
    }
    if policy.require_https {
        for (name, provider) in providers {
            if let Some(connection) = provider.connection()
                && !has_https_scheme(connection.base_url())
            {
                return Err(policy_violation(
                    "require_https",
                    format!("provider {name:?} has a non-HTTPS base URL"),
                ));
            }
        }
    }
    Ok(())
}

fn has_https_scheme(value: &str) -> bool {
    value
        .get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
}

fn policy_violation(rule: &'static str, message: String) -> ConfigError {
    ConfigError::PolicyViolation { rule, message }
}
