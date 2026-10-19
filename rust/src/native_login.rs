use crate::native_credential::{NativeAccessLevel, NativeCredential};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Client, Response};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::{self, BufRead, IsTerminal, Write};
use url::Url;

// T3 returns the code in its decision response. No loopback listener is needed.
const REDIRECT_URI: &str = "http://127.0.0.1/callback";
const NO_PAIRING_CODE: &str = "No pairing code was supplied on stdin.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeLoginOptions {
    pub access: NativeAccessLevel,
}

pub struct NativeLoginInput<'a> {
    pub resource: &'a str,
    pub access: NativeAccessLevel,
    pub pairing_code: &'a str,
    pub now: Option<fn() -> i64>,
}

#[derive(Deserialize)]
struct ResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
}

#[derive(Deserialize)]
struct AuthorizationServerMetadata {
    issuer: String,
    registration_endpoint: String,
    token_endpoint: String,
}

#[derive(Deserialize)]
struct RegisteredClient {
    #[serde(deserialize_with = "nonempty_string")]
    client_id: String,
}

#[derive(Deserialize)]
struct DecisionRedirect {
    #[serde(rename = "redirectTo")]
    redirect_to: String,
}

#[derive(Deserialize)]
enum TokenType {
    Bearer,
}

#[derive(Deserialize)]
struct TokenResult {
    #[serde(deserialize_with = "nonempty_string")]
    access_token: String,
    #[serde(rename = "token_type")]
    _token_type: TokenType,
    #[serde(deserialize_with = "positive_number")]
    expires_in: f64,
    scope: String,
}

fn nonempty_string<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    if value.is_empty() {
        return Err(serde::de::Error::custom(
            "must contain at least one character",
        ));
    }
    Ok(value)
}

fn positive_number<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    let value = f64::deserialize(deserializer)?;
    if !value.is_finite() || value <= 0.0 {
        return Err(serde::de::Error::custom("must be a positive finite number"));
    }
    Ok(value)
}

pub fn parse_native_login_args(args: &[String]) -> Result<NativeLoginOptions, String> {
    let mut access = NativeAccessLevel::FullAccess;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--access" {
            access = args
                .next()
                .and_then(|value| serde_json::from_value(json!(value)).ok())
                .ok_or_else(|| {
                    "--access must be one of read-only, approval-required, auto-accept-edits, auto, full-access."
                        .to_owned()
                })?;
            continue;
        }
        if arg.starts_with("--code") {
            return Err("Do not pass the pairing code as an argument; process lists expose it. Pipe it on stdin or type it at the prompt.".into());
        }
        return Err(
            "Use native-login [--access <level>] and supply the pairing code on stdin.".into(),
        );
    }
    Ok(NativeLoginOptions { access })
}

pub fn read_pairing_code() -> Result<String, String> {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        return read_pairing_code_from(stdin.lock());
    }
    let mut stderr = io::stderr().lock();
    stderr
        .write_all(b"T3 pairing code (from `t3 auth pairing create`): ")
        .and_then(|()| stderr.flush())
        .map_err(|error| error.to_string())?;
    let result = (|| loop {
        let config = rpassword::ConfigBuilder::new().output_discard();
        // Read the same terminal as stdin, even when it differs from /dev/tty.
        #[cfg(unix)]
        let config = config.input_file_path("/dev/stdin");
        let line = match rpassword::read_password_with_config(config.build()) {
            Ok(line) => line,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(NO_PAIRING_CODE.to_owned());
            }
            Err(error) => return Err(error.to_string()),
        };
        let code = trim_pairing_code(&line);
        if !code.is_empty() {
            return Ok(code.to_owned());
        }
    })();
    let newline = writeln!(stderr).map_err(|error| error.to_string());
    match result {
        Ok(code) => newline.map(|()| code),
        Err(error) => Err(error),
    }
}

