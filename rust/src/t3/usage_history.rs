use super::failure::sanitize_failure_text;
use chrono::{DateTime, FixedOffset, NaiveDate};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

type Parser = fn(&Value) -> Result<Value, String>;

pub fn validate_input(args: &Map<String, Value>) -> Result<Value, String> {
    let mut input = object(
        &Value::Object(args.clone()),
        &[("sinceDay", day), ("untilDay", day), ("timeZone", nonempty)],
        &[
            ("resolution", resolution),
            ("sinceTime", timestamp),
            ("untilTime", timestamp),
            ("awaitRefresh", boolean),
        ],
    )?;
    input["timeZone"] = json!(input["timeZone"].as_str().unwrap().trim());
    let since = calendar_day(input["sinceDay"].as_str().unwrap())?;
    let until = calendar_day(input["untilDay"].as_str().unwrap())?;
    if since > until {
        return Err("untilDay must be on or after sinceDay".into());
    }
    let limit = 366;
    if (until - since).num_days() + 1 > limit {
        return Err(format!("The window must include at most {limit} days"));
    }
    if input["resolution"] == "hour" {
        let (Some(start), Some(end)) = (input["sinceTime"].as_str(), input["untilTime"].as_str())
        else {
            return Err("Hourly usage requires sinceTime and untilTime".into());
        };
        let milliseconds = |value: &str| -> Result<i64, String> {
            let (seconds, fraction) = instant(value)?;
            let digits = format!("{fraction:0<3}");
            Ok(seconds * 1_000 + digits[..3].parse::<i64>().unwrap())
        };
        let duration = milliseconds(end)? - milliseconds(start)?;
        if duration <= 0 || duration > 86_400_000 {
            return Err(
                "Hourly usage window must be greater than zero and at most 24 hours".into(),
            );
        }
    }
    if let (Some(start), Some(end)) = (input["sinceTime"].as_str(), input["untilTime"].as_str()) {
        let (start_seconds, mut start_fraction) = instant(start)?;
        let (end_seconds, mut end_fraction) = instant(end)?;
        let precision = start_fraction.len().max(end_fraction.len());
        start_fraction.extend(std::iter::repeat_n('0', precision - start_fraction.len()));
        end_fraction.extend(std::iter::repeat_n('0', precision - end_fraction.len()));
        if (start_seconds, start_fraction) > (end_seconds, end_fraction) {
            return Err("untilTime must be on or after sinceTime".into());
        }
    }
    Ok(input)
}

pub fn parse_summary(value: Value) -> Result<Value, String> {
    object(
        &value,
        &[
            ("contractVersion", number),
            ("readAt", string),
            ("timeZone", nonempty),
            ("sinceDay", day),
            ("untilDay", day),
            ("buckets", buckets),
            ("sources", sources),
            ("pricing", pricing),
            ("scanDurationMs", count),
        ],
        &[],
    )
}

fn bucket(value: &Value) -> Result<Value, String> {
    object(
        value,
        &[
            ("day", day),
            ("provider", nonempty),
            ("model", nonempty),
            ("totals", totals),
            ("costUsd", number),
            ("cacheSavingsUsd", number),
            ("costSource", nonempty),
            ("records", count),
            ("unpricedRecords", count),
            ("sessions", count),
        ],
        &[
            ("hourStart", nonempty),
            ("categoryCostUsd", category_cost),
            ("fastCostUsd", number),
            ("ultrafastCostUsd", number),
            ("speedPremiumUsd", number),
        ],
    )
}

fn totals(value: &Value) -> Result<Value, String> {
    object(
        value,
        &[
            ("uncachedInputTokens", count),
            ("cachedInputTokens", count),
            ("cacheCreationTokens", count),
            ("outputTokens", count),
            ("reasoningTokens", count),
        ],
        &[],
    )
}

fn category_cost(value: &Value) -> Result<Value, String> {
    object(
        value,
        &[
            ("input", number),
            ("cacheRead", number),
            ("cacheWrite", number),
            ("output", number),
        ],
        &[],
    )
}

fn source(value: &Value) -> Result<Value, String> {
    let mut source = object(
        value,
        &[
            ("fingerprint", fingerprint),
            ("status", nonempty),
            ("scannedFiles", count),
            ("skippedFiles", count),
            ("malformedRecords", count),
            ("distinctSessions", count),
            ("message", message),
        ],
        &[("action", nonempty), ("refreshing", refreshing)],
    )?;
    let fingerprint = source
        .as_object_mut()
        .unwrap()
        .remove("fingerprint")
        .unwrap();
    let identity = json!([
        fingerprint["hostId"],
        fingerprint["provider"],
        fingerprint["resolvedHomePath"],
        fingerprint["volumeId"],
    ]);
    let digest = format!("{:x}", Sha256::digest(identity.to_string().as_bytes()));
    source["sourceRef"] = json!(&digest[..16]);
    source["provider"] = fingerprint["provider"].clone();
    Ok(source)
}

