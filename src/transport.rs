use crate::{
    config::{Config, Credential, cookie_value},
    error::{Fault, Result},
    guard::{Lease, Scope},
    store::{now, uuid},
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{pin::Pin, time::Duration};
use tokio::sync::Mutex;
use wreq::{Client, IntoEmulation, Method, header::OrigHeaderMap};
use wreq_util::Emulation;
pub fn error_code(event: &Value) -> &str {
    event
        .pointer("/error/error_type")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| event.pointer("/error/type").and_then(Value::as_str))
        .unwrap_or("")
}
pub fn error_status(code: &str) -> u16 {
    match code {
        "rate_limit_error" => 429,
        "authentication_error" => 401,
        "permission_error" => 403,
        "overloaded_error" => 529,
        _ => 502,
    }
}

pub struct CompletionRequest<'a> {
    pub target: &'a str,
    pub parent: &'a str,
    pub model: &'a str,
    pub prompt: &'a str,
    pub tools: Value,
    pub thinking: &'a str,
}

// Completions target an already-created conversation. Creation preferences and
// unrelated UI feature switches do not belong to this request.
#[derive(Serialize)]
struct CompletionBody<'a> {
    prompt: &'a str,
    model: &'a str,
    parent_message_uuid: String,
    turn_message_uuids: MessageIds,
    tools: Value,
    locale: &'a str,
    timezone: &'a str,
    effort: &'a str,
    thinking_mode: &'a str,
    rendering_mode: &'static str,
    attachments: [Value; 0],
    files: [Value; 0],
    sync_sources: [Value; 0],
}

#[derive(Serialize)]
struct MessageIds {
    human_message_uuid: String,
    assistant_message_uuid: String,
}

