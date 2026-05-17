//! Local Warp server emulator for offline/AI-proxy mode.
//!
//! When `~/.warp/setting.json` exists with DeepSeek credentials, this module
//! starts a local HTTP server that emulates the Warp server API:
//! 1. `/ai/multi-agent` — routes to DeepSeek API
//! 2. `/ai/*` — empty stubs
//! 3. `/graphql/v2*` — returns stub responses for all queries
//! 4. `/client/login` — 200 OK no-op
//! 5. `/api/v1/*` — empty stubs
//! 6. Everything else — 200 OK empty

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::body::Body;
use axum::extract::State;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, post};
use axum::Router;
use base64::Engine;
use futures::StreamExt;
use prost::Message;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use warp_multi_agent_api as api;

/// Configuration loaded from `~/.warp/setting.json`
#[derive(Debug, Clone, Deserialize)]
pub struct LocalAiConfig {
    pub ai_provider: Option<String>,
    pub deepseek_api_key: Option<String>,
    pub deepseek_model: Option<String>,
    pub deepseek_base_url: Option<String>,
    pub appearance: Option<AppearanceConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppearanceConfig {
    pub app_name: Option<String>,
    pub hide_login_screen: Option<bool>,
}

impl LocalAiConfig {
    /// Load config from ~/.warp/setting.json
    pub fn load() -> Self {
        let path = dirs::home_dir()
            .map(|p| p.join(".warp").join("setting.json"))
            .unwrap_or_else(|| PathBuf::from("/nonexistent"));

        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(config) => {
                    eprintln!("[warp-oss] Loaded config from {:?}", path);
                    config
                }
                Err(e) => {
                    eprintln!("[warp-oss] Failed to parse config: {e}");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn has_deepseek_config(&self) -> bool {
        self.deepseek_api_key
            .as_ref()
            .map_or(false, |k| !k.is_empty())
    }

    pub fn deepseek_base_url(&self) -> &str {
        self.deepseek_base_url
            .as_deref()
            .unwrap_or("https://api.deepseek.com/v1")
    }

    pub fn deepseek_model(&self) -> &str {
        self.deepseek_model
            .as_deref()
            .unwrap_or("deepseek-v4-flash")
    }
}

impl Default for LocalAiConfig {
    fn default() -> Self {
        Self {
            ai_provider: None,
            deepseek_api_key: None,
            deepseek_model: None,
            deepseek_base_url: None,
            appearance: None,
        }
    }
}

struct ProxyState {
    config: LocalAiConfig,
}

/// Start the local server. Returns (port, shutdown_token).
pub async fn start_proxy(config: LocalAiConfig) -> Result<(u16, CancellationToken)> {
    let shutdown_token = CancellationToken::new();
    let state = Arc::new(ProxyState { config });

    let app = Router::new()
        .route("/ai/multi-agent", post(handle_ai_multi_agent))
        .route("/ai/passive-suggestions", post(handle_passive_suggestions))
        .route("/client/login", post(|| async { (axum::http::StatusCode::OK, "{}") }))
        .route("/{*path}", any(handle_all))
        .with_state(state);

    let mut port: u16 = 18080;
    let listener = loop {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => break l,
            Err(_) => {
                port += 1;
                if port > 18100 {
                    return Err(anyhow::anyhow!("No free port found"));
                }
            }
        }
    };

    let addr = listener.local_addr()?;
    eprintln!("[warp-oss] Proxy listening on http://127.0.0.1:{}", addr.port());

    let token = shutdown_token.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { token.cancelled().await })
            .await
            .ok();
    });

    Ok((addr.port(), shutdown_token))
}

// ─── SSE helpers ─────────────────────────────────────────────────────────
// Format: `data: "<base64url-protobuf>"` per event

fn encode_event(event: &api::ResponseEvent) -> Vec<u8> {
    let mut buf = Vec::new();
    event.encode(&mut buf).unwrap();
    let b64 = base64::engine::general_purpose::URL_SAFE.encode(&buf);
    format!("data: \"{b64}\"\n\n").into_bytes()
}

fn init_event(cid: &str, rid: &str, run: &str) -> api::ResponseEvent {
    api::ResponseEvent {
        r#type: Some(api::response_event::Type::Init(api::response_event::StreamInit {
            conversation_id: cid.into(),
            request_id: rid.into(),
            run_id: run.into(),
        })),
    }
}

