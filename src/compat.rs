use crate::{
    engine::Engine,
    error::{Fault, Result},
    guard::{Lease, Scope},
    observability::RequestTrace,
    protocol,
    store::{Usage, nonce, now},
    transport::{Completion, CompletionRequest},
};
use axum::{
    Extension, Json,
    body::{Body, Bytes},
    extract::{Path, State},
    http::{HeaderName, HeaderValue, header},
    response::{IntoResponse, Response},
};
use serde_json::{Map, Value, json};
use std::{collections::HashSet, convert::Infallible, sync::Arc};
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

const BRIDGE: &str = "Continue the conversation using this JSON transcript. Reply only with the assistant's answer. Do not use tools.";
const MAX_OUTPUT_BYTES: usize = 8 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    Chat,
    Responses,
}

#[derive(Clone, Debug)]
struct Projection {
    requested_model: String,
    stream: bool,
    include_usage: bool,
    conversation_id: Option<String>,
    prompt: String,
}

impl Projection {
    fn chat(raw: &Value) -> Result<Self> {
        let object = request_object(raw)?;
        let requested_model = model_field(object)?;
        let stream = optional_bool(object, "stream", false)?;
        let conversation_id =
            validate_conversation_id(optional_string(object, "conversation_id")?)?;
        reject_tools(object)?;
        let include_usage = match object.get("stream_options") {
            None => false,
            Some(options) => {
                let options = options
                    .as_object()
                    .ok_or_else(|| Fault::invalid("stream_options must be an object"))?;
                optional_bool(options, "include_usage", false)?
            }
        };
        let raw_messages = object
            .get("messages")
            .and_then(Value::as_array)
            .filter(|messages| !messages.is_empty())
            .ok_or_else(|| Fault::invalid("messages must be a nonempty array"))?;
        let mut messages = Vec::with_capacity(raw_messages.len());
        for (index, raw_message) in raw_messages.iter().enumerate() {
            let message = raw_message
                .as_object()
                .ok_or_else(|| Fault::invalid(format!("messages[{index}] must be an object")))?;
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .filter(|role| matches!(*role, "system" | "developer" | "user" | "assistant"))
                .ok_or_else(|| {
                    Fault::invalid(format!(
                        "messages[{index}].role must be system, developer, user, or assistant"
                    ))
                })?;
            if ["tool_calls", "function_call", "tool_call_id"]
                .iter()
                .any(|field| message.contains_key(*field))
            {
                return Err(Fault::invalid("tool messages are unsupported"));
            }
            let content = message
                .get("content")
                .ok_or_else(|| Fault::invalid(format!("messages[{index}].content is required")))?;
            messages.push(json!({
                "role": role,
                "content": chat_content(content, index)?,
            }));
        }
        let prompt = transcript_prompt(None, &messages)?;
        Ok(Self {
            requested_model,
            stream,
            include_usage,
            conversation_id,
            prompt,
        })
    }

    fn responses(raw: &Value) -> Result<Self> {
        let object = request_object(raw)?;
        let requested_model = model_field(object)?;
        let stream = optional_bool(object, "stream", false)?;
        let conversation_id =
            validate_conversation_id(optional_string(object, "conversation_id")?)?;
        reject_tools(object)?;
        let instructions = match object.get("instructions") {
            None => None,
            Some(value) => Some(
                value
                    .as_str()
                    .ok_or_else(|| Fault::invalid("instructions must be a string"))?
                    .to_owned(),
            ),
        };
        let input = object
            .get("input")
            .ok_or_else(|| Fault::invalid("input is required"))?;
        let messages = if let Some(text) = input.as_str() {
            vec![json!({"role":"user","content":text})]
        } else {
            let raw_messages = input
                .as_array()
                .filter(|messages| !messages.is_empty())
                .ok_or_else(|| {
                    Fault::invalid("input must be a string or nonempty message array")
                })?;
            let mut messages = Vec::with_capacity(raw_messages.len());
            for (index, raw_message) in raw_messages.iter().enumerate() {
                let message = raw_message.as_object().ok_or_else(|| {
                    Fault::invalid(format!("input[{index}] must be a message object"))
                })?;
                if message
                    .get("type")
                    .is_some_and(|kind| kind.as_str() != Some("message"))
                {
                    return Err(Fault::invalid(format!(
                        "input[{index}] must be a text message"
                    )));
                }
                let role = message
                    .get("role")
                    .and_then(Value::as_str)
                    .filter(|role| matches!(*role, "system" | "developer" | "user" | "assistant"))
                    .ok_or_else(|| {
                        Fault::invalid(format!(
                            "input[{index}].role must be system, developer, user, or assistant"
                        ))
                    })?;
                if ["tool_calls", "function_call", "call_id"]
                    .iter()
                    .any(|field| message.contains_key(*field))
                {
                    return Err(Fault::invalid("tool messages are unsupported"));
                }
                let content = message
                    .get("content")
                    .ok_or_else(|| Fault::invalid(format!("input[{index}].content is required")))?;
                messages.push(json!({
                    "role": role,
                    "content": responses_content(content, index)?,
                }));
            }
            messages
        };
        let prompt = transcript_prompt(instructions.as_deref(), &messages)?;
        Ok(Self {
            requested_model,
            stream,
            include_usage: false,
            conversation_id,
            prompt,
        })
    }
}

