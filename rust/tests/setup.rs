#[path = "../src/setup.rs"]
mod setup;

use setup::{SetupOptions, parse_setup_args, render_setup};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = env::temp_dir().join(format!(
            "t3-setup-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn options(&self) -> SetupOptions {
        parse_setup_args(&[
            format!("--workspace={}", self.0.join("workspace").display()),
            format!("--out={}", self.0.join("units").display()),
            format!("--env-path={}", self.0.join("gateway.env").display()),
            "--executable=/opt/t3-code-mcp".into(),
            "--tunnel-id=tunnel_test123".into(),
            "--environment-id=env-test-1".into(),
        ])
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn defaults_use_current_executable_and_keep_existing_cli_defaults() {
    let options = parse_setup_args(&[
        "--node=/usr/bin/node",
        "--tunnel-id=tunnel",
        "--environment-id=environment",
    ])
    .unwrap();
    let cwd = env::current_dir().unwrap();
    let home = env::var_os("HOME").unwrap_or_else(|| "~".into());
    let home = cwd.join(home);
    assert_eq!(options.workspace_dir, cwd);
    assert_eq!(options.executable, env::current_exe().unwrap());
    assert_eq!(options.tunnel_bin, "/usr/local/bin/tunnel-client");
    assert_eq!(options.environment_label, "coding");
    assert_eq!(options.host, "127.0.0.1");
    assert_eq!(options.port, 8787);
    assert_eq!(options.tunnel_health_port, 8080);
    assert_eq!(options.data_dir, cwd.join("data"));
    assert_eq!(options.out_dir, home.join(".config/systemd/user"));
    assert_eq!(options.env_path, home.join(".config/t3-code-mcp.env"));
}

#[test]
fn arguments_keep_first_match_relative_paths_and_empty_values() {
    let options = parse_setup_args(&[
        "--workspace=missing/../project",
        "--workspace=/ignored",
        "--node=/usr/bin/node",
        "--executable=bin/custom-runtime",
        "--tunnel-bin=bin/tunnel",
        "--tunnel-id=",
        "--environment-id=",
        "--environment-label=",
        "--host=localhost",
        "--port=9e3",
        "--tunnel-health-port=0x2000",
        "--data-dir=relative/../data",
        "--out=units/../out",
        "--env-path=secret/../gateway.env",
        "--ignored=value",
    ])
    .unwrap();
    let cwd = env::current_dir().unwrap();
    assert_eq!(options.workspace_dir, cwd.join("project"));
    assert_eq!(options.executable, Path::new("bin/custom-runtime"));
    assert_eq!(options.tunnel_bin, "bin/tunnel");
    assert_eq!(options.tunnel_id, "");
    assert_eq!(options.environment_id, "");
    assert_eq!(options.environment_label, "");
    assert_eq!(options.host, "localhost");
    assert_eq!(options.port, 9000);
    assert_eq!(options.tunnel_health_port, 8192);
    assert_eq!(options.data_dir, Path::new("relative/../data"));
    assert_eq!(options.out_dir, cwd.join("out"));
    assert_eq!(options.env_path, cwd.join("gateway.env"));
}

#[test]
fn requires_both_explicit_ids_with_equals_spelling() {
    for (args, missing) in [
        (vec!["--workspace=/tmp/x"], "tunnel-id"),
        (vec!["--tunnel-id=tunnel"], "environment-id"),
        (
            vec!["--tunnel-id", "tunnel", "--environment-id=environment"],
            "tunnel-id",
        ),
    ] {
        let error = parse_setup_args(&args).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            format!("setup requires --{missing}=<value>.")
        );
    }
}

#[test]
fn validates_positive_safe_integers_without_imposing_a_tcp_port_limit() {
    for flag in ["port", "tunnel-health-port"] {
        for value in [
            "",
            "0",
            "-1",
            "1.5",
            "NaN",
            "Infinity",
            "no",
            "9007199254740992",
            "0x+1",
            "\u{0085}1",
        ] {
            let error = parse_setup_args(&[
                "--tunnel-id=tunnel".into(),
                "--environment-id=environment".into(),
                format!("--{flag}={value}"),
            ])
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("setup --{flag} must be a positive integer.")
            );
        }
    }
    for (value, expected) in [
        ("65536", 65536),
        ("9007199254740991", 9_007_199_254_740_991),
        ("  +8787.0  ", 8787),
        ("0b100", 4),
        ("0o10", 8),
    ] {
        let options = parse_setup_args(&[
            "--tunnel-id=tunnel".into(),
            "--environment-id=environment".into(),
            format!("--port={value}"),
        ])
        .unwrap();
        assert_eq!(options.port, expected);
    }
}

