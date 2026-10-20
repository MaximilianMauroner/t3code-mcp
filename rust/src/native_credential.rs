use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::{
    fs::{self, Metadata, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const NATIVE_CREDENTIAL_FILE: &str = "native-mcp-credential.json";
pub const NATIVE_ACCESS_LEVELS: [NativeAccessLevel; 5] = [
    NativeAccessLevel::ReadOnly,
    NativeAccessLevel::ApprovalRequired,
    NativeAccessLevel::AutoAcceptEdits,
    NativeAccessLevel::Auto,
    NativeAccessLevel::FullAccess,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeAccessLevel {
    ReadOnly,
    ApprovalRequired,
    AutoAcceptEdits,
    Auto,
    FullAccess,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeCredential {
    #[serde(deserialize_with = "nonempty")]
    pub resource: String,
    #[serde(deserialize_with = "nonempty")]
    pub client_id: String,
    #[serde(deserialize_with = "nonempty")]
    pub access_token: String,
    pub access: NativeAccessLevel,
    pub scopes: Vec<String>,
    pub obtained_at: String,
    pub expires_at: String,
}

fn nonempty<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    if value.is_empty() {
        return Err(serde::de::Error::custom(
            "must contain at least one character",
        ));
    }
    Ok(value)
}

#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum NativeCredentialState {
    Missing { reason: String },
    Expired { credential: NativeCredential },
    Valid { credential: NativeCredential },
}

#[derive(Debug, PartialEq, Eq)]
struct FileIdentity {
    modified: Option<SystemTime>,
    size: u64,
    #[cfg(unix)]
    inode: u64,
}
impl From<&Metadata> for FileIdentity {
    fn from(info: &Metadata) -> Self {
        Self {
            modified: info.modified().ok(),
            size: info.len(),
            #[cfg(unix)]
            inode: {
                use std::os::unix::fs::MetadataExt;
                info.ino()
            },
        }
    }
}

pub struct NativeCredentialStore {
    pub file_path: PathBuf,
    cached: Option<(FileIdentity, NativeCredential)>,
}

impl NativeCredentialStore {
    pub fn new(file_path: impl Into<PathBuf>) -> Self {
        Self {
            file_path: file_path.into(),
            cached: None,
        }
    }
    pub fn in_data_dir(data_dir: impl AsRef<Path>) -> Self {
        Self::new(data_dir.as_ref().join(NATIVE_CREDENTIAL_FILE))
    }
    pub fn read(&mut self) -> NativeCredentialState {
        self.read_at(now_millis())
    }
    pub fn read_at(&mut self, now_ms: i64) -> NativeCredentialState {
        let identity = match fs::metadata(&self.file_path) {
            Ok(info) => FileIdentity::from(&info),
            Err(error) => {
                self.cached = None;
                return NativeCredentialState::Missing {
                    reason: if error.kind() == io::ErrorKind::NotFound {
                        "No native MCP credential. Run `t3-code-mcp native-login` with a T3 pairing code.".into()
                    } else {
                        unreadable_reason(&error)
                    },
                };
            }
        };
        if self.cached.as_ref().is_none_or(|(key, _)| key != &identity) {
            let text = match fs::read(&self.file_path) {
                Ok(text) => text,
                Err(error) => {
                    self.cached = None;
                    return NativeCredentialState::Missing {
                        reason: unreadable_reason(&error),
                    };
                }
            };
            let credential = match serde_json::from_str::<NativeCredential>(
                &String::from_utf8_lossy(&text),
            ) {
                Ok(credential) => credential,
                Err(_) => {
                    self.cached = None;
                    return NativeCredentialState::Missing { reason: "The native MCP credential file is invalid. Run `t3-code-mcp native-login` again.".into() };
                }
            };
            self.cached = Some((identity, credential));
        }
        let credential = self
            .cached
            .as_ref()
            .expect("credential was loaded")
            .1
            .clone();
        if parse_date_millis(&credential.expires_at).is_some_and(|expires| expires <= now_ms) {
            NativeCredentialState::Expired { credential }
        } else {
            NativeCredentialState::Valid { credential }
        }
    }

    pub fn write(&self, credential: &NativeCredential) -> io::Result<()> {
        let parent = self
            .file_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut directory = fs::DirBuilder::new();
        directory.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        directory.create(parent)?;
        let mut temporary = self.file_path.as_os_str().to_owned();
        temporary.push(format!(".{}.tmp", std::process::id()));
        let temporary = PathBuf::from(temporary);
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        serde_json::to_writer_pretty(&mut file, credential)?;
        file.write_all(b"\n")?;
        drop(file);
        fs::rename(temporary, &self.file_path)
    }
}

fn unreadable_reason(error: &io::Error) -> String {
    let code = match error.raw_os_error() {
        Some(1) => "EPERM",
        Some(2) => "ENOENT",
        Some(5) => "EIO",
        Some(13) => "EACCES",
        Some(20) => "ENOTDIR",
        Some(21) => "EISDIR",
        Some(24) => "EMFILE",
        Some(40) => "ELOOP",
        _ => "unknown error",
    };
    format!(
        "The native MCP credential file cannot be read ({code}). Check the owner and mode of the file and its directory, or run `t3-code-mcp native-login` as the gateway user."
    )
}

fn now_millis() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i64,
        Err(error) => -(error.duration().as_millis() as i64),
    }
}

