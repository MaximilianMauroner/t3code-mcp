use serde::{
    Deserialize, Deserializer, Serialize,
    de::{DeserializeOwned, Error},
};
use serde_json::{Map, Value, json};
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
}

impl ProviderSnapshot {
    pub fn instance_id(&self) -> &str {
        &self.provider.instance_id.0
    }

    pub fn into_usage(self) -> Value {
        let usage = match self.usage_limits {
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
                        let mut value =
                            serde_json::to_value(window).expect("validated usage window");
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
        };
        let mut value = serde_json::to_value(self.provider).expect("validated provider catalog");
        value["usage"] = usage;
        value
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
