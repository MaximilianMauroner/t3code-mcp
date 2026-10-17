use super::audit_log::{
    AuditError, AuditEvent, AuditSource, AuditValue, in_time_window, normalize_json, number_value,
    parse_filter_time, parse_timestamp, scan_audit_file,
};
use indexmap::IndexMap;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::LazyLock;

const LATENCY_BOUNDS: [f64; 12] = [
    1.0,
    5.0,
    10.0,
    50.0,
    100.0,
    500.0,
    1_000.0,
    5_000.0,
    10_000.0,
    30_000.0,
    60_000.0,
    f64::INFINITY,
];
const MAX_TRACKED_THREADS: usize = 10_000;
const MAX_OPERATIONS: usize = 1_000;
const SIGNATURE_FIELDS: [&str; 8] = [
    "activity",
    "latestTurn",
    "updatedAt",
    "latestResponse",
    "hasPendingApprovals",
    "hasPendingUserInput",
    "messageCount",
    "activityCount",
];

#[derive(Debug, Default, Deserialize)]
pub struct UsageWindow {
    pub since: Option<String>,
    pub until: Option<String>,
}

#[derive(Default)]
struct Latency {
    count: u64,
    total_ms: f64,
    min_ms: f64,
    max_ms: f64,
    buckets: [u64; 12],
}

impl Latency {
    fn add(&mut self, duration: Option<f64>) {
        let Some(duration) = duration.filter(|v| v.is_finite() && *v >= 0.0) else {
            return;
        };
        self.min_ms = if self.count == 0 {
            duration
        } else {
            self.min_ms.min(duration)
        };
        self.max_ms = self.max_ms.max(duration);
        self.count += 1;
        self.total_ms += duration;
        self.buckets[LATENCY_BOUNDS
            .iter()
            .position(|bound| duration <= *bound)
            .unwrap()] += 1;
    }

    fn quantile(&self, fraction: f64) -> Value {
        if self.count == 0 {
            return Value::Null;
        }
        let target = (self.count as f64 * fraction).ceil() as u64;
        let mut cumulative = 0;
        for (bound, count) in LATENCY_BOUNDS.iter().zip(self.buckets) {
            cumulative += count;
            if cumulative >= target {
                return number_value(*bound);
            }
        }
        Value::Null
    }

    fn summary(&self) -> Value {
        json!({
            "count": self.count,
            "meanMs": if self.count > 0 { number_value(self.total_ms / self.count as f64) } else { Value::Null },
            "minMs": if self.count > 0 { number_value(self.min_ms) } else { Value::Null },
            "maxMs": if self.count > 0 { number_value(self.max_ms) } else { Value::Null },
            "medianUpperBoundMs": self.quantile(0.5), "p95UpperBoundMs": self.quantile(0.95),
        })
    }
}

#[derive(Default)]
struct Tool {
    calls: u64,
    completed: u64,
    errors: u64,
    latency: Latency,
}

#[derive(Default)]
struct Endpoint {
    requests: u64,
    errors: u64,
    latency: Latency,
}

#[derive(Default)]
struct Usage {
    tools: IndexMap<String, Tool>,
    endpoints: IndexMap<String, Endpoint>,
    attributed: IndexMap<String, u64>,
    errors: IndexMap<String, u64>,
    previous: IndexMap<String, String>,
    included_events: u64,
    tool_calls: u64,
    upstream_requests: u64,
    attributed_requests: u64,
    dispatch_calls: u64,
    model_selection_calls: u64,
    omitted_model_selection: u64,
    waits: u64,
    unchanged_active_waits: u64,
    comparable_thread_pairs: u64,
    unchanged_thread_pairs: u64,
    evicted_thread_states: u64,
    wait_observations: f64,
    measured_waits: u64,
    first: Option<(i64, String)>,
    last: Option<(i64, String)>,
}