pub fn read_pairing_code_from(mut reader: impl BufRead) -> Result<String, String> {
    let mut line = Vec::new();
    loop {
        let buffer = reader.fill_buf().map_err(|error| error.to_string())?;
        if buffer.is_empty() {
            let decoded = String::from_utf8_lossy(&line);
            let code = trim_pairing_code(&decoded);
            return if code.is_empty() {
                Err(NO_PAIRING_CODE.to_owned())
            } else {
                Ok(code.to_owned())
            };
        }
        let end = buffer.iter().position(|byte| matches!(byte, b'\n' | b'\r'));
        let count = end.unwrap_or(buffer.len());
        line.extend_from_slice(&buffer[..count]);
        reader.consume(count + usize::from(end.is_some()));
        if end.is_some() {
            let decoded = String::from_utf8_lossy(&line);
            let code = trim_pairing_code(&decoded);
            if !code.is_empty() {
                return Ok(code.to_owned());
            }
            line.clear();
        }
    }
}

fn trim_pairing_code(value: &str) -> &str {
    value.trim_matches(|c: char| {
        matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}'
            | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
            | '\u{205f}' | '\u{3000}' | '\u{feff}')
    })
}

pub async fn login_native(input: NativeLoginInput<'_>) -> Result<NativeCredential, String> {
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::limited(20))
        .build()
        .map_err(|error| error.to_string())?;
    let resource_url = Url::parse(input.resource).map_err(|error| error.to_string())?;
    let discovery = Url::parse(&format!(
        "{}/.well-known/oauth-protected-resource{}",
        resource_url.origin().ascii_serialization(),
        resource_url.path()
    ))
    .map_err(|error| error.to_string())?;
    let metadata: ResourceMetadata = get_json(&client, discovery).await?;
    let issuer = metadata
        .authorization_servers
        .first()
        .ok_or("Native T3 advertised no authorization server.")?;
    let server_discovery = Url::parse(issuer)
        .and_then(|url| url.join("/.well-known/oauth-authorization-server"))
        .map_err(|error| error.to_string())?;
    let server: AuthorizationServerMetadata = get_json(&client, server_discovery).await?;
    let registered: RegisteredClient = post_json(
        &client,
        &server.registration_endpoint,
        json!({
            "client_name": "t3-code-mcp gateway",
            "redirect_uris": [REDIRECT_URI],
            "token_endpoint_auth_method": "none",
        }),
    )
    .await?;
    let verifier = random_urlsafe(32)?;
    let state = random_urlsafe(16)?;
    let decision_url = Url::parse(&server.issuer)
        .and_then(|url| url.join("/oauth/mcp/decision"))
        .map_err(|error| error.to_string())?;
    let decision: DecisionRedirect = post_json(
        &client,
        decision_url.as_str(),
        json!({
            "authorization": {
                "response_type": "code",
                "client_id": registered.client_id,
                "redirect_uri": REDIRECT_URI,
                "code_challenge": URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
                "code_challenge_method": "S256",
                "state": state,
                "resource": metadata.resource,
            },
            "decision": {"_tag": "pairing-code", "access": input.access, "code": input.pairing_code},
        }),
    )
    .await?;
    let redirect = Url::parse(&decision.redirect_to).map_err(|error| error.to_string())?;
    let query = |name: &str| {
        redirect
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    if query("state").as_deref() != Some(state.as_str()) {
        return Err("Native T3 returned a mismatched OAuth state.".into());
    }
    let code = query("code").ok_or_else(|| {
        format!(
            "Native T3 denied the login: {}.",
            query("error").as_deref().unwrap_or("no authorization code")
        )
    })?;
    let obtained_at = input
        .now
        .map_or_else(|| chrono::Utc::now().timestamp_millis(), |now| now());
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", registered.client_id.as_str()),
            ("code_verifier", verifier.as_str()),
            ("resource", metadata.resource.as_str()),
        ])
        .finish();
    let response = client
        .post(&server.token_endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let token: TokenResult = read_json(response, "token exchange").await?;
    Ok(NativeCredential {
        resource: metadata.resource,
        client_id: registered.client_id,
        access_token: token.access_token,
        access: input.access,
        scopes: token
            .scope
            .split(' ')
            .filter(|scope| !scope.is_empty())
            .map(str::to_owned)
            .collect(),
        obtained_at: iso_date(obtained_at as f64)?,
        expires_at: iso_date(obtained_at as f64 + token.expires_in * 1000.0)?,
    })
}

