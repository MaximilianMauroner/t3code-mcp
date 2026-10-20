use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "t3-setup-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn legacy_node_flag_still_runs_setup_with_the_rust_executable() {
    let fixture = Fixture::new();
    let workspace = &fixture.0;
    let out = fixture.0.join("out");
    let env = fixture.0.join("config/t3-code-mcp.env");
    fs::create_dir(fixture.0.join("config")).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_t3-code-mcp"))
        .args([
            "setup".to_owned(),
            format!("--workspace={}", workspace.display()),
            "--node=/usr/bin/node".to_owned(),
            "--tunnel-id=tunnel-test".to_owned(),
            "--environment-id=environment-test".to_owned(),
            format!("--out={}", out.display()),
            format!("--env-path={}", env.display()),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let baseline: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/node-compat-baseline.json"
    ))
    .unwrap();
    let output = String::from_utf8(result.stdout)
        .unwrap()
        .replace(fixture.0.to_str().unwrap(), "<SETUP_ROOT>");
    assert_eq!(output, baseline["cli"]["setup"]["stdout"].as_str().unwrap());
    let executable = fs::canonicalize(env!("CARGO_BIN_EXE_t3-code-mcp")).unwrap();
    let service = fs::read_to_string(out.join("t3-code-mcp.service")).unwrap();
    assert!(service.contains(&format!("ExecStart={} serve\n", executable.display())));
    assert!(!service.contains("/usr/bin/node"));
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(stderr.starts_with(
        "setup --node is ignored by the Rust runtime; use --executable to select the Rust binary.\n"
    ));
    assert!(stderr.ends_with(baseline["cli"]["setup"]["stderr"].as_str().unwrap()));
}
