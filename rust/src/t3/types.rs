pub use crate::settings::RuntimeMode;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub(crate) fn nonempty<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let value = String::deserialize(d)?;
    if value.is_empty() {
        return Err(serde::de::Error::custom("expected nonempty string"));
    }
    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelection {
    #[serde(deserialize_with = "nonempty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InteractionMode {
    Default,
    Plan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnState {
    Running,
    Interrupted,
    Completed,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestTurn {
    #[serde(deserialize_with = "nonempty")]
    pub turn_id: String,
    pub state: TurnState,
    #[serde(deserialize_with = "nonempty")]
    pub requested_at: String,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub assistant_message_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub status: String,
    #[serde(default)]
    pub provider_name: Option<String>,
    #[serde(default)]
    pub provider_instance_id: Option<String>,
    #[serde(default)]
    pub active_turn_id: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_error_class: Option<String>,
    #[serde(default)]
    pub failure_code: Option<String>,
    #[serde(default)]
    pub failure_category: Option<String>,
    #[serde(default)]
    pub reset_at: Option<String>,
    #[serde(default)]
    pub retry_after: Option<Value>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadShell {
    #[serde(deserialize_with = "nonempty")]
    pub id: String,
    #[serde(deserialize_with = "nonempty")]
    pub project_id: String,
    #[serde(deserialize_with = "nonempty")]
    pub title: String,
    pub model_selection: ModelSelection,
    pub runtime_mode: RuntimeMode,
    #[serde(default)]
    pub interaction_mode: Option<InteractionMode>,
    #[serde(default)]
    pub latest_turn: Option<LatestTurn>,
    #[serde(default)]
    pub session: Option<Session>,
    #[serde(default)]
    pub evidence_order: Option<FailureEvidenceOrder>,
    #[serde(default)]
    pub archived_at: Option<String>,
    #[serde(default)]
    pub settled_override: Option<String>,
    #[serde(default)]
    pub settled_at: Option<String>,
    #[serde(default)]
    pub snoozed_until: Option<String>,
    #[serde(default)]
    pub snoozed_at: Option<String>,
    #[serde(default)]
    pub pinned_at: Option<String>,
    #[serde(default)]
    pub latest_user_message_at: Option<String>,
    #[serde(default)]
    pub has_actionable_proposed_plan: Option<bool>,
    #[serde(default)]
    pub background_liveness: Option<String>,
    #[serde(default)]
    pub has_pending_approvals: Option<bool>,
    #[serde(default)]
    pub has_pending_user_input: Option<bool>,
    #[serde(default)]
    pub lineage: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    #[serde(deserialize_with = "nonempty")]
    pub id: String,
    pub role: MessageRole,
    pub text: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    #[serde(flatten)]
    pub shell: ThreadShell,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub activities: Vec<Value>,
    #[serde(default)]
    pub checkpoints: Vec<Value>,
    #[serde(default)]
    pub proposed_plans: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_failures: Option<Vec<TurnFailure>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_recoveries: Option<Vec<TurnRecovery>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    #[serde(deserialize_with = "nonempty")]
    pub id: String,
    #[serde(deserialize_with = "nonempty")]
    pub title: String,
    #[serde(deserialize_with = "nonempty")]
    pub workspace_root: String,
    #[serde(default)]
    pub default_model_selection: Option<ModelSelection>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellSnapshot {
    pub snapshot_sequence: u64,
    pub projects: Vec<Project>,
    pub threads: Vec<ThreadShell>,
    #[serde(deserialize_with = "nonempty")]
    pub updated_at: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSnapshot {
    pub snapshot_sequence: u64,
    pub thread: Thread,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    #[serde(deserialize_with = "nonempty")]
    pub environment_id: String,
    pub label: String,
    #[serde(deserialize_with = "nonempty")]
    pub server_version: String,
    #[serde(default)]
    pub orchestration_protocol_version: Option<std::num::NonZeroU32>,
    #[serde(default)]
    pub capabilities: Map<String, Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthSession {
    pub authenticated: bool,
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    #[serde(default)]
    pub session_method: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DispatchResult {
    pub sequence: u64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderFailure {
    pub class: String,
    pub message: String,
    pub code: Option<String>,
    #[serde(default)]
    pub retryable: Option<bool>,
    #[serde(default)]
    pub reset_at: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRetry {
    pub attempt: std::num::NonZeroU64,
    pub max_attempts: Option<std::num::NonZeroU64>,
    pub retry_delay_ms: Option<u64>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceScope {
    Full,
    Shell,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureEvidenceOrder {
    pub protocol_version: u8,
    pub scope: EvidenceScope,
    pub snapshot_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_identity: Option<RunIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<ItemOrder>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunIdentity {
    pub provider: Option<String>,
    pub model: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemOrder {
    pub updated_at: String,
    pub ordinal: u64,
    pub id: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnFailure {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<FailureEvidenceOrder>,
    pub turn_id: String,
    pub provider: String,
    pub model_selection: ModelSelection,
    pub failure: Option<ProviderFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<ProviderRetry>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnRecovery {
    pub turn_id: String,
    pub state: TurnState,
    pub order: FailureEvidenceOrder,
}
