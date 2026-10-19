use super::*;

impl T3Gateway {
    pub(in crate::gateway) async fn result_get(
        &self,
        args: &Map<String, Value>,
    ) -> Result<Value, ToolFailure> {
        let input: ResultGetInput = parse_args(args)?;
        let detail = self
            .task_get(&Map::from_iter([("taskRef".into(), json!(input.task_ref))]))
            .await?;
        let task = &detail["task"];
        let observed_at = now_iso();
        let mut limitations = Vec::new();
        let mut git = Value::Null;
        if task["threadId"].is_null() {
            limitations.push(
                "The task has no proven T3 thread, so workspace evidence is unavailable."
                    .to_owned(),
            );
        } else if task["baselineRevision"].is_null() {
            limitations.push(
                "The gateway could not capture a Git baseline after T3 accepted the workspace."
                    .to_owned(),
            );
        } else {
            let target = json!({"projectId": task["projectId"], "threadId": task["threadId"]});
            let mut status_args = target.as_object().cloned().unwrap_or_default();
            status_args.insert("maxEntries".into(), json!(100));
            let mut diff_args = target.as_object().cloned().unwrap_or_default();
            diff_args.insert("paths".into(), json!(input.paths));
            diff_args.insert("maxBytes".into(), json!(input.max_bytes));
            let mut compare_args = diff_args.clone();
            compare_args.insert("baseRevision".into(), task["baselineRevision"].clone());
            compare_args.insert("headRevision".into(), json!("HEAD"));
            let mut staged_args = diff_args.clone();
            staged_args.insert("mode".into(), json!("staged"));
            diff_args.insert("mode".into(), json!("unstaged"));
            match tokio::try_join!(
                self.git_status(&status_args),
                self.git_compare(&compare_args),
                self.git_diff(&staged_args),
                self.git_diff(&diff_args),
            ) {
                Ok((status, committed, staged, unstaged)) => {
                    if task["baselineAttribution"] != "clean" {
                        limitations.push("The captured baseline workspace was not clean, so task attribution is incomplete.".to_owned());
                    }
                    if status["clean"] != true {
                        limitations.push("The current workspace has uncommitted or untracked changes; inspect staged, unstaged, and status evidence separately.".to_owned());
                    }
                    git = json!({
                        "source": "git_observed",
                        "baselineRevision": task["baselineRevision"],
                        "status": status,
                        "committed": committed,
                        "staged": staged,
                        "unstaged": unstaged,
                    });
                }
                Err(error) => {
                    limitations.push(format!("Git evidence is unavailable: {}", error.message))
                }
            }
        }
        if !task["run"]["latestResponse"].is_null() {
            limitations.push("The assistant response is agent-reported output; it is not independent command verification.".to_owned());
        }
        Ok(json!({
            "environmentId": self.known_environment_id().await,
            "observedAt": observed_at,
            "task": task,
            "evidence": {
                "taskStateSource": if task["thread"].is_null() { "gateway_journal" } else { "t3_observed" },
                "git": git,
            },
            "limitations": limitations,
        }))
    }
}
