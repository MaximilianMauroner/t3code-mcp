use std::{
    env,
    fs::{self, DirBuilder, OpenOptions},
    io::{self, Write},
    path::{Component, Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupOptions {
    pub workspace_dir: PathBuf,
    pub executable: PathBuf,
    pub tunnel_bin: String,
    pub tunnel_id: String,
    pub environment_id: String,
    pub environment_label: String,
    pub host: String,
    pub port: u64,
    pub tunnel_health_port: u64,
    pub data_dir: PathBuf,
    pub out_dir: PathBuf,
    pub env_path: PathBuf,
}

#[derive(Debug, serde::Serialize)]
pub struct SetupResult {
    pub files: Vec<PathBuf>,
}

pub fn parse_setup_args(argv: &[impl AsRef<str>]) -> io::Result<SetupOptions> {
    let get = |name: &str| {
        let prefix = format!("--{name}=");
        argv.iter()
            .find_map(|arg| arg.as_ref().strip_prefix(&prefix))
    };
    let cwd = env::current_dir()?;
    let workspace_dir = resolve(&cwd, Path::new(get("workspace").unwrap_or("")));
    let executable = match get("executable") {
        Some(value) => PathBuf::from(value),
        None => env::current_exe()?,
    };
    let required = |name| {
        get(name).map(str::to_owned).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("setup requires --{name}=<value>."),
            )
        })
    };
    let tunnel_id = required("tunnel-id")?;
    let environment_id = required("environment-id")?;
    let home = env::var_os("HOME").unwrap_or_else(|| "~".into());
    let home = PathBuf::from(home);
    Ok(SetupOptions {
        data_dir: get("data-dir")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace_dir.join("data")),
        out_dir: resolve(
            &cwd,
            &get("out")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config/systemd/user")),
        ),
        env_path: resolve(
            &cwd,
            &get("env-path")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config/t3-code-mcp.env")),
        ),
        workspace_dir,
        executable,
        tunnel_bin: get("tunnel-bin")
            .unwrap_or("/usr/local/bin/tunnel-client")
            .into(),
        tunnel_id,
        environment_id,
        environment_label: get("environment-label").unwrap_or("coding").into(),
        host: get("host").unwrap_or("127.0.0.1").into(),
        port: parse_port(get("port").unwrap_or("8787"), "port")?,
        tunnel_health_port: parse_port(
            get("tunnel-health-port").unwrap_or("8080"),
            "tunnel-health-port",
        )?,
    })
}

