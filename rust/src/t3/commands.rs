use super::types::{InteractionMode, ModelSelection, RuntimeMode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum T3Command {
    #[serde(rename = "thread.create", rename_all = "camelCase")]
    Create {
        command_id: String,
        thread_id: String,
        project_id: String,
        title: String,
        model_selection: ModelSelection,
        runtime_mode: RuntimeMode,
        interaction_mode: InteractionMode,
        branch: Option<String>,
        worktree_path: Option<String>,
        created_at: String,
    },
    #[serde(rename = "thread.turn.start", rename_all = "camelCase")]
    TurnStart {
        command_id: String,
        thread_id: String,
        message: UserMessage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_selection: Option<ModelSelection>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title_seed: Option<String>,
        runtime_mode: RuntimeMode,
        interaction_mode: InteractionMode,
        created_at: String,
    },
    #[serde(rename = "thread.approval.respond", rename_all = "camelCase")]
    ApprovalResponse {
        command_id: String,
        thread_id: String,
        request_id: String,
        decision: ApprovalDecision,
        created_at: String,
    },
    #[serde(rename = "thread.user-input.respond", rename_all = "camelCase")]
    UserInputResponse {
        command_id: String,
        thread_id: String,
        request_id: String,
        answers: Map<String, Value>,
        created_at: String,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub message_id: String,
    pub role: UserRole,
    pub text: String,
    pub attachments: Vec<Value>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserRole {
    #[serde(rename = "user")]
    User,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalDecision {
    Accept,
    AcceptForSession,
    Decline,
    Cancel,
}

impl T3Command {
    pub fn v2_commands(&self) -> Vec<Value> {
        match self {
            Self::Create { .. } => {
                let mut value = serde_json::to_value(self).expect("command serializes");
                value["createdBy"] = "user".into();
                value["creationSource"] = "mcp".into();
                vec![value]
            }
            Self::TurnStart {
                command_id,
                thread_id,
                message,
                model_selection,
                title_seed,
                runtime_mode,
                interaction_mode,
                ..
            } => {
                let mut dispatch = json!({"type":"message.dispatch", "commandId":command_id, "threadId":thread_id,
                    "messageId":message.message_id, "text":message.text, "attachments":message.attachments,
                    "dispatchMode":{"type":"start_immediately"}, "createdBy":"user", "creationSource":"mcp"});
                if let Some(model) = model_selection {
                    dispatch["modelSelection"] =
                        serde_json::to_value(model).expect("model serializes");
                }
                if let Some(title) = title_seed.as_ref().filter(|t| !t.is_empty()) {
                    dispatch["titleSeed"] = title.clone().into();
                }
                vec![
                    json!({"type":"thread.runtime-mode.set", "commandId":format!("{command_id}:runtime"), "threadId":thread_id, "runtimeMode":runtime_mode}),
                    json!({"type":"thread.interaction-mode.set", "commandId":format!("{command_id}:interaction"), "threadId":thread_id,"interactionMode":interaction_mode}),
                    dispatch,
                ]
            }
            Self::ApprovalResponse {
                command_id,
                thread_id,
                request_id,
                decision,
                ..
            } => vec![
                json!({"type":"runtime-request.respond","commandId":command_id,"threadId":thread_id,"requestId":request_id,"decision":decision}),
            ],
            Self::UserInputResponse {
                command_id,
                thread_id,
                request_id,
                answers,
                ..
            } => vec![
                json!({"type":"runtime-request.respond","commandId":command_id,"threadId":thread_id,"requestId":request_id,"answers":answers}),
            ],
        }
    }
}
