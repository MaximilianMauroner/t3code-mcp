use crate::{
    config::GatewayConfig,
    contract::{tool_names, tool_schema_fingerprint},
    gateway::T3Gateway,
    mcp::{IncomingMessage, McpServer, ToolHandler, ToolsListPage},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

#[derive(Serialize)]
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn check(checks: &mut Vec<Check>, name: &'static str, ok: bool, detail: String) {
    checks.push(Check { name, ok, detail });
}

pub async fn run_doctor(
    gateway: &T3Gateway,
    config: &GatewayConfig,
    host_names: Option<&[String]>,
    tunnel_url: Option<&str>,
) -> Value {
    let mut checks = Vec::new();
    check(
        &mut checks,
        "config",
        true,
        format!(
            "readOnly={} host={} port={} dataDir={} staleAfterMs={}",
            config.read_only,
            config.host,
            config.port,
            config.endpoint.data_dir.display(),
            config.stale_after_ms,
        ),
    );
    let writable = can_write(&config.endpoint.data_dir);
    let mode = file_mode(&config.endpoint.data_dir);
    check(
        &mut checks,
        "journal",
        writable,
        if writable {
            format!(
                "dataDir {} writable{}",
                config.endpoint.data_dir.display(),
                mode.map(|mode| format!(" mode={mode:o}"))
                    .unwrap_or_default()
            )
        } else {
            format!(
                "dataDir {} not writable; preserve T3_MCP_DATA_DIR across restarts and run one gateway per directory",
                config.endpoint.data_dir.display()
            )
        },
    );
    let status = gateway.connection_status().await;
    let native = &status["native"];
    check(
        &mut checks,
        "t3_identity",
        status["connectionStatus"] == "connected",
        format!(
            "env={} version={} scopes=[{}]{}{}",
            text(&status["environment"], "environmentId", "unknown"),
            text(&status["environment"], "serverVersion", "unknown"),
            strings(&status["upstreamScopes"]).join(","),
            expiry_note(&status["sessionExpiresAt"]),
            error_note(&status),
        ),
    );
    let missing = strings(&native["missingTools"]);
    check(
        &mut checks,
        "native_mcp",
        native["credential"] == "valid" && native["error"].is_null() && missing.is_empty(),
        format!(
            "url={} credential={} access={} tools={}{}{}{}{}",
            text(native, "url", "none"),
            text(native, "credential", "missing"),
            text(native, "access", "none"),
            native["availableTools"],
            if missing.is_empty() {
                String::new()
            } else {
                format!(" missing=[{}]", missing.join(","))
            },
            expiry_note(&native["expiresAt"]),
            error_note(native),
            if native["credential"] == "valid" {
                ""
            } else {
                " Run native-login with a T3 pairing code."
            },
        ),
    );
    let disabled = status["disabledOperations"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|operation| {
            format!(
                "{}:{}",
                text(operation, "operation", ""),
                text(operation, "reasonCode", "")
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    check(
        &mut checks,
        "effective_access",
        true,
        format!(
            "mode={} callable={} disabled={}",
            text(&status, "effectiveAccessMode", "read-only"),
            status["callableOperations"].as_array().map_or(0, Vec::len),
            if disabled.is_empty() {
                "none"
            } else {
                &disabled
            },
        ),
    );
    check(
        &mut checks,
        "freshness",
        status["stateFreshness"] != "unknown",
        format!(
            "freshness={} lastObservedAt={} observedAt={}",
            text(&status, "stateFreshness", "unknown"),
            text(&status, "lastObservedAt", "never"),
            text(&status, "observedAt", ""),
        ),
    );
    check(
        &mut checks,
        "build",
        status["toolSchemaFingerprint"] == tool_schema_fingerprint(),
        format!(
            "version={} commit={} fingerprint={}",
            text(&status, "gatewayVersion", ""),
            text(&status, "gatewayCommit", "unknown"),
            text(&status, "toolSchemaFingerprint", ""),
        ),
    );
    let mut host = host_names.map(<[String]>::to_vec);
    if let Some(host) = &mut host {
        host.sort();
    }
    let mut manifest = json!({
        "gatewayVersion":status["gatewayVersion"], "gatewayCommit":status["gatewayCommit"],
        "toolSchemaFingerprint":status["toolSchemaFingerprint"], "declaredOperations":tool_names().collect::<Vec<_>>(),
        "locallyDiscoveredOperations":[], "callableOperations":status["callableOperations"],
        "disabledOperations":status["disabledOperations"].as_array().into_iter().flatten().map(|operation|json!({
            "operation":operation["operation"],"reasonCode":operation["reasonCode"],
        })).collect::<Vec<_>>(), "effectiveAccessMode":status["effectiveAccessMode"],
        "upstreamScopes":status["upstreamScopes"], "hostDiscoveredOperations":host,
    });
    let catalog = gateway
        .tool_catalog()
        .await
        .unwrap_or_else(ToolsListPage::gateway);
    let server = McpServer::new(catalog);
    let response = server
        .handle_message(
            IncomingMessage::parse(json!({
                "jsonrpc":"2.0","id":1,"method":"tools/list","params":{},
            }))
            .expect("static discovery request"),
            gateway,
        )
        .await
        .expect("request response");
    let mut discovered = response["result"]["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    discovered.sort();
    let mut expected = tool_names().map(str::to_owned).collect::<Vec<_>>();
    expected.sort();
    check(
        &mut checks,
        "mcp_discovery",
        discovered == expected,
        format!(
            "declared={} discovered={} fingerprint={}",
            expected.len(),
            discovered.len(),
            tool_schema_fingerprint(),
        ),
    );
    manifest["locallyDiscoveredOperations"] = json!(discovered);
    if let Some(host_names) = host_names {
        let mut observed = host_names.to_vec();
        observed.sort();
        observed.dedup();
        let missing = expected
            .iter()
            .filter(|name| !observed.contains(name))
            .cloned()
            .collect::<Vec<_>>();
        let unexpected = observed
            .iter()
            .filter(|name| !expected.contains(name))
            .cloned()
            .collect::<Vec<_>>();
        check(
            &mut checks,
            "host_discovery",
            missing.is_empty() && unexpected.is_empty(),
            format!(
                "observed={} missing=[{}] unexpected=[{}]",
                observed.len(),
                missing.join(","),
                unexpected.join(","),
            ),
        );
    }
    match gateway
        .threads_overview(
            json!({"includeArchived":false,"runningLimit":5})
                .as_object()
                .unwrap(),
        )
        .await
    {
        Ok(overview) => check(
            &mut checks,
            "overview_read",
            overview_bounded(&overview),
            format!(
                "total={} needsAttention={} highlights={} observedAt={}",
                overview["total"],
                overview["needsAttentionCount"],
                overview["highlights"].as_array().map_or(0, Vec::len),
                text(&overview, "observedAt", ""),
            ),
        ),
        Err(error) => check(&mut checks, "overview_read", false, error.message),
    }
    let tunnel_url = tunnel_url.unwrap_or("http://127.0.0.1:8080/readyz");
    let ready = match reqwest::Client::new()
        .get(tunnel_url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
    {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
    };
    check(
        &mut checks,
        "tunnel",
        ready,
        if ready {
            format!("tunnel ready at {tunnel_url}")
        } else {
            format!("tunnel not ready at {tunnel_url}; start t3-code-mcp-tunnel.service")
        },
    );
    let ok = checks
        .iter()
        .filter(|check| check.name != "tunnel")
        .all(|check| check.ok);
    json!({"ok":ok,"checks":checks,"manifest":manifest})
}

pub fn overview_bounded(overview: &Value) -> bool {
    overview["highlights"].as_array().is_some_and(|highlights| {
        highlights.len() <= 5
            && highlights.iter().all(|highlight| {
                highlight["latestResponseExcerpt"]
                    .as_str()
                    .unwrap_or("")
                    .encode_utf16()
                    .count()
                    <= 201
            })
    })
}

fn text<'a>(value: &'a Value, field: &str, fallback: &'a str) -> &'a str {
    value[field].as_str().unwrap_or(fallback)
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect()
}

fn error_note(value: &Value) -> String {
    value["error"]
        .as_str()
        .filter(|error| !error.is_empty())
        .map(|error| format!(" error={error}"))
        .unwrap_or_default()
}

fn expiry_note(value: &Value) -> String {
    let Some(expires_at) = value.as_str() else {
        return String::new();
    };
    let Some(expires) = crate::operations::parse_timestamp(expires_at) else {
        return String::new();
    };
    let remaining = expires.saturating_sub(chrono::Utc::now().timestamp_millis());
    if remaining < 0 {
        format!(" credential EXPIRED at {expires_at}")
    } else if remaining < 7 * 24 * 3_600_000 {
        format!(" credential expires soon at {expires_at}")
    } else {
        format!(" credential expires at {expires_at}")
    }
}

#[cfg(unix)]
fn can_write(path: &Path) -> bool {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    CString::new(path.as_os_str().as_bytes())
        .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 })
}

#[cfg(not(unix))]
fn can_write(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| !metadata.permissions().readonly())
}

#[cfg(unix)]
fn file_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn file_mode(path: &Path) -> Option<u32> {
    std::fs::metadata(path).ok().map(|metadata| {
        if metadata.permissions().readonly() {
            0o444
        } else {
            0o666
        }
    })
}
