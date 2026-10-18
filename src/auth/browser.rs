//! CDP-driven browser login (`slaq login --browser`).
//!
//! Launches the installed Chromium/Chrome with a dedicated profile under the
//! app data dir and drives it over the Chrome DevTools Protocol to complete
//! Slack's Google OAuth flow and read back the two session secrets: the `d`
//! cookie (`xoxd`) and the `api_token` exposed by the web client's `boot_data`
//! (`xoxc`).
//!
//! The profile holds session cookies, is kept between runs so the Google login
//! is reused, and its contents are never read or logged. No secret is ever
//! printed: only presence and lengths.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::auth::store_credentials;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const BROWSER_CANDIDATES: [&str; 6] = [
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
    "brave-browser",
    "brave",
];

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const READY_TIMEOUT: Duration = Duration::from_secs(30);

const TOKEN_JS: &str = r#"(()=>{
  try{ if(window.__slaq_xoxc) return window.__slaq_xoxc; }catch(e){}
  try{
    for(const k of Object.keys(window)){
      try{
        const v=window[k];
        if(v&&typeof v==='object'&&typeof v.api_token==='string'&&v.api_token.startsWith('xoxc-'))return v.api_token;
      }catch(e){}
    }
  }catch(e){}
  const re=/xoxc-[A-Za-z0-9_-]+/;
  try{
    for(let i=0;i<localStorage.length;i++){
      const key=localStorage.key(i);
      const val=key?localStorage.getItem(key):null;
      if(typeof val==='string'){const m=val.match(re);if(m)return m[0];}
    }
  }catch(e){}
  return '';
})()"#;

const HOOK_JS: &str = r#"(()=>{
  if(window.__slaq_hook)return;
  window.__slaq_hook=1;
  const re=/xoxc-[A-Za-z0-9_-]+/;
  const rec=(s)=>{try{if(typeof s==='string'&&!window.__slaq_xoxc){const m=s.match(re);if(m)window.__slaq_xoxc=m[0];}}catch(e){}};
  try{
    const original=window.fetch;
    window.fetch=function(...a){try{const r=a[0];rec(typeof r==='string'?r:(r&&r.url)||'');const o=a[1];if(o&&typeof o.body==='string')rec(o.body);}catch(e){}return original.apply(this,a);};
  }catch(e){}
  try{
    const open=XMLHttpRequest.prototype.open;
    XMLHttpRequest.prototype.open=function(m,u,...rest){rec(u);return open.call(this,m,u,...rest);};
    const send=XMLHttpRequest.prototype.send;
    XMLHttpRequest.prototype.send=function(body){if(typeof body==='string')rec(body);return send.call(this,body);};
  }catch(e){}
})()"#;

pub async fn run(workspace: Option<&str>) -> Result<()> {
    let browser = find_browser().ok_or_else(|| {
        anyhow!(
            "no Chromium/Chrome found in PATH (tried {})",
            BROWSER_CANDIDATES.join(", ")
        )
    })?;

    let profile = profile_dir()?;
    std::fs::create_dir_all(&profile).context("failed to create the browser profile directory")?;
    prepare_profile(&profile);

    let start = start_url(workspace);
    let child = Command::new(&browser)
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--remote-debugging-port=0")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg(format!("--app={start}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to launch {browser}"))?;

    let _guard = BrowserGuard { child };

    eprintln!("login: browser launched, waiting for DevTools...");
    let (port, browser_ws) = wait_for_port(&profile).await?;
    let page_ws = wait_for_page_ws(port).await?;

    let debug = debug_enabled();
    let mut cdp = CdpClient::connect(&page_ws, debug).await?;
    cdp.call("Page.enable", json!({})).await?;
    cdp.call("Runtime.enable", json!({})).await?;
    cdp.call("Network.enable", json!({})).await?;
    let _ = cdp
        .call(
            "Runtime.evaluate",
            json!({ "expression": HOOK_JS, "returnByValue": true }),
        )
        .await;

    let match_host = workspace.map(normalize_workspace);
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    let mut token: Option<String> = None;
    let mut cookie: Option<String> = None;
    let mut last_url = String::new();

    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(1)).await;

        let href = eval_string(&mut cdp, "location.href").await.unwrap_or_default();
        let stripped = strip_query(&href);
        if stripped != last_url && !stripped.is_empty() {
            eprintln!("login: navigated to {stripped}");
            last_url = stripped.clone();
        }

        let tok = eval_string(&mut cdp, TOKEN_JS).await.unwrap_or_default();
        if !tok.is_empty() {
            token = Some(tok);
        }
        if token.is_none() {
            if let Some(captured) = cdp.captured_token() {
                token = Some(captured);
            }
        }

        let mut probe = fetch_d_cookie(&mut cdp, workspace, debug).await;
        if probe.d.is_none() && probe.total == 0 {
            if let Some(fallback) = fetch_d_cookie_browser(port, &browser_ws, debug).await {
                probe.d = fallback.d;
                if fallback.total > 0 {
                    probe.total = fallback.total;
                }
            }
        }
        if cookie.is_none() {
            cookie = probe.d.clone();
        }

        if debug {
            eprintln!(
                "login[debug]: poll href={} token_found={} token_len={} cookie_d_found={} cookies_total={} requests_seen={}",
                stripped,
                token.is_some(),
                token.as_ref().map(|t| t.len()).unwrap_or(0),
                cookie.is_some(),
                probe.total,
                cdp.requests_seen(),
            );
        }

        if is_complete_url(&href, match_host.as_deref()) && token.is_some() && cookie.is_some() {
            break;
        }
    }

    let token = token.ok_or_else(|| anyhow!("login timed out: no xoxc token captured"))?;
    let cookie = cookie.ok_or_else(|| anyhow!("login timed out: no d cookie captured"))?;

    let store_workspace = workspace
        .map(normalize_workspace)
        .unwrap_or_else(|| "app.slack.com".to_string());
    store_credentials(&store_workspace, &token, &cookie).context("failed to store credentials")?;

    println!(
        "login: cookie_d=found token=found (cookie_len={}, token_len={})",
        cookie.len(),
        token.len()
    );

    graceful_close(port, &browser_ws).await;
    Ok(())
}

