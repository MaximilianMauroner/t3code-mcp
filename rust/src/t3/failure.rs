use super::{
    iso, timestamp,
    types::{MessageRole, ProviderRetry, Thread, ThreadShell, TurnState},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    Quota,
    RateLimit,
    AuthBilling,
    ProviderInternal,
    ProviderError,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureSource {
    #[serde(rename = "t3_v2_turn_item")]
    V2TurnItem,
    #[serde(rename = "t3_turn")]
    Turn,
    #[serde(rename = "t3_message")]
    Message,
    #[serde(rename = "t3_activity")]
    Activity,
    #[serde(rename = "t3_session")]
    Session,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureInfo {
    pub category: FailureCategory,
    pub class: Option<String>,
    pub code: Option<String>,
    pub message: String,
    pub provider: Option<String>,
    pub model: String,
    pub turn_id: String,
    pub reset_at: Option<String>,
    pub retry_after: Option<String>,
    pub retryable: Option<bool>,
    pub retry: Option<ProviderRetry>,
    pub source: FailureSource,
    #[serde(skip)]
    pub identity: String,
}
fn known_category(
    class: Option<&str>,
    code: Option<&str>,
    message: Option<&str>,
) -> FailureCategory {
    if matches!(
        code,
        Some("credits_required" | "auth_unavailable" | "authentication_error")
    ) || message
        == Some("API Error: Request rejected (429) · Usage credits are required for this model.")
    {
        FailureCategory::AuthBilling
    } else if code == Some("api_error_429") {
        FailureCategory::Unknown
    } else if matches!(code, Some("rate_limit_error" | "rateLimitExceeded")) {
        FailureCategory::RateLimit
    } else if matches!(code, Some("usageLimitExceeded" | "usage_limit"))
        || class == Some("usage_limit")
        || matches!(
            message,
            Some(
                "Claude usage limit reached. Send the message again once the limit resets."
                    | "Codex usage limit reached. Send the message again once the limit resets."
            )
        )
    {
        FailureCategory::Quota
    } else if class == Some("provider_error") {
        FailureCategory::ProviderError
    } else {
        FailureCategory::Unknown
    }
}
pub fn category_for_failure(
    class: Option<&str>,
    code: Option<&str>,
    message: Option<&str>,
) -> FailureCategory {
    let derived = message
        .filter(|m| !m.is_empty())
        .map(|m| code_for_failure(code, m));
    known_category(
        class,
        derived.as_ref().map(|v| v.as_deref()).unwrap_or(code),
        message,
    )
}
fn recognized(code: &str) -> bool {
    code == "api_error_429" || known_category(None, Some(code), None) != FailureCategory::Unknown
}
fn code_for_failure(code: Option<&str>, message: &str) -> Option<String> {
    if code.is_some_and(|c| !c.is_empty() && recognized(c)) {
        return code.map(str::to_owned);
    }
    let parsed = provider_code(message);
    if parsed.as_deref().is_some_and(recognized) {
        parsed
    } else {
        code.map(str::to_owned).or(parsed)
    }
}
fn provider_code(message: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<Value>(message)
        && v["error"].is_object()
    {
        let candidates = [v["error"]["type"].as_str(), v["error"]["code"].as_str()];
        if let Some(code) = candidates
            .into_iter()
            .flatten()
            .find(|c| known_category(None, Some(c), None) != FailureCategory::Unknown)
            .or_else(|| candidates.into_iter().flatten().next())
        {
            return Some(code.into());
        }
    }
    static API: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^API Error:\s*(?:\d{3}\s+)?(rate_limit_error|auth_unavailable|authentication_error|credits_required)\s*:").unwrap()
    });
    static COOLING: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^API Error: Request rejected \(\d{3}\) · All credentials for model \S+ are cooling down \(last error: (rate_limit_error):").unwrap()
    });
    API.captures(message)
        .or_else(|| COOLING.captures(message))
        .and_then(|c| c.get(1).map(|m| m.as_str().into()))
}
pub fn failure_info(thread: &Thread, expected_turn_id: Option<&str>) -> Option<FailureInfo> {
    extract(&thread.shell, Some(thread), expected_turn_id)
}
pub fn shell_failure_info(
    thread: &ThreadShell,
    expected_turn_id: Option<&str>,
) -> Option<FailureInfo> {
    extract(thread, None, expected_turn_id)
}
fn extract(
    shell: &ThreadShell,
    full: Option<&Thread>,
    expected: Option<&str>,
) -> Option<FailureInfo> {
    let turn = shell.latest_turn.as_ref();
    let session = shell.session.as_ref();
    let turn_id = expected
        .or_else(|| turn.map(|t| t.turn_id.as_str()))
        .or_else(|| session.and_then(|s| s.active_turn_id.as_deref()))?;
    if let Some(persisted) = full
        .and_then(|t| t.turn_failures.as_ref())
        .and_then(|v| v.iter().find(|f| f.turn_id == turn_id))
    {
        let mut payload = persisted
            .failure
            .as_ref()
            .map(|v| serde_json::to_value(v).unwrap())
            .unwrap_or(json!({}));
        if let Some(retry) = &persisted.retry {
            payload["retry"] = serde_json::to_value(retry).unwrap();
        }
        return Some(build(
            &payload,
            turn_id,
            Some(&persisted.provider),
            &persisted.model_selection.model,
            if persisted.failure.is_some() {
                FailureSource::V2TurnItem
            } else {
                FailureSource::Turn
            },
        ));
    }
    if turn.is_some_and(|t| t.turn_id != turn_id || t.state != TurnState::Error)
        || (turn.is_none() && session.is_none_or(|s| s.status != "error"))
    {
        return None;
    }
    let activities: Vec<&Value> = full
        .map(|t| {
            t.activities
                .iter()
                .filter(|v| {
                    matches!(
                        v["kind"].as_str(),
                        Some("runtime.error" | "provider.turn.start.failed")
                    ) && v["turnId"].as_str() == Some(turn_id)
                        && v["payload"].is_object()
                })
                .map(|v| &v["payload"])
                .collect()
        })
        .unwrap_or_default();
    let activity = activities.last().copied();
    let assistant = full.and_then(|t| {
        t.messages.iter().rev().find(|m| {
            m.role == MessageRole::Assistant
                && m.turn_id.as_deref() == Some(turn_id)
                && m.text.starts_with("API Error:")
        })
    });
    let assistant_code = assistant.and_then(|m| provider_code(&m.text));
    let matches=session.is_some_and(|s| s.active_turn_id.as_deref()==Some(turn_id) || (s.active_turn_id.is_none() && s.status=="error" && turn.is_some_and(|t|t.turn_id==turn_id && matches!((timestamp(s.updated_at.as_deref()),timestamp(Some(&t.requested_at)),timestamp(t.completed_at.as_deref())),(Some(a),Some(b),Some(c)) if a>=b && a<=c))));
    let provider = if matches {
        session.and_then(|s| {
            if shell
                .extra
                .get("orchestrationProtocolVersion")
                .and_then(Value::as_u64)
                == Some(2)
            {
                s.provider_instance_id
                    .as_deref()
                    .or(s.provider_name.as_deref())
            } else {
                s.provider_name.as_deref()
            }
        })
    } else {
        None
    };
    let provider = provider
        .or(shell.model_selection.provider.as_deref())
        .or(shell.model_selection.instance_id.as_deref());
    let make = |payload: &Value, source| {
        build(
            payload,
            turn_id,
            provider,
            &shell.model_selection.model,
            source,
        )
    };
    if let Some(assistant) = assistant.filter(|m| {
        category_for_failure(None, assistant_code.as_deref(), Some(&m.text))
            != FailureCategory::Unknown
    }) {
        let mut payload = activities
            .iter()
            .rev()
            .find(|v| v["message"].as_str().or(v["detail"].as_str()) == Some(&assistant.text))
            .map(|v| (*v).clone())
            .unwrap_or(json!({}));
        payload["message"] = assistant.text.clone().into();
        if let Some(code) = &assistant_code {
            payload["code"] = code.clone().into();
        }
        return Some(make(&payload, FailureSource::Message));
    }
    if let Some(activity) = activity.filter(|a| {
        a.as_object()
            .unwrap()
            .values()
            .any(|v| !v.is_null() && v.as_str() != Some(""))
    }) {
        return Some(make(activity, FailureSource::Activity));
    }
    if let Some(s) = session.filter(|s| {
        matches
            && (s.last_error.as_ref().is_some_and(|v| !v.trim().is_empty())
                || s.last_error_class.as_ref().is_some_and(|v| !v.is_empty())
                || s.failure_code.as_ref().is_some_and(|v| !v.is_empty())
                || s.reset_at.as_ref().is_some_and(|v| !v.is_empty())
                || s.retry_after.is_some())
    }) {
        let message = s.last_error.as_deref().filter(|v| !v.trim().is_empty());
        let code = s
            .failure_code
            .clone()
            .or_else(|| message.and_then(provider_code));
        let mut payload = json!({"class":s.last_error_class,"code":code,"resetAt":s.reset_at,"retryAfter":s.retry_after});
        if let Some(message) = message {
            payload["message"] = message.into();
        }
        let mut failure = make(&payload, FailureSource::Session);
        if let Some(category) = s.failure_category.as_deref() {
            if matches!(
                category,
                "quota" | "rate_limit" | "auth_billing" | "provider_internal" | "provider_error"
            ) {
                failure.category = serde_json::from_value(category.into()).unwrap();
            } else if category == "unknown" && s.last_error_class.as_deref() == Some("usage_limit")
            {
                failure.category = category_for_failure(None, code.as_deref(), message);
            }
        }
        return Some(failure);
    }
    if let Some(assistant) = assistant {
        return Some(make(
            &json!({"message":assistant.text,"code":assistant_code}),
            FailureSource::Message,
        ));
    }
    if turn.is_none() && session.and_then(|s| s.active_turn_id.as_deref()) != Some(turn_id) {
        return None;
    }
    Some(make(&json!({}), FailureSource::Turn))
}
fn build(
    p: &Value,
    turn_id: &str,
    provider: Option<&str>,
    model: &str,
    source: FailureSource,
) -> FailureInfo {
    let message = p["message"]
        .as_str()
        .or(p["detail"].as_str())
        .unwrap_or("T3 reported that the provider turn failed without an error message.");
    let code = code_for_failure(p["code"].as_str().or(p["type"].as_str()), message);
    FailureInfo {
        category: category_for_failure(p["class"].as_str(), code.as_deref(), Some(message)),
        class: p["class"]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(|v| sanitize_metadata(v, 200)),
        code: code
            .filter(|v| !v.is_empty())
            .map(|v| sanitize_metadata(&v, 200)),
        message: sanitize_failure_text(message, 2000),
        provider: provider.map(sanitize_failure_identifier),
        model: sanitize_failure_identifier(model),
        turn_id: turn_id.into(),
        reset_at: timestamp(p["resetAt"].as_str()).map(iso),
        retry_after: p.get("retryAfter").filter(|v| !v.is_null()).map(|v| {
            sanitize_failure_text(
                &v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string()),
                200,
            )
        }),
        retryable: p["retryable"].as_bool(),
        retry: p
            .get("retry")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        source,
        identity: format!("{:x}", Sha256::digest(message.as_bytes())),
    }
}
fn truncate(value: &str, max: usize) -> String {
    String::from_utf16_lossy(&value.encode_utf16().take(max).collect::<Vec<_>>())
}
pub fn sanitize_failure_identifier(value: &str) -> String {
    truncate(
        &value
            .chars()
            .map(|c| {
                if c <= '\u{1f}' || c == '\u{7f}' {
                    ' '
                } else {
                    c
                }
            })
            .collect::<String>(),
        200,
    )
}
fn sanitize_metadata(value: &str, max: usize) -> String {
    static PATTERNS: std::sync::LazyLock<[(fancy_regex::Regex, &str); 5]> =
        std::sync::LazyLock::new(|| {
            let key = r"(?:[A-Za-z][A-Za-z0-9]*[_-])*(?:api[_-]?key|(?:access|refresh|id|session)[_-]?token|(?:secret|access|private)[_-]?key(?:[_-]?id)?|authorization|credentials?|password|secret|token)";
            [
                (
                    format!(r#"(?i)(["'])({key})\1\s*:\s*(["'])(?:\\.|(?!\3)[^\\])*\3"#),
                    "$1$2$1:$3[REDACTED]$3",
                ),
                (
                    format!(
                        r#"(?i)(^|[\s,{{(])({key})\s*:\s*(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\s,;]+)"#
                    ),
                    "$1$2=[REDACTED]",
                ),
                (
                    r"(?i)\b(Bearer|Basic)\s+[^\s,;]+".into(),
                    "$1 [REDACTED]",
                ),
                (r"\bsk-[A-Za-z0-9_-]{12,}\b".into(), "[REDACTED]"),
                (
                    format!(r#"(?i)\b({key})\s*=\s*(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\s,;]+)"#),
                    "$1=[REDACTED]",
                ),
            ]
            .map(|(pattern, replacement)| {
                (
                    fancy_regex::Regex::new(&pattern).expect("valid redaction expression"),
                    replacement,
                )
            })
        });
    let mut v = value.to_owned();
    for (pattern, replacement) in PATTERNS.iter() {
        v = pattern.replace_all(&v, *replacement).into_owned();
    }
    truncate(
        &v.chars()
            .map(|c| {
                if c <= '\u{1f}' || c == '\u{7f}' {
                    ' '
                } else {
                    c
                }
            })
            .collect::<String>(),
        max,
    )
}
pub fn sanitize_failure_text(value: &str, max: usize) -> String {
    static BEFORE_IPV6: std::sync::LazyLock<[(fancy_regex::Regex, &str); 5]> =
        std::sync::LazyLock::new(|| {
            [
                (r"\n\s+at\s[^\n]*", ""),
                (r"(?i)(?:https?|file)://[^\s)]+", "[REDACTED URL]"),
                (
                    r#"(["'])(?:/|[A-Za-z]:[\\/]|\\\\)[^\r\n]*?\1"#,
                    "$1[REDACTED PATH]$1",
                ),
                (
                    r#"(^|[^\p{L}\p{N}_\\/])(?:/[^\s"'<>\])}]+|[A-Za-z]:[\\/][^\s"'<>\])}]+|\\\\[^\s"'<>\])}]+)"#,
                    "$1[REDACTED PATH]",
                ),
                (r"\b(?:\d{1,3}\.){3}\d{1,3}(?::\d+)?\b", "[REDACTED HOST]"),
            ]
            .map(|(pattern, replacement)| {
                (
                    fancy_regex::Regex::new(pattern).expect("valid redaction expression"),
                    replacement,
                )
            })
        });
    let mut v = value.to_owned();
    for (pattern, replacement) in BEFORE_IPV6.iter() {
        v = pattern.replace_all(&v, *replacement).into_owned();
    }
    static IPV6: std::sync::LazyLock<[fancy_regex::Regex; 2]> = std::sync::LazyLock::new(|| {
        [
            r"(?i)\[([\da-f:.]+(?:%[\w.-]+)?)\](?::\d+)?",
            r"(?i)(?<![\w:])(?:[\da-f]{0,4}:){2,}[\da-f:.]+(?:%[\w.-]+)?(?![\w:])",
        ]
        .map(|pattern| fancy_regex::Regex::new(pattern).unwrap())
    });
    for re in IPV6.iter() {
        v = re
            .replace_all(&v, |c: &fancy_regex::Captures<'_>| {
                let host = c.get(1).or_else(|| c.get(0)).unwrap().as_str();
                if host.parse::<std::net::Ipv6Addr>().is_ok() {
                    "[REDACTED HOST]".into()
                } else {
                    c.get(0).unwrap().as_str().to_owned()
                }
            })
            .into_owned();
    }
    static AFTER_IPV6: std::sync::LazyLock<[(fancy_regex::Regex, &str); 3]> =
        std::sync::LazyLock::new(|| {
            let key = r"(?:[A-Za-z][A-Za-z0-9]*[_-])*(?:api[_-]?key|(?:access|refresh|id|session)[_-]?token|(?:secret|access|private)[_-]?key(?:[_-]?id)?|authorization|credentials?|password|secret|token)";
            [
                (
                    r"(?i)\b(?:[a-z\d](?:[a-z\d-]*[a-z\d])?\.)+[a-z][a-z\d-]*(?::\d+)?\b".into(),
                    "[REDACTED HOST]",
                ),
                (r"(?i)\b[a-z][a-z\d-]*:\d{1,5}\b".into(), "[REDACTED HOST]"),
                (
                    format!(
                        r#"(?i)\b({key})\s*:\s*(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\s,;]+)"#
                    ),
                    "$1=[REDACTED]",
                ),
            ]
            .map(|(pattern, replacement)| {
                (
                    fancy_regex::Regex::new(&pattern).expect("valid redaction expression"),
                    replacement,
                )
            })
        });
    for (pattern, replacement) in AFTER_IPV6.iter() {
        v = pattern.replace_all(&v, *replacement).into_owned();
    }
    sanitize_metadata(&v, max)
}
