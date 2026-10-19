use crate::observability::{REQUEST_ID, RequestTrace};
use crate::{
    engine::{Engine, Output},
    error::{Fault, Result},
    protocol::{self, Block},
    store::nonce,
};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Extension, Request, State},
    http::{HeaderMap, HeaderValue},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{convert::Infallible, sync::Arc};
use subtle::ConstantTimeEq;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub fn equal(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}
pub fn provided_key(headers: &HeaderMap) -> Option<&str> {
    if headers.get_all("x-api-key").iter().count() > 1
        || headers.get_all("authorization").iter().count() > 1
    {
        return None;
    }
    let api = headers.get("x-api-key").and_then(|h| h.to_str().ok());
    let bearer = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if headers.contains_key("authorization") && bearer.is_none() {
        return None;
    }
    if let (Some(a), Some(b)) = (api, bearer)
        && !equal(a, b)
    {
        return None;
    }
    api.or(bearer).filter(|s| !s.is_empty())
}
pub fn authenticated(headers: &HeaderMap, key: &str) -> bool {
    provided_key(headers).is_some_and(|s| equal(s, key))
}
async fn auth(State(e): State<Arc<Engine>>, req: Request, next: Next) -> Response {
    let Some(key) = provided_key(req.headers()) else {
        return Fault::new(401, "authentication_error", "invalid API token").into_response();
    };
    let admission = match e.tokens.admit(key).await {
        Ok(guard) => guard,
        Err(f) => {
            if let Some(trace) = req.extensions().get::<Arc<RequestTrace>>()
                && let Some((id, label)) = e.tokens.identify(key).await
            {
                trace.bind_token(id);
                trace.bind_token_name(&label);
            }
            return f.into_response();
        }
    };
    if let Some(trace) = req.extensions().get::<Arc<RequestTrace>>() {
        trace.bind_token(admission.id.clone());
        trace.bind_token_name(&admission.label);
    }
    let settings = e.store.read().await.settings;
    let max = settings.max_request_bytes;
    if req
        .headers()
        .get("content-length")
        .and_then(|h| h.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|n| n > max)
    {
        return Fault::new(413, "request_too_large", "request body exceeds limit").into_response();
    }
    let (parts, body) = req.into_parts();
    let body = match tokio::time::timeout(
        std::time::Duration::from_secs(settings.request_timeout_seconds.min(30)),
        axum::body::to_bytes(body, max),
    )
    .await
    {
        Ok(Ok(b)) => b,
        Ok(Err(_)) => {
            return Fault::new(413, "request_too_large", "request body exceeds limit")
                .into_response();
        }
        Err(_) => return Fault::timeout().into_response(),
    };
    let mut response = next.run(Request::from_parts(parts, Body::from(body))).await;
    if !response.headers().contains_key("cache-control") {
        response
            .headers_mut()
            .insert("cache-control", HeaderValue::from_static("no-store"));
    }
    if response
        .headers()
        .get("content-type")
        .is_some_and(|v| v == "application/json")
    {
        response.headers_mut().insert(
            "content-type",
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
    }
    if admission.id.is_none() {
        return response;
    }
    let (parts, body) = response.into_parts();
    let output = async_stream::stream! {let _admission=admission;let mut input=body.into_data_stream();while let Some(frame)=input.next().await {yield frame;}};
    Response::from_parts(parts, Body::from_stream(output))
}
async fn not_found() -> Fault {
    Fault::new(404, "not_found_error", "unknown endpoint")
}
async fn observe(State(e): State<Arc<Engine>>, mut req: Request, next: Next) -> Response {
    let route = match req.uri().path() {
        "/v1/messages" => "messages",
        "/v1/chat/completions" => "chat",
        "/v1/responses" => "responses",
        "/v1/models" => "models",
        "/health" => "health",
        p if p.starts_with("/v1/conversations/") => "conversation_delete",
        _ => "unknown",
    };
    if route == "health" {
        return next.run(req).await;
    }
    let trace = RequestTrace::new(&e.store, route);
    req.extensions_mut().insert(trace.clone());
    let mut span = trace.span("downstream", "json", "client_request", "");
    let mut response = REQUEST_ID.scope(trace.id.clone(), next.run(req)).await;
    let request_id = trace.id.clone();
    response.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(&request_id).expect("generated ID"),
    );
    let status = response.status().as_u16();
    trace.streaming(
        response
            .headers()
            .get("content-type")
            .is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream")),
    );
    span.headers(status);
    let (parts, body) = response.into_parts();
    let mut input = body.into_data_stream();
    let output = async_stream::stream! {
        while let Some(chunk)=input.next().await {
            match chunk {
                Ok(bytes)=>{span.chunk(bytes.len());yield Ok::<_,axum::Error>(bytes);},
                Err(error)=>{span.finish(502);yield Err(error);return;}
            }
        }
        let outcome=trace.outcome();
        span.finish(if outcome>0{outcome}else{status});
    };
    Response::from_parts(parts, Body::from_stream(output))
}
pub fn public(e: Arc<Engine>) -> Router {
    let api = Router::new()
        .route("/messages", post(messages))
        .route("/models", get(models))
        .route("/chat/completions", post(crate::compat::chat))
        .route("/responses", post(crate::compat::responses))
        .route("/conversations/{id}", delete(crate::compat::remove))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(e.clone(), auth));
    Router::new()
        .route("/health", get(health))
        .nest("/v1", api)
        .fallback(not_found)
        .layer(DefaultBodyLimit::disable())
        .layer(middleware::from_fn_with_state(e.clone(), observe))
        .with_state(e)
}
async fn health(State(e): State<Arc<Engine>>) -> Response {
    if e.store.healthy() {
        Json(json!({"status":"ok"})).into_response()
    } else {
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status":"degraded"})),
        )
            .into_response()
    }
}
pub fn parse_json(bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|_| Fault::invalid("invalid JSON request body"))
}
async fn messages(
    State(e): State<Arc<Engine>>,
    Extension(trace): Extension<Arc<RequestTrace>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Response> {
    let raw = parse_json(&bytes)?;
    trace.requested_model(raw["model"].as_str().unwrap_or(""));
    let mut input = protocol::parse(&raw)?;
    let selected = e
        .store
        .read()
        .await
        .models
        .resolve(raw["model"].as_str().unwrap_or(""), "")?;
    input.model = selected.upstream;
    let stream = input.stream;
    let model = selected.public;
    let mut identity = protocol::identity(&headers, &raw, &e.config.gateway_key);
    // Returned opaque handles must resolve directly before hashing caller fingerprints.
    if let Some(handle) = headers
        .get("x-claud2api-session-id")
        .or_else(|| headers.get("x-c2a-session-id"))
        .and_then(|v| v.to_str().ok())
        && let Some(session) = e.store.read().await.sessions.get(handle)
    {
        if session.token_id != trace.token_id() {
            return Err(Fault::new(
                403,
                "permission_error",
                "session is not available to this token",
            ));
        }
        identity = handle.to_owned();
    }
    trace.streaming(stream);
    let mut reservation = e.reserve_owned(input, identity, trace.token_id()).await?;
    reservation.scope.trace = Some(trace.clone());
    if let Some(lease) = &reservation.lease {
        trace.bind_account(&lease.account_id);
    }
    let settings = reservation.settings.clone();
    let handle = reservation.session.id.clone();
    let scope = reservation.scope.clone();
    let guard = CancelOnDrop(scope.cancel.clone());
    let (tx, mut rx) = mpsc::channel(16);
    tokio::spawn(e.run(reservation, tx));
    let mut first = Vec::new();
    // A pre-generation rejection retains an HTTP error; pings are needed only after 15s.
    let initial = tokio::select! {v=rx.recv()=>v,_=tokio::time::sleep(std::time::Duration::from_secs(15))=>None};
    if let Some(Output::Error(error)) = initial {
        return Err(error);
    } else if let Some(event) = initial {
        first.push(event);
    } else if rx.is_closed() {
        return Err(Fault::upstream());
    } else {
        first.push(Output::Ping);
    }
    let id = format!("msg_{}", nonce());
    let mut view = MessageView::new(id, model, settings.simulated_cache);
    view.request_id = trace.id.clone();
    let mut response = if stream {
        let stream = async_stream::stream! {
            let _guard=guard;let mut events=first.into_iter();
            loop{
                let event=if let Some(v)=events.next(){Some(v)}else{tokio::select!{v=rx.recv()=>v,_=tokio::time::sleep(std::time::Duration::from_secs(15))=>{yield Ok::<_,Infallible>(Bytes::from_static(b"event: ping\ndata: {\"type\":\"ping\"}\n\n"));continue;}}};
                let Some(event)=event else{break;};let terminal=matches!(event,Output::Done{..}|Output::Error(_));
                for value in view.event(event){yield Ok::<_,Infallible>(Bytes::from(format!("event: {}\ndata: {}\n\n",value["type"].as_str().unwrap_or("error"),value)));}
                if terminal{break;}
            }
        };
        let mut response = Body::from_stream(stream).into_response();
        response.headers_mut().insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );
        response
    } else {
        let _guard = guard;
        let mut done = false;
        for event in first {
            done |= matches!(event, Output::Done { .. });
            view.event(event);
        }
        while !done {
            let event = rx.recv().await.ok_or_else(Fault::upstream)?;
            match event {
                Output::Error(f) => return Err(f),
                Output::Done {
                    reason,
                    sequence,
                    usage,
                } => {
                    view.reason = reason;
                    view.sequence = sequence;
                    view.usage = usage;
                    done = true;
                }
                event => {
                    view.event(event);
                }
            }
        }
        Json(view.document()).into_response()
    };
    for (k, v) in [
        ("x-c2a-session-id", handle.as_str()),
        ("x-claud2api-session-id", handle.as_str()),
        (
            "x-claud2api-limitations",
            if settings.simulated_cache {
                "usage-estimated,max-tokens-advisory,text-tool-bridge,controls-not-forwarded,cache-simulated"
            } else {
                "usage-estimated,max-tokens-advisory,text-tool-bridge,controls-not-forwarded"
            },
        ),
        ("x-accel-buffering", "no"),
    ] {
        response
            .headers_mut()
            .insert(k, HeaderValue::from_str(v).map_err(|_| Fault::internal())?);
    }
    if settings.simulated_cache {
        response.headers_mut().insert(
            "x-claud2api-cache",
            HeaderValue::from_static("simulated-1h"),
        );
    }
    if settings.compaction_enabled {
        response.headers_mut().insert(
            "x-claud2api-context",
            HeaderValue::from_static("managed-estimates"),
        );
    }
    Ok(response)
}
pub struct CancelOnDrop(pub CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
pub struct MessageView {
    request_id: String,
    id: String,
    model: String,
    cache: bool,
    blocks: Vec<Block>,
    text_open: bool,
    started: bool,
    reason: String,
    sequence: Option<String>,
    usage: Value,
}
impl MessageView {
    pub fn new(id: String, model: String, cache: bool) -> Self {
        Self {
            request_id: format!("req_{}", nonce()),
            id,
            model,
            cache,
            blocks: vec![],
            text_open: false,
            started: false,
            reason: String::new(),
            sequence: None,
            usage: json!({}),
        }
    }
    fn close(&mut self, out: &mut Vec<Value>) {
        if self.text_open {
            out.push(json!({"type":"content_block_stop","index":self.blocks.len()-1}));
            self.text_open = false;
        }
    }
    pub fn event(&mut self, event: Output) -> Vec<Value> {
        let mut out = Vec::new();
        match event {
            Output::Ping => out.push(json!({"type":"ping"})),
            Output::Start { input, read } => {
                self.started = true;
                self.usage = protocol::usage(input, 0, self.cache, read);
                out.push(json!({"type":"message_start","message":{"id":self.id,"type":"message","role":"assistant","content":[],"model":self.model,"stop_reason":null,"stop_sequence":null,"usage":self.usage}}));
            }
            Output::Block(b) => match &b {
                Block::Text { text } => {
                    if !self.text_open {
                        let index = self.blocks.len();
                        self.blocks.push(Block::text(""));
                        self.text_open = true;
                        out.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}));
                    }
                    let index = self.blocks.len() - 1;
                    if let Some(Block::Text { text: all }) = self.blocks.last_mut() {
                        all.push_str(text);
                    }
                    out.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}));
                }
                Block::Tool { id, name, input } => {
                    self.close(&mut out);
                    let index = self.blocks.len();
                    out.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}));
                    out.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":protocol::go_json(input)}}));
                    out.push(json!({"type":"content_block_stop","index":index}));
                    self.blocks.push(b);
                }
                _ => {}
            },
            Output::Done {
                reason,
                sequence,
                usage,
            } => {
                self.close(&mut out);
                self.reason = reason;
                self.sequence = sequence;
                self.usage = usage;
                out.push(json!({"type":"message_delta","delta":{"stop_reason":self.reason,"stop_sequence":self.sequence},"usage":{"output_tokens":self.usage["output_tokens"]}}));
                out.push(json!({"type":"message_stop"}));
            }
            Output::Error(f) => {
                out.push(f.envelope(&self.request_id));
            }
        }
        out
    }
    pub fn document(&self) -> Value {
        json!({"id":self.id,"type":"message","role":"assistant","content":self.blocks,"model":self.model,"stop_reason":self.reason,"stop_sequence":self.sequence,"usage":self.usage})
    }
}
async fn models(
    State(e): State<Arc<Engine>>,
    Extension(trace): Extension<Arc<RequestTrace>>,
) -> Result<Json<Value>> {
    let mut scope = crate::guard::Scope::new(e.store.read().await.settings.request_timeout_seconds);
    scope.trace = Some(trace);
    let lease = e
        .pool
        .acquire(None, &std::collections::HashSet::new(), &scope, false)
        .await?;
    let ids = lease.slot.client.models(&lease, &scope, true).await?;
    Ok(Json(e.store.read().await.models.catalog(&ids)))
}