fn request_object(raw: &Value) -> Result<&Map<String, Value>> {
    raw.as_object()
        .ok_or_else(|| Fault::invalid("request body must be a JSON object"))
}

fn model_field(object: &Map<String, Value>) -> Result<String> {
    match object.get("model") {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(model)) => Ok(model.clone()),
        Some(_) => Err(Fault::invalid("model must be a string or null")),
    }
}

fn reject_tools(object: &Map<String, Value>) -> Result<()> {
    if ["tools", "tool_choice", "functions", "function_call"]
        .iter()
        .any(|field| object.contains_key(*field))
    {
        return Err(Fault::invalid(
            "tool calling is unsupported by this text adapter",
        ));
    }
    Ok(())
}

fn token_usage(prompt: &str, output: &str) -> Value {
    let input_tokens = protocol::estimate(prompt);
    let output_tokens = protocol::estimate(output);
    json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": input_tokens + output_tokens,
    })
}

fn optional_bool(object: &Map<String, Value>, key: &str, default: bool) -> Result<bool> {
    match object.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| Fault::invalid(format!("{key} must be a boolean"))),
    }
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>> {
    match object.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(|value| Some(value.to_owned()))
            .ok_or_else(|| Fault::invalid(format!("{key} must be a string"))),
    }
}

fn validate_conversation_id(id: Option<String>) -> Result<Option<String>> {
    if id.as_ref().is_some_and(|id| id.len() > 512) {
        return Err(Fault::invalid("conversation_id is too long"));
    }
    Ok(id)
}

fn chat_content(value: &Value, index: usize) -> Result<Value> {
    if let Some(text) = value.as_str() {
        return Ok(Value::String(text.to_owned()));
    }
    let parts = value.as_array().ok_or_else(|| {
        Fault::invalid(format!(
            "messages[{index}].content must be a string or text array"
        ))
    })?;
    let mut projected = Vec::with_capacity(parts.len());
    for (part_index, part) in parts.iter().enumerate() {
        let part = part.as_object().ok_or_else(|| {
            Fault::invalid(format!(
                "messages[{index}].content[{part_index}] must be an object"
            ))
        })?;
        if part.get("type").and_then(Value::as_str) != Some("text") {
            return Err(Fault::invalid(format!(
                "messages[{index}].content[{part_index}] must be a text part"
            )));
        }
        let text = part.get("text").and_then(Value::as_str).ok_or_else(|| {
            Fault::invalid(format!(
                "messages[{index}].content[{part_index}].text must be a string"
            ))
        })?;
        projected.push(json!({"type":"text","text":text}));
    }
    Ok(Value::Array(projected))
}

fn responses_content(value: &Value, index: usize) -> Result<Value> {
    if let Some(text) = value.as_str() {
        return Ok(Value::String(text.to_owned()));
    }
    let parts = value.as_array().ok_or_else(|| {
        Fault::invalid(format!(
            "input[{index}].content must be a string or text array"
        ))
    })?;
    let mut projected = Vec::with_capacity(parts.len());
    for (part_index, part) in parts.iter().enumerate() {
        let part = part.as_object().ok_or_else(|| {
            Fault::invalid(format!(
                "input[{index}].content[{part_index}] must be an object"
            ))
        })?;
        let kind = part
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| matches!(*kind, "input_text" | "text" | "output_text"))
            .ok_or_else(|| {
                Fault::invalid(format!(
                    "input[{index}].content[{part_index}] must be a text part"
                ))
            })?;
        let text = part.get("text").and_then(Value::as_str).ok_or_else(|| {
            Fault::invalid(format!(
                "input[{index}].content[{part_index}].text must be a string"
            ))
        })?;
        projected.push(json!({"type":kind,"text":text}));
    }
    Ok(Value::Array(projected))
}