#[test]
fn renders_services_launcher_and_secret_free_template_with_ordered_json() {
    let fixture = Fixture::new();
    let mut options = fixture.options();
    options.host = "localhost".into();
    options.port = 9001;
    options.tunnel_health_port = 9002;
    options.environment_label = "development".into();
    options.data_dir = "relative-data".into();
    let result = render_setup(&options).unwrap();
    let gateway = options.out_dir.join("t3-code-mcp.service");
    let tunnel = options.out_dir.join("t3-code-mcp-tunnel.service");
    let launcher = options
        .workspace_dir
        .join("deploy/systemd/t3-code-mcp-tunnel.sh.generated");
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        serde_json::json!({"files": [gateway, tunnel, launcher, options.env_path]})
    );
    let workspace = options.workspace_dir.display();
    let env_path = options.env_path.display();
    assert_eq!(
        fs::read_to_string(&gateway).unwrap(),
        format!(
            "[Unit]\nDescription=T3 Code MCP gateway\nWants=network-online.target\nAfter=network-online.target\nStartLimitIntervalSec=1min\nStartLimitBurst=5\n\n[Service]\nType=simple\nWorkingDirectory={workspace}\nEnvironmentFile={env_path}\nExecStart=/opt/t3-code-mcp serve\nRestart=on-failure\nRestartSec=5s\nTimeoutStopSec=10s\nPrivateTmp=yes\nProtectSystem=full\nProtectKernelTunables=yes\nProtectControlGroups=yes\nRestrictSUIDSGID=yes\nNoNewPrivileges=yes\nUMask=0077\n\n[Install]\nWantedBy=default.target\n"
        )
    );
    assert_eq!(
        fs::read_to_string(&tunnel).unwrap(),
        format!(
            "[Unit]\nDescription=OpenAI Secure MCP Tunnel for T3 Code\nRequires=t3-code-mcp.service\nWants=network-online.target\nAfter=t3-code-mcp.service network-online.target\nStartLimitIntervalSec=1min\nStartLimitBurst=5\n\n[Service]\nType=simple\nWorkingDirectory={workspace}\nEnvironmentFile={env_path}\nExecStart={workspace}/deploy/systemd/t3-code-mcp-tunnel.sh.generated\nRestart=on-failure\nRestartSec=10s\nTimeoutStopSec=20s\nPrivateTmp=yes\nProtectSystem=full\nProtectKernelTunables=yes\nProtectControlGroups=yes\nRestrictSUIDSGID=yes\nNoNewPrivileges=yes\nUMask=0077\n\n[Install]\nWantedBy=default.target\n"
        )
    );
    let script = fs::read_to_string(&launcher).unwrap();
    for line in [
        "#!/bin/sh",
        "set -eu",
        ": \"${MCP_BEARER_TOKEN:?MCP_BEARER_TOKEN must be set}\"",
        ": \"${CONTROL_PLANE_API_KEY:?CONTROL_PLANE_API_KEY must be set}\"",
        "MCP_AUTH_HEADER=\"Bearer ${MCP_BEARER_TOKEN}\"",
        "export MCP_AUTH_HEADER",
        "while [ \"$attempt\" -lt 60 ]; do",
        "  if /usr/bin/curl --silent --fail --max-time 2 http://localhost:9001/healthz >/dev/null 2>&1; then",
        "if [ \"$attempt\" -ge 60 ]; then",
        "exec /usr/local/bin/tunnel-client run \\",
        "  --control-plane.tunnel-id=tunnel_test123 \\",
        "  --control-plane.api-key=env:CONTROL_PLANE_API_KEY \\",
        "  --mcp.server-url=url=http://localhost:9001/mcp,channel=main \\",
        "  '--mcp.extra-headers=Authorization: env:MCP_AUTH_HEADER' \\",
        "  '--mcp.discovery-extra-headers=Authorization: env:MCP_AUTH_HEADER' \\",
        "  --health.listen-addr=localhost:9002",
    ] {
        assert!(script.lines().any(|actual| actual == line), "{line}");
    }
    let template = fs::read_to_string(&options.env_path).unwrap();
    let assignments: Vec<_> = template
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    assert_eq!(
        assignments,
        [
            "T3_HTTP_BASE_URL=http://127.0.0.1:3773",
            "T3_ENVIRONMENT_ID=env-test-1",
            "T3_ENVIRONMENT_LABEL=development",
            "T3_ACCESS_TOKEN=",
            "MCP_BEARER_TOKEN=",
            "MCP_READ_ONLY=true",
            "MCP_TRANSPORT=http",
            "MCP_HOST=localhost",
            "MCP_PORT=9001",
            "T3_MCP_DATA_DIR=relative-data",
            "T3_MCP_WORKTREE_ROOT=",
            "CONTROL_PLANE_API_KEY=",
        ]
    );
    assert!(!template.contains("tunnel_test123"));
    assert!(!template.contains("node dist/cli.js"));
    assert!(
        !options
            .workspace_dir
            .join("deploy/systemd/t3-code-mcp-tunnel.sh")
            .exists()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for (path, mode) in [
            (&options.out_dir, 0o700),
            (&gateway, 0o644),
            (&tunnel, 0o644),
            (&launcher, 0o755),
            (&options.env_path, 0o600),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                mode
            );
        }
    }
}