fn fingerprint(value: &Value) -> Result<Value, String> {
    object(
        value,
        &[
            ("hostId", nonempty),
            ("provider", nonempty),
            ("resolvedHomePath", nonempty),
            ("volumeId", string),
        ],
        &[],
    )
}

fn pricing(value: &Value) -> Result<Value, String> {
    object(
        value,
        &[
            ("status", pricing_status),
            ("source", nonempty),
            ("fetchedAt", nullable_string),
            ("knownModels", count),
        ],
        &[],
    )
}

fn object(
    value: &Value,
    required: &[(&str, Parser)],
    optional: &[(&str, Parser)],
) -> Result<Value, String> {
    let fields = value.as_object().ok_or("Expected an object")?;
    let mut output = Map::new();
    for (key, parse) in required {
        let field = fields.get(*key).ok_or_else(|| format!("Missing {key}"))?;
        output.insert(
            (*key).into(),
            parse(field).map_err(|error| format!("{key}: {error}"))?,
        );
    }
    for (key, parse) in optional {
        if let Some(field) = fields.get(*key) {
            output.insert(
                (*key).into(),
                parse(field).map_err(|error| format!("{key}: {error}"))?,
            );
        }
    }
    Ok(Value::Object(output))
}

fn array(value: &Value, parse: Parser) -> Result<Value, String> {
    let entries = value.as_array().ok_or("Expected an array")?;
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| parse(entry).map_err(|error| format!("[{index}]: {error}")))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

fn buckets(value: &Value) -> Result<Value, String> {
    array(value, bucket)
}

fn sources(value: &Value) -> Result<Value, String> {
    array(value, source)
}

fn string(value: &Value) -> Result<Value, String> {
    value
        .as_str()
        .ok_or("Expected a string")
        .map(|_| value.clone())
        .map_err(Into::into)
}

fn nonempty(value: &Value) -> Result<Value, String> {
    string(value)?;
    if value.as_str().unwrap().trim().is_empty() {
        return Err("Expected a nonempty string".into());
    }
    Ok(value.clone())
}

fn nullable_string(value: &Value) -> Result<Value, String> {
    if value.is_null() {
        Ok(Value::Null)
    } else {
        string(value)
    }
}

fn message(value: &Value) -> Result<Value, String> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    nonempty(value)?;
    Ok(json!(sanitize_failure_text(value.as_str().unwrap(), 2_000)))
}

fn number(value: &Value) -> Result<Value, String> {
    if value.as_f64().is_some_and(f64::is_finite) {
        Ok(value.clone())
    } else {
        Err("Expected a finite number".into())
    }
}

fn count(value: &Value) -> Result<Value, String> {
    number(value)?;
    let count = value.as_f64().unwrap();
    if !(0.0..=9_007_199_254_740_991.0).contains(&count) || count.fract() != 0.0 {
        return Err("Expected a nonnegative safe integer".into());
    }
    Ok(value.clone())
}

fn boolean(value: &Value) -> Result<Value, String> {
    value
        .as_bool()
        .ok_or("Expected a boolean")
        .map(|_| value.clone())
        .map_err(Into::into)
}

fn choice(value: &Value, allowed: &[&str]) -> Result<Value, String> {
    if value.as_str().is_some_and(|value| allowed.contains(&value)) {
        Ok(value.clone())
    } else {
        Err(format!("Expected one of {}", allowed.join(", ")))
    }
}

fn resolution(value: &Value) -> Result<Value, String> {
    choice(value, &["day", "hour"])
}

fn pricing_status(value: &Value) -> Result<Value, String> {
    choice(value, &["fresh", "cached", "unavailable"])
}

fn refreshing(value: &Value) -> Result<Value, String> {
    if value == &Value::Bool(true) {
        Ok(value.clone())
    } else {
        Err("Expected true".into())
    }
}

fn calendar_day(value: &str) -> Result<NaiveDate, String> {
    static SHAPE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$").unwrap());
    if !SHAPE.is_match(value) {
        return Err("Expected a real calendar date in YYYY-MM-DD format".into());
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| "Expected a real calendar date in YYYY-MM-DD format".into())
}