struct BrowserGuard {
    child: Child,
}

impl Drop for BrowserGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct CdpClient {
    ws: Ws,
    next_id: u64,
    captured_token: Option<String>,
    requests_seen: u64,
    debug: bool,
}

impl CdpClient {
    async fn connect(url: &str, debug: bool) -> Result<Self> {
        let (ws, _response) = connect_async(url)
            .await
            .context("failed to open the CDP WebSocket")?;
        Ok(Self {
            ws,
            next_id: 1,
            captured_token: None,
            requests_seen: 0,
            debug,
        })
    }

    fn captured_token(&self) -> Option<String> {
        self.captured_token.clone()
    }

    fn requests_seen(&self) -> u64 {
        self.requests_seen
    }

    fn capture_from_event(&mut self, message: &Value) {
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        if self.debug && !method.is_empty() {
            eprintln!("login[debug]: event {method}");
        }
        if method == "Network.requestWillBeSent" {
            self.requests_seen += 1;
            let request = &message["params"]["request"];
            let url = request.get("url").and_then(Value::as_str).unwrap_or("");
            let found = scan_request(request);
            if self.debug {
                eprintln!(
                    "login[debug]: requestWillBeSent #{} {} has_xoxc={}",
                    self.requests_seen,
                    strip_query(url),
                    found.is_some()
                );
            }
            if self.captured_token.is_none() {
                self.captured_token = found;
            }
            return;
        }
        if self.captured_token.is_none() && method.starts_with("Network.") {
            if let Some(token) = find_xoxc(&message.to_string()) {
                self.captured_token = Some(token);
            }
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;

        let request = json!({ "id": id, "method": method, "params": params });
        self.ws
            .send(Message::Text(request.to_string()))
            .await
            .with_context(|| format!("failed to send {method}"))?;

        loop {
            let frame = self
                .ws
                .next()
                .await
                .ok_or_else(|| anyhow!("CDP socket closed while awaiting {method}"))?
                .with_context(|| format!("CDP transport error while awaiting {method}"))?;

            let text = match frame {
                Message::Text(text) => text,
                Message::Close(_) => anyhow::bail!("CDP socket closed while awaiting {method}"),
                _ => continue,
            };
            let message: Value = match serde_json::from_str(&text) {
                Ok(value) => value,
                Err(_) => continue,
            };

            match message.get("id").and_then(Value::as_u64) {
                Some(got) if got == id => {
                    if let Some(error) = message.get("error") {
                        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                        let text = error.get("message").and_then(Value::as_str).unwrap_or("");
                        anyhow::bail!("CDP {method} failed ({code}): {text}");
                    }
                    return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                }
                Some(_) => continue,
                None => {
                    self.capture_from_event(&message);
                    continue;
                }
            }
        }
    }
}

async fn eval_string(cdp: &mut CdpClient, expression: &str) -> Result<String> {
    let result = cdp
        .call(
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true, "awaitPromise": false }),
        )
        .await?;
    Ok(result["result"]["value"].as_str().unwrap_or_default().to_string())
}