fn transcript_prompt(instructions: Option<&str>, messages: &[Value]) -> Result<String> {
    let transcript = json!({
        "instructions": instructions,
        "messages": messages,
    });
    let transcript = serde_json::to_string(&transcript)
        .map_err(|_| Fault::invalid("could not encode text transcript"))?;
    Ok(format!("{BRIDGE}\n{transcript}"))
}

struct Active {
    engine: Arc<Engine>,
    trace: Arc<RequestTrace>,
    scope: Scope,
    lease: Lease,
    completion: Option<Completion>,
    parent: String,
    thinking: String,
    protocol: Protocol,
    response_id: String,
    item_id: String,
    created: i64,
    public_model: String,
    model: String,
    prompt: String,
    target: String,
    conversation_key: Option<String>,
    persistent: bool,
    assistant: String,
    _conversation_lock: Option<OwnedSemaphorePermit>,
    cleanup: Option<PendingCleanup>,
}

enum Pull {
    Text(String),
    Done,
}

struct PendingCleanup {
    engine: Arc<Engine>,
    account_id: String,
    conversation_id: String,
    armed: bool,
}

impl PendingCleanup {
    fn new(engine: Arc<Engine>, account_id: String, conversation_id: String) -> Self {
        Self {
            engine,
            account_id,
            conversation_id,
            armed: true,
        }
    }

    async fn run_with_lease(&mut self, lease: &Lease) {
        if delete_queued_with_lease(&self.engine, lease, &self.account_id, &self.conversation_id)
            .await
        {
            self.armed = false;
        }
    }
}

impl Drop for PendingCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let engine = self.engine.clone();
        let account_id = self.account_id.clone();
        let conversation_id = self.conversation_id.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let timeout = engine.store.read().await.settings.request_timeout_seconds;
                let scope = Scope::new(timeout);
                let excluded = HashSet::new();
                if let Ok(lease) = engine
                    .pool
                    .acquire(Some(&account_id), &excluded, &scope, false)
                    .await
                {
                    let _ =
                        delete_queued_with_lease(&engine, &lease, &account_id, &conversation_id)
                            .await;
                }
            });
        }
    }
}

async fn delete_queued_with_lease(
    engine: &Arc<Engine>,
    lease: &Lease,
    account_id: &str,
    conversation_id: &str,
) -> bool {
    let timeout = engine.store.read().await.settings.request_timeout_seconds;
    let cleanup_scope = Scope::new(timeout);
    if lease
        .slot
        .client
        .delete(lease, &cleanup_scope, conversation_id)
        .await
        .is_err()
    {
        return false;
    }
    engine
        .store
        .mutate("", json!({}), |state| {
            state.pending.retain(|job| {
                job.account_id != account_id || job.conversation_id != conversation_id
            });
            Ok(())
        })
        .await
        .is_ok()
}

