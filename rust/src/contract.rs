use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub const GATEWAY_VERSION: &str = "0.1.0";
pub const TOOL_SCHEMA_VERSION: u32 = 12;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FingerprintInput<'a> {
    version: &'a str,
    schema_version: u32,
    tools: Vec<&'a str>,
}

pub fn tool_schema_fingerprint() -> String {
    let mut tools: Vec<_> = tool_names().collect();
    tools.sort_unstable();
    let canonical = serde_json::to_vec(&FingerprintInput {
        version: GATEWAY_VERSION,
        schema_version: TOOL_SCHEMA_VERSION,
        tools,
    })
    .expect("tool fingerprint input is serializable");
    let digest = Sha256::digest(canonical);
    let hex = format!("{digest:x}");
    hex[..16].to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolDefinition {
    pub name: String,
    pub title: String,
    pub description: String,
    pub input_schema: Map<String, Value>,
    pub output_schema: Map<String, Value>,
    pub annotations: Map<String, Value>,
    pub execution: Map<String, Value>,
}

pub fn gateway_tools() -> &'static [ToolDefinition] {
    static TOOLS: OnceLock<Vec<ToolDefinition>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        let tools: Vec<ToolDefinition> = serde_json::from_str(include_str!("contract-tools.json"))
            .expect("embedded gateway tool contract must be valid");
        let names = tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert!(
            names.len() == tools.len()
                && tools.len() == GATEWAY_TOOL_NAMES.len()
                && GATEWAY_TOOL_NAMES.iter().all(|name| names.contains(name)),
            "embedded gateway tools must map one-to-one to the executable routes"
        );
        tools
    })
}

macro_rules! gateway_routes {
    ($($variant:ident => $name:literal),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum GatewayTool { $($variant),+ }

        impl GatewayTool {
            pub fn parse(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        pub const GATEWAY_TOOL_NAMES: &[&str] = &[$($name),+];
    };
}

gateway_routes! {
    ConnectionStatus => "t3_connection_status",
    ModelsUsage => "t3_models_usage",
    AuditLog => "t3_audit_log",
    GitStatus => "t3_git_status",
    GitDiff => "t3_git_diff",
    GitCompare => "t3_git_compare",
    ThreadsList => "t3_threads_list",
    ThreadsOverview => "t3_threads_overview",
    PendingActionsList => "t3_pending_actions_list",
    PendingActionRespond => "t3_pending_action_respond",
    AttachmentUpload => "t3_attachment_upload",
    TaskStart => "t3_task_start",
    TaskGet => "t3_task_get",
    TasksList => "t3_tasks_list",
    TaskCancel => "t3_task_cancel",
    ResultGet => "t3_result_get",
}

pub const NATIVE_READ_TOOL_NAMES: &[&str] = &[
    "t3_project_list",
    "t3_project_read",
    "t3_thread_list",
    "t3_thread_search",
    "t3_thread_read",
    "t3_thread_wait",
    "t3_thread_configuration",
    "t3_thread_transfers",
    "t3_queue_list",
    "t3_queue_read",
    "list_scheduled_tasks",
    "list_thread_pull_requests",
    "read_pull_request_guide",
    "t3_worktree_list",
    "orchestrator_capabilities",
];

pub const NATIVE_MUTATING_TOOL_NAMES: &[&str] = &[
    "t3_project_create",
    "t3_project_update",
    "t3_project_clone",
    "t3_project_delete",
    "t3_thread_launch",
    "t3_thread_send",
    "t3_thread_interrupt",
    "t3_thread_update",
    "t3_thread_organize",
    "t3_thread_configure",
    "t3_thread_fork",
    "t3_thread_merge_back",
    "t3_queue_edit",
    "t3_queue_cancel",
    "t3_queue_reorder",
    "t3_queue_promote_to_steer",
    "t3_attachment_discard",
    "t3_thread_send_attachments",
    "schedule_task",
    "update_scheduled_task",
    "delete_scheduled_task",
    "run_scheduled_task_now",
    "link_pull_request",
    "unlink_pull_request",
    "watch_pull_request",
    "unwatch_pull_request",
    "write_pull_request_guides",
];

pub const NATIVE_INTERNAL_TOOL_NAMES: &[&str] = &["t3_attachment_prepare_upload"];

pub fn native_tool_names() -> impl Iterator<Item = &'static str> + Clone {
    NATIVE_READ_TOOL_NAMES
        .iter()
        .chain(NATIVE_MUTATING_TOOL_NAMES)
        .copied()
}

pub fn native_called_tool_names() -> impl Iterator<Item = &'static str> + Clone {
    native_tool_names().chain(NATIVE_INTERNAL_TOOL_NAMES.iter().copied())
}

pub fn tool_names() -> impl Iterator<Item = &'static str> + Clone {
    GATEWAY_TOOL_NAMES
        .iter()
        .copied()
        .chain(native_tool_names())
}

pub fn is_native_tool_name(name: &str) -> bool {
    native_tool_names().any(|native| native == name)
}