impl Usage {
    fn observe(&mut self, event: AuditEvent) {
        self.included_events += 1;
        if let Some(timestamp) = event.timestamp.as_str()
            && let Some(time) = parse_timestamp(timestamp)
        {
            if self.first.as_ref().is_none_or(|(first, _)| time < *first) {
                self.first = Some((time, timestamp.to_owned()));
            }
            if self.last.as_ref().is_none_or(|(last, _)| time > *last) {
                self.last = Some((time, timestamp.to_owned()));
            }
        }
        let empty = AuditValue::Null;
        let details = event.details.as_ref();
        let detail = |key: &str| details.and_then(|d| d.get(key)).unwrap_or(&empty);
        let operation = event
            .operation
            .as_ref()
            .and_then(|value| value.as_str())
            .unwrap_or("unknown");
        if event.source == AuditSource::Mcp {
            let name = bounded_key(&self.tools, operation);
            let entry = self.tools.entry(name.clone()).or_default();
            if event.event == "tool.call" {
                entry.calls += 1;
                self.tool_calls += 1;
                if [
                    "t3_task_start",
                    "t3_thread_launch",
                    "t3_thread_create",
                    "t3_thread_send",
                ]
                .contains(&operation)
                {
                    self.dispatch_calls += 1;
                }
                if ["t3_task_start", "t3_thread_launch", "t3_thread_create"].contains(&operation)
                    || (operation == "t3_thread_send"
                        && detail("native") != &AuditValue::Bool(true))
                {
                    self.model_selection_calls += 1;
                    if object_get(detail("arguments"), "modelSelection").is_null() {
                        self.omitted_model_selection += 1;
                    }
                }
            }
            if event.event == "tool.result" {
                if event.outcome.as_ref().and_then(|value| value.as_str()) == Some("error") {
                    entry.errors += 1;
                    increment(
                        &mut self.errors,
                        &format!(
                            "{name}:{}",
                            detail("errorCode").as_str().unwrap_or("unknown")
                        ),
                    );
                } else if event.outcome.as_ref().and_then(|value| value.as_str())
                    == Some("completed")
                {
                    entry.completed += 1;
                }
                entry.latency.add(event.duration_ms);
                let result = detail("result");
                if operation == "t3_thread_wait"
                    && event.outcome.as_ref().and_then(|value| value.as_str()) == Some("completed")
                {
                    self.waits += 1;
                    if object_get(result, "timedOut") == &AuditValue::Bool(true)
                        && ["preparing", "queued", "starting", "running", "waiting"]
                            .contains(&js_string(object_get(result, "status")).as_str())
                    {
                        self.unchanged_active_waits += 1;
                    }
                }
                if operation == "t3_run_wait"
                    && event.outcome.as_ref().and_then(|value| value.as_str()) == Some("completed")
                {
                    self.waits += 1;
                    if object_get(result, "timedOut") == &AuditValue::Bool(true)
                        && ["running", "accepted"]
                            .contains(&js_string(object_get(result, "runStatus")).as_str())
                    {
                        self.unchanged_active_waits += 1;
                    }
                    if let Some(observations) =
                        object_get(object_get(result, "monitoring"), "observations").as_f64()
                    {
                        self.measured_waits += 1;
                        self.wait_observations += observations;
                    }
                }
                if operation == "t3_thread_get"
                    && event.outcome.as_ref().and_then(|value| value.as_str()) == Some("completed")
                {
                    let thread = object_get(result, "thread");
                    if let Some(id) = object_get(thread, "id").as_str() {
                        let selected = AuditValue::Array(
                            SIGNATURE_FIELDS
                                .iter()
                                .map(|field| object_get(thread, field).clone())
                                .collect(),
                        );
                        let signature = selected.to_string();
                        if let Some(before) = self.previous.shift_remove(id) {
                            self.comparable_thread_pairs += 1;
                            if before == signature {
                                self.unchanged_thread_pairs += 1;
                            }
                        }
                        self.previous.insert(id.into(), signature);
                        if self.previous.len() > MAX_TRACKED_THREADS {
                            self.previous.shift_remove_index(0);
                            self.evicted_thread_states += 1;
                        }
                    }
                }
            }
        }
        if event.source == AuditSource::T3
            && event
                .event
                .as_str()
                .is_some_and(|name| ["upstream.request", "upstream.response"].contains(&name))
        {
            let name = bounded_key(&self.endpoints, &normalize_endpoint(operation));
            let entry = self.endpoints.entry(name).or_default();
            if event.event == "upstream.request" {
                entry.requests += 1;
                self.upstream_requests += 1;
                if event
                    .parent_correlation_id
                    .as_ref()
                    .is_some_and(|s| !s.is_empty())
                    && let Some(parent) = event
                        .parent_operation
                        .as_ref()
                        .filter(|s| !s.is_empty())
                        .and_then(|value| value.as_str())
                {
                    self.attributed_requests += 1;
                    increment(&mut self.attributed, parent);
                }
            } else {
                if event.outcome.as_ref().and_then(|value| value.as_str()) == Some("error") {
                    entry.errors += 1;
                }
                entry.latency.add(event.duration_ms);
            }
        }
    }
}

