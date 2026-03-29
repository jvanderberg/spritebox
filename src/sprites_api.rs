use futures_util::{SinkExt, StreamExt};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const API_BASE: &str = "https://api.sprites.dev";
const EXEC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Clone)]
pub struct SpritesClient {
    http: reqwest::Client,
    token: String,
    verbose: bool,
}

// ---------------------------------------------------------------------------
// API types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct CreateSpriteRequest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<SpriteConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<std::collections::HashMap<String, String>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SpriteConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ram_mb: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_gb: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct CreateSpriteResponse {
    #[allow(dead_code)]
    pub name: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SpriteInfo {
    pub name: String,
    pub status: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub config: Option<SpriteConfig>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub created_at: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SpriteList {
    pub sprites: Vec<SpriteInfo>,
    #[serde(default)]
    #[allow(dead_code)]
    pub has_more: bool,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    error: String,
    #[serde(default)]
    message: String,
}

// WebSocket control messages
#[derive(Debug, Deserialize)]
struct WsControlMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    #[allow(dead_code)]
    session_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Client implementation
// ---------------------------------------------------------------------------

impl SpritesClient {
    pub fn new(token: String) -> Result<Self, String> {
        let mut headers = HeaderMap::new();
        let auth_value = format!("Bearer {token}");
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&auth_value)
                .map_err(|e| format!("invalid auth token: {e}"))?,
        );

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|e| format!("failed to create HTTP client: {e}"))?;

        Ok(Self {
            http,
            token,
            verbose: false,
        })
    }

    pub fn set_verbose(&mut self, verbose: bool) {
        self.verbose = verbose;
    }

    // -- Token exchange (static, no client needed) ---------------------------

    /// Exchange a Fly.io macaroon token for a Sprites access token.
    pub async fn exchange_fly_token(fly_token: &str, org: &str) -> Result<String, String> {
        let http = reqwest::Client::new();
        let resp = http
            .post(format!("{API_BASE}/v1/organizations/{org}/tokens"))
            .header(AUTHORIZATION, format!("FlyV1 {fly_token}"))
            .json(&serde_json::json!({"description": "spritebox CLI"}))
            .send()
            .await
            .map_err(|e| format!("token exchange request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }

        #[derive(Deserialize)]
        struct TokenResponse {
            token: String,
        }

        let body = resp
            .json::<TokenResponse>()
            .await
            .map_err(|e| format!("failed to parse token response: {e}"))?;
        Ok(body.token)
    }

    // -- CRUD ---------------------------------------------------------------

    pub async fn create_sprite(&self, req: &CreateSpriteRequest) -> Result<CreateSpriteResponse, String> {
        let resp = self
            .http
            .post(format!("{API_BASE}/v1/sprites"))
            .json(req)
            .send()
            .await
            .map_err(|e| format!("create sprite request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }

        resp.json::<CreateSpriteResponse>()
            .await
            .map_err(|e| format!("failed to parse create response: {e}"))
    }

    pub async fn get_sprite(&self, name: &str) -> Result<Option<SpriteInfo>, String> {
        let resp = self
            .http
            .get(format!("{API_BASE}/v1/sprites/{name}"))
            .send()
            .await
            .map_err(|e| format!("get sprite request failed: {e}"))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }

        resp.json::<SpriteInfo>()
            .await
            .map(Some)
            .map_err(|e| format!("failed to parse sprite info: {e}"))
    }

    pub async fn list_sprites(&self) -> Result<SpriteList, String> {
        let resp = self
            .http
            .get(format!("{API_BASE}/v1/sprites"))
            .send()
            .await
            .map_err(|e| format!("list sprites request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }

        resp.json::<SpriteList>()
            .await
            .map_err(|e| format!("failed to parse sprite list: {e}"))
    }

    pub async fn stop_sprite(&self, name: &str) -> Result<(), String> {
        let resp = self
            .http
            .post(format!("{API_BASE}/v1/sprites/{name}/stop"))
            .send()
            .await
            .map_err(|e| format!("stop sprite request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }

        Ok(())
    }

    pub async fn delete_sprite(&self, name: &str) -> Result<(), String> {
        let resp = self
            .http
            .delete(format!("{API_BASE}/v1/sprites/{name}"))
            .send()
            .await
            .map_err(|e| format!("delete sprite request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }

        Ok(())
    }

    // -- Filesystem API -----------------------------------------------------

    /// Read a file from the sprite via the filesystem API.
    pub async fn read_file(
        &self,
        sprite_name: &str,
        path: &str,
    ) -> Result<Vec<u8>, String> {
        let url = format!("{API_BASE}/v1/sprites/{sprite_name}/fs/read?path={path}");
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("read file request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("failed to read file body: {e}"))
    }

    /// Write a file to the sprite via the filesystem API.
    pub async fn write_file(
        &self,
        sprite_name: &str,
        path: &str,
        data: &[u8],
    ) -> Result<(), String> {
        let url = format!("{API_BASE}/v1/sprites/{sprite_name}/fs/write?path={path}");
        let resp = self
            .http
            .put(&url)
            .body(data.to_vec())
            .send()
            .await
            .map_err(|e| format!("write file request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }
        Ok(())
    }

    // -- Exec (one-shot, non-TTY) -------------------------------------------

    /// Execute a command and return (exit_code, stdout, stderr).
    /// Times out after 120 seconds by default.
    pub async fn exec(
        &self,
        sprite_name: &str,
        cmd: &[&str],
        env: &[(&str, &str)],
        dir: Option<&str>,
    ) -> Result<ExecResult, String> {
        self.exec_with_timeout(sprite_name, cmd, env, dir, &[], EXEC_TIMEOUT)
            .await
    }

    /// Execute a command, sending `stdin_data` to stdin before EOF.
    /// Times out after 120 seconds by default.
    pub async fn exec_with_stdin(
        &self,
        sprite_name: &str,
        cmd: &[&str],
        env: &[(&str, &str)],
        dir: Option<&str>,
        stdin_data: &[u8],
    ) -> Result<ExecResult, String> {
        self.exec_with_timeout(sprite_name, cmd, env, dir, stdin_data, EXEC_TIMEOUT)
            .await
    }

    /// Execute a command with a custom timeout.
    ///
    /// Follows the Go SDK pattern: try a control connection first (5s timeout),
    /// fall back to direct exec WebSocket (10s timeout).
    pub async fn exec_with_timeout(
        &self,
        sprite_name: &str,
        cmd: &[&str],
        env: &[(&str, &str)],
        dir: Option<&str>,
        stdin_data: &[u8],
        timeout: std::time::Duration,
    ) -> Result<ExecResult, String> {
        if self.verbose {
            let cmd_preview: String = cmd.join(" ");
            let preview = if cmd_preview.len() > 80 {
                format!("{}...", &cmd_preview[..77])
            } else {
                cmd_preview
            };
            eprintln!("[exec] {preview}");
        }

        let exec_params = ExecParams {
            sprite_name, cmd, env, dir, tty: false, rows: None, cols: None,
        };

        // Try control connection first (Go SDK: ensureControlSupport + pool.checkout)
        if self.verbose {
            eprintln!("[exec] trying control connection...");
        }
        match self.connect_control(sprite_name).await {
            Ok(ws) => {
                if self.verbose {
                    eprintln!("[exec] control connected, sending op.start...");
                }
                let (mut sink, mut stream) = ws.split();

                // Go SDK: control:{"type":"op.start","op":"exec","args":{...}}
                let args = Self::build_control_args(&exec_params);
                let op_start = format!(
                    "control:{}",
                    serde_json::json!({"type": "op.start", "op": "exec", "args": args})
                );
                if self.verbose {
                    eprintln!("[exec] op.start: {op_start}");
                }
                sink.send(Message::Text(op_start.into()))
                    .await
                    .map_err(|e| format!("failed to send op.start: {e}"))?;

                let result = tokio::time::timeout(
                    timeout,
                    self.exec_inner(&mut sink, &mut stream, stdin_data),
                )
                .await
                .map_err(|_| format!("exec timed out after {}s", timeout.as_secs()))?;

                // Go SDK: sendRelease() — plain JSON, no "control:" prefix
                let _ = sink.send(Message::Text(
                    serde_json::json!({"type": "release"}).to_string().into()
                )).await;

                if self.verbose
                    && let Ok(ref r) = result
                {
                    eprintln!("[exec] exit_code={}", r.exit_code);
                }
                return result;
            }
            Err(e) => {
                if self.verbose {
                    eprintln!("[exec] control connection failed: {e}, falling back to direct");
                }
            }
        }

        // Fallback: direct exec WebSocket (Go SDK: direct dial with 10s timeout)
        let exec_url = self.build_exec_url(&exec_params)?;
        if self.verbose {
            eprintln!("[exec] connecting direct websocket...");
        }
        let (ws, _) = self.connect_ws(&exec_url).await?;
        if self.verbose {
            eprintln!("[exec] connected, running command...");
        }
        let (mut sink, mut stream) = ws.split();

        let result = tokio::time::timeout(
            timeout,
            self.exec_inner(&mut sink, &mut stream, stdin_data),
        )
        .await
        .map_err(|_| format!("exec timed out after {}s", timeout.as_secs()))?;

        if self.verbose
            && let Ok(ref r) = result
        {
            eprintln!("[exec] exit_code={}", r.exit_code);
        }

        result
    }

    async fn exec_inner(
        &self,
        sink: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
        stream: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
        stdin_data: &[u8],
    ) -> Result<ExecResult, String> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code: Option<i32> = None;

        // Send stdin data with 0x00 prefix, then EOF (0x04)
        if !stdin_data.is_empty() {
            let mut frame = Vec::with_capacity(1 + stdin_data.len());
            frame.push(0x00);
            frame.extend_from_slice(stdin_data);
            sink.send(Message::Binary(frame.into()))
                .await
                .map_err(|e| format!("failed to send stdin: {e}"))?;
        }
        sink.send(Message::Binary(vec![0x04].into()))
            .await
            .map_err(|e| format!("failed to send stdin eof: {e}"))?;

        while let Some(msg) = stream.next().await {
            let msg = msg.map_err(|e| format!("ws read error: {e}"))?;
            match msg {
                Message::Binary(data) => {
                    if self.verbose {
                        eprintln!("[exec] binary frame: {} bytes, first={:?}", data.len(), data.first());
                    }
                    if data.is_empty() {
                        continue;
                    }
                    match data[0] {
                        0x01 => stdout.extend_from_slice(&data[1..]),
                        0x02 => stderr.extend_from_slice(&data[1..]),
                        0x03 => {
                            exit_code = Some(if data.len() > 1 { data[1] as i32 } else { 0 });
                        }
                        _ => {}
                    }
                }
                Message::Text(text) => {
                    if self.verbose {
                        let preview = if text.len() > 200 { &text[..200] } else { &text };
                        eprintln!("[exec] text frame: {preview}");
                    }
                    // Skip control protocol messages (Go SDK: "control:" prefix)
                    if text.starts_with("control:") {
                        continue;
                    }
                    if let Ok(ctrl) = serde_json::from_str::<WsControlMessage>(&text)
                        && ctrl.msg_type == "exit"
                    {
                        if exit_code.is_none() {
                            exit_code = ctrl.exit_code;
                        }
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {
                    if self.verbose {
                        eprintln!("[exec] other frame: {msg:?}");
                    }
                }
            }
        }

        Ok(ExecResult {
            exit_code: exit_code.unwrap_or(-1),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }

    // -- Interactive console (TTY) ------------------------------------------

    /// Open an interactive TTY console session. This takes over the terminal
    /// and returns the exit code when the session ends.
    pub async fn console(
        &self,
        sprite_name: &str,
        cmd: &[&str],
        env: &[(&str, &str)],
        dir: Option<&str>,
    ) -> Result<i32, String> {
        let (cols, rows) = crossterm::terminal::size()
            .map_err(|e| format!("failed to get terminal size: {e}"))?;

        let console_params = ExecParams {
            sprite_name, cmd, env, dir, tty: true, rows: Some(rows), cols: Some(cols),
        };

        // Try control connection first, fall back to direct (matching Go SDK)
        let (mut sink, mut stream);
        if self.verbose {
            eprintln!("[console] trying control connection (tty {cols}x{rows})...");
        }
        match self.connect_control(sprite_name).await {
            Ok(ws) => {
                if self.verbose {
                    eprintln!("[console] control connected, sending op.start...");
                }
                let (s, st) = ws.split();
                sink = s;
                stream = st;
                // Go SDK: control:{"type":"op.start","op":"exec","args":{...}}
                let args = Self::build_control_args(&console_params);
                let op_start = format!(
                    "control:{}",
                    serde_json::json!({"type": "op.start", "op": "exec", "args": args})
                );
                if self.verbose {
                    eprintln!("[console] op.start: {op_start}");
                }
                sink.send(Message::Text(op_start.into()))
                    .await
                    .map_err(|e| format!("failed to send op.start: {e}"))?;
            }
            Err(e) => {
                if self.verbose {
                    eprintln!("[console] control failed: {e}, falling back to direct...");
                }
                let ws_url = self.build_exec_url(&console_params)?;
                let (ws, _) = self.connect_ws(&ws_url).await?;
                if self.verbose {
                    eprintln!("[console] connected");
                }
                let (s, st) = ws.split();
                sink = s;
                stream = st;
            }
        }

        let bridge = BridgeContext {
            client: self.clone(),
            sprite_name: sprite_name.to_string(),
        };

        // Enter raw terminal mode
        crossterm::terminal::enable_raw_mode()
            .map_err(|e| format!("failed to enable raw mode: {e}"))?;

        let exit_code = tokio::select! {
            result = Self::console_read_loop(&mut stream, &bridge) => {
                result
            }
            result = Self::console_write_loop(&mut sink) => {
                // Write loop ended (stdin EOF) — drain remaining server output
                // so terminal-restore sequences aren't lost
                Self::drain_remaining(&mut stream, &bridge).await;
                result
            }
        };

        crossterm::terminal::disable_raw_mode()
            .map_err(|e| format!("failed to disable raw mode: {e}"))?;

        exit_code
    }

    /// Drain remaining messages from the server after one side of the console
    /// exits. This ensures terminal-restore sequences (alternate screen exit,
    /// cursor restore, etc.) are written to stdout before we disable raw mode.
    async fn drain_remaining(
        stream: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
        bridge: &BridgeContext,
    ) {
        use tokio::io::AsyncWriteExt;
        let mut stdout = tokio::io::stdout();
        let mut osc_buf: Vec<u8> = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(Some(Ok(Message::Binary(data)))) => {
                    let output = filter_bridge_escapes(&mut osc_buf, &data, bridge);
                    if !output.is_empty() {
                        let _ = stdout.write_all(&output).await;
                        let _ = stdout.flush().await;
                    }
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Err(_) => break,
                _ => continue,
            }
        }
    }

    /// Read from WebSocket, write to stdout, intercept bridge escape sequences.
    async fn console_read_loop(
        stream: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
        bridge: &BridgeContext,
    ) -> Result<i32, String> {
        use tokio::io::AsyncWriteExt;
        let mut stdout = tokio::io::stdout();
        // Buffer for incomplete OSC sequences that span frames
        let mut osc_buf: Vec<u8> = Vec::new();

        loop {
            let msg = match stream.next().await {
                Some(Ok(msg)) => msg,
                Some(Err(e)) => return Err(format!("ws read error: {e}")),
                None => return Ok(0),
            };

            match msg {
                Message::Binary(data) => {
                    // Scan for \x1b]9999; escape sequences and strip them
                    let output = filter_bridge_escapes(&mut osc_buf, &data, bridge);
                    if !output.is_empty() {
                        stdout
                            .write_all(&output)
                            .await
                            .map_err(|e| format!("stdout write error: {e}"))?;
                        stdout
                            .flush()
                            .await
                            .map_err(|e| format!("stdout flush error: {e}"))?;
                    }
                }
                Message::Text(text) => {
                    // Parse control protocol messages — check for exit/op.complete
                    let json_str = text.strip_prefix("control:").unwrap_or(&text);
                    if let Ok(ctrl) = serde_json::from_str::<WsControlMessage>(json_str) {
                        match ctrl.msg_type.as_str() {
                            "exit" => return Ok(ctrl.exit_code.unwrap_or(0)),
                            "op.complete" | "op.error" => return Ok(0),
                            _ => {}
                        }
                    }
                }
                Message::Close(_) => return Ok(0),
                _ => {}
            }
        }
    }

    /// Read raw stdin bytes and forward to WebSocket. Also watches for resize.
    async fn console_write_loop(
        sink: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    ) -> Result<i32, String> {
        use tokio::io::AsyncReadExt;

        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 4096];

        // Handle SIGWINCH for resize via a separate task
        let (resize_tx, mut resize_rx) = tokio::sync::mpsc::channel::<(u16, u16)>(4);
        tokio::spawn(async move {
            let mut sigwinch = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::window_change(),
            ).expect("failed to register SIGWINCH");
            while sigwinch.recv().await.is_some() {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    let _ = resize_tx.send((cols, rows)).await;
                }
            }
        });

        loop {
            tokio::select! {
                n = stdin.read(&mut buf) => {
                    let n = n.map_err(|e| format!("stdin read error: {e}"))?;
                    if n == 0 {
                        return Ok(0);
                    }
                    sink.send(Message::Binary(buf[..n].to_vec().into()))
                        .await
                        .map_err(|e| format!("ws write error: {e}"))?;
                }
                Some((cols, rows)) = resize_rx.recv() => {
                    let resize = serde_json::json!({
                        "type": "resize",
                        "cols": cols,
                        "rows": rows,
                    });
                    sink.send(Message::Text(resize.to_string().into()))
                        .await
                        .map_err(|e| format!("ws write error: {e}"))?;
                }
            }
        }
    }

    // -- Helpers -------------------------------------------------------------

    /// Build structured args map for control connection op.start.
    /// Matches Go SDK websocket.go start() args format exactly.
    fn build_control_args(params: &ExecParams<'_>) -> serde_json::Value {
        let mut args = serde_json::Map::new();
        if !params.cmd.is_empty() {
            let cmd_arr: Vec<serde_json::Value> = params.cmd.iter()
                .map(|s| serde_json::Value::String(s.to_string()))
                .collect();
            args.insert("cmd".into(), serde_json::Value::Array(cmd_arr));
            if let Some(first) = params.cmd.first() {
                args.insert("path".into(), serde_json::Value::String(first.to_string()));
            }
        }
        if !params.env.is_empty() {
            let env_arr: Vec<serde_json::Value> = params.env.iter()
                .map(|(k, v)| serde_json::Value::String(format!("{k}={v}")))
                .collect();
            args.insert("env".into(), serde_json::Value::Array(env_arr));
        }
        if let Some(d) = params.dir {
            args.insert("dir".into(), serde_json::Value::String(d.to_string()));
        }
        // Go SDK: only includes tty/stdin when true (as string "true")
        if params.tty {
            args.insert("tty".into(), serde_json::Value::String("true".into()));
        }
        args.insert("stdin".into(), serde_json::Value::String("true".into()));
        serde_json::Value::Object(args)
    }

    fn build_exec_url(&self, params: &ExecParams<'_>) -> Result<String, String> {
        let mut url = url::Url::parse(&format!(
            "wss://api.sprites.dev/v1/sprites/{}/exec",
            params.sprite_name
        ))
        .map_err(|e| format!("invalid URL: {e}"))?;

        {
            let mut query = url.query_pairs_mut();
            for part in params.cmd {
                query.append_pair("cmd", part);
            }
            if let Some(first) = params.cmd.first() {
                query.append_pair("path", first);
            }
            for (k, v) in params.env {
                query.append_pair("env", &format!("{k}={v}"));
            }
            if let Some(d) = params.dir {
                query.append_pair("dir", d);
            }
            query.append_pair("tty", if params.tty { "true" } else { "false" });
            query.append_pair("stdin", "true");
            if let Some(r) = params.rows {
                query.append_pair("rows", &r.to_string());
            }
            if let Some(c) = params.cols {
                query.append_pair("cols", &c.to_string());
            }
        }

        Ok(url.to_string())
    }

    /// Dial a control WebSocket connection.
    /// Go SDK: control_pool.go dial() — wss://.../v1/sprites/{name}/control, 5s timeout.
    async fn connect_control(
        &self,
        sprite_name: &str,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        String,
    > {
        let url = format!("wss://api.sprites.dev/v1/sprites/{sprite_name}/control");
        let request = {
            let mut r = url
                .into_client_request()
                .map_err(|e| format!("failed to build control ws request: {e}"))?;
            r.headers_mut().insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", self.token))
                    .map_err(|e| format!("invalid auth header: {e}"))?,
            );
            r
        };

        // Go SDK: dialTimeout = 5 * time.Second
        let (ws, _) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio_tungstenite::connect_async(request),
        )
        .await
        .map_err(|_| "control connection timed out (5s)".to_string())?
        .map_err(|e| format!("control connection failed: {e}"))?;

        Ok(ws)
    }

    /// Direct exec WebSocket connection.
    /// Go SDK: websocket.go start() — 10s HandshakeTimeout, no retries.
    async fn connect_ws(
        &self,
        url: &str,
    ) -> Result<
        (
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            http::Response<Option<Vec<u8>>>,
        ),
        String,
    > {
        let request = {
            let mut r = url
                .into_client_request()
                .map_err(|e| format!("failed to build ws request: {e}"))?;
            r.headers_mut().insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", self.token))
                    .map_err(|e| format!("invalid auth header: {e}"))?,
            );
            r
        };

        // Go SDK: HandshakeTimeout = 10 * time.Second, no retries
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio_tungstenite::connect_async(request),
        )
        .await
        .map_err(|_| "ws connect timed out (10s)".to_string())?
        .map_err(|e| format!("ws connect failed: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Exec params
// ---------------------------------------------------------------------------

struct ExecParams<'a> {
    sprite_name: &'a str,
    cmd: &'a [&'a str],
    env: &'a [(&'a str, &'a str)],
    dir: Option<&'a str>,
    tty: bool,
    rows: Option<u16>,
    cols: Option<u16>,
}

// ---------------------------------------------------------------------------
// Exec result
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}


// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Bridge escape sequence handling
// ---------------------------------------------------------------------------

struct BridgeContext {
    client: SpritesClient,
    sprite_name: String,
}

/// OSC 9999 escape sequence format:
///   \x1b]9999;verb;payload\x1b\\   (ST = ESC \)
///   \x1b]9999;verb;payload\x07     (ST = BEL)
///
/// Scans `data` for these sequences, dispatches bridge actions, and returns
/// the data with those sequences stripped out.
fn filter_bridge_escapes(osc_buf: &mut Vec<u8>, data: &[u8], bridge: &BridgeContext) -> Vec<u8> {
    const OSC_START: &[u8] = b"\x1b]9999;";
    let mut output = Vec::with_capacity(data.len());
    let mut i = 0;

    // If we have a partial OSC from a previous frame, keep accumulating
    if !osc_buf.is_empty() {
        // First check: does the buffered prefix + new data still look like OSC 9999?
        // If we only had a partial prefix (e.g. \x1b or \x1b]99), verify it still matches.
        if osc_buf.len() < OSC_START.len() {
            // Still building the prefix — check byte by byte
            while i < data.len() && osc_buf.len() < OSC_START.len() {
                osc_buf.push(data[i]);
                i += 1;
                if !OSC_START.starts_with(osc_buf) {
                    // Not an OSC 9999 after all — flush buffer to output
                    output.extend_from_slice(osc_buf);
                    osc_buf.clear();
                    break;
                }
            }
            if osc_buf.is_empty() {
                // Was flushed — continue scanning
            }
        }
        // Now look for the terminator
        if !osc_buf.is_empty() {
            while i < data.len() {
                let b = data[i];
                i += 1;
                osc_buf.push(b);

                // Check for ST terminators: \x07 or \x1b\\
                if b == 0x07 {
                    dispatch_osc(osc_buf, bridge);
                    osc_buf.clear();
                    break;
                }
                if osc_buf.len() >= 2
                    && osc_buf[osc_buf.len() - 2] == 0x1b
                    && osc_buf[osc_buf.len() - 1] == b'\\'
                {
                    dispatch_osc(osc_buf, bridge);
                    osc_buf.clear();
                    break;
                }
                // Safety: don't buffer forever
                if osc_buf.len() > 8192 {
                    output.extend_from_slice(osc_buf);
                    osc_buf.clear();
                    break;
                }
            }
        }
    }

    // Scan remaining data for new OSC sequences
    while i < data.len() {
        if data[i] == 0x1b {
            let remaining = &data[i..];
            if remaining.starts_with(OSC_START) {
                // Full prefix matched — look for terminator
                let start = i;
                i += OSC_START.len();
                let mut found = false;
                while i < data.len() {
                    if data[i] == 0x07 {
                        let seq = &data[start..=i];
                        dispatch_osc_bytes(seq, bridge);
                        i += 1;
                        found = true;
                        break;
                    }
                    if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'\\' {
                        let seq = &data[start..=i + 1];
                        dispatch_osc_bytes(seq, bridge);
                        i += 2;
                        found = true;
                        break;
                    }
                    i += 1;
                }
                if !found {
                    // Incomplete — buffer for next frame
                    osc_buf.extend_from_slice(&data[start..]);
                    break;
                }
            } else if OSC_START.starts_with(remaining) {
                // Partial prefix at end of frame (e.g. just \x1b or \x1b]99)
                // — buffer it for the next frame
                osc_buf.extend_from_slice(remaining);
                break;
            } else {
                output.push(data[i]);
                i += 1;
            }
        } else {
            output.push(data[i]);
            i += 1;
        }
    }

    output
}

/// Dispatch a complete OSC 9999 sequence from a byte slice.
fn dispatch_osc_bytes(seq: &[u8], bridge: &BridgeContext) {
    let prefix = b"\x1b]9999;";
    if !seq.starts_with(prefix) {
        return;
    }
    let body = &seq[prefix.len()..];
    let body = if body.last() == Some(&0x07) {
        &body[..body.len() - 1]
    } else if body.len() >= 2 && body[body.len() - 2] == 0x1b && body[body.len() - 1] == b'\\' {
        &body[..body.len() - 2]
    } else {
        body
    };
    if let Ok(s) = std::str::from_utf8(body) {
        handle_bridge_command(s, bridge);
    }
}

fn dispatch_osc(buf: &[u8], bridge: &BridgeContext) {
    dispatch_osc_bytes(buf, bridge);
}

/// Handle a bridge command string like "browser-open;https://example.com".
fn handle_bridge_command(cmd: &str, bridge: &BridgeContext) {
    let (verb, payload) = match cmd.split_once(';') {
        Some((v, p)) => (v, p),
        None => (cmd, ""),
    };
    match verb {
        "browser-open" => {
            if !payload.is_empty() {
                let _ = std::process::Command::new("open")
                    .arg(payload)
                    .spawn();
            }
        }
        "open" => {
            if !payload.is_empty() {
                let client = bridge.client.clone();
                let sprite = bridge.sprite_name.clone();
                let path = payload.to_string();
                tokio::spawn(async move {
                    if let Err(e) = open_file_from_sprite(&client, &sprite, &path).await {
                        eprintln!("\r\n[bridge] open failed: {e}\r");
                    }
                });
            }
        }
        "paste-image" => {
            if !payload.is_empty() {
                let client = bridge.client.clone();
                let sprite = bridge.sprite_name.clone();
                let dest = payload.to_string();
                tokio::spawn(async move {
                    if let Err(e) = paste_image_to_sprite(&client, &sprite, &dest).await {
                        eprintln!("\r\n[bridge] paste-image failed: {e}\r");
                    }
                });
            }
        }
        _ => {}
    }
}

/// Allowed file extensions for the `open` bridge verb.
const OPEN_ALLOWED_EXTENSIONS: &[&str] = &[
    "html", "htm", "svg", "png", "jpg", "jpeg", "pdf", "md", "txt", "rtf", "csv",
];

/// Download a file from the sprite to a temp path, confirm, then open it.
async fn open_file_from_sprite(
    client: &SpritesClient,
    sprite_name: &str,
    guest_path: &str,
) -> Result<(), String> {
    // Validate extension against allowlist
    let extension = std::path::Path::new(guest_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let extension = match extension {
        Some(ref ext) if OPEN_ALLOWED_EXTENSIONS.contains(&ext.as_str()) => ext.clone(),
        Some(ext) => return Err(format!("{ext} is not an allowed file type (allowed: {})",
            OPEN_ALLOWED_EXTENSIONS.join(", "))),
        None => return Err("file has no extension".to_string()),
    };

    // Confirm with user via macOS dialog
    if !confirm_file_download(sprite_name, guest_path)? {
        return Ok(());
    }

    // Download the file from the sprite
    let data = client.read_file(sprite_name, guest_path).await?;

    // Write to temp file preserving extension so macOS opens with the right app
    let tmp = std::env::temp_dir().join(format!(
        "spritebox-open-{}.{extension}",
        std::process::id()
    ));
    std::fs::write(&tmp, &data)
        .map_err(|e| format!("failed to write temp file: {e}"))?;

    // Open with default handler
    std::process::Command::new("open")
        .arg(&tmp)
        .spawn()
        .map_err(|e| format!("failed to open file: {e}"))?;

    Ok(())
}

fn confirm_file_download(sprite_name: &str, guest_path: &str) -> Result<bool, String> {
    let script = r#"on run argv
set spriteName to item 1 of argv
set guestPath to item 2 of argv
try
    set promptText to "Sprite " & spriteName & " wants to open " & guestPath & " on your Mac. Download and open?"
    set response to display dialog promptText buttons {"Deny", "Allow"} default button "Allow" with icon caution
    if button returned of response is "Allow" then
        return "allow"
    end if
    return "deny"
on error number -128
    return "deny"
end try
end run"#;
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .arg(sprite_name)
        .arg(guest_path)
        .output()
        .map_err(|e| format!("failed to prompt for file download: {e}"))?;
    if !output.status.success() {
        return Err("file download prompt failed".to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim() == "allow")
}

/// Grab clipboard image on host, push to sprite via filesystem API.
async fn paste_image_to_sprite(
    client: &SpritesClient,
    sprite_name: &str,
    guest_path: &str,
) -> Result<(), String> {
    // Confirm with user via macOS dialog
    if !confirm_clipboard_import(sprite_name, guest_path)? {
        return Err("clipboard import denied".to_string());
    }

    // Export clipboard image to temp file
    let tmp = std::env::temp_dir().join(format!("spritebox-paste-{}.png", std::process::id()));
    export_clipboard_image(&tmp)?;

    // Read and push to sprite
    let data = std::fs::read(&tmp)
        .map_err(|e| format!("failed to read temp image: {e}"))?;
    let _ = std::fs::remove_file(&tmp);

    client.write_file(sprite_name, guest_path, &data).await?;
    Ok(())
}

fn confirm_clipboard_import(sprite_name: &str, guest_path: &str) -> Result<bool, String> {
    let script = r#"on run argv
set spriteName to item 1 of argv
set guestPath to item 2 of argv
try
    set promptText to "Sprite " & spriteName & " wants to import the current clipboard image to " & guestPath
    set response to display dialog promptText buttons {"Deny", "Allow"} default button "Allow" with icon caution
    if button returned of response is "Allow" then
        return "allow"
    end if
    return "deny"
on error number -128
    return "deny"
end try
end run"#;
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .arg(sprite_name)
        .arg(guest_path)
        .output()
        .map_err(|e| format!("failed to prompt for clipboard access: {e}"))?;
    if !output.status.success() {
        return Err("clipboard import prompt failed".to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim() == "allow")
}

fn export_clipboard_image(path: &std::path::Path) -> Result<(), String> {
    let script = r#"on run argv
set destPath to item 1 of argv
try
    set imageData to the clipboard as «class PNGf»
on error
    error "Clipboard does not contain a PNG image."
end try
set fileRef to open for access POSIX file destPath with write permission
try
    set eof of fileRef to 0
    write imageData to fileRef
    close access fileRef
on error errMsg number errNum
    try
        close access fileRef
    end try
    error errMsg number errNum
end try
return "ok"
end run"#;
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .arg(path)
        .output()
        .map_err(|e| format!("failed to read host clipboard image: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            Err("clipboard image export failed".to_string())
        } else {
            Err(stderr)
        }
    }
}

async fn api_error(resp: reqwest::Response) -> String {
    let status = resp.status();
    match resp.json::<ApiError>().await {
        Ok(err) => {
            let msg = if err.message.is_empty() {
                &err.error
            } else {
                &err.message
            };
            format!("API error ({status}): {msg}")
        }
        Err(_) => format!("API error ({status})"),
    }
}