impl CompletionBody<'_> {
    fn payload(&self) -> Value {
        serde_json::to_value(self).expect("serializable completion request")
    }
}
pub struct WebClient {
    http: Client,
    base: String,
    cookie: String,
    device: String,
    locale: String,
    timezone: String,
    effort: String,
    org: Mutex<String>,
    catalog: Mutex<(i64, Vec<String>)>,
}
struct WebResponse {
    response: wreq::Response,
    span: Option<crate::observability::Span>,
}
impl std::ops::Deref for WebResponse {
    type Target = wreq::Response;
    fn deref(&self) -> &Self::Target {
        &self.response
    }
}
impl WebClient {
    pub fn new(
        config: &Config,
        credential: &Credential,
        transport: &crate::browser::TransportSettings,
    ) -> Result<Self> {
        transport.validate()?;
        let profile = transport
            .browser_profile
            .unwrap_or(config.browser_profile)
            .native();
        let emulation = Emulation::builder()
            .profile(profile)
            .platform(wreq_util::Platform::Windows)
            .build();
        let mut emulation: wreq::Emulation = emulation.into_emulation();
        // These presets describe document navigation. API fetches retain native
        // UA/CH, compression, TLS and HTTP2, with their own fetch metadata below.
        const NAVIGATION_HEADERS: &[&str] =
            &["upgrade-insecure-requests", "sec-fetch-user", "priority"];
        for name in NAVIGATION_HEADERS {
            emulation.headers.remove(*name);
        }
        let mut order = OrigHeaderMap::new();
        for (name, original) in emulation.orig_headers.iter() {
            if !NAVIGATION_HEADERS.contains(&name.as_str()) {
                order.insert(original);
            }
        }
        emulation.orig_headers = order;
        let mut builder = Client::builder()
            .emulation(emulation)
            .no_proxy()
            .redirect(wreq::redirect::Policy::none())
            .timeout(Duration::from_secs(900))
            .cookie_store(true);
        if let Some(proxy) = &transport.proxy_url {
            builder = builder.proxy(
                wreq::Proxy::all(proxy.as_str())
                    .map_err(|_| Fault::invalid("invalid account proxy URL"))?,
            );
        }
        let http = builder
            .build()
            .map_err(|_| Fault::new(503, "api_error", "browser transport initialization failed"))?;
        let device = cookie_value(&credential.cookie, "anthropic-device-id").unwrap_or_else(|| {
            let mut bytes =
                Sha256::digest(format!("claude2api-device:{}", credential.session_key)).to_vec();
            bytes[6] = (bytes[6] & 15) | 64;
            bytes[8] = (bytes[8] & 63) | 128;
            uuid::Uuid::from_slice(&bytes[..16])
                .expect("16 bytes")
                .to_string()
        });
        let cookie = if credential.cookie.is_empty() {
            format!(
                "sessionKey={}; anthropic-device-id={device}",
                credential.session_key
            )
        } else {
            credential.cookie.clone()
        };
        Ok(Self {
            http,
            base: config.base_url.clone(),
            cookie,
            device,
            locale: config.locale.clone(),
            timezone: config.timezone.clone(),
            effort: config.effort.clone(),
            org: Mutex::new(cookie_value(&credential.cookie, "lastActiveOrg").unwrap_or_default()),
            catalog: Mutex::new((0, vec![])),
        })
    }
    async fn request(
        &self,
        lease: &Lease,
        scope: &Scope,
        method: Method,
        path: &str,
        referer: &str,
        payload: Option<&Value>,
    ) -> Result<WebResponse> {
        lease.check().await?;
        let mode = if path.ends_with("/completion") {
            "sse"
        } else {
            "json"
        };
        let mut span = scope.trace.as_ref().map(|trace| {
            trace.span(
                "upstream",
                mode,
                crate::observability::operation(path, method.as_str()),
                &lease.account_id,
            )
        });
        if path.ends_with("/completion")
            && let Some(prompt) = payload.and_then(|v| v["prompt"].as_str())
            && let Some(span) = &mut span
        {
            span.input_tokens(crate::protocol::estimate(prompt));
        }
        let language = self
            .locale
            .split_once('-')
            .map(|(base, _)| format!("{},{base};q=0.9", self.locale))
            .unwrap_or_else(|| self.locale.clone());
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header("origin", &self.base)
            .header("referer", format!("{}{referer}", self.base))
            .header("cookie", &self.cookie)
            .header("anthropic-client-platform", "web_claude_ai")
            .header("anthropic-device-id", &self.device)
            .header("accept-language", language)
            .header(
                "accept",
                if path.ends_with("/completion") {
                    "text/event-stream"
                } else {
                    "*/*"
                },
            )
            .header("sec-fetch-dest", "empty")
            .header("sec-fetch-mode", "cors")
            .header("sec-fetch-site", "same-origin")
            .header("content-type", "application/json");
        if let Some(body) = payload {
            request = request.json(body);
        }
        let response = match scope
            .wait(async { request.send().await.map_err(|_| Fault::upstream()) })
            .await
        {
            Ok(response) => response,
            Err(f) => {
                if let Some(span) = &mut span {
                    span.failed(&f);
                }
                return Err(f);
            }
        };
        let status = response.status().as_u16();
        if let Some(span) = &mut span {
            span.headers(status);
        }
        let retry = response
            .headers()
            .get("retry-after")
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned);
        if (300..400).contains(&status)
            || response
                .headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok())
                .is_some_and(|s| s.to_ascii_lowercase().starts_with("text/html"))
        {
            if let Some(span) = &mut span {
                span.finish(403);
            }
            drop(response);
            return Err(lease.failure(403, None).await?);
        }
        if status >= 400 {
            if let Some(span) = &mut span {
                span.finish(status);
            }
            drop(response);
            return Err(lease.failure(status, retry.as_deref()).await?);
        }
        Ok(WebResponse { response, span })
    }
    async fn decode(response: WebResponse, scope: &Scope) -> Result<Value> {
        let WebResponse { response, mut span } = response;
        let status = response.status().as_u16();
        let result = async {
            let mut stream = response.bytes_stream();
            let mut body = Vec::new();
            while let Some(chunk) = scope.wait(async { Ok(stream.next().await) }).await? {
                let chunk = chunk.map_err(|_| Fault::upstream())?;
                if let Some(span) = &mut span {
                    span.chunk(chunk.len());
                }
                if body.len() + chunk.len() > 4 << 20 {
                    return Err(Fault::upstream());
                }
                body.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&body).map_err(|_| Fault::upstream())
        }
        .await;
        if let Some(span) = &mut span {
            span.finish(result.as_ref().err().map_or(status, |f| f.status));
        }
        result
    }
    pub async fn organization(&self, lease: &Lease, scope: &Scope) -> Result<String> {
        let mut org = self.org.lock().await;
        if !org.is_empty() {
            return Ok(org.clone());
        }
        let response = self
            .request(
                lease,
                scope,
                Method::GET,
                "/api/organizations",
                "/new",
                None,
            )
            .await?;
        let value = Self::decode(response, scope).await?;
        let id = value
            .get(0)
            .and_then(|v| v["uuid"].as_str())
            .filter(|v| !v.is_empty() && v.len() <= 200)
            .ok_or_else(Fault::upstream)?;
        *org = id.to_owned();
        Ok(org.clone())
    }
    pub async fn models(&self, lease: &Lease, scope: &Scope, refresh: bool) -> Result<Vec<String>> {
        let mut cache = self.catalog.lock().await;
        if !refresh && cache.0 > now() {
            return Ok(cache.1.clone());
        }
        *cache = (0, vec![]);
        let org = self.organization(lease, scope).await?;
        let org = url::form_urlencoded::byte_serialize(org.as_bytes()).collect::<String>();
        let path = format!(
            "/edge-api/bootstrap/{org}/app_start?statsig_hashing_algorithm=djb2&growthbook_format=sdk&cache_bust=1&include_system_prompts=false"
        );
        let response = self
            .request(lease, scope, Method::GET, &path, "/new", None)
            .await?;
        let data = Self::decode(response, scope).await?;
        let list = data
            .pointer("/claude_ai_available_models/models")
            .and_then(Value::as_array)
            .ok_or_else(Fault::upstream)?;
        let mut ids = Vec::new();
        for item in list {
            if let Some(id) = item["model_id"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 150)
                && !ids.iter().any(|v| v == id)
            {
                ids.push(id.to_owned());
            }
        }
        if ids.is_empty() {
            return Err(Fault::upstream());
        }
        *cache = (now() + 300, ids.clone());
        Ok(ids)
    }
    pub async fn create(&self, lease: &Lease, scope: &Scope) -> Result<String> {
        let org = self.organization(lease, scope).await?;
        let path = format!("/api/organizations/{org}/chat_conversations");
        let payload = json!({"name":"Claud2Api","organization_uuid":org});
        let response = self
            .request(lease, scope, Method::POST, &path, "/new", Some(&payload))
            .await?;
        if !matches!(response.status().as_u16(), 200 | 201) {
            return Err(Fault::upstream());
        }
        let data = Self::decode(response, scope).await?;
        let id = data["uuid"]
            .as_str()
            .filter(|s| uuid::Uuid::parse_str(s).is_ok())
            .ok_or_else(Fault::upstream)?;
        Ok(id.to_owned())
    }
    pub async fn delete(&self, lease: &Lease, scope: &Scope, id: &str) -> Result<()> {
        if uuid::Uuid::parse_str(id).is_err() {
            return Err(Fault::invalid("invalid physical conversation id"));
        }
        let org = self.organization(lease, scope).await?;
        let path = format!("/api/organizations/{org}/chat_conversations/{id}");
        match self
            .request(
                lease,
                scope,
                Method::DELETE,
                &path,
                &format!("/chat/{id}"),
                None,
            )
            .await
        {
            Ok(mut r) if matches!(r.status().as_u16(), 200 | 204) => {
                let status = r.status().as_u16();
                if let Some(span) = &mut r.span {
                    span.finish(status);
                }
                Ok(())
            }
            Err(f) if f.status == 404 => Ok(()),
            Err(f) => Err(f),
            _ => Err(Fault::upstream()),
        }
    }
    pub async fn completion(
        &self,
        lease: &Lease,
        scope: &Scope,
        request: CompletionRequest<'_>,
    ) -> Result<Completion> {
        let CompletionRequest {
            target: id,
            parent,
            model,
            prompt,
            tools: native_tools,
            thinking,
        } = request;
        let org = self.organization(lease, scope).await?;
        let assistant = uuid();
        let payload = CompletionBody {
            prompt,
            model,
            parent_message_uuid: if parent.is_empty() {
                uuid()
            } else {
                parent.to_owned()
            },
            turn_message_uuids: MessageIds {
                human_message_uuid: uuid(),
                assistant_message_uuid: assistant.clone(),
            },
            tools: native_tools,
            locale: &self.locale,
            timezone: &self.timezone,
            effort: &self.effort,
            thinking_mode: thinking,
            rendering_mode: "messages",
            attachments: [],
            files: [],
            sync_sources: [],
        }
        .payload();
        let path = format!("/api/organizations/{org}/chat_conversations/{id}/completion");
        let response = self
            .request(
                lease,
                scope,
                Method::POST,
                &path,
                &format!("/chat/{id}"),
                Some(&payload),
            )
            .await?;
        if response.status().as_u16() != 200 {
            return Err(Fault::upstream());
        }
        Ok(Completion {
            stream: Box::pin(response.response.bytes_stream()),
            span: response.span,
            decoder: SseDecoder::default(),
            assistant,
            pending: std::collections::VecDeque::new(),
            ended: false,
            output_chars: 0,
        })
    }
}
pub struct Completion {
    stream: Pin<Box<dyn Stream<Item = std::result::Result<Bytes, wreq::Error>> + Send>>,
    decoder: SseDecoder,
    pub assistant: String,
    pending: std::collections::VecDeque<Value>,
    ended: bool,
    output_chars: usize,
    span: Option<crate::observability::Span>,
}
impl Completion {
    pub async fn next(&mut self, scope: &Scope) -> Result<Value> {
        let result = self.next_inner(scope).await;
        if let Ok(event) = &result {
            for key in ["text", "thinking", "partial_json"] {
                if let Some(text) = event["delta"][key].as_str() {
                    self.output_chars = self.output_chars.saturating_add(text.chars().count());
                }
            }
            if (self.output_chars > 0 || event["type"] == "message_stop")
                && let Some(span) = &mut self.span
            {
                span.output_tokens(
                    self.output_chars.div_ceil(4),
                    event["type"] == "message_stop",
                );
            }
        }
        if let Some(span) = &mut self.span {
            match &result {
                Ok(event) if event["type"] == "message_stop" => span.finish(200),
                Ok(event)
                    if event["type"] == "error"
                        || event.get("error").is_some_and(|v| !v.is_null()) =>
                {
                    span.finish(error_status(error_code(event)))
                }
                Err(f) => span.failed(f),
                _ => {}
            }
        }
        result
    }
    async fn next_inner(&mut self, scope: &Scope) -> Result<Value> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(event);
            }
            if self.ended {
                return Err(Fault::new(
                    502,
                    "api_error",
                    "upstream stream was truncated",
                ));
            }
            match scope
                .wait(async {
                    self.stream
                        .next()
                        .await
                        .transpose()
                        .map_err(|_| Fault::upstream())
                })
                .await?
            {
                Some(chunk) => {
                    if let Some(span) = &mut self.span {
                        span.chunk(chunk.len());
                    }
                    self.pending.extend(self.decoder.feed(&chunk)?);
                }
                None => {
                    self.ended = true;
                    self.pending.extend(self.decoder.finish()?);
                }
            }
        }
    }
}
#[derive(Default)]
pub struct SseDecoder {
    line: Vec<u8>,
    kind: String,
    data: String,
}
impl SseDecoder {
    fn line(&mut self) -> Result<Option<Value>> {
        let line = std::str::from_utf8(&self.line)
            .map_err(|_| Fault::upstream())?
            .trim_end_matches('\r')
            .to_owned();
        self.line.clear();
        if line.is_empty() {
            return self.dispatch();
        }
        if let Some(kind) = line.strip_prefix("event:") {
            self.kind = kind.trim().to_owned();
        }
        if let Some(data) = line.strip_prefix("data:") {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
            if self.data.len() > 2 << 20 {
                return Err(Fault::upstream());
            }
        }
        Ok(None)
    }
    fn dispatch(&mut self) -> Result<Option<Value>> {
        if self.data.is_empty() {
            return Ok(None);
        }
        let mut value: Value = serde_json::from_str(&self.data)
            .map_err(|_| Fault::new(502, "api_error", "invalid upstream event"))?;
        self.data.clear();
        if !value.is_object() {
            return Err(Fault::upstream());
        }
        if value.get("type").is_none() {
            value["type"] = json!(self.kind);
        }
        self.kind.clear();
        Ok(Some(value))
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Value>> {
        let mut events = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                if let Some(e) = self.line()? {
                    events.push(e);
                }
            } else {
                self.line.push(b);
                if self.line.len() > 2 << 20 {
                    return Err(Fault::upstream());
                }
            }
        }
        Ok(events)
    }
    pub fn finish(&mut self) -> Result<Vec<Value>> {
        let mut events = Vec::new();
        if !self.line.is_empty()
            && let Some(e) = self.line()?
        {
            events.push(e);
        }
        if let Some(e) = self.dispatch()? {
            events.push(e);
        }
        Ok(events)
    }
}
