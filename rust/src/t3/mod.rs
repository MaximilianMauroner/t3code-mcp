pub mod commands;
pub mod failure;
pub mod http_client;
pub mod models_usage;
pub mod native_mcp;
pub mod rpc_client;
pub mod thread_state;
pub mod types;
pub mod usage_history;
pub mod v2;

pub use crate::native_credential;
pub use http_client::T3HttpClient;
pub use native_mcp::NativeMcpClient;

fn timestamp(value: Option<&str>) -> Option<i64> {
    value.and_then(crate::operations::parse_timestamp)
}

fn iso(milliseconds: i64) -> String {
    chrono::DateTime::from_timestamp_millis(milliseconds)
        .expect("timestamp in date range")
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
pub(crate) mod test_server;
#[cfg(test)]
mod tests;