async fn begin(
    engine: Arc<Engine>,
    trace: Arc<RequestTrace>,
    projection: Projection,
    protocol: Protocol,
) -> Result<Active> {
    trace.requested_model(&projection.requested_model);
    trace.streaming(projection.stream);

    let selected = engine
        .store
        .read()
        .await
        .models
        .resolve(&projection.requested_model, &engine.config.default_model)?;
    let public_model = selected.public;
    let model = selected.upstream;
    let settings = engine.store.read().await.settings;
    let mut scope = Scope::new(settings.request_timeout_seconds);
    scope.trace = Some(trace.clone());

    let conversation_key = projection
        .conversation_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(|id| trace.legacy_key(id, &engine.config.gateway_key));
    let conversation_lock = if let Some(key) = &conversation_key {
        engine.legacy_lock(key).await?
    } else {
        None
    };
    let previous = if let Some(key) = &conversation_key {
        engine.legacy.lock().await.get(key).cloned()
    } else {
        None
    };

    let excluded = HashSet::new();
    let lease = engine
        .pool
        .acquire(
            previous.as_ref().map(|entry| entry.0.as_str()),
            &excluded,
            &scope,
            false,
        )
        .await?;
    trace.bind_account(&lease.account_id);
    let available_models = lease.slot.client.models(&lease, &scope, false).await?;
    if !available_models.contains(&model) {
        return Err(Fault::invalid(
            "model is not available in this account's web catalog; use an exact ID from GET /v1/models",
        ));
    }

    let input_tokens = protocol::estimate(&projection.prompt);
    trace.begin_usage(input_tokens);
    let (target, parent) = if let Some(previous) = &previous {
        (previous.1.clone(), previous.2.clone())
    } else {
        let target = lease.slot.client.create(&lease, &scope).await?;
        if let Err(fault) = engine.own(&lease.account_id, &target).await {
            let cleanup_scope = Scope::new(settings.request_timeout_seconds);
            let _ = lease
                .slot
                .client
                .delete(&lease, &cleanup_scope, &target)
                .await;
            return Err(fault);
        }
        (target, String::new())
    };
    let persistent = conversation_key.is_some();
    let cleanup = (!persistent)
        .then(|| PendingCleanup::new(engine.clone(), lease.account_id.clone(), target.clone()));

    let response_id = match protocol {
        Protocol::Chat => format!("chatcmpl-{}", nonce()),
        Protocol::Responses => format!("resp_{}", nonce()),
    };
    let item_id = format!("msg_{}", nonce());
    let thinking = engine.config.legacy_thinking.clone();
    let active = Active {
        engine,
        trace,
        scope,
        lease,
        completion: None,
        parent,
        thinking,
        protocol,
        response_id,
        item_id,
        created: now(),
        public_model,
        model,
        prompt: projection.prompt,
        target,
        conversation_key,
        persistent,
        assistant: String::new(),
        _conversation_lock: conversation_lock,
        cleanup,
    };
    Ok(active)
}

async fn record_usage(
    engine: &Arc<Engine>,
    lease: &Lease,
    scope: &Scope,
    model: &str,
    prompt: &str,
    output: &str,
    status: u16,
) -> Result<()> {
    let input = protocol::estimate(prompt);
    let output_tokens = protocol::estimate(output);
    engine
        .store
        .record_scoped(
            Usage {
                input_estimate: input,
                output_estimate: output_tokens,
                physical_estimate: protocol::physical(prompt) + protocol::physical(output),
                status,
                account_id: lease.account_id.clone(),
                model: model.to_owned(),
                ..Usage::default()
            },
            scope,
            false,
        )
        .await?;
    Ok(())
}

async fn start_completion(active: &mut Active) -> Result<()> {
    active.lease.start(&active.scope).await?;
    let completion = active
        .lease
        .slot
        .client
        .completion(
            &active.lease,
            &active.scope,
            CompletionRequest {
                target: &active.target,
                parent: &active.parent,
                model: &active.model,
                prompt: &active.prompt,
                tools: json!([]),
                thinking: &active.thinking,
            },
        )
        .await?;
    active.assistant = completion.assistant.clone();
    active.completion = Some(completion);
    Ok(())
}

async fn complete_active(active: &mut Active, text: &str) -> Result<Value> {
    active.completion.take();
    if active.persistent {
        if let Some(key) = &active.conversation_key {
            active
                .engine
                .store
                .mutate("", json!({}), |state| {
                    state.pending.retain(|job| {
                        job.account_id != active.lease.account_id
                            || job.conversation_id != active.target
                    });
                    Ok(())
                })
                .await?;
            active.engine.legacy.lock().await.insert(
                key.clone(),
                (
                    active.lease.account_id.clone(),
                    active.target.clone(),
                    active.assistant.clone(),
                ),
            );
        }
    } else if let Some(cleanup) = &mut active.cleanup {
        cleanup.run_with_lease(&active.lease).await;
    }

    let usage = token_usage(&active.prompt, text);
    record_usage(
        &active.engine,
        &active.lease,
        &active.scope,
        &active.model,
        &active.prompt,
        text,
        200,
    )
    .await?;
    Ok(usage)
}

async fn pull_text(active: &mut Active, output: &mut String) -> Result<Pull> {
    loop {
        let scope = &active.scope;
        let event = active
            .completion
            .as_mut()
            .ok_or_else(Fault::internal)?
            .next(scope)
            .await?;
        if event["type"] == "error" || event.get("error").is_some_and(|error| !error.is_null()) {
            let status = crate::transport::error_status(crate::transport::error_code(&event));
            return Err(active.lease.failure(status, None).await?);
        }
        if event["type"] == "message_stop" {
            return Ok(Pull::Done);
        }
        if let Some(delta) = event.pointer("/delta/text").and_then(Value::as_str) {
            if output.len().saturating_add(delta.len()) > MAX_OUTPUT_BYTES {
                return Err(Fault::new(
                    502,
                    "api_error",
                    "upstream output exceeds limit",
                ));
            }
            output.push_str(delta);
            return Ok(Pull::Text(delta.to_owned()));
        }
    }
}