#[test]
fn rerender_preserves_existing_credentials_and_env_mode_but_updates_services() {
    let fixture = Fixture::new();
    let mut options = fixture.options();
    render_setup(&options).unwrap();
    let secrets =
        "T3_ACCESS_TOKEN=secret123\nMCP_BEARER_TOKEN=private456\nCONTROL_PLANE_API_KEY=key789\n";
    fs::write(&options.env_path, secrets).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&options.env_path, fs::Permissions::from_mode(0o640)).unwrap();
    }
    options.executable = "/opt/updated-runtime".into();
    let result = render_setup(&options).unwrap();
    assert_eq!(fs::read_to_string(&options.env_path).unwrap(), secrets);
    assert_eq!(result.files.len(), 3);
    assert!(!result.files.contains(&options.env_path));
    assert_eq!(
        serde_json::to_value(&result).unwrap()["files"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let service = fs::read_to_string(&result.files[0]).unwrap();
    assert!(service.contains("ExecStart=/opt/updated-runtime serve\n"));
    for path in &result.files {
        let content = fs::read_to_string(path).unwrap();
        for secret in ["secret123", "private456", "key789"] {
            assert!(!content.contains(secret));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&options.env_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }
}

#[test]
fn reports_io_errors_and_validates_options_before_writing() {
    let fixture = Fixture::new();
    for health_port in [false, true] {
        let mut options = fixture.options();
        if health_port {
            options.tunnel_health_port = 0;
        } else {
            options.port = 9_007_199_254_740_992;
        }
        assert_eq!(
            render_setup(&options).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(!options.out_dir.exists());
        assert!(!options.workspace_dir.exists());
    }
    let mut options = fixture.options();
    options.env_path = fixture.0.join("missing-parent/gateway.env");
    assert_eq!(
        render_setup(&options).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    fs::write(options.out_dir.join("t3-code-mcp.service"), "old service").unwrap();
    options.out_dir = fixture.0.join("not-a-directory");
    fs::write(&options.out_dir, "file").unwrap();
    assert!(render_setup(&options).is_err());
}

#[cfg(unix)]
#[test]
fn existing_env_symlink_and_its_target_are_untouched() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let options = fixture.options();
    let target = fixture.0.join("operator-secret");
    fs::write(&target, "secret").unwrap();
    symlink(&target, &options.env_path).unwrap();
    let result = render_setup(&options).unwrap();
    assert_eq!(fs::read_link(&options.env_path).unwrap(), target);
    assert_eq!(fs::read_to_string(&target).unwrap(), "secret");
    assert!(!result.files.contains(&options.env_path));
}

#[cfg(unix)]
#[test]
fn rendered_unit_launcher_checks_health_and_passes_auth_by_environment() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        os::unix::fs::PermissionsExt,
        process::Command,
        thread,
        time::{Duration, Instant},
    };
    let fixture = Fixture::new();
    let mut options = fixture.options();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    options.port = u64::from(listener.local_addr().unwrap().port());
    let fake_tunnel = fixture.0.join("tunnel");
    fs::write(
        &fake_tunnel,
        "#!/bin/sh\nprintf '%s\\n' \"$MCP_AUTH_HEADER\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_tunnel, fs::Permissions::from_mode(0o755)).unwrap();
    options.tunnel_bin = fake_tunnel.to_str().unwrap().into();
    render_setup(&options).unwrap();
    let unit = fs::read_to_string(options.out_dir.join("t3-code-mcp-tunnel.service")).unwrap();
    let launcher = unit
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart="))
        .expect("tunnel unit has an ExecStart launcher");
    assert!(
        Command::new("/bin/sh")
            .arg("-n")
            .arg(launcher)
            .status()
            .unwrap()
            .success()
    );
    for bearer in [None, Some("gateway-secret")] {
        let mut command = Command::new(launcher);
        command.env_clear();
        if let Some(token) = bearer {
            command.env("MCP_BEARER_TOKEN", token);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        let missing = if bearer.is_some() {
            "CONTROL_PLANE_API_KEY"
        } else {
            "MCP_BEARER_TOKEN"
        };
        assert!(String::from_utf8(output.stderr).unwrap().contains(missing));
    }
    let health = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut buffer = [0; 4096];
                    let size = stream.read(&mut buffer).unwrap();
                    assert!(
                        std::str::from_utf8(&buffer[..size])
                            .unwrap()
                            .starts_with("GET /healthz ")
                    );
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "launcher did not check gateway health"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        }
    });
    let output = Command::new(launcher)
        .env_clear()
        .env("MCP_BEARER_TOKEN", "gateway-secret")
        .env("CONTROL_PLANE_API_KEY", "control-secret")
        .output()
        .unwrap();
    health.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "Bearer gateway-secret\nrun\n--control-plane.tunnel-id=tunnel_test123\n--control-plane.api-key=env:CONTROL_PLANE_API_KEY\n--mcp.server-url=url=http://127.0.0.1:{}/mcp,channel=main\n--mcp.extra-headers=Authorization: env:MCP_AUTH_HEADER\n--mcp.discovery-extra-headers=Authorization: env:MCP_AUTH_HEADER\n--health.listen-addr=127.0.0.1:8080\n",
            options.port
        )
    );
}
