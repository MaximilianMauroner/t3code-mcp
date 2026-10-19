use super::{Fixture, thread};
use crate::{
    mcp::{IncomingMessage, McpServer},
    operations::audit_log::{AuditEventInput, AuditSource},
};
use serde_json::{Value, json};

async fn call(fixture: &Fixture, name: &str, arguments: &str) -> Value {
    let request: Value = serde_json::from_str(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#
    ))
    .unwrap();
    let response = McpServer::gateway()
        .handle_message(IncomingMessage::parse(request).unwrap(), &fixture.gateway)
        .await
        .unwrap();
    let result = &response["result"];
    assert_ne!(result["isError"], true, "{response}");
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, result["structuredContent"]);
    result["structuredContent"].clone()
}

#[tokio::test]
async fn decimal_and_exponent_audit_limits_preserve_each_page() {
    let fixture = Fixture::new().await;
    for row in 1..=3 {
        let mut event = AuditEventInput::new(AuditSource::System, "numeric-limit");
        event.correlation_id = Some(format!("row-{row}"));
        fixture.gateway.audit.record(event);
    }
    for literal in ["1.0", "1e0"] {
        for offset in 0..3 {
            let cursor = if offset == 0 {
                String::new()
            } else {
                format!(r#", "cursor":"{offset}""#)
            };
            let value = call(
                &fixture,
                "t3_audit_log",
                &format!(r#"{{"event":"numeric-limit","limit":{literal}{cursor}}}"#),
            )
            .await;
            let page = &value["page"];
            assert_eq!(page["total"], 3);
            assert_eq!(
                page["items"].as_array().unwrap().len(),
                1,
                "{literal}: {value}"
            );
            assert_eq!(
                page["items"][0]["correlationId"],
                format!("row-{}", offset + 1)
            );
            assert_eq!(page["hasMore"], offset < 2);
            assert_eq!(
                page["nextCursor"],
                if offset < 2 {
                    json!((offset + 1).to_string())
                } else {
                    Value::Null
                }
            );
        }
    }
}

#[tokio::test]
async fn decimal_and_exponent_limits_reach_typed_thread_pagination() {
    let fixture = Fixture::new().await;
    let mut second = thread();
    second["id"] = json!("second-thread");
    fixture.shell_threads.lock().unwrap().push(second);
    for literal in ["1.0", "1e0"] {
        let value = call(
            &fixture,
            "t3_threads_list",
            &format!(r#"{{"status":"all","limit":{literal}}}"#),
        )
        .await;
        let page = &value["page"];
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert_eq!(page["total"], 2);
        assert_eq!(page["hasMore"], true);
        assert_eq!(page["nextCursor"], "1");
    }
}

#[tokio::test]
async fn decimal_and_exponent_git_byte_limits_bound_the_real_patch() {
    let fixture = Fixture::new().await;
    let repository = fixture.directory.join("numeric-limit-repository");
    std::fs::create_dir(&repository).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=Limit Fixture",
                "-c",
                "user.email=fixture@example.test",
            ])
            .args(args)
            .current_dir(&repository)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    git(&["init", "-b", "main"]);
    std::fs::write(repository.join("selected.txt"), "baseline\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-m", "Baseline"]);
    std::fs::write(
        repository.join("selected.txt"),
        "modified line ".repeat(400) + "\n",
    )
    .unwrap();
    let full_patch = git(&[
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--",
        "selected.txt",
    ]);
    assert!(full_patch.is_ascii() && full_patch.len() > 1024);
    fixture.shell_threads.lock().unwrap()[0]["worktreePath"] = json!(repository);
    let identity = thread();
    for literal in ["1024.0", "1.024e3"] {
        let value = call(
            &fixture,
            "t3_git_diff",
            &format!(
                r#"{{"projectId":{},"threadId":{},"mode":"unstaged","maxBytes":{literal}}}"#,
                identity["projectId"], identity["id"]
            ),
        )
        .await;
        assert_eq!(value["patch"], &full_patch[..1024]);
        assert_eq!(
            value["truncation"],
            json!({
                "truncated":true,"reason":"byte_limit","maxBytes":1024,"capturedBytes":1024,
                "totalBytes":full_patch.len(),"omittedBytes":full_patch.len() - 1024,
            })
        );
    }
}