pub fn summarize_usage(path: impl AsRef<Path>, window: &UsageWindow) -> Result<Value, AuditError> {
    let since = parse_filter_time(window.since.as_deref(), "since")?;
    let until = parse_filter_time(window.until.as_deref(), "until")?;
    if let (Some(since), Some(until)) = (since, until)
        && since > until
    {
        return Err(AuditError::InvalidInput(
            "since must be before or equal to until.".into(),
        ));
    }
    let mut usage = Usage::default();
    let scan = scan_audit_file(path, |event| {
        if in_time_window(&event.timestamp, since, until) {
            usage.observe(event);
        }
    })?;
    let tools = sorted_object(
        &usage.tools,
        |entry| json!({"calls":entry.calls, "completed":entry.completed, "errors":entry.errors, "latency":entry.latency.summary()}),
    );
    let endpoints = sorted_object(
        &usage.endpoints,
        |entry| json!({"requests":entry.requests, "errors":entry.errors, "latency":entry.latency.summary()}),
    );
    let mut report = json!({
        "window": {"since":window.since, "until":window.until, "firstEventAt":usage.first.map(|(_, time)| time), "lastEventAt":usage.last.map(|(_, time)| time)},
        "scan": {"invalidLines":scan.invalid_lines, "bytes":scan.bytes, "validLines":scan.valid_lines, "includedEvents":usage.included_events},
        "denominators": {"toolCalls":usage.tool_calls, "dispatchCalls":usage.dispatch_calls, "modelSelectionCalls":usage.model_selection_calls,
            "upstreamRequests":usage.upstream_requests, "waits":usage.waits, "comparableThreadPairs":usage.comparable_thread_pairs},
        "launches": {"omittedModelSelection":usage.omitted_model_selection, "omissionMeans":"May intentionally use a project default or thread inheritance; not proof of an incorrect model."},
        "monitoring": {"unchangedActiveWaits":usage.unchanged_active_waits, "measuredWaits":usage.measured_waits, "waitObservations":number_value(usage.wait_observations),
            "unchangedThreadPairs":usage.unchanged_thread_pairs, "evictedThreadStates":usage.evicted_thread_states,
            "comparison":"Successive successful same-thread detail results in file order; may bridge failed reads. Thread states use bounded LRU retention.",
            "signatureFields":SIGNATURE_FIELDS, "interpretation":"Unchanged selected state is not proof a read was unnecessary. Wait timeout does not cancel work."},
        "tools":tools, "endpoints":endpoints, "errorCodes":sorted_object(&usage.errors, |count| json!(count)),
        "attribution": {"attributedRequests":usage.attributed_requests, "unattributedRequests":usage.upstream_requests - usage.attributed_requests,
            "requestsByTool":sorted_object(&usage.attributed, |count| json!(count))},
        "limits": {"maxTrackedThreads":MAX_TRACKED_THREADS, "maxOperationGroups":MAX_OPERATIONS,
            "latency":"Fixed histogram upper bounds for median/p95; null upper bound means over 60 seconds. Mean/min/max are exact.",
            "outcomes":"Tool completion/accepted dispatch is not independently verified task success. No token or financial savings are inferred."},
    });
    normalize_json(&mut report);
    Ok(report)
}

fn object_get<'a>(value: &'a AuditValue, key: &str) -> &'a AuditValue {
    value.get(key).unwrap_or(&AuditValue::Null)
}

fn js_string(value: &AuditValue) -> String {
    match value {
        AuditValue::String(value) => value.clone(),
        AuditValue::Utf16String(value) => String::from_utf16_lossy(value),
        AuditValue::Object(_) => "[object Object]".into(),
        AuditValue::Array(values) => values
            .iter()
            .map(|v| {
                if v.is_null() {
                    String::new()
                } else {
                    js_string(v)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        _ => value.to_string(),
    }
}

fn bounded_key<T>(map: &IndexMap<String, T>, key: &str) -> String {
    if map.contains_key(key) || map.len() < MAX_OPERATIONS - 1 {
        key
    } else {
        "other"
    }
    .into()
}

fn increment(map: &mut IndexMap<String, u64>, key: &str) {
    let name = bounded_key(map, key);
    *map.entry(name).or_default() += 1;
}

fn sorted_object<T>(map: &IndexMap<String, T>, convert: impl Fn(&T) -> Value) -> Value {
    let mut entries: Vec<_> = map
        .iter()
        .map(|(key, entry)| {
            let value = convert(entry);
            (
                format!("{key},{}", js_string(&AuditValue::from(value.clone()))),
                key.clone(),
                value,
            )
        })
        .collect();
    entries.sort_by(|(a, _, _), (b, _, _)| a.encode_utf16().cmp(b.encode_utf16()));
    Value::Object(
        entries
            .into_iter()
            .map(|(_, key, value)| (key, value))
            .collect::<Map<_, _>>(),
    )
}

static THREAD_ENDPOINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(/api/orchestration/threads/)[^/?\t\n\v\f\r \x{00a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}]+").unwrap()
});

fn normalize_endpoint(operation: &str) -> String {
    THREAD_ENDPOINT
        .replace(operation, "${1}:threadId")
        .into_owned()
}

#[cfg(test)]
#[path = "usage_summary_tests.rs"]
mod tests;