async fn wait_for_port(profile: &Path) -> Result<(u16, String)> {
    let file = profile.join("DevToolsActivePort");
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(contents) = std::fs::read_to_string(&file) {
            let mut lines = contents.lines();
            if let Some(port) = lines.next().and_then(|line| line.trim().parse::<u16>().ok()) {
                let ws_path = lines.next().map(str::trim).unwrap_or("").to_string();
                return Ok((port, ws_path));
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!("browser did not open its DevTools port in time")
}

async fn graceful_close(port: u16, browser_path: &str) {
    let url = if browser_path.starts_with("ws://") || browser_path.starts_with("wss://") {
        browser_path.to_string()
    } else {
        format!("ws://127.0.0.1:{port}{browser_path}")
    };
    if let Ok(mut cdp) = CdpClient::connect(&url, false).await {
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            cdp.call("Browser.close", json!({})),
        )
        .await;
    }
}

async fn wait_for_page_ws(port: u16) -> Result<String> {
    let url = format!("http://127.0.0.1:{port}/json");
    let client = reqwest::Client::new();
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(response) = client.get(&url).send().await {
            if let Ok(targets) = response.json::<Value>().await {
                if let Some(ws) = pick_page_ws(&targets) {
                    return Ok(ws);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    anyhow::bail!("no drivable page target appeared")
}

fn pick_page_ws(targets: &Value) -> Option<String> {
    let array = targets.as_array()?;
    // Prefer a real page target; fall back to any http(s) target so that an
    // app-typed window (or a Chrome that labels its window differently) is
    // still drivable.
    let ws_of = |target: &Value| {
        target
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    array
        // Prefer the Slack window: a Chromium-family browser (Brave, Chrome)
        // can expose other page targets (its own welcome, a new tab), and
        // driving the wrong one means capturing no token at all.
        .iter()
        .find(|target| {
            target.get("type").and_then(Value::as_str) == Some("page")
                && target
                    .get("url")
                    .and_then(Value::as_str)
                    .map(|url| url.contains("slack"))
                    .unwrap_or(false)
        })
        .and_then(ws_of)
        .or_else(|| {
            array
                .iter()
                .find(|target| target.get("type").and_then(Value::as_str) == Some("page"))
                .and_then(ws_of)
        })
        .or_else(|| {
            array
                .iter()
                .find(|target| {
                    target
                        .get("url")
                        .and_then(Value::as_str)
                        .map(|url| url.starts_with("http"))
                        .unwrap_or(false)
                })
                .and_then(ws_of)
        })
}

fn find_d_cookie(result: &Value) -> Option<String> {
    result
        .get("cookies")?
        .as_array()?
        .iter()
        .find(|cookie| {
            cookie.get("name").and_then(Value::as_str) == Some("d")
                && cookie
                    .get("domain")
                    .and_then(Value::as_str)
                    .map(|domain| domain.ends_with("slack.com"))
                    .unwrap_or(false)
        })
        .and_then(|cookie| cookie.get("value").and_then(Value::as_str))
        .map(str::to_owned)
}

/// Clears leftovers that would break a fresh launch on a persistent profile.
///
/// A stale `DevToolsActivePort` from a previous run is read immediately and
/// points at a dead endpoint, so the new browser is never reached; a leftover
/// `Singleton*` lock makes a new launch hand off to an old instance and exit
/// without writing a fresh port. Any browser still holding this profile is
/// stopped first, best effort.
fn prepare_profile(profile: &Path) {
    let _ = std::fs::remove_file(profile.join("DevToolsActivePort"));
    if let Ok(entries) = std::fs::read_dir(profile) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("Singleton") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let marker = format!("--user-data-dir={}", profile.display());
    let _ = Command::new("pkill").args(["-f", "--", &marker]).status();
}

fn profile_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => {
            let home =
                std::env::var_os("HOME").context("neither XDG_DATA_HOME nor HOME is set")?;
            PathBuf::from(home).join(".local").join("share")
        }
    };
    Ok(base.join("slaq").join("browser-login"))
}

fn scan_request(request: &Value) -> Option<String> {
    if let Some(url) = request.get("url").and_then(Value::as_str) {
        if let Some(token) = find_xoxc(&percent_decode(url)) {
            return Some(token);
        }
    }
    if let Some(post) = request.get("postData").and_then(Value::as_str) {
        if let Some(token) = find_xoxc(&percent_decode(post)) {
            return Some(token);
        }
        if let Some(token) = find_xoxc(post) {
            return Some(token);
        }
    }
    if let Some(headers) = request.get("headers").and_then(Value::as_object) {
        for value in headers.values() {
            if let Some(value) = value.as_str() {
                if let Some(token) = find_xoxc(value) {
                    return Some(token);
                }
            }
        }
    }
    None
}

fn find_xoxc(haystack: &str) -> Option<String> {
    const NEEDLE: &str = "xoxc-";
    let mut start = 0;
    while let Some(offset) = haystack[start..].find(NEEDLE) {
        let begin = start + offset;
        let rest = &haystack[begin..];
        let end = rest
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        if end > NEEDLE.len() {
            return Some(rest[..end].to_string());
        }
        start = begin + NEEDLE.len();
    }
    None
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

struct CookieProbe {
    d: Option<String>,
    total: usize,
}

async fn fetch_d_cookie(cdp: &mut CdpClient, workspace: Option<&str>, debug: bool) -> CookieProbe {
    let urls = cookie_urls(workspace);
    match cdp
        .call("Network.getCookies", json!({ "urls": urls }))
        .await
    {
        Ok(result) => {
            let total = result
                .get("cookies")
                .and_then(Value::as_array)
                .map(|cookies| cookies.len())
                .unwrap_or(0);
            CookieProbe {
                d: find_d_cookie(&result),
                total,
            }
        }
        Err(error) => {
            if debug {
                eprintln!("login[debug]: Network.getCookies failed: {error}");
            }
            CookieProbe { d: None, total: 0 }
        }
    }
}

async fn fetch_d_cookie_browser(port: u16, browser_ws: &str, debug: bool) -> Option<CookieProbe> {
    let url = if browser_ws.starts_with("ws://") || browser_ws.starts_with("wss://") {
        browser_ws.to_string()
    } else {
        format!("ws://127.0.0.1:{port}{browser_ws}")
    };
    let mut cdp = match CdpClient::connect(&url, debug).await {
        Ok(cdp) => cdp,
        Err(error) => {
            if debug {
                eprintln!("login[debug]: browser CDP connect failed: {error}");
            }
            return None;
        }
    };
    match cdp.call("Storage.getCookies", json!({})).await {
        Ok(result) => {
            let total = result
                .get("cookies")
                .and_then(Value::as_array)
                .map(|cookies| cookies.len())
                .unwrap_or(0);
            Some(CookieProbe {
                d: find_d_cookie(&result),
                total,
            })
        }
        Err(error) => {
            if debug {
                eprintln!("login[debug]: Storage.getCookies failed: {error}");
            }
            None
        }
    }
}

fn cookie_urls(workspace: Option<&str>) -> Vec<String> {
    let mut urls = vec![
        "https://app.slack.com".to_string(),
        "https://slack.com".to_string(),
    ];
    if let Some(ws) = workspace {
        urls.push(format!("https://{}/", normalize_workspace(ws)));
    }
    urls
}

fn debug_enabled() -> bool {
    std::env::var_os("SLAQ_LOGIN_DEBUG")
        .map(|value| !value.is_empty())
        .unwrap_or(false)
}

fn find_browser() -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in BROWSER_CANDIDATES {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn is_complete_url(url: &str, match_host: Option<&str>) -> bool {
    if url.contains("app.slack.com/client") {
        return true;
    }
    match match_host {
        Some(host) if !host.is_empty() => url.contains(host) && !url.contains("/signin"),
        _ => false,
    }
}

fn strip_query(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(parsed) => {
            let host = parsed.host_str().unwrap_or("");
            format!("{}{}", host, parsed.path())
        }
        Err(_) => "(unparseable-url)".to_string(),
    }
}

fn normalize_workspace(ws: &str) -> String {
    ws.trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

fn start_url(workspace: Option<&str>) -> String {
    match workspace {
        Some(ws) => format!("https://{}/", normalize_workspace(ws)),
        None => "https://slack.com/signin".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_token_in_form_body() {
        let body = "token=xoxc-1234-abcdef&foo=bar";
        assert_eq!(
            find_xoxc(&percent_decode(body)).as_deref(),
            Some("xoxc-1234-abcdef")
        );
    }

    #[test]
    fn finds_token_in_authorization_header() {
        assert_eq!(
            find_xoxc("Bearer xoxc-999-xyz").as_deref(),
            Some("xoxc-999-xyz")
        );
    }

    #[test]
    fn decodes_percent_encoded_token() {
        let body = "token=xoxc-12%2D34-abc";
        assert_eq!(
            find_xoxc(&percent_decode(body)).as_deref(),
            Some("xoxc-12-34-abc")
        );
    }

    #[test]
    fn ignores_bare_needle() {
        assert_eq!(find_xoxc("hello xoxc- world"), None);
        assert_eq!(find_xoxc("nothing here"), None);
    }

    #[test]
    fn scans_request_url_postdata_and_headers() {
        let request = serde_json::json!({
            "url": "https://example.slack.com/api/foo",
            "postData": "token=xoxc-abc-123&x=1",
            "headers": { "Authorization": "Bearer xoxc-should-not-win" }
        });
        assert_eq!(scan_request(&request).as_deref(), Some("xoxc-abc-123"));
    }
}
