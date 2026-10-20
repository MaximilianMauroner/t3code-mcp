use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeMode {
    ApprovalRequired,
    AutoAcceptEdits,
    Auto,
    FullAccess,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SafeModelSelection {
    pub model: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_string"
    )]
    pub instance_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_string"
    )]
    pub provider: Option<String>,
}

fn present_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelection {
    pub model: String,
    #[serde(default, deserialize_with = "present_string")]
    pub instance_id: Option<String>,
    #[serde(default, deserialize_with = "present_string")]
    pub provider: Option<String>,
}

pub fn safe_model_selection(selection: &ModelSelection) -> SafeModelSelection {
    SafeModelSelection {
        model: selection.model.clone(),
        instance_id: selection.instance_id.clone(),
        provider: selection.provider.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestedSettings {
    #[serde(deserialize_with = "required_nullable")]
    pub model_selection: Option<SafeModelSelection>,
    #[serde(deserialize_with = "required_nullable")]
    pub runtime_mode: Option<RuntimeMode>,
}

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResolvedSettings {
    pub model_selection: SafeModelSelection,
    pub runtime_mode: RuntimeMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSource {
    Explicit,
    ProjectDefault,
    ThreadInherited,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSource {
    Explicit,
    ThreadInherited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettingsReceipt {
    pub requested: RequestedSettings,
    pub resolved: ResolvedSettings,
    pub model_source: ModelSource,
    pub runtime_source: RuntimeSource,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSettings {
    pub model_selection: ModelSelection,
    pub runtime_mode: RuntimeMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveSettings {
    #[serde(flatten)]
    pub settings: ResolvedSettings,
    pub observed_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationState {
    Observed,
    Unresolved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsObservation {
    #[serde(flatten)]
    pub receipt: SettingsReceipt,
    pub effective: Option<EffectiveSettings>,
    pub state: ObservationState,
    pub matches_resolved: Option<bool>,
}

pub fn observe_settings(
    receipt: &SettingsReceipt,
    thread: Option<&ThreadSettings>,
) -> SettingsObservation {
    observe_settings_at(receipt, thread, SystemTime::now())
}

pub fn observe_settings_at(
    receipt: &SettingsReceipt,
    thread: Option<&ThreadSettings>,
    now: SystemTime,
) -> SettingsObservation {
    let effective = thread.map(|thread| EffectiveSettings {
        settings: ResolvedSettings {
            model_selection: safe_model_selection(&thread.model_selection),
            runtime_mode: thread.runtime_mode,
        },
        observed_at: iso_timestamp(now),
    });
    let matches_resolved = effective
        .as_ref()
        .map(|effective| effective.settings == receipt.resolved);
    SettingsObservation {
        receipt: receipt.clone(),
        state: if effective.is_some() {
            ObservationState::Observed
        } else {
            ObservationState::Unresolved
        },
        effective,
        matches_resolved,
    }
}

fn iso_timestamp(now: SystemTime) -> String {
    let millis = match now.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i128,
        Err(error) => -(error.duration().as_millis() as i128),
    };
    let days = millis.div_euclid(86_400_000) + 719468;
    let era = days.div_euclid(146097);
    let day_in_era = days - era * 146097;
    let year_in_era =
        (day_in_era - day_in_era / 1460 + day_in_era / 36524 - day_in_era / 146096) / 365;
    let mut year = year_in_era + era * 400;
    let day_in_year = day_in_era - (365 * year_in_era + year_in_era / 4 - year_in_era / 100);
    let shifted_month = (5 * day_in_year + 2) / 153;
    let day = day_in_year - (153 * shifted_month + 2) / 5 + 1;
    let month = shifted_month + if shifted_month < 10 { 3 } else { -9 };
    year += i128::from(month <= 2);
    let time = millis.rem_euclid(86_400_000);
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        format!("{year:+07}")
    };
    format!(
        "{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        time / 3_600_000,
        time / 60_000 % 60,
        time / 1000 % 60,
        time % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    fn receipt() -> SettingsReceipt {
        serde_json::from_value(json!({"requested":{"modelSelection":null,"runtimeMode":null},"resolved":{"modelSelection":{"model":"gpt","instanceId":"codex"},"runtimeMode":"auto"},"modelSource":"project_default","runtimeSource":"thread_inherited"})).unwrap()
    }
    #[test]
    fn strips_secrets_and_compares_observed_settings() {
        let thread: ThreadSettings = serde_json::from_value(json!({"modelSelection":{"model":"gpt","instanceId":"codex","options":{"apiKey":"secret"},"other":"secret"},"runtimeMode":"auto"})).unwrap();
        let observation = observe_settings_at(&receipt(), Some(&thread), UNIX_EPOCH);
        assert_eq!(observation.matches_resolved, Some(true));
        let value = serde_json::to_value(observation).unwrap();
        assert_eq!(value["effective"]["observedAt"], "1970-01-01T00:00:00.000Z");
        assert_eq!(
            value["effective"]["modelSelection"],
            json!({"model":"gpt","instanceId":"codex"})
        );
        assert!(!value.to_string().contains("secret"));
        let changed = ThreadSettings {
            runtime_mode: RuntimeMode::FullAccess,
            ..thread.clone()
        };
        assert_eq!(
            observe_settings(&receipt(), Some(&changed)).matches_resolved,
            Some(false)
        );
        let changed = ThreadSettings {
            model_selection: ModelSelection {
                provider: Some("other".into()),
                ..thread.model_selection
            },
            ..thread
        };
        assert_eq!(
            observe_settings(&receipt(), Some(&changed)).matches_resolved,
            Some(false)
        );
    }
    #[test]
    fn unresolved_receipt_preserves_null_fields() {
        let observation = serde_json::to_value(observe_settings(&receipt(), None)).unwrap();
        assert_eq!(observation["state"], "unresolved");
        assert_eq!(observation["effective"], Value::Null);
        assert_eq!(observation["matchesResolved"], Value::Null);
        assert_eq!(observation["requested"]["runtimeMode"], Value::Null);
    }
    #[test]
    fn strict_receipt_rejects_unsafe_fields_missing_fields_and_null_identity() {
        for pointer in [
            "/extra",
            "/requested/extra",
            "/resolved/extra",
            "/resolved/modelSelection/options",
            "/resolved/modelSelection/provider",
        ] {
            let mut value = serde_json::to_value(receipt()).unwrap();
            let (parent, field) = pointer.rsplit_once('/').unwrap();
            value
                .pointer_mut(parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert(field.into(), Value::Null);
            assert!(
                serde_json::from_value::<SettingsReceipt>(value).is_err(),
                "{pointer}"
            );
        }
        let mut value = serde_json::to_value(receipt()).unwrap();
        value["requested"]
            .as_object_mut()
            .unwrap()
            .remove("runtimeMode");
        assert!(serde_json::from_value::<SettingsReceipt>(value).is_err());
    }
    #[test]
    fn timestamp_calendar_boundaries() {
        use std::time::Duration;
        assert_eq!(
            iso_timestamp(UNIX_EPOCH + Duration::from_millis(951782400123)),
            "2000-02-29T00:00:00.123Z"
        );
        assert_eq!(
            iso_timestamp(UNIX_EPOCH - Duration::from_millis(1)),
            "1969-12-31T23:59:59.999Z"
        );
    }
}
