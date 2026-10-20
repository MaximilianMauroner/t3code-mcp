use super::*;
use std::io::{BufRead, BufReader};
use t3_code_mcp::operations::audit_log::AuditValue;

fn seed_audit(fixture: &Fixture) -> AuditValue {
    let row = r#"{"version":1,"eventId":"event-\ud800","timestamp":"2026-10-17T00:00:00.000Z","processId":1,"source":"system","event":"stored-surrogate","operation":"operation-\ud800","details":{"preview":"\ud83d","low":"\udc00","pair":"\ud83d\ude00","key-\udfff":"value-\ud800"}}"#;
    fs::create_dir_all(fixture.directory.join("data")).unwrap();
    fs::write(
        fixture.directory.join("data/audit.jsonl"),
        format!("{row}\n"),
    )
    .unwrap();
    serde_json::from_str(row).unwrap()
}

fn audit_request(id: u64) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
        "name":"t3_audit_log","arguments":{"event":"stored-surrogate"}
    }})
}

fn assert_audit_response(response: &AuditValue, row: &AuditValue) {
    let result = &response["result"];
    assert_ne!(result["isError"], true, "{response}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["page"]["total"], 1, "{response}");
    assert_eq!(structured["page"]["invalidLines"], 0);
    assert_eq!(&structured["page"]["items"][0], row);
    let text: AuditValue =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(&text, structured);
}

#[tokio::test]
async fn stdio_audit_surrogates_survive_and_a_later_request_succeeds() {
    let fixture = Fixture::new(Duration::ZERO).await;
    let row = seed_audit(&fixture);
    let mut command = fixture.command("serve");
    command.stdin(Stdio::piped());
    tokio::task::spawn_blocking(move || {
        let mut child = Process(Some(command.spawn().unwrap()));
        let process = child.0.as_mut().unwrap();
        let mut input = process.stdin.take().unwrap();
        let mut output = BufReader::new(process.stdout.take().unwrap());
        for request in [
            audit_request(1),
            json!({"jsonrpc":"2.0","id":2,"method":"ping"}),
        ] {
            writeln!(input, "{request}").unwrap();
            let mut line = String::new();
            output.read_line(&mut line).unwrap();
            if line.is_empty() {
                drop(input);
                let failed = child.0.take().unwrap().wait_with_output().unwrap();
                panic!(
                    "MCP reply missing: {}",
                    String::from_utf8_lossy(&failed.stderr)
                );
            }
            let response: AuditValue = serde_json::from_str(&line).unwrap();
            assert_eq!(response["id"], request["id"]);
            if request["id"] == 1 {
                assert_audit_response(&response, &row);
            } else {
                assert_eq!(response["result"], json!({}));
            }
        }
        drop(input);
        let ended = child.0.take().unwrap().wait_with_output().unwrap();
        assert!(
            ended.status.success(),
            "{}",
            String::from_utf8_lossy(&ended.stderr)
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn http_audit_surrogates_survive_single_batch_and_later_requests() {
    let fixture = Fixture::new(Duration::ZERO).await;
    let row = seed_audit(&fixture);
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}");
    drop(reservation);
    let child = Process(Some(
        fixture
            .command("serve")
            .env("MCP_TRANSPORT", "http")
            .env("MCP_PORT", port.to_string())
            .spawn()
            .unwrap(),
    ));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let started = tokio::time::Instant::now();
    loop {
        if client.get(format!("{url}/healthz")).send().await.is_ok() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "HTTP startup timed out"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let long = format!("{}😀", "a".repeat(511));
    let mut init = initialize();
    init["params"]["clientInfo"]["description"] = json!(long);
    let initialized = client
        .post(format!("{url}/mcp"))
        .bearer_auth("fixture-bearer")
        .header("accept", "application/json, text/event-stream")
        .json(&init)
        .send()
        .await
        .unwrap();
    let session = initialized.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        initialized.json::<Value>().await.unwrap()["result"]["protocolVersion"],
        "2024-10-07"
    );
    for request in [
        audit_request(2),
        json!([audit_request(3), {"jsonrpc":"2.0","id":4,"method":"ping"}]),
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"t3_task_get","arguments":{"taskRef":long}}}),
        json!({"jsonrpc":"2.0","id":6,"method":"ping"}),
    ] {
        let response = client
            .post(format!("{url}/mcp"))
            .bearer_auth("fixture-bearer")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", &session)
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "application/json");
        let body = response.text().await.unwrap();
        let value: AuditValue = serde_json::from_str(&body).unwrap();
        if request.is_array() {
            assert_eq!(value.as_array().unwrap().len(), 2);
            assert_audit_response(&value[0], &row);
            assert_eq!(value[1]["result"], json!({}));
        } else if request["id"] == 2 {
            assert_audit_response(&value, &row);
        } else if request["id"] == 5 {
            assert_eq!(
                value["result"]["structuredContent"]["error"]["code"], "task_not_found",
                "{value}"
            );
        } else {
            assert_eq!(value["result"], json!({}));
        }
    }
    let ended = tokio::task::spawn_blocking(move || child.finish())
        .await
        .unwrap();
    #[cfg(unix)]
    assert!(
        ended.status.success(),
        "{}",
        String::from_utf8_lossy(&ended.stderr)
    );
    let audit = fs::read_to_string(fixture.directory.join("data/audit.jsonl")).unwrap();
    let rows: Vec<AuditValue> = audit
        .lines()
        .map(|row| serde_json::from_str(row).unwrap())
        .collect();
    assert_eq!(rows[0], row);
    let preview = "a"
        .repeat(511)
        .encode_utf16()
        .chain([0xd83d])
        .collect::<Vec<_>>();
    assert!(rows.iter().any(|row| {
        row["details"]["rpc"]["clientInfo"]["description"]["preview"].as_utf16()
            == Some(preview.clone())
    }));
    assert!(rows.iter().any(|row| {
        row["details"]["rpc"]["arguments"]["taskRef"]["preview"].as_utf16() == Some(preview.clone())
    }));
    assert!(rows.iter().any(|row| row["event"] == "tool.call"
        && row["details"]["arguments"]["taskRef"]["preview"].as_utf16() == Some(preview.clone())));
}