async fn fail_active(active: &mut Active, output: &str, fault: &Fault) {
    active.completion.take();
    active.trace.fail(fault.status);
    let _ = record_usage(
        &active.engine,
        &active.lease,
        &active.scope,
        &active.model,
        &active.prompt,
        output,
        fault.status,
    )
    .await;
    if let Some(cleanup) = &mut active.cleanup {
        cleanup.run_with_lease(&active.lease).await;
    }
}

fn chat_chunk(active: &Active, delta: Value, finish_reason: Option<&str>) -> Value {
    json!({
        "id": active.response_id,
        "object": "chat.completion.chunk",
        "created": active.created,
        "model": active.public_model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }],
        "usage": null,
    })
}

fn chat_document(active: &Active, text: &str, usage: &Value) -> Value {
    json!({
        "id": active.response_id,
        "object": "chat.completion",
        "created": active.created,
        "model": active.public_model,
        "choices": [{
            "index": 0,
            "message": {"role":"assistant","content":text},
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": usage["input_tokens"],
            "completion_tokens": usage["output_tokens"],
            "total_tokens": usage["total_tokens"],
        },
    })
}

fn response_message(active: &Active, text: &str) -> Value {
    json!({
        "id": active.item_id,
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": text,
            "annotations": [],
        }],
    })
}

fn response_document(active: &Active, text: &str, usage: &Value) -> Value {
    json!({
        "id": active.response_id,
        "object": "response",
        "created_at": active.created,
        "status": "completed",
        "model": active.public_model,
        "output": [response_message(active, text)],
        "usage": usage,
    })
}

fn public_error(fault: &Fault) -> Value {
    json!({"type":fault.kind,"message":fault.message})
}

fn sse_data(value: &Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}

