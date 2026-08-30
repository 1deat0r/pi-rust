#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Real-PTY authentication coverage for the Rust interactive binary.
//!
//! This deliberately exercises the user-facing `/login openai-codex` path,
//! including its browser URL/manual fallback and cancellation, before proving
//! that a request without a credential produces actionable auth guidance.

#[cfg(unix)]
mod unix {
    use base64::Engine as _;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    fn test_binary() -> PathBuf {
        std::env::var_os("PI_RUST_TEST_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pi")))
    }

    struct MockAuthServer {
        address: std::net::SocketAddr,
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        join: Option<thread::JoinHandle<()>>,
    }

    impl MockAuthServer {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = stop.clone();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let requests_thread = requests.clone();
            let access_token = fake_access_token("pty-account");
            let join = thread::spawn(move || {
                while !stop_thread.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let mut buffer = [0_u8; 16 * 1024];
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                            let read = stream.read(&mut buffer).unwrap_or(0);
                            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                            requests_thread.lock().unwrap().push(request.clone());
                            let (status, content_type, body) = if request
                                .starts_with("POST /oauth/token ")
                            {
                                (
                                    "200 OK",
                                    "application/json",
                                    serde_json::json!({
                                        "access_token": access_token,
                                        "refresh_token": "pty-refresh",
                                        "expires_in": 3600,
                                    })
                                    .to_string(),
                                )
                            } else if request.starts_with("POST /api/accounts/deviceauth/usercode ")
                            {
                                (
                                    "200 OK",
                                    "application/json",
                                    serde_json::json!({
                                        "device_auth_id": "pty-device",
                                        "user_code": "ABCD-EFGH",
                                        "interval": 0,
                                    })
                                    .to_string(),
                                )
                            } else if request.starts_with("POST /api/accounts/deviceauth/token ") {
                                (
                                    "200 OK",
                                    "application/json",
                                    serde_json::json!({
                                        "authorization_code": "pty-device-code",
                                        "code_verifier": "pty-device-verifier",
                                    })
                                    .to_string(),
                                )
                            } else if request.starts_with("POST /backend-api/codex/responses ") {
                                (
                                        "200 OK",
                                        "text/event-stream",
                                        concat!(
                                            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\",\"id\":\"msg_pty\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\n",
                                            "data: {\"type\":\"response.content_part.added\",\"part\":{\"type\":\"output_text\",\"text\":\"\"}}\n\n",
                                            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"fixture response\"}\n\n",
                                            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"id\":\"msg_pty\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"fixture response\"}]}}\n\n",
                                            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3,\"total_tokens\":8}}}\n\n",
                                        )
                                        .to_string(),
                                    )
                            } else {
                                ("404 Not Found", "application/json", "{}".to_string())
                            };
                            let response = format!(
                                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = stream.write_all(response.as_bytes());
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                address,
                base_url: format!("http://{address}"),
                requests,
                stop,
                join: Some(join),
            }
        }
    }

    impl Drop for MockAuthServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(self.address);
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    struct MockLlamaRouter {
        address: std::net::SocketAddr,
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        join: Option<thread::JoinHandle<()>>,
    }

    impl MockLlamaRouter {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = stop.clone();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let requests_thread = requests.clone();
            let join = thread::spawn(move || {
                while !stop_thread.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let mut buffer = [0_u8; 16 * 1024];
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                            let read = stream.read(&mut buffer).unwrap_or(0);
                            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                            requests_thread.lock().unwrap().push(request.clone());
                            let (status, body) = if request.starts_with("GET /models ")
                                || request.starts_with("GET /models HTTP/")
                            {
                                (
                                    "200 OK",
                                    serde_json::json!({
                                        "data": [{
                                            "id": "router-model.gguf",
                                            "status": {"value": "loaded"},
                                            "aliases": [],
                                            "architecture": {
                                                "input_modalities": ["text"],
                                                "output_modalities": ["text"]
                                            },
                                            "meta": {"n_ctx": 4096}
                                        }]
                                    })
                                    .to_string(),
                                )
                            } else {
                                ("404 Not Found", "{}".to_string())
                            };
                            let response = format!(
                                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = stream.write_all(response.as_bytes());
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                address,
                base_url: format!("http://{address}"),
                requests,
                stop,
                join: Some(join),
            }
        }
    }

    impl Drop for MockLlamaRouter {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(self.address);
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    fn fake_access_token(account_id: &str) -> String {
        let encode = |value: serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
        };
        format!(
            "{}.{}.signature",
            encode(serde_json::json!({"alg": "none"})),
            encode(serde_json::json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
            }))
        )
    }

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn send_http_get(url: &str) {
        let parsed = url::Url::parse(url).unwrap();
        let host = parsed.host_str().unwrap();
        let port = parsed.port_or_known_default().unwrap();
        let path = if parsed.path().is_empty() {
            "/"
        } else {
            parsed.path()
        };
        let query = parsed
            .query()
            .map(|query| format!("?{query}"))
            .unwrap_or_default();
        let mut stream = TcpStream::connect((host, port)).unwrap();
        let request =
            format!("GET {path}{query} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "OAuth callback request {url} response: {response}"
        );
    }

    struct Sandbox {
        root: PathBuf,
        home: PathBuf,
        agent_dir: PathBuf,
        project: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("pi-interactive-auth-{}", uuid::Uuid::new_v4()));
            let home = root.join("home");
            let agent_dir = home.join(".pi").join("agent");
            let project = root.join("project");
            fs::create_dir_all(&agent_dir).unwrap();
            fs::create_dir_all(&project).unwrap();
            Self {
                root,
                home,
                agent_dir,
                project,
            }
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    struct TmuxSession {
        name: String,
    }

    impl TmuxSession {
        fn start(sandbox: &Sandbox) -> Self {
            Self::start_with_extra_env(sandbox, "")
        }

        fn start_with_extra_env(sandbox: &Sandbox, extra_env: &str) -> Self {
            let name = format!("pi-interactive-auth-{}", uuid::Uuid::new_v4());
            let created = tmux(&[
                "new-session",
                "-d",
                "-x",
                "110",
                "-y",
                "32",
                "-c",
                sandbox.project.to_str().unwrap(),
                "-s",
                &name,
                "tail",
                "-f",
                "/dev/null",
            ]);
            assert!(created.status.success(), "tmux start: {}", stderr(&created));
            let command = format!(
                "env HOME={} PI_CODING_AGENT_DIR={} PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 PI_OAUTH_NO_BROWSER=1 {} {} --approve --provider openai-codex --model gpt-5.5; exec tail -f /dev/null",
                shell_quote(&sandbox.home),
                shell_quote(&sandbox.agent_dir),
                extra_env,
                shell_quote(&test_binary()),
            );
            let configured = tmux(&["set-option", "-t", &name, "remain-on-exit", "on"]);
            assert!(
                configured.status.success(),
                "tmux remain-on-exit: {}",
                stderr(&configured)
            );
            let started = tmux(&["respawn-pane", "-k", "-t", &name, &command]);
            assert!(
                started.status.success(),
                "tmux launch: {}",
                stderr(&started)
            );
            Self { name }
        }

        fn capture(&self) -> String {
            let output = tmux(&["capture-pane", "-p", "-t", &self.name]);
            assert!(output.status.success(), "tmux capture: {}", stderr(&output));
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        fn wait_for<F>(&self, mut predicate: F) -> String
        where
            F: FnMut(&str) -> bool,
        {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let capture = self.capture();
                if predicate(&capture) {
                    return capture;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for TUI output; last capture:\n{capture}"
                );
                thread::sleep(Duration::from_millis(30));
            }
        }

        fn send_line(&self, line: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, line, "Enter"]);
            assert!(
                output.status.success(),
                "tmux send {line:?}: {}",
                stderr(&output)
            );
        }

        fn send_text(&self, text: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, text]);
            assert!(
                output.status.success(),
                "tmux send text {text:?}: {}",
                stderr(&output)
            );
        }

        fn send_bracketed_paste(&self, text: &str) {
            let mut loader = Command::new("tmux")
                .args(["load-buffer", "-"])
                .stdin(Stdio::piped())
                .spawn()
                .expect("tmux load-buffer must start");
            loader
                .stdin
                .take()
                .expect("tmux load-buffer stdin")
                .write_all(text.as_bytes())
                .expect("write synthetic API key paste");
            let loaded = loader.wait().expect("wait for tmux load-buffer");
            assert!(loaded.success(), "tmux load-buffer failed");
            let pasted = tmux(&["paste-buffer", "-p", "-d", "-t", &self.name]);
            assert!(
                pasted.status.success(),
                "tmux bracketed paste failed: {}",
                stderr(&pasted)
            );
        }

        fn send_key(&self, key: &str) {
            let output = tmux(&["send-keys", "-t", &self.name, key]);
            assert!(
                output.status.success(),
                "tmux send key {key:?}: {}",
                stderr(&output)
            );
        }

        fn resize(&self, width: u16, height: u16) {
            let width = width.to_string();
            let height = height.to_string();
            let output = tmux(&[
                "resize-window",
                "-t",
                &self.name,
                "-x",
                &width,
                "-y",
                &height,
            ]);
            assert!(
                output.status.success(),
                "tmux resize to {width}x{height} failed: {}",
                stderr(&output)
            );
            thread::sleep(Duration::from_millis(150));
        }

        fn send_escape(&self) {
            let output = tmux(&["send-keys", "-t", &self.name, "Escape"]);
            assert!(output.status.success(), "tmux escape: {}", stderr(&output));
        }

        fn pane_tty(&self) -> String {
            let output = tmux(&["display-message", "-p", "-t", &self.name, "#{pane_tty}"]);
            assert!(output.status.success(), "tmux tty: {}", stderr(&output));
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        fn wait_for_cooked_mode(&self) {
            let tty = self.pane_tty();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let output = Command::new("stty")
                    .args(["-a", "-F", &tty])
                    .output()
                    .expect("stty");
                let state = String::from_utf8_lossy(&output.stdout).to_lowercase();
                if state.split_whitespace().any(|token| token == "icanon")
                    && state.split_whitespace().any(|token| token == "echo")
                {
                    return;
                }
                assert!(Instant::now() < deadline, "terminal stayed raw: {state}");
                thread::sleep(Duration::from_millis(30));
            }
        }
    }

    impl Drop for TmuxSession {
        fn drop(&mut self) {
            let _ = tmux(&["kill-session", "-t", &self.name]);
        }
    }

    fn tmux(args: &[&str]) -> Output {
        Command::new("tmux")
            .args(args)
            .output()
            .expect("tmux must be installed")
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    }

    fn shell_quote(path: &Path) -> String {
        shell_quote_value(&path.to_string_lossy())
    }

    fn shell_quote_value(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    #[test]
    fn login_browser_fallback_cancel_and_no_credential_guidance() {
        let sandbox = Sandbox::new();
        let session = TmuxSession::start(&sandbox);
        session.wait_for(|capture| capture.contains("gpt-5.5 • medium"));

        session.send_text("/login");
        let completion = session.wait_for(|capture| {
            capture.contains("→ login") && capture.contains("Configure provider authentication")
        });
        assert!(
            completion.contains("→ login")
                && completion.contains("Configure provider authentication"),
            "slash completion row was not retained before /login modal: {completion}"
        );
        session.send_key("Enter");
        let method_selector =
            session.wait_for(|capture| capture.contains("Select authentication method:"));
        assert!(
            method_selector.contains("↑↓ navigate  Enter select  Esc cancel"),
            "auth selector footer missing: {method_selector}"
        );
        session.send_key("Down");
        let moved_selector =
            session.wait_for(|capture| capture.contains("→ Sign in with an API key"));
        assert_eq!(
            moved_selector.matches("→ Sign in with an API key").count(),
            1,
            "one Down press should select exactly one auth row: {moved_selector}"
        );
        session.send_escape();
        session.wait_for(|capture| capture.contains("Login cancelled"));

        session.send_line("/login openai-codex");
        session.wait_for(|capture| capture.contains("Select OpenAI Codex login method:"));
        session.send_line("1");
        let login_screen = session.wait_for(|capture| {
            capture.contains("Open this URL to sign in:")
                && capture.contains("https://auth.openai.com/oauth/authorize")
                && capture.contains("paste the authorization code")
        });
        assert!(login_screen.contains("Login to OpenAI Codex"));
        assert!(login_screen.contains("Ctrl+click to open"));
        session.resize(90, 28);
        let resized_login = session.wait_for(|capture| {
            capture.contains("Login to OpenAI Codex")
                && capture.contains("https://auth.openai.com/oauth/authorize")
        });
        assert!(
            resized_login.contains("Ctrl+click to open"),
            "auth dialog did not redraw after PTY resize: {resized_login}"
        );
        let compact_login = login_screen
            .chars()
            .filter(|character| !matches!(character, '│' | '╭' | '╮' | '╰' | '╯' | '─'))
            .collect::<String>()
            .split_whitespace()
            .collect::<String>();
        assert!(compact_login.contains("redirect_uri"));
        session.send_escape();
        session.wait_for(|capture| capture.contains("Login cancelled"));

        session.send_line("hello without credentials");
        let error = session.wait_for(|capture| {
            capture.contains("/login openai-codex") || capture.contains("not configured")
        });
        assert!(
            error.contains("/login openai-codex") || error.contains("not configured"),
            "missing auth guidance: {error}"
        );

        session.send_line("/quit");
        session.wait_for_cooked_mode();
    }

    #[test]
    fn browser_callback_login_persists_and_logout_uses_live_credential() {
        let mock = MockAuthServer::start();
        let callback_port = free_port();
        let sandbox = Sandbox::new();
        fs::write(
            sandbox.agent_dir.join("models.json"),
            serde_json::json!({
                "providers": {
                    "openai-codex": {
                        "baseUrl": format!("{}/backend-api", mock.base_url)
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let extra_env = format!(
            "PI_OPENAI_CODEX_AUTH_BASE_URL={} PI_OAUTH_CALLBACK_PORT={callback_port}",
            shell_quote_value(&mock.base_url)
        );
        let session = TmuxSession::start_with_extra_env(&sandbox, &extra_env);
        session.wait_for(|capture| capture.contains("gpt-5.5 • medium"));

        session.send_line("/login openai-codex");
        session.wait_for(|capture| capture.contains("Select OpenAI Codex login method:"));
        session.send_line("1");
        let login_screen = session.wait_for(|capture| {
            capture.contains(&mock.base_url) && capture.contains("paste the authorization code")
        });
        let compact = login_screen
            .chars()
            .filter(|character| !matches!(character, '│' | '╭' | '╮' | '╰' | '╯' | '─'))
            .collect::<String>()
            .split_whitespace()
            .collect::<String>();
        let state = regex::Regex::new(r"state=([0-9a-f]{32})&id_token_add_organizations")
            .unwrap()
            .captures(&compact)
            .and_then(|captures| captures.get(1))
            .map(|value| value.as_str().to_string())
            .unwrap_or_else(|| panic!("OAuth state not found in TUI URL: {compact}"));
        send_http_get(&format!(
            "http://127.0.0.1:{callback_port}/auth/callback?code=pty-code&state={state}"
        ));
        session.wait_for(|capture| capture.contains("logged in to openai-codex via OAuth"));

        let auth_path = sandbox.agent_dir.join("auth.json");
        let auth: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&auth_path).unwrap()).unwrap();
        assert_eq!(auth["openai-codex"]["type"], "oauth");
        assert_eq!(auth["openai-codex"]["refresh"], "pty-refresh");
        assert_eq!(auth["openai-codex"]["accountId"], "pty-account");
        let requests = mock.requests.lock().unwrap().join("\n");
        assert!(requests.contains("POST /oauth/token "));
        assert!(requests.contains("grant_type=authorization_code"));
        assert!(requests.contains("code=pty-code"));

        session.send_line("first prompt after login");
        session.wait_for(|capture| capture.contains("fixture response"));
        session.send_line("second prompt after login");
        let second_turn =
            session.wait_for(|capture| capture.matches("fixture response").count() >= 2);
        assert!(second_turn.matches("fixture response").count() >= 2);

        session.send_line("/logout");
        session.wait_for(|capture| capture.contains("Select provider to logout:"));
        session.send_line("1");
        session.wait_for(|capture| capture.contains("logged out openai-codex"));
        let auth: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&auth_path).unwrap()).unwrap();
        assert!(auth.get("openai-codex").is_none());

        session.send_line("/quit");
        session.wait_for_cooked_mode();
    }

    #[test]
    fn device_code_login_completes_through_the_real_tui() {
        let mock = MockAuthServer::start();
        let callback_port = free_port();
        let sandbox = Sandbox::new();
        fs::write(
            sandbox.agent_dir.join("models.json"),
            serde_json::json!({
                "providers": {
                    "openai-codex": {
                        "baseUrl": format!("{}/backend-api", mock.base_url)
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let extra_env = format!(
            "PI_OPENAI_CODEX_AUTH_BASE_URL={} PI_OAUTH_CALLBACK_PORT={callback_port}",
            shell_quote_value(&mock.base_url)
        );
        let session = TmuxSession::start_with_extra_env(&sandbox, &extra_env);
        session.wait_for(|capture| capture.contains("gpt-5.5 • medium"));
        session.send_line("/login openai-codex");
        session.wait_for(|capture| capture.contains("Select OpenAI Codex login method:"));
        session.send_line("2");
        session.wait_for(|capture| capture.contains("logged in to openai-codex via OAuth"));

        let requests = mock.requests.lock().unwrap().join("\n");
        assert!(requests.contains("POST /api/accounts/deviceauth/usercode "));
        assert!(requests.contains("POST /api/accounts/deviceauth/token "));
        assert!(requests.contains("POST /oauth/token "));

        session.send_line("/logout");
        session.wait_for(|capture| capture.contains("Select provider to logout:"));
        session.send_line("1");
        session.wait_for(|capture| capture.contains("logged out openai-codex"));
        session.send_line("/quit");
        session.wait_for_cooked_mode();
    }

    #[test]
    fn qwen_token_plan_api_key_paste_login_persists_and_logout() {
        let sandbox = Sandbox::new();
        let session = TmuxSession::start(&sandbox);
        session.wait_for(|capture| capture.contains("gpt-5.5 • medium"));

        session.send_line("/login qwen-token-plan");
        session.wait_for(|capture| {
            capture.contains("Login to Qwen Token Plan")
                && capture.contains("Enter Qwen Token Plan")
        });

        let secret = "qwen-paste-secret";
        let masked = "•".repeat(secret.chars().count());
        session.send_bracketed_paste(secret);
        let pasted = session.wait_for(|capture| capture.contains(&masked));
        assert!(
            !pasted.contains(secret),
            "pasted API key was rendered in clear text:\n{pasted}"
        );
        session.send_key("Enter");
        let logged_in = session
            .wait_for(|capture| capture.contains("logged in to qwen-token-plan via API key"));
        assert!(!logged_in.contains(secret));

        let auth_path = sandbox.agent_dir.join("auth.json");
        let auth: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&auth_path).unwrap()).unwrap();
        assert_eq!(auth["qwen-token-plan"]["type"], "api_key");
        assert_eq!(auth["qwen-token-plan"]["key"], secret);

        session.send_line("/logout qwen-token-plan");
        session.wait_for(|capture| capture.contains("logged out qwen-token-plan"));
        let auth: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&auth_path).unwrap()).unwrap();
        assert!(auth.get("qwen-token-plan").is_none());

        session.send_line("/quit");
        session.wait_for_cooked_mode();
    }

    #[test]
    fn llama_api_key_login_validates_real_router_and_persists_configuration() {
        let router = MockLlamaRouter::start();
        let sandbox = Sandbox::new();
        let session = TmuxSession::start(&sandbox);
        session.wait_for(|capture| capture.contains("gpt-5.5 • medium"));

        session.send_line("/login llama.cpp");
        session.wait_for(|capture| capture.contains("llama.cpp server URL"));
        session.send_line(&router.base_url);
        session.wait_for(|capture| capture.contains("API key (optional)"));
        session.send_line("llama-secret");
        session.wait_for(|capture| capture.contains("logged in to llama.cpp via API key"));

        let auth_path = sandbox.agent_dir.join("auth.json");
        let auth: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&auth_path).unwrap()).unwrap();
        assert_eq!(auth["llama.cpp"]["type"], "api_key");
        assert_eq!(auth["llama.cpp"]["key"], "llama-secret");
        assert_eq!(auth["llama.cpp"]["env"]["LLAMA_BASE_URL"], router.base_url);

        let requests = router.requests.lock().unwrap().join("\n");
        assert!(requests.contains("GET /models ") || requests.contains("GET /models HTTP/"));
        assert!(requests
            .to_ascii_lowercase()
            .contains("authorization: bearer llama-secret"));

        session.send_line("/logout llama.cpp");
        session.wait_for(|capture| capture.contains("logged out llama.cpp"));
        let auth: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&auth_path).unwrap()).unwrap();
        assert!(auth.get("llama.cpp").is_none());

        session.send_line("/quit");
        session.wait_for_cooked_mode();
    }
}
