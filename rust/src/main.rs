use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use serde_json::{Value, json};
use t3_code_mcp::operations::usage_summary::{UsageWindow, summarize_usage};
use t3_code_mcp::{config::load_config, doctor, gateway::T3Gateway, mcp::HttpOptions, runtime};

#[path = "setup.rs"]
mod setup;

fn main() -> ExitCode {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("CLI runtime");
    let result = runtime.block_on(run());
    // Tokio's blocking stdin read can outlive a stopped stdio session.
    runtime.shutdown_background();
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<ExitCode, String> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "serve".into());
    match command.as_str() {
        "setup" => return setup(args.collect()).map(|()| ExitCode::SUCCESS),
        "usage-summary" => return usage_summary(args.collect()).map(|()| ExitCode::SUCCESS),
        "native-login" => {
            return native_login(args.collect())
                .await
                .map(|()| ExitCode::SUCCESS);
        }
        _ => {}
    }
    let config = load_config().map_err(|error| error.to_string())?;
    let gateway = Arc::new(T3Gateway::new(config.clone()));
    let result = match command.as_str() {
        "serve" => {
            let transport = env_trimmed("MCP_TRANSPORT").unwrap_or_else(|| "stdio".into());
            match transport.as_str() {
                "stdio" => runtime::run_stdio(gateway)
                    .await
                    .map_err(|error| error.to_string()),
                "http" => runtime::run_http(
                    gateway,
                    HttpOptions {
                        bearer_token: config.mcp_bearer_token,
                        max_body_bytes: usize::try_from(config.max_body_bytes)
                            .map_err(|error| error.to_string())?,
                    },
                    &config.host,
                    config.port,
                    runtime::shutdown_signal(),
                )
                .await
                .map_err(|error| error.to_string()),
                _ => Err(format!(
                    "MCP_TRANSPORT must be stdio or http, received {transport}."
                )),
            }
        }
        "status" => print_json(&gateway.connection_status().await),
        "doctor" => {
            let host =
                parse_host_tool_names(std::env::var("MCP_HOST_TOOL_NAMES_JSON").ok().as_deref())?;
            let result = doctor::run_doctor(&gateway, &config, host.as_deref(), None).await;
            print_json(&result)?;
            return Ok(if result["ok"] == true {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            });
        }
        "smoke" | "spike" => {
            let status = gateway.connection_status().await;
            let overview = gateway
                .threads_overview(
                    json!({"includeArchived":false,"runningLimit":5})
                        .as_object()
                        .unwrap(),
                )
                .await
                .map_err(|error| error.message)?;
            print_json(&json!({"status":status,"overview":overview}))?;
            if command == "smoke" {
                if !doctor::overview_bounded(&overview) {
                    return Err("Smoke failed: overview highlights exceed bounds.".into());
                }
                eprintln!(
                    "Smoke ok: fingerprint={} total={} needsAttention={}. Next: force fresh client discovery and ask \"What's running and does anything need me?\" by voice.",
                    status["toolSchemaFingerprint"].as_str().unwrap_or_default(),
                    overview["total"],
                    overview["needsAttentionCount"]
                );
                return Ok(ExitCode::SUCCESS);
            }
            if std::env::var("T3_SPIKE_ENABLE_MUTATIONS").as_deref() != Ok("true") {
                eprintln!(
                    "Read-only connection spike complete. Set T3_SPIKE_ENABLE_MUTATIONS=true and T3_SPIKE_CONFIRM=I_UNDERSTAND to create a thread and submit a prompt."
                );
                return Ok(ExitCode::SUCCESS);
            }
            if std::env::var("T3_SPIKE_CONFIRM").as_deref() != Ok("I_UNDERSTAND") {
                return Err("Mutation spike requires T3_SPIKE_CONFIRM=I_UNDERSTAND.".into());
            }
            let (Some(project), Some(prompt)) = (
                env_trimmed("T3_SPIKE_PROJECT_ID"),
                env_trimmed("T3_SPIKE_PROMPT"),
            ) else {
                return Err(
                    "Mutation spike requires T3_SPIKE_PROJECT_ID and T3_SPIKE_PROMPT.".into(),
                );
            };
            let title = env_trimmed("T3_SPIKE_THREAD_TITLE")
                .unwrap_or_else(|| "t3-code-mcp connection spike".into());
            let prefix = env_trimmed("T3_SPIKE_IDEMPOTENCY_KEY")
                .unwrap_or_else(|| format!("spike-{}", chrono::Utc::now().timestamp_millis()));
            let task = gateway.task_start(json!({
                "projectId":project,"title":title,"instruction":prompt,"runtimeMode":"full-access","idempotencyKey":format!("{prefix}-thread"),
            }).as_object().unwrap()).await.map_err(|error|error.message)?;
            print_json(&json!({"task":task}))
        }
        _ => Err(format!(
            "Unknown command {command}. Use serve, status, doctor, smoke, spike, setup, native-login, or usage-summary."
        )),
    };
    result.map(|()| ExitCode::SUCCESS)
}

