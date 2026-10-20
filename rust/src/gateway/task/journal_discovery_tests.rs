use super::*;

#[tokio::test]
async fn discovery_retains_only_selected_failures_and_recovers_hidden_rows() {
    for (name, extra) in [
        ("t3_threads_overview", json!({"runningLimit": 0})),
        (
            "t3_threads_list",
            json!({"status": "all", "detail": "full"}),
        ),
        ("t3_threads_list", json!({"activity": "failed"})),
        ("t3_threads_list", json!({})),
    ] {
        let fixture = Fixture::empty().await;
        let mut rows = Vec::new();
        for (id, project, title, archived) in [
            ("selected", "selected-project", "keep", false),
            ("project-hidden", "other-project", "keep", false),
            ("query-hidden", "selected-project", "excluded", false),
            ("archive-hidden", "selected-project", "keep", true),
        ] {
            for recovered in [false, true] {
                let mut row = failed_thread("Synthetic failure");
                row["id"] = json!(format!(
                    "{id}-{}",
                    if recovered { "recovered" } else { "failed" }
                ));
                row["projectId"] = json!(project);
                row["title"] = json!(title);
                if archived {
                    row["archivedAt"] = json!("2026-10-18T00:00:00.000Z");
                }
                if recovered {
                    row["latestTurn"]["state"] = json!("completed");
                    row["session"] = json!({"status": "ready"});
                    row["settledOverride"] = json!("settled");
                }
                rows.push(row);
            }
        }
        *fixture.shell_threads.lock().unwrap() = rows;
        fixture.shell_sequence.store(101, Ordering::SeqCst);
        fixture.missing(503, "unavailable", None);
        let mut args =
            json!({"projectId": "selected-project", "query": "keep", "includeArchived": false});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let result = mcp_value(&fixture.gateway, name, args).await;
        let selected_rows = if name == "t3_threads_overview" {
            &result["highlights"]
        } else {
            &result["page"]["items"]
        };
        let selected = selected_rows
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "selected-failed")
            .unwrap();
        assert_eq!(selected["id"], "selected-failed");
        assert_eq!(selected["failure"]["message"], "Synthetic failure");
        let journal = stored_journal(&fixture);
        for id in [
            "selected",
            "project-hidden",
            "query-hidden",
            "archive-hidden",
        ] {
            assert_eq!(
                journal
                    .get_failure_by_turn_id(&format!("{id}-failed"), TURN)
                    .unwrap()
                    .is_some(),
                id == "selected",
                "{name} retained {id} outside its selection",
            );
            assert!(
                journal
                    .get_recovery_by_turn_id(&format!("{id}-recovered"), TURN)
                    .unwrap()
                    .is_some()
            );
            assert_eq!(
                journal
                    .get_failure_evidence_order(&format!("{id}-recovered"), TURN)
                    .unwrap()
                    .unwrap()["snapshotSequence"],
                101,
            );
        }
    }
}