fn parse_date_millis(value: &str) -> Option<i64> {
    crate::operations::parse_timestamp(value)
}

pub fn token_identity(token: &str) -> String {
    let digest = sha2::Sha256::digest(token.as_bytes());
    format!("{digest:x}")[..16].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "t3-credential-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )))
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn credential(token: &str) -> NativeCredential {
        NativeCredential {
            resource: "http://localhost/mcp".into(),
            client_id: "client".into(),
            access_token: token.into(),
            access: NativeAccessLevel::Auto,
            scopes: vec!["mcp".into()],
            obtained_at: "2026-01-01T00:00:00.000Z".into(),
            expires_at: "2026-01-02T00:00:00.000Z".into(),
        }
    }
    #[test]
    fn renewal_expiry_and_removal() {
        let dir = Temp::new();
        let mut store = NativeCredentialStore::in_data_dir(&dir.0);
        assert!(matches!(
            store.read_at(0),
            NativeCredentialState::Missing { .. }
        ));
        let first = credential("one");
        store.write(&first).unwrap();
        assert!(
            matches!(store.read_at(0), NativeCredentialState::Valid { credential } if credential == first)
        );
        assert!(matches!(
            store.read_at(1767312000000),
            NativeCredentialState::Expired { .. }
        ));
        #[cfg(unix)]
        let previous_modified = fs::metadata(&store.file_path).unwrap().modified().unwrap();
        let second = credential("two");
        store.write(&second).unwrap();
        #[cfg(unix)]
        fs::File::open(&store.file_path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(previous_modified))
            .unwrap();
        assert!(
            matches!(store.read_at(0), NativeCredentialState::Valid { credential } if credential == second)
        );
        fs::remove_file(&store.file_path).unwrap();
        assert!(matches!(
            store.read_at(0),
            NativeCredentialState::Missing { .. }
        ));
    }
    #[test]
    fn invalid_files_and_unparseable_expiry() {
        let dir = Temp::new();
        let mut store = NativeCredentialStore::in_data_dir(&dir.0);
        let mut value = credential("secret");
        value.expires_at = "not a date".into();
        store.write(&value).unwrap();
        assert!(matches!(
            store.read_at(i64::MAX),
            NativeCredentialState::Valid { .. }
        ));
        fs::write(&store.file_path, b"{bad json").unwrap();
        assert!(
            matches!(store.read_at(0), NativeCredentialState::Missing { reason } if reason.contains("invalid"))
        );
        fs::remove_file(&store.file_path).unwrap();
        fs::create_dir(&store.file_path).unwrap();
        assert!(
            matches!(store.read_at(0), NativeCredentialState::Missing { reason } if reason.contains("EISDIR"))
        );
    }
    #[test]
    fn schema_and_hash_compatibility() {
        let mut value = serde_json::to_value(credential("secret")).unwrap();
        value["extra"] = "ignored".into();
        assert!(serde_json::from_value::<NativeCredential>(value.clone()).is_ok());
        value["accessToken"] = "".into();
        assert!(serde_json::from_value::<NativeCredential>(value).is_err());
        assert_eq!(token_identity(""), "e3b0c44298fc1c14");
        assert_eq!(token_identity("abc"), "ba7816bf8f01cfea");
        assert_eq!(token_identity(&"a".repeat(1000)), "41edece42d63e8d9");
    }
    fn assert_expiry(expires_at: &str, timestamp: i64) {
        let dir = Temp::new();
        let mut store = NativeCredentialStore::in_data_dir(&dir.0);
        let mut value = credential("token");
        value.expires_at = expires_at.into();
        store.write(&value).unwrap();
        assert!(
            matches!(
                store.read_at(timestamp - 1),
                NativeCredentialState::Valid { .. }
            ),
            "{expires_at} before expiry"
        );
        assert!(
            matches!(
                store.read_at(timestamp),
                NativeCredentialState::Expired { .. }
            ),
            "{expires_at} at expiry"
        );
        assert!(
            matches!(
                store.read_at(timestamp + 1),
                NativeCredentialState::Expired { .. }
            ),
            "{expires_at} after expiry"
        );
    }

    #[test]
    fn extended_year_and_legacy_expiry_at_fixed_timestamps() {
        for (date, timestamp) in [
            ("-000001-01-01T00:00:00.000Z", -62198755200000),
            ("+010000-01-01T00:00:00.000Z", 253402300800000),
            ("Thu, 01 Jan 1970 00:00:00 GMT", 0),
            (
                "Wed Dec 31 1969 19:00:00 GMT-0500 (Eastern Standard Time)",
                0,
            ),
            ("2026-01-02T00:00:00+05:30", 1767292200000),
        ] {
            assert_expiry(date, timestamp);
        }
        let dir = Temp::new();
        let mut store = NativeCredentialStore::in_data_dir(&dir.0);
        for date in [
            "-000000-01-01T00:00:00.000Z",
            "2026-01-02T00:00:00.Z",
            "not a date",
        ] {
            let mut value = credential("token");
            value.expires_at = date.into();
            store.write(&value).unwrap();
            assert!(
                matches!(store.read_at(i64::MAX), NativeCredentialState::Valid { .. }),
                "{date}"
            );
        }
    }

    #[test]
    fn expiry_uses_process_timezone() {
        if let Ok(zone) = std::env::var("T3_CREDENTIAL_TEST_TIMEZONE") {
            let local_offset = if zone == "America/New_York" {
                5 * 60 * 60 * 1000
            } else {
                0
            };
            for date in ["2026-01-02T00:00:00", "01/02/2026", "January 2, 2026"] {
                assert_expiry(date, 1767312000000 + local_offset);
            }
            assert_expiry("2026-01-02", 1767312000000);
            if zone == "America/New_York" {
                assert_expiry("2026-07-02T00:00:00", 1782964800000);
                assert_expiry("2026-03-08T02:30:00", 1772955000000);
                assert_expiry("2026-11-01T01:30:00", 1793511000000);
            }
            return;
        }
        for zone in ["UTC", "America/New_York"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "native_credential::tests::expiry_uses_process_timezone",
                    "--nocapture",
                ])
                .env("TZ", zone)
                .env("T3_CREDENTIAL_TEST_TIMEZONE", zone)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{zone}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn iso_expiry_offsets() {
        assert_eq!(parse_date_millis("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(parse_date_millis("1970-01-01"), Some(0));
        assert_eq!(parse_date_millis("1970-01-01T00:00:00.123Z"), Some(123));
        assert_eq!(
            parse_date_millis("1970-01-01T24:00:00.000Z"),
            Some(86400000)
        );
    }
    #[cfg(unix)]
    #[test]
    fn private_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Temp::new();
        let store = NativeCredentialStore::in_data_dir(&dir.0);
        store.write(&credential("secret")).unwrap();
        assert_eq!(
            fs::metadata(&dir.0).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&store.file_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&store.file_path, fs::Permissions::from_mode(0o644)).unwrap();
        store.write(&credential("renewed")).unwrap();
        assert_eq!(
            fs::metadata(&store.file_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            fs::read_to_string(&store.file_path)
                .unwrap()
                .ends_with('\n')
        );
    }
}
