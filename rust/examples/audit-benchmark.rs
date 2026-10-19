use serde_json::{Value, json};
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use t3_code_mcp::operations::audit_log::{AuditLog, AuditQueryInput, AuditSource};
use t3_code_mcp::operations::usage_summary::{UsageWindow, summarize_usage};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const EVENTS: u64 = 131_072;
const CURSOR: &str = "65535";
const TIMESTAMP: &str = "2026-09-30T00:00:00Z";

fn fixture_event() -> Value {
    json!({
        "version": 1, "eventId": "benchmark", "timestamp": TIMESTAMP, "processId": 1,
        "source": "mcp", "event": "tool.call", "operation": "t3_thread_get",
        "details": {"padding": "x".repeat(400)},
    })
}

fn fixture_row() -> Result<Vec<u8>> {
    let mut row = serde_json::to_vec(&fixture_event())?;
    row.push(b'\n');
    Ok(row)
}

struct Fixture(PathBuf);

impl Fixture {
    fn create() -> io::Result<Self> {
        let directory =
            std::env::temp_dir().join(format!("t3-audit-benchmark-{}", uuid::Uuid::new_v4()));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&directory)?;
        Ok(Self(directory))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn require(condition: bool, message: &str) -> Result<()> {
    if !condition {
        return Err(message.into());
    }
    Ok(())
}

fn child_report(path: &Path) -> Result<Value> {
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--child")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let result = (|| {
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() >= Duration::from_secs(60) {
                return Err("audit scan child exceeded 60 seconds".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        require(
            status.success(),
            &format!("audit scan child failed with {status}"),
        )?;
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .ok_or("audit scan child has no stdout")?
            .take(16_385)
            .read_to_end(&mut output)?;
        require(
            output.len() <= 16_384,
            "audit scan child report is too large",
        )?;
        let report: Value = serde_json::from_slice(&output)?;
        require(
            report["scansCompleted"] == 2,
            "audit scans did not complete",
        )?;
        Ok(report)
    })();
    if result.is_err() {
        let _ = child.kill();
        child.wait()?;
    }
    result
}

fn parent() -> Result<()> {
    let fixture = Fixture::create()?;
    let result = (|| {
        let path = fixture.0.join("audit.jsonl");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = BufWriter::new(options.open(&path)?);
        let row = fixture_row()?;
        for _ in 0..EVENTS {
            output.write_all(&row)?;
        }
        output.flush()?;
        drop(output);
        require(
            fs::metadata(&path)?.len() == row.len() as u64 * EVENTS,
            "generated fixture byte count differs",
        )?;
        child_report(&path)
    })();
    fs::remove_dir_all(&fixture.0)
        .map_err(|error| format!("fixture cleanup failed: {error}; scan result: {result:?}"))?;
    let mut report = result?;
    report["fixtureCleanup"] = json!("completed");
    println!("{report}");
    Ok(())
}

#[cfg(target_os = "linux")]
fn enforce_limit() -> Result<Value> {
    let limit = libc::rlimit {
        rlim_cur: 32 * 1024 * 1024,
        rlim_max: 32 * 1024 * 1024,
    };
    // Both pointers refer to valid rlimit storage for the duration of each call.
    if unsafe { libc::setrlimit(libc::RLIMIT_DATA, &limit) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let mut actual = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    if unsafe { libc::getrlimit(libc::RLIMIT_DATA, actual.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    // A successful getrlimit initialized the entire value.
    let actual = unsafe { actual.assume_init() };
    require(
        actual.rlim_cur == limit.rlim_cur && actual.rlim_max == limit.rlim_max,
        "RLIMIT_DATA readback differs from 32 MiB",
    )?;
    Ok(json!({
        "name": "RLIMIT_DATA", "enforced": true,
        "softBytes": actual.rlim_cur, "hardBytes": actual.rlim_max,
        "scope": "Linux process data limit, not a JS heap or RSS limit",
    }))
}

#[cfg(not(target_os = "linux"))]
fn enforce_limit() -> Result<Value> {
    Ok(json!({
        "name": "RLIMIT_DATA", "enforced": false,
        "softBytes": null, "hardBytes": null,
        "reason": "enforcement unavailable on this platform",
    }))
}

#[cfg(target_os = "linux")]
fn peak_memory() -> Result<Value> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // getrusage writes to valid storage, which is read only after success.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let usage = unsafe { usage.assume_init() };
    let bytes = u64::try_from(usage.ru_maxrss)?
        .checked_mul(1024)
        .ok_or("peak RSS byte count overflow")?;
    require(bytes > 0, "getrusage returned no peak RSS measurement")?;
    Ok(json!({
        "peakRssBytes": bytes, "method": "getrusage(RUSAGE_SELF).ru_maxrss * 1024",
        "scope": "scan child process lifetime, including startup",
    }))
}

#[cfg(not(target_os = "linux"))]
fn peak_memory() -> Result<Value> {
    Ok(json!({"peakRssBytes": null, "reason": "peak measurement unavailable on this platform"}))
}

fn scan_child(path: &Path) -> Result<()> {
    let limit = enforce_limit()?;
    let bytes = fs::metadata(path)?.len();
    require(
        bytes == fixture_row()?.len() as u64 * EVENTS,
        "scan input byte count differs from the synthetic fixture",
    )?;
    let log = AuditLog::new(path);
    let query = AuditQueryInput {
        source: Some(AuditSource::Mcp),
        cursor: Some(CURSOR.into()),
        limit: 1,
        ..AuditQueryInput::default()
    };
    let started = Instant::now();
    let page = log.query(&query)?;
    let query_ms = started.elapsed().as_secs_f64() * 1000.0;
    let summary_started = Instant::now();
    let summary = summarize_usage(path, &UsageWindow::default())?;
    let summary_ms = summary_started.elapsed().as_secs_f64() * 1000.0;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;

    require(
        log.diagnostic_error().is_none(),
        "audit query recorded an I/O error",
    )?;
    require(page.total == EVENTS, "audit query total differs")?;
    require(page.items.len() == 1, "audit query must retain one event")?;
    require(
        page.next_cursor.as_deref() == Some("65536") && page.has_more,
        "audit query next cursor or hasMore differs",
    )?;
    require(page.invalid_lines == 0, "audit query found invalid lines")?;
    require(
        serde_json::to_value(&page.items[0])? == fixture_event(),
        "audit query returned a different event",
    )?;
    require(
        summary["scan"]
            == json!({
                "bytes": bytes, "validLines": EVENTS, "includedEvents": EVENTS, "invalidLines": 0,
            }),
        "usage summary scan did not consume the complete fixture",
    )?;
    require(
        summary["denominators"]["toolCalls"] == EVENTS
            && summary["tools"]
                .as_object()
                .is_some_and(|tools| tools.len() == 1)
            && summary["tools"]["t3_thread_get"]["calls"] == EVENTS
            && summary["tools"]["t3_thread_get"]["completed"] == 0
            && summary["tools"]["t3_thread_get"]["errors"] == 0,
        "usage summary tool counts differ",
    )?;
    let peak = peak_memory()?;
    println!(
        "{}",
        json!({
            "bytes": bytes, "validEvents": page.total, "retainedPageEvents": page.items.len(),
            "cursor": CURSOR, "nextCursor": page.next_cursor, "hasMore": page.has_more,
            "queryInvalidLines": page.invalid_lines, "summaryScan": summary["scan"],
            "toolCalls": summary["denominators"]["toolCalls"], "scansCompleted": 2,
            "elapsedMs": elapsed_ms, "queryElapsedMs": query_ms, "summaryElapsedMs": summary_ms,
            "timingScope": "query and summary calls; excludes fixture generation and validation",
            "memoryLimit": limit, "memory": peak,
        })
    );
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [] => parent(),
        [mode, path] if mode == "--child" => scan_child(Path::new(path)),
        _ => Err("usage: audit-benchmark (the --child mode is internal)".into()),
    }
}