#[tokio::test]
async fn stdio_split_previews_in_client_arguments_and_results_preserve_the_session() {
    let fixture = Fixture::new(Duration::ZERO).await;
    let long = format!("{}😀", "a".repeat(511));
    fs::create_dir_all(fixture.directory.join("data")).unwrap();
    fs::write(fixture.directory.join("data/audit.jsonl"), format!("{}\n", json!({
        "version":1,"eventId":"generated","timestamp":"2026-10-17T00:00:00.000Z","processId":1,
        "source":"system","event":long,"operation":"generated-preview"
    }))).unwrap();
    let mut init = initialize();
    init["params"]["clientInfo"]["description"] = json!(long);
    let requests = [
        init,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"t3_task_get","arguments":{"taskRef":long}
        }}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
            "name":"t3_audit_log","arguments":{"operation":"generated-preview"}
        }}),
        json!({"jsonrpc":"2.0","id":4,"method":"ping"}),
    ];
    let mut command = fixture.command("serve");
    command.stdin(Stdio::piped());
    tokio::task::spawn_blocking(move || {
        let mut child = Process(Some(command.spawn().unwrap()));
        let process = child.0.as_mut().unwrap();
        let mut input = process.stdin.take().unwrap();
        let mut output = BufReader::new(process.stdout.take().unwrap());
        for request in requests {
            writeln!(input, "{request}").unwrap();
            let mut line = String::new();
            output.read_line(&mut line).unwrap();
            assert!(!line.is_empty(), "MCP reply missing for {}", request["id"]);
            let response: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(response["id"], request["id"]);
            assert!(response.get("error").is_none(), "{response}");
            if request["id"] == 2 {
                assert_eq!(
                    response["result"]["structuredContent"]["error"]["code"], "task_not_found",
                    "{response}"
                );
            } else if request["id"] == 3 {
                assert_eq!(
                    response["result"]["structuredContent"]["page"]["items"][0]["event"],
                    long
                );
            }
        }
        drop(input);
        let ended = child.0.take().unwrap().wait_with_output().unwrap();
        assert!(
            ended.status.success(),
            "{}",
            String::from_utf8_lossy(&ended.stderr)
        );
    })
    .await
    .unwrap();
    let rows: Vec<AuditValue> = fs::read_to_string(fixture.directory.join("data/audit.jsonl"))
        .unwrap()
        .lines()
        .map(|row| serde_json::from_str(row).unwrap())
        .collect();
    let preview = "a"
        .repeat(511)
        .encode_utf16()
        .chain([0xd83d])
        .collect::<Vec<_>>();
    assert!(rows.iter().any(
        |row| row["details"]["clientInfo"]["description"]["preview"].as_utf16()
            == Some(preview.clone())
    ));
    assert!(rows.iter().any(
        |row| row["details"]["arguments"]["taskRef"]["preview"].as_utf16() == Some(preview.clone())
    ));
    assert!(rows.iter().any(|row| {
        row["details"]["result"]["page"]["items"][0]["event"]["preview"].as_utf16()
            == Some(preview.clone())
    }));
}