fn sse_event(kind: &str, value: &Value) -> Bytes {
    Bytes::from(format!("event: {kind}\ndata: {value}\n\n"))
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn sse_response(
    stream: impl futures_util::Stream<Item = std::result::Result<Bytes, Infallible>> + Send + 'static,
) -> Response {
    let mut response = Body::from_stream(stream).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    response
}

fn chat_stream(mut active: Active, include_usage: bool) -> Response {
    let stream = async_stream::stream! {
        let _cancel = CancelOnDrop(active.scope.cancel.clone());
        let role = chat_chunk(&active, json!({"role":"assistant"}), None);
        yield Ok::<_, Infallible>(sse_data(&role));
        let mut output = String::new();
        if let Err(fault) = start_completion(&mut active).await {
            fail_active(&mut active, &output, &fault).await;
            let event = json!({"error":public_error(&fault)});
            yield Ok(sse_event("error", &event));
            return;
        }
        loop {
            match pull_text(&mut active, &mut output).await {
                Ok(Pull::Text(delta)) => {
                    let chunk = chat_chunk(&active, json!({"content":delta}), None);
                    yield Ok(sse_data(&chunk));
                }
                Ok(Pull::Done) => {
                    match complete_active(&mut active, &output).await {
                        Ok(usage) => {
                            let finish = chat_chunk(&active, json!({}), Some("stop"));
                            yield Ok(sse_data(&finish));
                            if include_usage {
                                let usage_chunk = json!({
                                    "id": active.response_id,
                                    "object": "chat.completion.chunk",
                                    "created": active.created,
                                    "model": active.public_model,
                                    "choices": [],
                                    "usage": {
                                        "prompt_tokens": usage["input_tokens"],
                                        "completion_tokens": usage["output_tokens"],
                                        "total_tokens": usage["total_tokens"],
                                    },
                                });
                                yield Ok(sse_data(&usage_chunk));
                            }
                            yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
                        }
                        Err(fault) => {
                            fail_active(&mut active, &output, &fault).await;
                            let event = json!({"error":public_error(&fault)});
                            yield Ok(sse_event("error", &event));
                        }
                    }
                    break;
                }
                Err(fault) => {
                    fail_active(&mut active, &output, &fault).await;
                    let event = json!({"error":public_error(&fault)});
                    yield Ok(sse_event("error", &event));
                    break;
                }
            }
        }
    };
    sse_response(stream)
}

fn response_event(kind: &str, sequence: usize, payload: Value) -> Value {
    let mut event = payload.as_object().cloned().unwrap_or_default();
    event.insert("type".into(), Value::String(kind.to_owned()));
    event.insert("sequence_number".into(), json!(sequence));
    Value::Object(event)
}

fn response_stream(mut active: Active) -> Response {
    let stream = async_stream::stream! {
        let _cancel = CancelOnDrop(active.scope.cancel.clone());
        let mut sequence = 0usize;
        let created = response_event("response.created", sequence, json!({
            "response": {
                "id": active.response_id,
                "object": "response",
                "created_at": active.created,
                "status": "in_progress",
                "model": active.public_model,
                "output": [],
            }
        }));
        yield Ok::<_, Infallible>(sse_event("response.created", &created));
        sequence += 1;
        let item = json!({
            "id": active.item_id,
            "type": "message",
            "status": "in_progress",
            "role": "assistant",
            "content": [],
        });
        let added = response_event("response.output_item.added", sequence, json!({
            "output_index": 0,
            "item": item,
        }));
        yield Ok(sse_event("response.output_item.added", &added));
        sequence += 1;
        let part = json!({"type":"output_text","text":"","annotations":[]});
        let part_added = response_event("response.content_part.added", sequence, json!({
            "item_id": active.item_id,
            "output_index": 0,
            "content_index": 0,
            "part": part,
        }));
        yield Ok(sse_event("response.content_part.added", &part_added));
        sequence += 1;

        let mut output = String::new();
        if let Err(fault) = start_completion(&mut active).await {
            fail_active(&mut active, &output, &fault).await;
            let failed = response_event("response.failed", sequence, json!({
                "response": {
                    "id": active.response_id,
                    "object": "response",
                    "created_at": active.created,
                    "status": "failed",
                    "model": active.public_model,
                    "output": [],
                    "error": public_error(&fault),
                }
            }));
            yield Ok(sse_event("response.failed", &failed));
            return;
        }
        loop {
            match pull_text(&mut active, &mut output).await {
                Ok(Pull::Text(delta)) => {
                    let event = response_event("response.output_text.delta", sequence, json!({
                        "item_id": active.item_id,
                        "output_index": 0,
                        "content_index": 0,
                        "delta": delta,
                    }));
                    yield Ok(sse_event("response.output_text.delta", &event));
                    sequence += 1;
                }
                Ok(Pull::Done) => {
                    match complete_active(&mut active, &output).await {
                        Ok(usage) => {
                            let output_done = response_event("response.output_text.done", sequence, json!({
                                "item_id": active.item_id,
                                "output_index": 0,
                                "content_index": 0,
                                "text": output,
                            }));
                            yield Ok(sse_event("response.output_text.done", &output_done));
                            sequence += 1;
                            let part_done = response_event("response.content_part.done", sequence, json!({
                                "item_id": active.item_id,
                                "output_index": 0,
                                "content_index": 0,
                                "part": {"type":"output_text","text":output,"annotations":[]},
                            }));
                            yield Ok(sse_event("response.content_part.done", &part_done));
                            sequence += 1;
                            let item_done = response_event("response.output_item.done", sequence, json!({
                                "output_index": 0,
                                "item": response_message(&active, &output),
                            }));
                            yield Ok(sse_event("response.output_item.done", &item_done));
                            sequence += 1;
                            let completed = response_event("response.completed", sequence, json!({
                                "response": response_document(&active, &output, &usage),
                            }));
                            yield Ok(sse_event("response.completed", &completed));
                        }
                        Err(fault) => {
                            fail_active(&mut active, &output, &fault).await;
                            let failed = response_event("response.failed", sequence, json!({
                                "response": {
                                    "id": active.response_id,
                                    "object": "response",
                                    "created_at": active.created,
                                    "status": "failed",
                                    "model": active.public_model,
                                    "output": [],
                                    "error": public_error(&fault),
                                }
                            }));
                            yield Ok(sse_event("response.failed", &failed));
                        }
                    }
                    break;
                }
                Err(fault) => {
                    fail_active(&mut active, &output, &fault).await;
                    let failed = response_event("response.failed", sequence, json!({
                        "response": {
                            "id": active.response_id,
                            "object": "response",
                            "created_at": active.created,
                            "status": "failed",
                            "model": active.public_model,
                            "output": [],
                            "error": public_error(&fault),
                        }
                    }));
                    yield Ok(sse_event("response.failed", &failed));
                    break;
                }
            }
        }
    };
    sse_response(stream)
}

fn json_response(active: &Active, text: &str, usage: &Value) -> Response {
    match active.protocol {
        Protocol::Chat => Json(chat_document(active, text, usage)).into_response(),
        Protocol::Responses => Json(response_document(active, text, usage)).into_response(),
    }
}

async fn run_nonstream(mut active: Active) -> Result<Response> {
    let mut output = String::new();
    if let Err(fault) = start_completion(&mut active).await {
        fail_active(&mut active, &output, &fault).await;
        return Err(fault);
    }
    loop {
        match pull_text(&mut active, &mut output).await {
            Ok(Pull::Text(_)) => {}
            Ok(Pull::Done) => {
                let usage = complete_active(&mut active, &output).await?;
                return Ok(json_response(&active, &output, &usage));
            }
            Err(fault) => {
                fail_active(&mut active, &output, &fault).await;
                return Err(fault);
            }
        }
    }
}

async fn handle(
    engine: Arc<Engine>,
    trace: Arc<RequestTrace>,
    bytes: Bytes,
    protocol: Protocol,
) -> Result<Response> {
    let raw: Value =
        serde_json::from_slice(&bytes).map_err(|_| Fault::invalid("invalid JSON request body"))?;
    trace.requested_model(raw.get("model").and_then(Value::as_str).unwrap_or(""));
    let projection = match protocol {
        Protocol::Chat => Projection::chat(&raw)?,
        Protocol::Responses => Projection::responses(&raw)?,
    };
    let stream = projection.stream;
    let include_usage = projection.include_usage;
    let active = begin(engine, trace, projection, protocol).await?;
    if !stream {
        return run_nonstream(active).await;
    }
    Ok(match protocol {
        Protocol::Chat => chat_stream(active, include_usage),
        Protocol::Responses => response_stream(active),
    })
}

pub async fn chat(
    State(engine): State<Arc<Engine>>,
    Extension(trace): Extension<Arc<RequestTrace>>,
    bytes: Bytes,
) -> Result<Response> {
    handle(engine, trace, bytes, Protocol::Chat).await
}

pub async fn responses(
    State(engine): State<Arc<Engine>>,
    Extension(trace): Extension<Arc<RequestTrace>>,
    bytes: Bytes,
) -> Result<Response> {
    handle(engine, trace, bytes, Protocol::Responses).await
}

pub async fn remove(
    State(engine): State<Arc<Engine>>,
    Extension(trace): Extension<Arc<RequestTrace>>,
    Path(conversation_id): Path<String>,
) -> Result<Json<Value>> {
    if conversation_id.is_empty() || conversation_id.len() > 512 {
        return Err(Fault::invalid("invalid conversation_id"));
    }
    let key = trace.legacy_key(&conversation_id, &engine.config.gateway_key);
    let _lock = engine.legacy_lock(&key).await?;
    let mapping = engine.legacy.lock().await.get(&key).cloned();
    let Some((account_id, upstream_id, _)) = mapping else {
        return Err(Fault::new(404, "not_found_error", "conversation not found"));
    };
    let settings = engine.store.read().await.settings;
    let scope = Scope::new(settings.request_timeout_seconds);
    let lease = engine
        .pool
        .acquire(Some(&account_id), &HashSet::new(), &scope, false)
        .await?;
    lease
        .slot
        .client
        .delete(&lease, &scope, &upstream_id)
        .await?;
    engine.legacy.lock().await.remove(&key);
    engine
        .store
        .mutate("", json!({}), |state| {
            state
                .pending
                .retain(|job| job.account_id != account_id || job.conversation_id != upstream_id);
            Ok(())
        })
        .await?;
    Ok(Json(
        json!({"id":conversation_id,"object":"conversation.deleted","deleted":true}),
    ))
}

impl Drop for Active {
    fn drop(&mut self) {
        self.scope.cancel.cancel();
        self.completion.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript(prompt: &str) -> Value {
        let (_, encoded) = prompt.rsplit_once('\n').expect("transcript line");
        serde_json::from_str(encoded).expect("valid trailing transcript JSON")
    }

    #[test]
    fn chat_projection_preserves_roles_and_json_escapes_text() {
        let raw = json!({
            "model":"claude-test",
            "messages":[
                {"role":"system","content":"rules \"quoted\"\nnext"},
                {"role":"assistant","content":[{"type":"text","text":"earlier"}]},
                {"role":"user","content":"answer"}
            ]
        });
        let projection = Projection::chat(&raw).expect("valid chat input");
        let encoded = projection.prompt.rsplit_once('\n').unwrap().1;
        assert_eq!(encoded.lines().count(), 1);
        let transcript = transcript(&projection.prompt);
        assert_eq!(transcript["instructions"], Value::Null);
        assert_eq!(transcript["messages"][0]["role"], "system");
        assert_eq!(transcript["messages"][1]["role"], "assistant");
        assert_eq!(transcript["messages"][2]["role"], "user");
        assert_eq!(
            transcript["messages"][0]["content"],
            "rules \"quoted\"\nnext"
        );
        assert!(!projection.prompt.contains("[System]"));
        assert!(!projection.prompt.contains("[Assistant]"));
    }

    #[test]
    fn responses_projection_keeps_instructions_and_original_roles() {
        let raw = json!({
            "model":"claude-test",
            "instructions":"follow \"these\" rules\ncarefully",
            "input":[
                {"type":"message","role":"developer","content":[{"type":"input_text","text":"policy"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"prior"}]},
                {"type":"message","role":"user","content":[{"type":"text","text":"next"}]}
            ]
        });
        let projection = Projection::responses(&raw).expect("valid responses input");
        let transcript = transcript(&projection.prompt);
        assert_eq!(
            transcript["instructions"],
            "follow \"these\" rules\ncarefully"
        );
        assert_eq!(transcript["messages"][0]["role"], "developer");
        assert_eq!(transcript["messages"][1]["role"], "assistant");
        assert_eq!(transcript["messages"][2]["role"], "user");
        assert_eq!(
            transcript["messages"][0]["content"][0]["type"],
            "input_text"
        );
    }

    #[test]
    fn text_projection_rejects_images_tools_and_wrong_field_types() {
        let image = json!({
            "model":"claude-test",
            "messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"x"}}]}]
        });
        assert_eq!(Projection::chat(&image).unwrap_err().status, 400);

        let tool = json!({
            "model":"claude-test",
            "tools":[{"type":"function","function":{"name":"f"}}],
            "messages":[{"role":"user","content":"run"}]
        });
        assert_eq!(Projection::chat(&tool).unwrap_err().status, 400);

        let malformed = json!({
            "model":"claude-test",
            "stream":"false",
            "input":"hello"
        });
        assert_eq!(Projection::responses(&malformed).unwrap_err().status, 400);
    }

    #[test]
    fn usage_counts_the_actual_prompt_and_output_and_sums_exactly() {
        let prompt = "bridge\n{\"messages\":[{\"role\":\"user\",\"content\":\"你好\"}]}";
        let output = "A quoted \"answer\"\n続き";
        let usage = token_usage(prompt, output);
        assert_eq!(usage["input_tokens"], protocol::estimate(prompt));
        assert_eq!(usage["output_tokens"], protocol::estimate(output));
        assert_eq!(
            usage["total_tokens"],
            usage["input_tokens"].as_u64().unwrap() + usage["output_tokens"].as_u64().unwrap()
        );
    }

    #[test]
    fn failure_renderers_emit_errors_without_success_terminators() {
        let fault = Fault::new(429, "rate_limit_error", "upstream account rate limited");
        let chat_error = sse_event("error", &json!({"error":public_error(&fault)}));
        let chat_error = String::from_utf8(chat_error.to_vec()).unwrap();
        assert!(chat_error.contains("event: error\n"));
        let data = chat_error
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap();
        let event: Value = serde_json::from_str(data).unwrap();
        assert_eq!(event["error"]["type"], "rate_limit_error");
        assert!(!chat_error.contains("[DONE]"));
        assert!(!chat_error.contains("[error:"));

        let response_error = response_event(
            "response.failed",
            0,
            json!({"response":{"status":"failed","error":public_error(&fault)}}),
        );
        assert_eq!(response_error["type"], "response.failed");
        assert_eq!(response_error["sequence_number"], 0);
        assert_eq!(
            response_error["response"]["error"]["type"],
            "rate_limit_error"
        );
        assert_ne!(response_error["type"], "response.completed");
    }
}
