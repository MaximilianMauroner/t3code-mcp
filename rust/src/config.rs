use std::{
    collections::HashMap,
    env, fmt,
    path::{Component, Path, PathBuf},
};

pub type Environment = HashMap<String, String>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeEndpointConfig {
    pub t3_http_base_url: String,
    pub t3_native_mcp_url: String,
    pub data_dir: PathBuf,
}

#[derive(Clone, PartialEq, Eq)]
pub struct GatewayConfig {
    pub endpoint: NativeEndpointConfig,
    pub t3_access_token: String,
    pub mcp_bearer_token: Option<String>,
    pub read_only: bool,
    pub host: String,
    pub port: u64,
    pub max_body_bytes: u64,
    pub environment_id: Option<String>,
    pub environment_label: Option<String>,
    pub worktree_root: Option<PathBuf>,
    pub stale_after_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ConfigError {}

pub fn load_native_endpoint_config() -> Result<NativeEndpointConfig, ConfigError> {
    load_native_endpoint_config_from(&env::vars().collect(), &current_dir()?)
}

pub fn load_config() -> Result<GatewayConfig, ConfigError> {
    load_config_from(&env::vars().collect(), &current_dir()?)
}

fn current_dir() -> Result<PathBuf, ConfigError> {
    env::current_dir().map_err(|error| ConfigError(error.to_string()))
}

pub fn load_native_endpoint_config_from(
    env: &Environment,
    cwd: &Path,
) -> Result<NativeEndpointConfig, ConfigError> {
    let base = env
        .get("T3_HTTP_BASE_URL")
        .map(String::as_str)
        .unwrap_or("http://127.0.0.1:3773");
    if js_trim(base).is_empty() {
        return Err(ConfigError("T3_HTTP_BASE_URL must be set.".into()));
    }
    let t3_http_base_url = http_url(js_trim(base), "T3_HTTP_BASE_URL")?;
    let t3_native_mcp_url = match optional(env, "T3_NATIVE_MCP_URL") {
        Some(value) => http_url(&value, "T3_NATIVE_MCP_URL")?,
        None => format!("{t3_http_base_url}/mcp"),
    };
    Ok(NativeEndpointConfig {
        t3_http_base_url,
        t3_native_mcp_url,
        data_dir: resolve(
            cwd,
            &optional(env, "T3_MCP_DATA_DIR").unwrap_or_else(|| "./data".into()),
        ),
    })
}

pub fn load_config_from(env: &Environment, cwd: &Path) -> Result<GatewayConfig, ConfigError> {
    let endpoint = load_native_endpoint_config_from(env, cwd)?;
    let t3_access_token = optional(env, "T3_ACCESS_TOKEN")
        .ok_or_else(|| ConfigError("T3_ACCESS_TOKEN must be set.".into()))?;
    Ok(GatewayConfig {
        endpoint,
        t3_access_token,
        mcp_bearer_token: optional(env, "MCP_BEARER_TOKEN"),
        read_only: boolean(env, "MCP_READ_ONLY", false)?,
        host: optional(env, "MCP_HOST").unwrap_or_else(|| "127.0.0.1".into()),
        port: integer(env, "MCP_PORT", 8787)?,
        max_body_bytes: integer(env, "MCP_MAX_BODY_BYTES", 2 * 1024 * 1024)?,
        environment_id: optional(env, "T3_ENVIRONMENT_ID"),
        environment_label: optional(env, "T3_ENVIRONMENT_LABEL"),
        worktree_root: optional(env, "T3_MCP_WORKTREE_ROOT").map(|value| resolve(cwd, &value)),
        stale_after_ms: integer(env, "T3_STALE_AFTER_MS", 30_000)?,
    })
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|character| matches!(character, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'))
}

fn optional(env: &Environment, name: &str) -> Option<String> {
    env.get(name)
        .map(|value| js_trim(value))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn integer(env: &Environment, name: &str, fallback: u64) -> Result<u64, ConfigError> {
    let Some(value) = optional(env, name) else {
        return Ok(fallback);
    };
    let radix = [("0x", 16), ("0b", 2), ("0o", 8)]
        .into_iter()
        .find(|(prefix, _)| value.to_ascii_lowercase().starts_with(prefix));
    let number = match radix {
        Some((_, radix)) => u64::from_str_radix(&value[2..], radix)
            .map(|number| number as f64)
            .ok(),
        None => value.parse::<f64>().ok(),
    };
    match number {
        Some(number)
            if number.is_finite()
                && (1.0..=9_007_199_254_740_991.0).contains(&number)
                && number.fract() == 0.0 =>
        {
            Ok(number as u64)
        }
        _ => Err(ConfigError(format!("{name} must be a positive integer."))),
    }
}

fn boolean(env: &Environment, name: &str, fallback: bool) -> Result<bool, ConfigError> {
    let Some(value) = env.get(name).filter(|value| !js_trim(value).is_empty()) else {
        return Ok(fallback);
    };
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" => Ok(true),
        "0" | "false" => Ok(false),
        _ => Err(ConfigError(format!("{name} must be true or false."))),
    }
}

fn resolve(cwd: &Path, value: &str) -> PathBuf {
    let mut result = PathBuf::new();
    for component in cwd.join(value).components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn http_url(value: &str, name: &str) -> Result<String, ConfigError> {
    let parsed =
        url::Url::parse(value).map_err(|_| ConfigError(format!("{name} must be a valid URL.")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ConfigError(format!("{name} must use http or https.")));
    }
    let mut normalized: String = parsed.into();
    if normalized.ends_with('/') {
        normalized.pop();
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn environment(values: &[(&str, &str)]) -> Environment {
        values
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect()
    }
    #[test]
    fn defaults_and_paths() {
        let config = load_config_from(
            &environment(&[
                ("T3_ACCESS_TOKEN", " token "),
                ("T3_MCP_DATA_DIR", "./a/../data"),
            ]),
            Path::new("/work"),
        )
        .unwrap();
        assert_eq!(
            config.endpoint.t3_native_mcp_url,
            "http://127.0.0.1:3773/mcp"
        );
        assert_eq!(config.endpoint.data_dir, Path::new("/work/data"));
        assert_eq!(config.port, 8787);
        assert_eq!(config.t3_access_token, "token");
        assert!(!config.read_only);
        assert_eq!(config.worktree_root, None);
    }
    #[test]
    fn javascript_numeric_and_boolean_rules() {
        for value in ["1e3", "0x3e8", "0b1111101000", "1000.0", "+1000"] {
            assert_eq!(
                load_config_from(
                    &environment(&[("T3_ACCESS_TOKEN", "t"), ("MCP_PORT", value)]),
                    Path::new("/work")
                )
                .unwrap()
                .port,
                1000
            );
        }
        for value in ["0", "1.5", "9007199254740992", "NaN"] {
            assert!(
                load_config_from(
                    &environment(&[("T3_ACCESS_TOKEN", "t"), ("MCP_PORT", value)]),
                    Path::new("/work")
                )
                .is_err()
            );
        }
        assert!(
            load_config_from(
                &environment(&[("T3_ACCESS_TOKEN", "t"), ("MCP_READ_ONLY", " true ")]),
                Path::new("/work")
            )
            .is_err()
        );
    }
    #[test]
    fn whatwg_url_normalization() {
        for (input, expected) in [
            ("https://例.test/", "https://xn--fsq.test"),
            ("http://0x7f.1/", "http://127.0.0.1"),
            ("http://2130706433/", "http://127.0.0.1"),
            ("http://0177.0.0.1/", "http://127.0.0.1"),
            (
                "https://EXAMPLE.com:443/a/%2e%2e/é?q=two words#é",
                "https://example.com/%C3%A9?q=two%20words#%C3%A9",
            ),
            (
                "https://u@ser:p a@example.com/",
                "https://u%40ser:p%20a@example.com",
            ),
            ("http://[0:0:0:0:0:0:0:1]:80/", "http://[::1]"),
            ("https://example.com/a/b/..", "https://example.com/a"),
            ("https://example.com/?q=/", "https://example.com/?q="),
        ] {
            let env = environment(&[("T3_HTTP_BASE_URL", input), ("T3_NATIVE_MCP_URL", input)]);
            let config = load_native_endpoint_config_from(&env, Path::new("/work")).unwrap();
            assert_eq!(config.t3_http_base_url, expected, "{input}");
            assert_eq!(config.t3_native_mcp_url, expected, "{input}");
        }
        for input in [
            "http://example.com:65536/",
            "http://[invalid]/",
            "http://a%00b/",
        ] {
            let error = load_native_endpoint_config_from(
                &environment(&[("T3_HTTP_BASE_URL", input)]),
                Path::new("/work"),
            )
            .unwrap_err();
            assert_eq!(error.0, "T3_HTTP_BASE_URL must be a valid URL.");
        }
    }

    #[test]
    fn endpoint_normalization() {
        let config = load_native_endpoint_config_from(
            &environment(&[
                ("T3_HTTP_BASE_URL", "HTTPS://Example.COM:443/a/../b/"),
                ("T3_NATIVE_MCP_URL", "https://other.test/mcp/?x=1#f"),
            ]),
            Path::new("/work"),
        )
        .unwrap();
        assert_eq!(config.t3_http_base_url, "https://example.com/b");
        assert_eq!(config.t3_native_mcp_url, "https://other.test/mcp/?x=1#f");
        assert!(
            load_native_endpoint_config_from(
                &environment(&[("T3_HTTP_BASE_URL", "")]),
                Path::new("/work")
            )
            .is_err()
        );
        assert!(
            load_native_endpoint_config_from(
                &environment(&[("T3_HTTP_BASE_URL", "file:///tmp")]),
                Path::new("/work")
            )
            .unwrap_err()
            .0
            .contains("http or https")
        );
    }
}