fn resolve(cwd: &Path, path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in cwd.join(path).components() {
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

fn parse_port(value: &str, name: &str) -> io::Result<u64> {
    let value = value.trim_matches(|c| matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'));
    let radix = [("0x", 16), ("0b", 2), ("0o", 8)]
        .into_iter()
        .find(|(prefix, _)| value.to_ascii_lowercase().starts_with(prefix));
    let number = match radix {
        Some((_, radix)) => {
            let digits = &value[2..];
            if digits.starts_with(['+', '-']) {
                None
            } else {
                u64::from_str_radix(digits, radix)
                    .ok()
                    .map(|number| number as f64)
            }
        }
        None => value.parse::<f64>().ok(),
    };
    match number {
        Some(number)
            if (1.0..=9_007_199_254_740_991.0).contains(&number) && number.fract() == 0.0 =>
        {
            Ok(number as u64)
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("setup --{name} must be a positive integer."),
        )),
    }
}

pub fn render_setup(options: &SetupOptions) -> io::Result<SetupResult> {
    for (name, value) in [
        ("port", options.port),
        ("tunnel-health-port", options.tunnel_health_port),
    ] {
        if !(1..=9_007_199_254_740_991).contains(&value) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("setup --{name} must be a positive integer."),
            ));
        }
    }
    let workspace = options.workspace_dir.display();
    let env_path = options.env_path.display();
    let gateway_service = format!(
        "[Unit]
Description=T3 Code MCP gateway
Wants=network-online.target
After=network-online.target
StartLimitIntervalSec=1min
StartLimitBurst=5

[Service]
Type=simple
WorkingDirectory={workspace}
EnvironmentFile={env_path}
ExecStart={} serve
Restart=on-failure
RestartSec=5s
TimeoutStopSec=10s
PrivateTmp=yes
ProtectSystem=full
ProtectKernelTunables=yes
ProtectControlGroups=yes
RestrictSUIDSGID=yes
NoNewPrivileges=yes
UMask=0077

[Install]
WantedBy=default.target
",
        options.executable.display()
    );
    let tunnel_service = format!(
        "[Unit]
Description=OpenAI Secure MCP Tunnel for T3 Code
Requires=t3-code-mcp.service
Wants=network-online.target
After=t3-code-mcp.service network-online.target
StartLimitIntervalSec=1min
StartLimitBurst=5

[Service]
Type=simple
WorkingDirectory={workspace}
EnvironmentFile={env_path}
ExecStart={}
Restart=on-failure
RestartSec=10s
TimeoutStopSec=20s
PrivateTmp=yes
ProtectSystem=full
ProtectKernelTunables=yes
ProtectControlGroups=yes
RestrictSUIDSGID=yes
NoNewPrivileges=yes
UMask=0077

[Install]
WantedBy=default.target
",
        options
            .workspace_dir
            .join("deploy/systemd/t3-code-mcp-tunnel.sh.generated")
            .display()
    );
    let tunnel_launcher = format!(
        r##"#!/bin/sh

set -eu

: "${{MCP_BEARER_TOKEN:?MCP_BEARER_TOKEN must be set}}"
: "${{CONTROL_PLANE_API_KEY:?CONTROL_PLANE_API_KEY must be set}}"

# tunnel-client resolves an entire header value from an env: reference. Build
# the scheme-qualified value at launch so it stays synchronized with the
# gateway token without putting the token in ExecStart or a second secret file.
MCP_AUTH_HEADER="Bearer ${{MCP_BEARER_TOKEN}}"
export MCP_AUTH_HEADER

attempt=0
while [ "$attempt" -lt 60 ]; do
  if /usr/bin/curl --silent --fail --max-time 2 http://{host}:{port}/healthz >/dev/null 2>&1; then
    break
  fi
  attempt=$((attempt + 1))
  /usr/bin/sleep 1
done
if [ "$attempt" -ge 60 ]; then
  echo "gateway health check did not become ready" >&2
  exit 1
fi

exec {tunnel_bin} run \
  --control-plane.tunnel-id={tunnel_id} \
  --control-plane.api-key=env:CONTROL_PLANE_API_KEY \
  --mcp.server-url=url=http://{host}:{port}/mcp,channel=main \
  '--mcp.extra-headers=Authorization: env:MCP_AUTH_HEADER' \
  '--mcp.discovery-extra-headers=Authorization: env:MCP_AUTH_HEADER' \
  --health.listen-addr={host}:{health_port}
"##,
        host = options.host,
        port = options.port,
        tunnel_bin = options.tunnel_bin,
        tunnel_id = options.tunnel_id,
        health_port = options.tunnel_health_port,
    );
    let env_template = format!(
        "# Generated by `t3-code-mcp setup`. Keep credentials server-side with mode 600.
# Never commit this file or paste tokens into voice prompts.

T3_HTTP_BASE_URL=http://127.0.0.1:3773
T3_ENVIRONMENT_ID={}
T3_ENVIRONMENT_LABEL={}
T3_ACCESS_TOKEN=

# A separate, randomly generated gateway token. Do not reuse T3_ACCESS_TOKEN.
MCP_BEARER_TOKEN=

# Start read-only for the first connection check; false enables control tools.
MCP_READ_ONLY=true
MCP_TRANSPORT=http
MCP_HOST={}
MCP_PORT={}
# This directory also receives audit.jsonl, the append-only redacted usage trail,
# and native-mcp-credential.json from `t3-code-mcp native-login`.
T3_MCP_DATA_DIR={}
# Optional absolute root for gateway-created isolated Git worktrees.
T3_MCP_WORKTREE_ROOT=

# OpenAI Secure MCP Tunnel runtime key. This is not an admin key.
CONTROL_PLANE_API_KEY=
",
        options.environment_id,
        options.environment_label,
        options.host,
        options.port,
        options.data_dir.display(),
    );

    let mut out_dir = DirBuilder::new();
    out_dir.recursive(true);
    #[cfg(unix)]
    out_dir.mode(0o700);
    out_dir.create(&options.out_dir)?;
    let gateway_path = options.out_dir.join("t3-code-mcp.service");
    let tunnel_path = options.out_dir.join("t3-code-mcp-tunnel.service");
    let launcher_dir = options.workspace_dir.join("deploy/systemd");
    fs::create_dir_all(&launcher_dir)?;
    let launcher_path = launcher_dir.join("t3-code-mcp-tunnel.sh.generated");
    write_file(&gateway_path, &gateway_service, 0o644)?;
    write_file(&tunnel_path, &tunnel_service, 0o644)?;
    write_file(&launcher_path, &tunnel_launcher, 0o755)?;
    let mut files = vec![gateway_path, tunnel_path, launcher_path];
    let mut env_options = OpenOptions::new();
    env_options.write(true).create_new(true);
    #[cfg(unix)]
    env_options.mode(0o600);
    match env_options.open(&options.env_path) {
        Ok(mut file) => {
            file.write_all(env_template.as_bytes())?;
            #[cfg(unix)]
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            files.push(options.env_path.clone());
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    Ok(SetupResult { files })
}

fn write_file(path: &Path, content: &str, mode: u32) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(mode);
    #[cfg(not(unix))]
    let _ = mode;
    options.open(path)?.write_all(content.as_bytes())
}
