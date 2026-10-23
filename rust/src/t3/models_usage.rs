use super::{
    failure::sanitize_failure_text,
    http_client::{ClientError, ErrorKind},
    rpc_client::{RpcEvent, StreamEnd},
};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{DeserializeOwned, Error},
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

#[derive(Deserialize)]
#[serde(try_from = "Map<String, Value>")]
pub struct ProvidersConfig {
    pub providers: Vec<ProviderSnapshot>,
}

impl TryFrom<Map<String, Value>> for ProvidersConfig {
    type Error = serde_json::Error;

    fn try_from(mut value: Map<String, Value>) -> Result<Self, Self::Error> {
        let providers = value
            .remove("providers")
            .ok_or_else(|| Self::Error::missing_field("providers"))?;
        Ok(Self {
            providers: objects(providers)?,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSnapshot {
    #[serde(flatten)]
    provider: Provider,
    #[serde(default, deserialize_with = "present_object")]
    usage_limits: Option<UsageLimits>,
    #[serde(default, deserialize_with = "present_object")]
    auth: Option<ProviderAuth>,
}

impl ProviderSnapshot {
    pub fn instance_id(&self) -> &str {
        &self.provider.instance_id.0
    }

    pub fn into_usage(self) -> Value {
        let usage = usage_value(self.usage_limits);
        let mut value = serde_json::to_value(self.provider).expect("validated provider catalog");
        if let Some(label) = self.auth.and_then(|auth| auth.label) {
            value["accountLabel"] = json!(sanitize_failure_text(&label.0, 2000));
        }
        value["usage"] = usage;
        value
    }
}

#[derive(Deserialize)]
struct ProviderAuth {
    #[serde(default, deserialize_with = "present")]
    label: Option<Nonempty>,
}

fn usage_value(limits: Option<UsageLimits>) -> Value {
    match limits {
        None => json!({"status":"missing","windows":[]}),
        Some(snapshot) => {
            let status = match &snapshot.unavailable {
                Some(unavailable) => match unavailable.reason {
                    UnavailableReason::Unsupported => "unsupported",
                    UnavailableReason::ProbeFailed => "probeFailed",
                },
                None if snapshot.windows.is_empty() => "empty",
                None => "reported",
            };
            let windows = snapshot
                .windows
                .into_iter()
                .map(|window| {
                    let remaining = 100.0 - window.used_percent.0;
                    let mut value = serde_json::to_value(window).expect("validated usage window");
                    value["remainingPercent"] = json!(remaining);
                    value
                })
                .collect::<Vec<_>>();
            let mut value =
                json!({"status":status,"checkedAt":snapshot.checked_at,"windows":windows});
            if let Some(unavailable) = snapshot.unavailable {
                value["unavailable"] = json!(unavailable);
            }
            if let Some(credits) = snapshot.reset_credits {
                value["resetCredits"] = json!(credits);
            }
            value
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Provider {
    instance_id: Nonempty,
    driver: Nonempty,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    display_name: Option<Nonempty>,
    enabled: bool,
    installed: bool,
    status: ProviderStatus,
    checked_at: Timestamp,
    #[serde(deserialize_with = "objects")]
    models: Vec<Model>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
enum ProviderStatus {
    Ready,
    Warning,
    Error,
    Disabled,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Model {
    slug: Nonempty,
    name: Nonempty,
    is_custom: bool,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    is_default: Option<bool>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    is_legacy: Option<bool>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    aliases: Option<Vec<Nonempty>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    sub_provider: Option<Nonempty>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageLimits {
    checked_at: Timestamp,
    #[serde(deserialize_with = "objects")]
    windows: Vec<Window>,
    #[serde(default, deserialize_with = "present_object")]
    unavailable: Option<Unavailable>,
    #[serde(default, deserialize_with = "present_object")]
    reset_credits: Option<ResetCredits>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Window {
    id: Nonempty,
    kind: WindowKind,
    label: Nonempty,
    used_percent: Percent,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    resets_at: Option<Timestamp>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    window_duration_mins: Option<Count>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
enum WindowKind {
    Session,
    Weekly,
    Monthly,
    Other,
}

#[derive(Deserialize, Serialize)]
struct Unavailable {
    reason: UnavailableReason,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
enum UnavailableReason {
    Unsupported,
    ProbeFailed,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResetCredits {
    available_count: Count,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    next_expires_at: Option<Timestamp>,
}

#[derive(Deserialize)]
struct SourcesPayload {
    #[serde(deserialize_with = "objects")]
    sources: Vec<UsageSource>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageSource {
    id: Nonempty,
    kind: Nonempty,
    label: Nonempty,
    checked_at: Timestamp,
    #[serde(default, deserialize_with = "present")]
    error: Option<Nonempty>,
    #[serde(deserialize_with = "objects")]
    accounts: Vec<SourceAccount>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceAccount {
    id: Nonempty,
    driver: Nonempty,
    #[serde(default, deserialize_with = "present")]
    plan: Option<Nonempty>,
    #[serde(deserialize_with = "object")]
    usage_limits: UsageLimits,
}

fn source_usage(sources: Vec<UsageSource>) -> Value {
    let reference =
        |value: &str| format!("{:x}", Sha256::digest(value.as_bytes()))[..16].to_owned();
    let items = sources.into_iter().map(|source| {
        let accounts = source.accounts.into_iter().map(|account| {
            let mut value = json!({
                "accountRef":format!("acct_{}", reference(&format!("{}\0{}", source.id.0, account.id.0))),
                "driver":account.driver,
                "usage":usage_value(Some(account.usage_limits))
            });
            if let Some(plan) = account.plan {
                value["plan"] = json!(sanitize_failure_text(&plan.0, 2000));
            }
            value
        }).collect::<Vec<_>>();
        let mut value = json!({
            "sourceRef":format!("src_{}", reference(&source.id.0)),
            "kind":source.kind,
            "label":sanitize_failure_text(&source.label.0, 2000),
            "checkedAt":source.checked_at,
            "accounts":accounts
        });
        if let Some(error) = source.error {
            value["error"] = json!(sanitize_failure_text(&error.0, 2000));
        }
        value
    }).collect::<Vec<_>>();
    json!({"status":"reported","items":items})
}

pub struct UsageLimitsRead {
    pub providers: Vec<ProviderSnapshot>,
    pub sources: Value,
    pub refresh: Value,
}

#[derive(Default)]
pub struct UsageLimitsCollector {
    providers: Option<Vec<ProviderSnapshot>>,
    sources: Option<Vec<UsageSource>>,
}

impl UsageLimitsCollector {
    pub fn collect(
        &mut self,
        event: RpcEvent,
    ) -> Result<Option<UsageLimitsRead>, Box<ClientError>> {
        let invalid = || {
            ClientError::boxed(
                ErrorKind::InvalidResponse,
                "T3 RPC subscribeServerConfig returned an invalid response.",
            )
        };
        let reason = match event {
            RpcEvent::Chunk(values) => {
                for value in values {
                    let event = value.as_object().ok_or_else(invalid)?;
                    if event.get("version") != Some(&json!(1)) {
                        return Err(invalid());
                    }
                    match event
                        .get("type")
                        .and_then(Value::as_str)
                        .ok_or_else(invalid)?
                    {
                        "snapshot" => {
                            let config = event.get("config").ok_or_else(invalid)?;
                            let parsed: ProvidersConfig =
                                serde_json::from_value(config.clone()).map_err(|_| invalid())?;
                            self.providers = Some(parsed.providers);
                        }
                        "providerStatuses" => {
                            let payload = event.get("payload").ok_or_else(invalid)?;
                            let parsed: ProvidersConfig =
                                serde_json::from_value(payload.clone()).map_err(|_| invalid())?;
                            if self.providers.is_some() {
                                self.providers = Some(parsed.providers);
                            }
                        }
                        "usageLimitSourcesUpdated" => {
                            let payload = event
                                .get("payload")
                                .and_then(Value::as_object)
                                .ok_or_else(invalid)?;
                            let parsed: SourcesPayload =
                                serde_json::from_value(Value::Object(payload.clone()))
                                    .map_err(|_| invalid())?;
                            if self.sources.is_none() {
                                self.sources = Some(parsed.sources);
                            }
                        }
                        _ => {}
                    }
                }
                if self.providers.is_none() || self.sources.is_none() {
                    return Ok(None);
                }
                None
            }
            RpcEvent::Success(_) => Some(StreamEnd::StreamEnded),
            RpcEvent::Interrupted(reason) => Some(reason),
        };
        let providers = self.providers.take().ok_or_else(|| {
            ClientError::boxed(
                ErrorKind::Rpc,
                format!(
                    "T3 RPC subscribeServerConfig {} without a provider snapshot.",
                    reason.map(StreamEnd::as_str).unwrap_or("ended")
                ),
            )
        })?;
        let sources = match self.sources.take() {
            Some(sources) => source_usage(sources),
            None => {
                json!({"status":"unavailable","reason":reason.expect("incomplete stream has a reason").as_str()})
            }
        };
        Ok(Some(UsageLimitsRead {
            providers,
            sources,
            refresh: json!({"status":"notRequested"}),
        }))
    }
}

fn object<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<T, D::Error> {
    let value = Map::<String, Value>::deserialize(d)?;
    serde_json::from_value(Value::Object(value)).map_err(D::Error::custom)
}

fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

fn present_object<'de, D: Deserializer<'de>, T: DeserializeOwned>(
    d: D,
) -> Result<Option<T>, D::Error> {
    let value = Map::<String, Value>::deserialize(d)?;
    serde_json::from_value(Value::Object(value))
        .map(Some)
        .map_err(D::Error::custom)
}

fn objects<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Vec<T>, D::Error> {
    let values = Vec::<Map<String, Value>>::deserialize(d)?;
    values
        .into_iter()
        .map(|value| serde_json::from_value(Value::Object(value)).map_err(D::Error::custom))
        .collect()
}

#[derive(Serialize)]
struct Nonempty(String);
impl<'de> Deserialize<'de> for Nonempty {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = String::deserialize(d)?;
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(D::Error::custom("expected nonempty string"));
        }
        Ok(Self(trimmed.to_owned()))
    }
}

#[derive(Serialize)]
struct Timestamp(String);
impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        static SHAPE: OnceLock<regex::Regex> = OnceLock::new();
        let shape = SHAPE.get_or_init(|| regex::Regex::new(
            r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T(?:[01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9](?:\.[0-9]+)?(?:Z|[+-](?:[01][0-9]|2[0-3]):[0-5][0-9])$"
        ).expect("timestamp pattern"));
        let value = String::deserialize(d)?;
        if !shape.is_match(&value) || chrono::DateTime::parse_from_rfc3339(&value).is_err() {
            return Err(D::Error::custom("expected ISO timestamp with offset"));
        }
        Ok(Self(value))
    }
}

#[derive(Serialize)]
struct Percent(f64);
impl<'de> Deserialize<'de> for Percent {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = f64::deserialize(d)?;
        if !value.is_finite() || !(0.0..=100.0).contains(&value) {
            return Err(D::Error::custom("expected percent from 0 to 100"));
        }
        Ok(Self(value))
    }
}

#[derive(Serialize)]
struct Count(u64);
impl<'de> Deserialize<'de> for Count {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = f64::deserialize(d)?;
        if !value.is_finite()
            || !(0.0..=9_007_199_254_740_991.0).contains(&value)
            || value.fract() != 0.0
        {
            return Err(D::Error::custom("expected nonnegative safe integer"));
        }
        Ok(Self(value as u64))
    }
}