pub fn is_native_mutating_tool(name: &str) -> bool {
    NATIVE_MUTATING_TOOL_NAMES.contains(&name)
}

pub fn is_mutating_tool(name: &str) -> bool {
    gateway_tools()
        .iter()
        .find(|tool| tool.name == name)
        .map(|tool| tool.annotations.get("readOnlyHint") == Some(&Value::Bool(false)))
        .unwrap_or_else(|| is_native_mutating_tool(name))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn node_contract() -> Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/node-compat-contract.json"
        ))
        .expect("captured Node contract")
    }

    fn node_names<'a>(contract: &'a Value, field: &str) -> Vec<&'a str> {
        contract[field]
            .as_array()
            .expect("Node contract contains a name list")
            .iter()
            .map(|name| name.as_str().expect("tool names are strings"))
            .collect()
    }

    #[test]
    fn gateway_contract_matches_node_capture_in_order() {
        let captured: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/node-compat-tools.json"))
                .expect("captured Node tool contract");
        let embedded: Value = serde_json::from_str(include_str!("contract-tools.json"))
            .expect("embedded gateway contract");
        assert_eq!(embedded, captured);
        assert_eq!(serde_json::to_value(gateway_tools()).unwrap(), captured);

        let declared: HashSet<_> = GATEWAY_TOOL_NAMES.iter().copied().collect();
        let registered: HashSet<_> = gateway_tools()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(registered, declared);
        assert_eq!(registered.len(), gateway_tools().len());
    }

    #[test]
    fn declared_names_and_native_classifications_match_node_capture() {
        let contract = node_contract();
        assert_eq!(
            GATEWAY_TOOL_NAMES,
            node_names(&contract, "gatewayToolNames")
        );
        assert_eq!(
            NATIVE_READ_TOOL_NAMES,
            node_names(&contract, "nativeReadToolNames")
        );
        assert_eq!(
            NATIVE_MUTATING_TOOL_NAMES,
            node_names(&contract, "nativeMutatingToolNames")
        );
        assert_eq!(
            NATIVE_INTERNAL_TOOL_NAMES,
            node_names(&contract, "nativeInternalToolNames")
        );

        let native = [
            node_names(&contract, "nativeReadToolNames"),
            node_names(&contract, "nativeMutatingToolNames"),
        ]
        .concat();
        assert_eq!(native_tool_names().collect::<Vec<_>>(), native);
        assert_eq!(
            native_called_tool_names().collect::<Vec<_>>(),
            [
                native.clone(),
                node_names(&contract, "nativeInternalToolNames")
            ]
            .concat()
        );
        let declared = [node_names(&contract, "gatewayToolNames"), native].concat();
        assert_eq!(tool_names().collect::<Vec<_>>(), declared);
        assert_eq!(tool_names().collect::<HashSet<_>>().len(), declared.len());
    }

    #[test]
    fn gateway_mutation_policy_matches_node_annotations() {
        let captured: Vec<Value> =
            serde_json::from_str(include_str!("../../tests/fixtures/node-compat-tools.json"))
                .expect("captured Node tool contract");
        let mut mutating = Vec::new();
        for tool in captured {
            let name = tool["name"].as_str().unwrap();
            let expected = tool["annotations"]["readOnlyHint"] == false;
            assert_eq!(
                is_mutating_tool(name),
                expected,
                "gateway mutation policy for {name}"
            );
            if expected {
                mutating.push(name.to_owned());
            }
        }
        mutating.sort();
        assert_eq!(
            mutating,
            [
                "t3_attachment_upload",
                "t3_pending_action_respond",
                "t3_task_cancel",
                "t3_task_start"
            ]
        );
        for name in NATIVE_READ_TOOL_NAMES {
            assert!(!is_mutating_tool(name), "native read tool {name}");
        }
        for name in NATIVE_MUTATING_TOOL_NAMES {
            assert!(is_mutating_tool(name), "native mutating tool {name}");
        }
        for name in ["unknown", "", "t3_attachment_prepare_upload"] {
            assert!(!is_native_tool_name(name));
            assert!(!is_mutating_tool(name));
        }
    }

    #[test]
    fn static_identity_matches_captured_node_build() {
        let contract = node_contract();
        let baseline: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/node-compat-baseline.json"
        ))
        .expect("Node compatibility baseline");
        let status = &baseline["mcp"]["http"]["status"]["body"]["result"]["structuredContent"];
        assert_eq!(GATEWAY_VERSION, status["gatewayVersion"]);
        assert_eq!(tool_schema_fingerprint(), status["toolSchemaFingerprint"]);
        assert_eq!(GATEWAY_VERSION, contract["gatewayVersion"]);
        assert_eq!(
            Some(TOOL_SCHEMA_VERSION as u64),
            contract["toolSchemaVersion"].as_u64()
        );
        assert_eq!(tool_schema_fingerprint(), contract["toolSchemaFingerprint"]);
    }
}