fn action_event(action: api::client_action::Action) -> api::ResponseEvent {
    api::ResponseEvent {
        r#type: Some(api::response_event::Type::ClientActions(
            api::response_event::ClientActions {
                actions: vec![api::ClientAction {
                    action: Some(action),
                }],
            },
        )),
    }
}

fn finish_event() -> api::ResponseEvent {
    api::ResponseEvent {
        r#type: Some(api::response_event::Type::Finished(
            api::response_event::StreamFinished {
                reason: Some(api::response_event::stream_finished::Reason::Done(
                    api::response_event::stream_finished::Done {},
                )),
                ..Default::default()
            },
        )),
    }
}

// ─── AI endpoint handlers ─────────────────────────────────────────────────

async fn handle_ai_multi_agent(
    state: axum::extract::State<Arc<ProxyState>>,
    req: Request,
) -> Response {
    if !state.config.has_deepseek_config() {
        return error_stream("DeepSeek not configured");
    }

    let body = match axum::body::to_bytes(req.into_body(), 10_485_760).await {
        Ok(b) => b,
        Err(e) => return error_stream(&e.to_string()),
    };

    let proto = match api::Request::decode(&body[..]) {
        Ok(r) => r,
        Err(e) => return error_stream(&format!("protobuf: {e}")),
    };

    let query = extract_query(&proto);
    if query.is_empty() {
        return empty_stream();
    }

    let api_key = state.config.deepseek_api_key.as_deref().unwrap_or("");
    let model = state.config.deepseek_model();
    let base_url = state.config.deepseek_base_url();

    let payload = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "You are a helpful AI assistant (Oz) in the Warp terminal."},
            {"role": "user", "content": query}
        ],
        "stream": true,
        "max_tokens": 8192,
    });

    let client = reqwest::Client::new();
    let resp = match client
        .post(format!("{base_url}/chat/completions"))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return error_stream(&format!("DeepSeek error: {e}")),
    };

    if !resp.status().is_success() {
        let s = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        return error_stream(&format!("DeepSeek returned {s}: {txt}"));
    }

    let mut full_text = String::new();
    let mut ds_stream = resp.bytes_stream();
    while let Some(chunk) = ds_stream.next().await {
        if let Ok(chunk) = chunk {
            let s = String::from_utf8_lossy(&chunk);
            for line in s.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" { continue; }
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
                        if let Some(d) = v["choices"][0]["delta"]["content"].as_str() {
                            full_text.push_str(d);
                        }
                    }
                }
            }
        }
    }

    let cid = uuid::Uuid::new_v4().to_string();
    let rid = uuid::Uuid::new_v4().to_string();
    let tid = uuid::Uuid::new_v4().to_string();
    let mid = uuid::Uuid::new_v4().to_string();

    let mut out = Vec::new();
    out.extend_from_slice(&encode_event(&init_event(&cid, &rid, &tid)));
    out.extend_from_slice(&encode_event(&action_event(
        api::client_action::Action::CreateTask(api::client_action::CreateTask {
            task: Some(api::Task {
                id: tid.clone(),
                description: String::new(),
                dependencies: None,
                messages: vec![],
                summary: String::new(),
                server_data: String::new(),
            }),
        }),
    )));
    out.extend_from_slice(&encode_event(&action_event(
        api::client_action::Action::AddMessagesToTask(api::client_action::AddMessagesToTask {
            task_id: tid.clone(),
            messages: vec![api::Message {
                id: mid,
                task_id: tid,
                request_id: rid,
                timestamp: None,
                server_message_data: String::new(),
                citations: vec![],
                message: Some(api::message::Message::AgentOutput(
                    api::message::AgentOutput { text: full_text },
                )),
            }],
        }),
    )));
    out.extend_from_slice(&encode_event(&finish_event()));

    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from(out))
        .unwrap()
}

async fn handle_passive_suggestions() -> Response {
    empty_stream()
}

// ─── Catch-all handler ───────────────────────────────────────────────────