async fn native_login(args: Vec<String>) -> Result<(), String> {
    use std::time::Duration;
    use t3_code_mcp::{
        config::load_native_endpoint_config,
        native_credential::NativeCredentialStore,
        native_login::{
            NativeLoginInput, login_native, parse_native_login_args, read_pairing_code,
        },
        operations::audit_log::AuditLog,
        t3::NativeMcpClient,
    };
    let options = parse_native_login_args(&args)?;
    let endpoint = load_native_endpoint_config().map_err(|error| error.to_string())?;
    let code = read_pairing_code()?;
    let credential = login_native(NativeLoginInput {
        resource: &endpoint.t3_native_mcp_url,
        access: options.access,
        pairing_code: &code,
        now: None,
    })
    .await?;
    let verifier = NativeMcpClient::with_credential(
        &endpoint.t3_native_mcp_url,
        credential.clone(),
        Some(Arc::new(AuditLog::new(
            endpoint.data_dir.join("audit.jsonl"),
        ))),
        Duration::from_secs(30),
    );
    let snapshot = verifier.snapshot().await;
    verifier.close();
    if let Some(error) = snapshot.error {
        return Err(format!(
            "Login succeeded, but the token could not list native tools: {}",
            error.message
        ));
    }
    NativeCredentialStore::in_data_dir(&endpoint.data_dir)
        .write(&credential)
        .map_err(|error| error.to_string())?;
    print_json(&json!({
        "ok":true,"access":credential.access,"scopes":credential.scopes,"expiresAt":credential.expires_at,
        "nativeTools":snapshot.tools.len(),"missingNativeTools":snapshot.missing,
    }))?;
    eprintln!(
        "An HTTP gateway uses this login on its next request. A stdio session lists the native tools that it found at start. If it lists none, restart the stdio server (the MCP server process that your client starts). A renewal needs no restart while the session lists native tools."
    );
    Ok(())
}

fn print_json(value: &Value) -> Result<(), String> {
    use std::io::Write;
    let mut text = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    text.push(b'\n');
    std::io::stdout()
        .write_all(&text)
        .map_err(|error| error.to_string())
}

fn env_trimmed(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|value|value.trim_matches(|character| matches!(character,
        '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )).to_owned()).filter(|value|!value.is_empty())
}

fn parse_host_tool_names(raw: Option<&str>) -> Result<Option<Vec<String>>, String> {
    let Some(raw) = raw.filter(|raw| !raw.trim().is_empty()) else {
        return Ok(None);
    };
    let value: Value = serde_json::from_str(raw).map_err(|error| error.to_string())?;
    let message = "MCP_HOST_TOOL_NAMES_JSON must be a JSON array of non-empty tool names captured from the actual host.";
    value
        .as_array()
        .ok_or_else(|| message.to_owned())?
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|name| !name.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| message.to_owned())
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn setup(args: Vec<String>) -> Result<(), String> {
    let options = setup::parse_setup_args(&args).map_err(|error| error.to_string())?;
    let result = setup::render_setup(&options).map_err(|error| error.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({"ok": true, "files": result.files}))
            .map_err(|error| error.to_string())?
    );
    if args.iter().any(|arg| arg.starts_with("--node=")) {
        eprintln!(
            "setup --node is ignored by the Rust runtime; use --executable to select the Rust binary."
        );
    }
    eprintln!(
        "Setup rendered without secrets. Edit the env file to set T3_ACCESS_TOKEN, MCP_BEARER_TOKEN, and CONTROL_PLANE_API_KEY (mode 600), then enable the user services."
    );
    Ok(())
}

#[derive(Default)]
struct UsageArgs {
    file: Option<PathBuf>,
    since: Option<String>,
    until: Option<String>,
}

fn parse_usage_args(args: Vec<String>) -> Result<UsageArgs, String> {
    let mut parsed = UsageArgs::default();
    let usage = "Use usage-summary [--file audit.jsonl] [--since ISO] [--until ISO].";
    let mut values = args.into_iter();
    while let Some(name) = values.next() {
        let Some(value) = values.next() else {
            return Err(usage.into());
        };
        if value.is_empty() || value.starts_with("--") {
            return Err(usage.into());
        }
        match name.as_str() {
            "--file" if parsed.file.is_none() => parsed.file = Some(PathBuf::from(value)),
            "--since" if parsed.since.is_none() => parsed.since = Some(value),
            "--until" if parsed.until.is_none() => parsed.until = Some(value),
            _ => return Err(usage.into()),
        }
    }
    Ok(parsed)
}

fn usage_summary(args: Vec<String>) -> Result<(), String> {
    let args = parse_usage_args(args)?;
    let file = match args.file {
        Some(file) => file,
        None => {
            let data_dir = std::env::var("T3_MCP_DATA_DIR").unwrap_or_default();
            let data_dir = if data_dir.trim().is_empty() {
                PathBuf::from("./data")
            } else {
                PathBuf::from(data_dir.trim())
            };
            data_dir.join("audit.jsonl")
        }
    };
    let report = summarize_usage(
        file,
        &UsageWindow {
            since: args.since,
            until: args.until,
        },
    )
    .map_err(|error| error.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{UsageArgs, parse_usage_args};

    #[test]
    fn usage_summary_arguments_match_the_node_cli_contract() {
        let parsed = parse_usage_args(vec![
            "--file".into(),
            "audit.jsonl".into(),
            "--since".into(),
            "2026-01-01".into(),
            "--until".into(),
            "2026-02-01".into(),
        ])
        .unwrap();
        assert_eq!(
            parsed.file.unwrap(),
            std::path::PathBuf::from("audit.jsonl")
        );
        assert_eq!(parsed.since.as_deref(), Some("2026-01-01"));
        assert_eq!(parsed.until.as_deref(), Some("2026-02-01"));
    }

    #[test]
    fn usage_summary_rejects_unknown_duplicate_and_missing_arguments() {
        for args in [
            vec!["--unknown".into(), "value".into()],
            vec!["--file".into(), "one".into(), "--file".into(), "two".into()],
            vec!["--since".into()],
            vec!["--since".into(), "--until".into()],
            vec!["--file".into(), "".into()],
        ] {
            assert!(parse_usage_args(args).is_err());
        }
    }

    #[test]
    fn usage_summary_defaults_to_no_filters() {
        assert_eq!(
            parse_usage_args(vec![]).unwrap().file,
            UsageArgs::default().file
        );
    }
}