fn day(value: &Value) -> Result<Value, String> {
    string(value)?;
    calendar_day(value.as_str().unwrap())?;
    Ok(value.clone())
}

fn instant(value: &str) -> Result<(i64, String), String> {
    static SHAPE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
        r"^([0-9]{4}-[0-9]{2}-[0-9]{2})[Tt]((?:[01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9])(?:\.([0-9]+))?([Zz]|[+-](?:[01][0-9]|2[0-3]):[0-5][0-9])$"
    ).unwrap()
    });
    let captures = SHAPE
        .captures(value)
        .ok_or("Expected an RFC3339 timestamp with a timezone")?;
    calendar_day(&captures[1])?;
    let whole = format!(
        "{}T{}{}",
        &captures[1],
        &captures[2],
        captures[4].to_uppercase()
    );
    let parsed: DateTime<FixedOffset> = DateTime::parse_from_rfc3339(&whole)
        .map_err(|_| "Expected an RFC3339 timestamp with a timezone")?;
    Ok((
        parsed.timestamp(),
        captures.get(3).map_or("", |value| value.as_str()).into(),
    ))
}

fn timestamp(value: &Value) -> Result<Value, String> {
    string(value)?;
    instant(value.as_str().unwrap())?;
    Ok(value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> Value {
        json!({"sinceDay":"2026-10-01","untilDay":"2026-10-07","timeZone":"UTC"})
    }

    fn validate(value: Value) -> Result<Value, String> {
        validate_input(value.as_object().unwrap())
    }

    fn summary() -> Value {
        json!({
            "contractVersion":6,"readAt":"2026-10-23T12:00:00Z","timeZone":"UTC",
            "sinceDay":"2026-10-01","untilDay":"2026-10-07","scanDurationMs":17,"secret":"hidden-root",
            "buckets":[{
                "day":"2026-10-01","hourStart":"2026-10-01T01:00:00Z","provider":"codex","model":"vendor/model-v1",
                "sourcePath":"/home/max/.codex/private.jsonl",
                "totals":{"uncachedInputTokens":11,"cachedInputTokens":12,"cacheCreationTokens":13,"outputTokens":14,"reasoningTokens":5,"secret":"hidden-total"},
                "costUsd":0.125,"cacheSavingsUsd":0.025,
                "categoryCostUsd":{"input":0.01,"cacheRead":0.02,"cacheWrite":0.03,"output":0.065,"secret":"hidden-category"},
                "fastCostUsd":0.02,"ultrafastCostUsd":0.03,"speedPremiumUsd":0.04,
                "costSource":"providerReported","records":3,"unpricedRecords":1,"sessions":2,"secret":"hidden-bucket"
            }],
            "sources":[{
                "fingerprint":{"hostId":"host-a","provider":"codex","resolvedHomePath":"/home/max/.codex","volumeId":"7:42","secret":"hidden-fingerprint"},
                "status":"partial","scannedFiles":8,"skippedFiles":2,"malformedRecords":1,"distinctSessions":2,
                "message":"Cannot read /home/max/.codex/private.jsonl token=private-token at https://private.example/log",
                "action":"enableCursorKeychain","refreshing":true,"secret":"hidden-source"
            }],
            "pricing":{"status":"cached","source":"model catalog","fetchedAt":null,"knownModels":100,"secret":"hidden-pricing"}
        })
    }

    #[test]
    fn validates_calendar_dates_and_inclusive_windows() {
        for date in [
            "2026-02-29",
            "2026-02-30",
            "1900-02-29",
            "2026-04-31",
            "2026-00-01",
            "2026-13-01",
            "2026-10-00",
            "2026-1-01",
            " 2026-10-01",
        ] {
            let mut value = input();
            value["sinceDay"] = json!(date);
            assert!(validate(value).is_err(), "{date}");
        }
        for date in ["0000-02-29", "2000-02-29", "2024-02-29", "0099-12-31"] {
            assert!(
                validate(json!({"sinceDay":date,"untilDay":date,"timeZone":"UTC"})).is_ok(),
                "{date}"
            );
        }
        for (since, until, resolution, valid) in [
            ("2024-01-01", "2024-12-31", "day", true),
            ("2024-01-01", "2025-01-01", "day", false),
            ("2026-01-01", "2027-01-01", "day", true),
            ("2026-01-01", "2027-01-02", "day", false),
            ("2026-10-01", "2026-10-07", "hour", false),
            ("2026-10-01", "2026-10-08", "hour", false),
            ("2026-10-07", "2026-10-07", "hour", false),
            ("2026-10-08", "2026-10-07", "day", false),
        ] {
            assert_eq!(validate(json!({"sinceDay":since,"untilDay":until,"timeZone":"UTC","resolution":resolution})).is_ok(), valid, "{since} {until} {resolution}");
        }
        let mut value = input();
        value["timeZone"] = json!(" UTC ");
        value["extra"] = json!("stripped");
        assert_eq!(validate(value).unwrap(), input());
    }

    #[test]
    fn validates_hourly_bounds_like_the_usage_service() {
        for (start, end, valid) in [
            (None, None, false),
            (Some("2026-10-01T00:00:00Z"), None, false),
            (None, Some("2026-10-02T00:00:00Z"), false),
            (
                Some("2026-10-01T00:00:00Z"),
                Some("2026-10-02T00:00:00Z"),
                true,
            ),
            (
                Some("2026-10-01T02:00:00+02:00"),
                Some("2026-10-01T01:00:00Z"),
                true,
            ),
            (
                Some("2026-10-01T00:00:00Z"),
                Some("2026-10-01T00:00:00Z"),
                false,
            ),
            (
                Some("2026-10-01T00:00:00.0000000001Z"),
                Some("2026-10-01T00:00:00.0000000002Z"),
                false,
            ),
            (
                Some("2026-10-01T00:00:00Z"),
                Some("2026-10-02T00:00:00.001Z"),
                false,
            ),
        ] {
            let mut value = input();
            value["resolution"] = json!("hour");
            if let Some(start) = start {
                value["sinceTime"] = json!(start);
            }
            if let Some(end) = end {
                value["untilTime"] = json!(end);
            }
            assert_eq!(validate(value).is_ok(), valid, "{start:?} {end:?}");
        }
    }

    #[test]
    fn retains_future_history_tags() {
        let mut value = summary();
        value["buckets"][0]["costSource"] = json!("future-cost");
        value["sources"][0]["status"] = json!("future-status");
        value["sources"][0]["action"] = json!("future-action");
        let value = parse_summary(value).unwrap();
        assert_eq!(value["buckets"][0]["costSource"], "future-cost");
        assert_eq!(value["sources"][0]["status"], "future-status");
        assert_eq!(value["sources"][0]["action"], "future-action");
    }

    #[test]
    fn validates_timestamp_offsets_and_fractional_precision() {
        for (start, end, valid) in [
            ("2026-10-01T02:00:00+02:00", "2026-10-01T00:00:00Z", true),
            (
                "2026-10-01T00:00:00.0000000002Z",
                "2026-10-01T00:00:00.0000000001Z",
                false,
            ),
            ("2026-10-01t00:00:00.10z", "2026-10-01T00:00:00.1Z", true),
            ("2026-10-01T00:00:01Z", "2026-10-01T00:00:00Z", false),
        ] {
            let mut value = input();
            value["sinceTime"] = json!(start);
            value["untilTime"] = json!(end);
            assert_eq!(validate(value).is_ok(), valid, "{start} {end}");
        }
        for invalid in [
            "2026-02-30T00:00:00Z",
            "2026-10-01T00:00:00",
            "2026-10-01",
            "2026-10-01T24:00:00Z",
            "2026-10-01T01:60:00Z",
            "2026-10-01T01:00:00+24:00",
            "2026-10-01T00:00:00Z\n",
        ] {
            let mut value = input();
            value["sinceTime"] = json!(invalid);
            assert!(validate(value).is_err(), "{invalid}");
        }
        assert!(validate(json!({"sinceDay":"2026-10-23","untilDay":"2026-10-23","timeZone":"America/Los_Angeles","sinceTime":"2026-10-24T06:00:00Z"})).is_ok());
        for (key, invalid) in [
            ("timeZone", json!(" ")),
            ("timeZone", Value::Null),
            ("resolution", json!("week")),
            ("sinceTime", Value::Null),
            ("awaitRefresh", json!("true")),
        ] {
            let mut value = input();
            value[key] = invalid;
            assert!(validate(value).is_err(), "{key}");
        }
    }

    #[test]
    fn preserves_metrics_and_projects_the_same_source_identity_as_typescript() {
        let value = parse_summary(summary()).unwrap();
        assert_eq!(
            value["buckets"][0],
            json!({
                "day":"2026-10-01","hourStart":"2026-10-01T01:00:00Z","provider":"codex","model":"vendor/model-v1",
                "totals":{"uncachedInputTokens":11,"cachedInputTokens":12,"cacheCreationTokens":13,"outputTokens":14,"reasoningTokens":5},
                "costUsd":0.125,"cacheSavingsUsd":0.025,"categoryCostUsd":{"input":0.01,"cacheRead":0.02,"cacheWrite":0.03,"output":0.065},
                "fastCostUsd":0.02,"ultrafastCostUsd":0.03,"speedPremiumUsd":0.04,
                "costSource":"providerReported","records":3,"unpricedRecords":1,"sessions":2
            })
        );
        assert_eq!(
            value["sources"][0],
            json!({
                "sourceRef":"56295e100793dd16","provider":"codex","status":"partial",
                "scannedFiles":8,"skippedFiles":2,"malformedRecords":1,"distinctSessions":2,
                "message":"Cannot read [REDACTED PATH] token=[REDACTED] at [REDACTED URL]",
                "action":"enableCursorKeychain","refreshing":true
            })
        );
        assert_eq!(
            value["pricing"],
            json!({"status":"cached","source":"model catalog","fetchedAt":null,"knownModels":100})
        );
        assert_eq!(value["scanDurationMs"], 17);
        for private in [
            "hidden-",
            "/home/max",
            "volumeId",
            "fingerprint",
            "host-a",
            "sourcePath",
            "private-token",
            "private.example",
        ] {
            assert!(!value.to_string().contains(private), "{private}");
        }
        assert_eq!(parse_summary(summary()).unwrap(), value);
        for key in ["hostId", "provider", "resolvedHomePath", "volumeId"] {
            let mut changed = summary();
            changed["sources"][0]["fingerprint"][key] = json!("other");
            assert_ne!(
                parse_summary(changed).unwrap()["sources"][0]["sourceRef"],
                value["sources"][0]["sourceRef"]
            );
        }
        let mut future = summary();
        future["buckets"][0]["provider"] = json!("future-provider");
        future["sources"][0]["fingerprint"]["provider"] = json!("future-provider");
        let future = parse_summary(future).unwrap();
        assert_eq!(future["buckets"][0]["provider"], "future-provider");
        assert_eq!(future["sources"][0]["provider"], "future-provider");
    }

    #[test]
    fn rejects_arrays_at_every_object_boundary_and_invalid_metrics() {
        assert!(parse_summary(json!([])).is_err());
        for (path, invalid) in [
            ("/buckets", json!({})),
            ("/buckets/0", json!([])),
            ("/buckets/0/totals", json!([1, 2, 3, 4, 5])),
            ("/buckets/0/categoryCostUsd", json!([1, 2, 3, 4])),
            ("/buckets/0/totals/outputTokens", json!(-1)),
            ("/buckets/0/records", json!(0.5)),
            ("/buckets/0/sessions", json!(9_007_199_254_740_992_u64)),
            ("/buckets/0/costUsd", json!("0.1")),
            ("/buckets/0/model", json!(" ")),
            ("/buckets/0/provider", Value::Null),
            ("/buckets/0/costSource", json!(false)),
            ("/buckets/0/fastCostUsd", Value::Null),
            ("/sources", Value::Null),
            ("/sources/0", json!([])),
            (
                "/sources/0/fingerprint",
                json!(["host-a", "codex", "/home/max/.codex", "7:42"]),
            ),
            ("/sources/0/fingerprint/volumeId", Value::Null),
            ("/sources/0/status", json!("")),
            ("/sources/0/refreshing", json!(false)),
            ("/sources/0/action", json!(false)),
            ("/pricing", json!(["cached", "catalog", null, 100])),
            ("/pricing/knownModels", json!("100")),
            ("/pricing/fetchedAt", json!(false)),
            ("/scanDurationMs", json!(-1)),
        ] {
            let mut value = summary();
            *value.pointer_mut(path).unwrap() = invalid;
            assert!(parse_summary(value).is_err(), "{path}");
        }
        for key in summary().as_object().unwrap().keys() {
            if key == "secret" {
                continue;
            }
            let mut value = summary();
            value.as_object_mut().unwrap().remove(key);
            assert!(parse_summary(value).is_err(), "{key}");
        }
    }

    #[test]
    fn keeps_empty_histories_null_messages_and_absent_optional_fields() {
        let mut value = summary();
        let source = value["sources"][0].as_object_mut().unwrap();
        source.remove("action");
        source.remove("refreshing");
        source.insert("message".into(), Value::Null);
        let value = parse_summary(value).unwrap();
        assert!(value["sources"][0]["message"].is_null());
        assert!(value["sources"][0].get("action").is_none());
        assert!(value["sources"][0].get("refreshing").is_none());
        let mut value = summary();
        value["sources"] = json!([]);
        value["buckets"] = json!([]);
        assert!(parse_summary(value).is_ok());
    }
}