fn random_urlsafe(count: usize) -> Result<String, String> {
    let mut bytes = vec![0; count];
    getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

async fn get_json<T: DeserializeOwned>(client: &Client, url: Url) -> Result<T, String> {
    let response = client
        .get(url.clone())
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|error| error.to_string())?;
    read_json(response, url.path()).await
}

async fn post_json<T: DeserializeOwned>(
    client: &Client,
    url: &str,
    body: serde_json::Value,
) -> Result<T, String> {
    let response = client
        .post(url)
        .header("accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let url = Url::parse(url).map_err(|error| error.to_string())?;
    read_json(response, url.path()).await
}

async fn read_json<T: DeserializeOwned>(response: Response, step: &str) -> Result<T, String> {
    let status = response.status();
    let bytes = response.bytes().await.map_err(|error| error.to_string())?;
    let decoded = String::from_utf8_lossy(&bytes);
    let text = decoded.strip_prefix('\u{feff}').unwrap_or(&decoded);
    if !status.is_success() {
        let excerpt = String::from_utf16_lossy(&text.encode_utf16().take(300).collect::<Vec<_>>());
        return Err(format!(
            "Native T3 {step} failed with HTTP {}: {excerpt}",
            status.as_u16()
        ));
    }
    serde_json::from_str(text).map_err(|error| error.to_string())
}

fn iso_date(millis: f64) -> Result<String, String> {
    if !millis.is_finite() || millis.abs() > 8_640_000_000_000_000.0 {
        return Err("Invalid time value".into());
    }
    let millis = millis.trunc() as i64;
    let days = millis.div_euclid(86_400_000);
    let time = millis.rem_euclid(86_400_000);
    // The JavaScript Date range exceeds chrono's range. Use the Gregorian calendar directly.
    let shifted = days + 719468;
    let era = shifted.div_euclid(146097);
    let day_of_era = shifted - era * 146097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = month_index + if month_index < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        format!("{year:+07}")
    };
    Ok(format!(
        "{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        time / 3_600_000,
        time / 60_000 % 60,
        time / 1000 % 60,
        time % 1000
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::{
        collections::HashMap,
        io::{BufReader, Read},
        net::{TcpListener, TcpStream},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
        time::Duration,
    };

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_access_levels_and_uses_the_last_access_flag() {
        assert_eq!(
            parse_native_login_args(&[]).unwrap().access,
            NativeAccessLevel::FullAccess
        );
        for (name, expected) in [
            ("read-only", NativeAccessLevel::ReadOnly),
            ("approval-required", NativeAccessLevel::ApprovalRequired),
            ("auto-accept-edits", NativeAccessLevel::AutoAcceptEdits),
            ("auto", NativeAccessLevel::Auto),
            ("full-access", NativeAccessLevel::FullAccess),
        ] {
            assert_eq!(
                parse_native_login_args(&args(&["--access", name]))
                    .unwrap()
                    .access,
                expected
            );
        }
        assert_eq!(
            parse_native_login_args(&args(&["--access", "read-only", "--access", "auto"]))
                .unwrap()
                .access,
            NativeAccessLevel::Auto
        );
    }

    #[test]
    fn rejects_invalid_options_with_the_source_messages() {
        for input in [
            vec!["--access"],
            vec!["--access", "root"],
            vec!["--access", "--code=secret"],
            vec!["--access", " auto"],
        ] {
            assert_eq!(
                parse_native_login_args(&args(&input)).unwrap_err(),
                "--access must be one of read-only, approval-required, auto-accept-edits, auto, full-access."
            );
        }
        for input in ["--code", "--code=secret", "--codes", "--code-anything"] {
            assert_eq!(
                parse_native_login_args(&args(&[input])).unwrap_err(),
                "Do not pass the pairing code as an argument; process lists expose it. Pipe it on stdin or type it at the prompt."
            );
        }
        for input in ["--access=auto", "--help", "secret", ""] {
            assert_eq!(
                parse_native_login_args(&args(&[input])).unwrap_err(),
                "Use native-login [--access <level>] and supply the pairing code on stdin."
            );
        }
    }

    #[test]
    fn reads_the_first_nonblank_stdin_line_with_javascript_trimming() {
        for input in [
            "\n \t\r\n  PAIR-1234  \nignored\n",
            " \rPAIR-1234\rignored",
            "\u{feff}\u{00a0}PAIR-1234\u{3000}",
            "PAIR-1234",
        ] {
            assert_eq!(
                read_pairing_code_from(input.as_bytes()).unwrap(),
                "PAIR-1234"
            );
        }
        for input in ["", "\n\r \t\n", "\u{feff}\u{2003}"] {
            assert_eq!(
                read_pairing_code_from(input.as_bytes()).unwrap_err(),
                "No pairing code was supplied on stdin."
            );
        }
        assert_eq!(
            read_pairing_code_from("\u{0085}code\u{0085}".as_bytes()).unwrap(),
            "\u{0085}code\u{0085}"
        );
        assert_eq!(
            read_pairing_code_from(&b"  PAIR-\xff\n"[..]).unwrap(),
            "PAIR-\u{fffd}"
        );
    }

    #[test]
    fn stdin_reader_returns_at_a_carriage_return_without_waiting_for_eof() {
        struct LineThenError(bool);
        impl Read for LineThenError {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                unreachable!()
            }
        }
        impl BufRead for LineThenError {
            fn fill_buf(&mut self) -> io::Result<&[u8]> {
                if self.0 {
                    Err(io::Error::other("must not read another line"))
                } else {
                    Ok(b"PAIR\r")
                }
            }
            fn consume(&mut self, count: usize) {
                assert_eq!(count, 5);
                self.0 = true;
            }
        }
        assert_eq!(
            read_pairing_code_from(LineThenError(false)).unwrap(),
            "PAIR"
        );
    }

    #[derive(Clone)]
    struct Request {
        method: String,
        target: String,
        headers: HashMap<String, String>,
        body: String,
    }

    struct OAuthServer {
        base_url: String,
        requests: Arc<Mutex<Vec<Request>>>,
        stopped: Arc<AtomicBool>,
        task: Option<JoinHandle<()>>,
    }

    #[derive(Default)]
    struct ServerOptions {
        redirect: Option<&'static str>,
        failure: Option<(&'static str, u16, String)>,
        token: Option<Value>,
        registration: Option<Value>,
        resource_metadata: Option<Value>,
    }

    impl OAuthServer {
        fn start(options: ServerOptions) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let origin = base_url.clone();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let received = Arc::clone(&requests);
            let stopped = Arc::new(AtomicBool::new(false));
            let stop = Arc::clone(&stopped);
            let task = thread::spawn(move || {
                while let Ok((mut stream, _)) = listener.accept() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let request = read_request(&mut stream);
                    let path = request.target.split('?').next().unwrap();
                    let mut requests = received.lock().unwrap();
                    requests.push(request.clone());
                    let response = match &options.failure {
                        Some((failure_path, status, body)) if path == *failure_path => {
                            (*status, body.clone())
                        }
                        _ => oauth_response(&origin, &request, &requests, &options),
                    };
                    drop(requests);
                    write!(
                        stream,
                        "HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response.0,
                        response.1.len(),
                        response.1
                    )
                    .unwrap();
                }
            });
            Self {
                base_url,
                requests,
                stopped,
                task: Some(task),
            }
        }

        async fn login(&self, access: NativeAccessLevel) -> Result<NativeCredential, String> {
            login_native(NativeLoginInput {
                resource: &format!("{}/nested/mcp?ignored=1#fragment", self.base_url),
                access,
                pairing_code: "PAIR-1234",
                now: Some(|| 0),
            })
            .await
        }
    }

    impl Drop for OAuthServer {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::Release);
            let url = Url::parse(&self.base_url).unwrap();
            let _ = TcpStream::connect(("127.0.0.1", url.port().unwrap()));
            let joined = self.task.take().unwrap().join();
            if !thread::panicking() {
                joined.unwrap();
            }
        }
    }

    fn read_request(stream: &mut TcpStream) -> Request {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut words = line.split_whitespace();
        let method = words.next().unwrap().to_owned();
        let target = words.next().unwrap().to_owned();
        let mut headers = HashMap::new();
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            let (name, value) = line.split_once(':').unwrap();
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
        let count = headers
            .get("content-length")
            .map_or(0, |value| value.parse().unwrap());
        let mut body = vec![0; count];
        reader.read_exact(&mut body).unwrap();
        Request {
            method,
            target,
            headers,
            body: String::from_utf8(body).unwrap(),
        }
    }

    fn oauth_response(
        origin: &str,
        request: &Request,
        requests: &[Request],
        options: &ServerOptions,
    ) -> (u16, String) {
        let path = request.target.split('?').next().unwrap();
        let body = match path {
            "/.well-known/oauth-protected-resource/nested/mcp" => {
                options.resource_metadata.clone().unwrap_or_else(|| json!({
                    "resource": format!("{origin}/canonical/mcp"),
                    "authorization_servers": [format!("{origin}/issuer/base"), "http://127.0.0.1:9/ignored"],
                }))
            }
            "/.well-known/oauth-authorization-server" => json!({
                "issuer": format!("{origin}/another/issuer"),
                "registration_endpoint": format!("{origin}/registration?client=1"),
                "token_endpoint": format!("{origin}/token?client=1"),
            }),
            "/registration" => options.registration.clone().unwrap_or_else(|| json!({"client_id": "client-1"})),
            "/oauth/mcp/decision" => {
                let body: Value = serde_json::from_str(&request.body).unwrap();
                let state = body["authorization"]["state"].as_str().unwrap();
                let query = options.redirect
                    .map(|query| query.replace("$state", state))
                    .unwrap_or_else(|| format!("state={state}&code=oauth-code"));
                json!({"redirectTo": format!("{REDIRECT_URI}?{query}")})
            }
            "/token" => {
                let form: HashMap<_, _> = url::form_urlencoded::parse(request.body.as_bytes())
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect();
                let decision = requests.iter().find(|request| request.target == "/oauth/mcp/decision").unwrap();
                let decision: Value = serde_json::from_str(&decision.body).unwrap();
                assert_eq!(form["grant_type"], "authorization_code");
                assert_eq!(form["client_id"], "client-1");
                assert_eq!(form["redirect_uri"], REDIRECT_URI);
                assert_eq!(form["resource"], format!("{origin}/canonical/mcp"));
                assert_eq!(
                    decision["authorization"]["code_challenge"],
                    URL_SAFE_NO_PAD.encode(Sha256::digest(form["code_verifier"].as_bytes()))
                );
                assert_eq!(URL_SAFE_NO_PAD.decode(&form["code_verifier"]).unwrap().len(), 32);
                options.token.clone().unwrap_or_else(valid_token)
            }
            _ => panic!("unexpected OAuth request {} {}", request.method, request.target),
        };
        (200, body.to_string())
    }

    fn valid_token() -> Value {
        json!({
            "access_token": "native-token",
            "token_type": "Bearer",
            "expires_in": 1.2509,
            "scope": " orchestration:read  orchestration:operate\tcombined orchestration:read ",
        })
    }

    #[tokio::test]
    async fn obtains_a_credential_through_discovery_registration_pairing_and_pkce() {
        let server = OAuthServer::start(ServerOptions::default());
        let credential = server.login(NativeAccessLevel::Auto).await.unwrap();
        assert_eq!(
            credential.resource,
            format!("{}/canonical/mcp", server.base_url)
        );
        assert_eq!(credential.client_id, "client-1");
        assert_eq!(credential.access_token, "native-token");
        assert_eq!(credential.access, NativeAccessLevel::Auto);
        assert_eq!(
            credential.scopes,
            [
                "orchestration:read",
                "orchestration:operate\tcombined",
                "orchestration:read"
            ]
        );
        assert_eq!(credential.obtained_at, "1970-01-01T00:00:00.000Z");
        assert_eq!(credential.expires_at, "1970-01-01T00:00:01.250Z");
        let requests = server.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|request| request.target.as_str())
                .collect::<Vec<_>>(),
            [
                "/.well-known/oauth-protected-resource/nested/mcp",
                "/.well-known/oauth-authorization-server",
                "/registration?client=1",
                "/oauth/mcp/decision",
                "/token?client=1",
            ]
        );
        assert_eq!(
            requests
                .iter()
                .map(|request| request.method.as_str())
                .collect::<Vec<_>>(),
            ["GET", "GET", "POST", "POST", "POST"]
        );
        for request in &requests[..4] {
            assert_eq!(
                request.headers.get("accept").map(String::as_str),
                Some("application/json")
            );
            assert!(!request.headers.contains_key("authorization"));
        }
        let registration: Value = serde_json::from_str(&requests[2].body).unwrap();
        assert_eq!(
            registration,
            json!({
                "client_name": "t3-code-mcp gateway",
                "redirect_uris": ["http://127.0.0.1/callback"],
                "token_endpoint_auth_method": "none",
            })
        );
        let decision: Value = serde_json::from_str(&requests[3].body).unwrap();
        assert_eq!(
            decision["decision"],
            json!({"_tag": "pairing-code", "access": "auto", "code": "PAIR-1234"})
        );
        let authorization = &decision["authorization"];
        assert_eq!(authorization["response_type"], "code");
        assert_eq!(authorization["client_id"], "client-1");
        assert_eq!(authorization["redirect_uri"], "http://127.0.0.1/callback");
        assert_eq!(authorization["resource"], credential.resource);
        assert_eq!(authorization["code_challenge_method"], "S256");
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(authorization["state"].as_str().unwrap())
                .unwrap()
                .len(),
            16
        );
        assert_eq!(requests[2].headers["content-type"], "application/json");
        assert_eq!(requests[3].headers["content-type"], "application/json");
        assert_eq!(
            requests[4].headers["content-type"],
            "application/x-www-form-urlencoded"
        );
    }

    #[tokio::test]
    async fn refuses_denials_and_bad_states_before_exchanging_a_token() {
        for (redirect, expected) in [
            (
                "state=$state&error=access_denied",
                "Native T3 denied the login: access_denied.",
            ),
            (
                "state=$state",
                "Native T3 denied the login: no authorization code.",
            ),
            ("state=$state&error=", "Native T3 denied the login: ."),
            ("code=code", "Native T3 returned a mismatched OAuth state."),
            (
                "state=wrong&code=code",
                "Native T3 returned a mismatched OAuth state.",
            ),
            (
                "state=wrong&error=access_denied",
                "Native T3 returned a mismatched OAuth state.",
            ),
        ] {
            let server = OAuthServer::start(ServerOptions {
                redirect: Some(redirect),
                ..Default::default()
            });
            let error = server
                .login(NativeAccessLevel::ReadOnly)
                .await
                .err()
                .unwrap();
            assert_eq!(error, expected);
            assert_eq!(server.requests.lock().unwrap().len(), 4);
        }
    }

    #[tokio::test]
    async fn accepts_an_empty_authorization_code_and_the_first_query_value() {
        let server = OAuthServer::start(ServerOptions {
            redirect: Some("state=$state&state=wrong&code=&code=ignored&error=access_denied"),
            ..Default::default()
        });
        assert!(server.login(NativeAccessLevel::FullAccess).await.is_ok());
        let requests = server.requests.lock().unwrap();
        let form: HashMap<_, _> =
            url::form_urlencoded::parse(requests[4].body.as_bytes()).collect();
        assert_eq!(form.get("code").unwrap(), "");
    }

    #[tokio::test]
    async fn reports_http_status_path_and_a_bounded_body_for_every_step() {
        for (path, step) in [
            (
                "/.well-known/oauth-protected-resource/nested/mcp",
                "/.well-known/oauth-protected-resource/nested/mcp",
            ),
            (
                "/.well-known/oauth-authorization-server",
                "/.well-known/oauth-authorization-server",
            ),
            ("/registration", "/registration"),
            ("/oauth/mcp/decision", "/oauth/mcp/decision"),
            ("/token", "token exchange"),
        ] {
            let server = OAuthServer::start(ServerOptions {
                failure: Some((path, 403, "x".repeat(350))),
                ..Default::default()
            });
            let error = server
                .login(NativeAccessLevel::FullAccess)
                .await
                .err()
                .unwrap();
            assert_eq!(
                error,
                format!("Native T3 {step} failed with HTTP 403: {}", "x".repeat(300))
            );
        }
        let server = OAuthServer::start(ServerOptions {
            failure: Some(("/token", 400, "😀".repeat(200))),
            ..Default::default()
        });
        assert_eq!(
            server
                .login(NativeAccessLevel::FullAccess)
                .await
                .err()
                .unwrap(),
            format!(
                "Native T3 token exchange failed with HTTP 400: {}",
                "😀".repeat(150)
            )
        );
    }

    #[tokio::test]
    async fn rejects_invalid_token_fields_and_accepts_empty_scopes() {
        for (field, value) in [
            ("access_token", json!("")),
            ("token_type", json!("bearer")),
            ("token_type", json!("MAC")),
            ("expires_in", json!(0)),
            ("expires_in", json!(-1)),
            ("expires_in", json!("3600")),
            ("scope", json!(null)),
        ] {
            let mut token = valid_token();
            token[field] = value;
            let server = OAuthServer::start(ServerOptions {
                token: Some(token),
                ..Default::default()
            });
            assert!(
                server.login(NativeAccessLevel::FullAccess).await.is_err(),
                "{field}"
            );
        }
        let mut token = valid_token();
        token["scope"] = json!("");
        let server = OAuthServer::start(ServerOptions {
            token: Some(token),
            ..Default::default()
        });
        assert!(
            server
                .login(NativeAccessLevel::ReadOnly)
                .await
                .unwrap()
                .scopes
                .is_empty()
        );
    }

    #[tokio::test]
    async fn rejects_empty_client_ids_and_invalid_resource_metadata() {
        let server = OAuthServer::start(ServerOptions {
            registration: Some(json!({"client_id": ""})),
            ..Default::default()
        });
        assert!(server.login(NativeAccessLevel::Auto).await.is_err());
        assert_eq!(server.requests.lock().unwrap().len(), 3);
        let server = OAuthServer::start(ServerOptions {
            resource_metadata: Some(json!({"resource": "resource", "authorization_servers": []})),
            ..Default::default()
        });
        assert!(server.login(NativeAccessLevel::Auto).await.is_err());
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rejects_malformed_json_without_treating_it_as_an_http_error() {
        let server = OAuthServer::start(ServerOptions {
            failure: Some(("/token", 200, "not JSON".into())),
            ..Default::default()
        });
        let error = server
            .login(NativeAccessLevel::FullAccess)
            .await
            .err()
            .unwrap();
        assert!(!error.contains("failed with HTTP"));
        assert!(error.contains("line 1"));
    }

    #[test]
    fn formats_javascript_dates_across_the_supported_range() {
        for (millis, expected) in [
            (0.0, "1970-01-01T00:00:00.000Z"),
            (-0.9, "1970-01-01T00:00:00.000Z"),
            (-1.9, "1969-12-31T23:59:59.999Z"),
            (951782400000.0, "2000-02-29T00:00:00.000Z"),
            (-62167219200000.0, "0000-01-01T00:00:00.000Z"),
            (-62198755200000.0, "-000001-01-01T00:00:00.000Z"),
            (253402300800000.0, "+010000-01-01T00:00:00.000Z"),
            (8_640_000_000_000_000.0, "+275760-09-13T00:00:00.000Z"),
            (-8_640_000_000_000_000.0, "-271821-04-20T00:00:00.000Z"),
        ] {
            assert_eq!(iso_date(millis).unwrap(), expected);
        }
        for millis in [f64::NAN, f64::INFINITY, 8_640_000_000_000_001.0] {
            assert_eq!(iso_date(millis).unwrap_err(), "Invalid time value");
        }
    }
}