async fn handle_all(
    State(state): axum::extract::State<Arc<ProxyState>>,
    req: Request,
) -> Response {
    let path = req.uri().path().to_string();

    if path.contains("/graphql") {
        return handle_graphql(req).await;
    }
    if path.contains("/ai/") {
        return empty_stream();
    }
    if path.contains("/api/v1") {
        return (axum::http::StatusCode::OK, "{}").into_response();
    }
    // everything else
    (axum::http::StatusCode::OK, "").into_response()
}

async fn handle_graphql(req: Request) -> Response {
    let op = req.uri()
        .query()
        .and_then(|q| {
            for pair in q.split('&') {
                if let Some(val) = pair.strip_prefix("op=") {
                    return Some(urlencoding::decode(val).ok()?.into_owned());
                }
            }
            None
        })
        .unwrap_or_default();

    let body = match op.as_str() {
        "getUser" => r#"{"data":{"user":{"__typename":"UserOutput","apiKeyOwnerType":null,"principalType":"USER","user":{"anonymousUserInfo":null,"experiments":[],"globalSkills":[],"isOnboarded":true,"isOnWorkDomain":false,"profile":{"displayName":"Local User","email":"local@warp.dev","needsSsoLink":false,"photoUrl":null,"uid":"local-user-1"},"llms":{"agentMode":{"defaultId":"deepseek-v4-flash","choices":[{"id":"local-deepseek","displayName":"DeepSeek V4 Flash","baseModelName":"deepseek-v4-flash","reasoningLevel":null,"description":"Local AI via DeepSeek","disableReason":null,"visionSupported":true,"onboardingInfo":null}]}}}}}}"#,
        "getUserSettings" => r#"{"data":{"userSettings":null}}"#,
        "getDiscoverableTeams" => r#"{"data":{"discoverableTeams":[]}}"#,
        "getBlocksForUser" => r#"{"data":{"blocksForUser":[]}}"#,
        _ => r#"{"data":{}}"#,
    };

    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

// ─── Helpers ──────────────────────────────────────────────────────────────

fn extract_query(req: &api::Request) -> String {
    if let Some(input) = &req.input {
        if let Some(ty) = &input.r#type {
            match ty {
                api::request::input::Type::UserInputs(inputs) => {
                    for inp in inputs.inputs.iter().rev() {
                        if let Some(api::request::input::user_inputs::user_input::Input::UserQuery(q)) = &inp.input {
                            return q.query.clone();
                        }
                    }
                }
                _ => {}
            }
        }
    }
    // fallback: latest UserQuery from tasks
    if let Some(tc) = &req.task_context {
        for task in tc.tasks.iter().rev() {
            for msg in task.messages.iter().rev() {
                if let Some(api::message::Message::UserQuery(q)) = &msg.message {
                    return q.query.clone();
                }
            }
        }
    }
    String::new()
}

fn empty_stream() -> Response {
    let cid = uuid::Uuid::new_v4().to_string();
    let rid = uuid::Uuid::new_v4().to_string();
    let tid = uuid::Uuid::new_v4().to_string();
    let mut out = Vec::new();
    out.extend_from_slice(&encode_event(&init_event(&cid, &rid, &tid)));
    out.extend_from_slice(&encode_event(&finish_event()));
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from(out))
        .unwrap()
}

fn error_stream(msg: &str) -> Response {
    let cid = uuid::Uuid::new_v4().to_string();
    let rid = uuid::Uuid::new_v4().to_string();
    let tid = uuid::Uuid::new_v4().to_string();
    let mid = uuid::Uuid::new_v4().to_string();
    let mut out = Vec::new();
    out.extend_from_slice(&encode_event(&init_event(&cid, &rid, &tid)));
    out.extend_from_slice(&encode_event(&action_event(
        api::client_action::Action::AddMessagesToTask(api::client_action::AddMessagesToTask {
            task_id: tid.clone(),
            messages: vec![api::Message {
                id: mid,
                task_id: tid,
                request_id: rid,
                timestamp: None,
                server_message_data: String::new(),
                citations: vec![],
                message: Some(api::message::Message::AgentOutput(
                    api::message::AgentOutput { text: format!("Error: {msg}") },
                )),
            }],
        }),
    )));
    out.extend_from_slice(&encode_event(&finish_event()));
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from(out))
        .unwrap()
}
