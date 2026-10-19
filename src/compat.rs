use crate::{
    engine::Engine,
    error::{Fault, Result},
    guard::{Lease, Scope},
    native_stream::{WebBlockDecoder, WebEmission},
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
use std::{
    collections::{HashSet, VecDeque},
    convert::Infallible,
    sync::Arc,
};
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

const BRIDGE: &str =
    "Continue the conversation using this JSON transcript. Reply only with the assistant's answer.";
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
    native_tools: Option<Value>,
    project_uuid: Option<String>,
    prompt: String,
}

impl Projection {
    fn chat(raw: &Value) -> Result<Self> {
        let object = request_object(raw)?;
        let requested_model = model_field(object)?;
        let stream = optional_bool(object, "stream", false)?;
        let conversation_id =
            validate_conversation_id(optional_string(object, "conversation_id")?)?;
        let native_tools = object.get("claud2api_tools").cloned();
        let project_uuid = crate::native_tools::project(raw)?;
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
            native_tools,
            project_uuid,
            prompt,
        })
    }

    fn responses(raw: &Value) -> Result<Self> {
        let object = request_object(raw)?;
        let requested_model = model_field(object)?;
        let stream = optional_bool(object, "stream", false)?;
        let conversation_id =
            validate_conversation_id(optional_string(object, "conversation_id")?)?;
        let native_tools = object.get("claud2api_tools").cloned();
        let project_uuid = crate::native_tools::project(raw)?;
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
            native_tools,
            project_uuid,
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
    tool_descriptors: Value,
    decoder: WebBlockDecoder,
    emissions: VecDeque<WebEmission>,
    upstream_done: bool,
    native_blocks: Vec<Value>,
    citations: Vec<Value>,
    native_bytes: usize,
    file_until: i64,
    persistent: bool,
    assistant: String,
    _conversation_lock: Option<OwnedSemaphorePermit>,
    cleanup: Option<PendingCleanup>,
}

enum Pull {
    Text(String),
    NativeStart(Value),
    NativeDelta(Value),
    NativeStop(Value),
    Citation(Value),
    Done,
}

fn profiled_legacy_key(base_key: &str, gateway_key: &str, groups: &[String]) -> String {
    protocol::digest(
        gateway_key,
        "legacy-token-session-native-tools",
        &(base_key, groups),
    )
}

fn project_legacy_key(
    base_key: &str,
    gateway_key: &str,
    groups: &[String],
    project: &Option<String>,
) -> String {
    if groups.is_empty() && project.is_none() {
        return base_key.to_owned();
    }
    match project {
        Some(project) => format!(
            "{base_key}:{}",
            protocol::digest(gateway_key, "legacy-project-tools", &(groups, project))
        ),
        None => format!(
            "{base_key}:{}",
            profiled_legacy_key(base_key, gateway_key, groups)
        ),
    }
}

fn legacy_profile_keys(base_key: &str, gateway_key: &str) -> Vec<String> {
    let groups = ["web_search", "artifacts", "repl", "widgets"];
    let mut keys = vec![base_key.to_owned()];
    for mask in 0..(1usize << groups.len()) {
        let profile: Vec<String> = groups
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1 << index) != 0)
            .map(|(_, group)| (*group).to_owned())
            .collect();
        keys.push(profiled_legacy_key(base_key, gateway_key, &profile));
    }
    keys
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
    let mut native_options = projection
        .native_tools
        .as_ref()
        .map(|value| json!({"claud2api_tools":value}))
        .unwrap_or_else(|| json!({}));
    if let Some(project) = &projection.project_uuid {
        native_options["claud2api_project_uuid"] = json!(project);
    }
    let native_profile = crate::native_tools::resolve(&native_options, &settings)?;
    let native_allowed_names = crate::native_tools::allowed_names(&native_profile);
    let tool_descriptors = crate::native_tools::descriptors(&native_profile);
    let mut scope = Scope::new(settings.request_timeout_seconds);
    scope.trace = Some(trace.clone());

    let conversation_base_key = projection
        .conversation_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(|id| trace.legacy_key(id, &engine.config.gateway_key));
    let conversation_key = conversation_base_key.as_ref().map(|base_key| {
        project_legacy_key(
            base_key,
            &engine.config.gateway_key,
            &native_profile,
            &projection.project_uuid,
        )
    });
    let conversation_lock = if let Some(key) = &conversation_base_key {
        engine.legacy_lock(key).await?
    } else {
        None
    };
    let previous = if let Some(key) = &conversation_key {
        let legacy = engine.legacy.lock().await;
        if let Some(previous) = legacy.get(key) {
            Some(previous.clone())
        } else {
            let base_key = conversation_base_key
                .as_ref()
                .expect("conversation key exists");
            if legacy_profile_keys(base_key, &engine.config.gateway_key)
                .iter()
                .any(|candidate| legacy.contains_key(candidate))
                || legacy
                    .keys()
                    .any(|candidate| candidate.starts_with(&format!("{base_key}:")))
            {
                return Err(Fault::new(
                    409,
                    "invalid_request_error",
                    "native tools or project changed for this conversation; retain its configuration or use a new conversation_id",
                ));
            }
            None
        }
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
        if let Some(project) = &projection.project_uuid {
            lease
                .slot
                .client
                .bind_project(&lease, &scope, &target, project)
                .await?;
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
        tool_descriptors,
        decoder: WebBlockDecoder::new(native_allowed_names),
        emissions: VecDeque::new(),
        upstream_done: false,
        native_blocks: Vec::new(),
        citations: Vec::new(),
        native_bytes: 0,
        file_until: 0,
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
                tools: active.tool_descriptors.clone(),
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
                            || job.next_attempt > now()
                    });
                    if active.file_until > now() {
                        state.queue(&active.lease.account_id, &active.target);
                        if let Some(job) = state.pending.iter_mut().find(|job| {
                            job.account_id == active.lease.account_id
                                && job.conversation_id == active.target
                        }) {
                            job.next_attempt = job.next_attempt.max(active.file_until);
                        }
                    }
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
        if active.file_until > now() {
            active
                .engine
                .store
                .mutate("", json!({}), |state| {
                    state.queue(&active.lease.account_id, &active.target);
                    if let Some(job) = state.pending.iter_mut().find(|job| {
                        job.account_id == active.lease.account_id
                            && job.conversation_id == active.target
                    }) {
                        job.next_attempt = job.next_attempt.max(active.file_until);
                    }
                    Ok(())
                })
                .await?;
            cleanup.armed = false;
        } else {
            cleanup.run_with_lease(&active.lease).await;
        }
    }

    let accounted_output = accounting_output(text, &active.native_blocks, &active.citations)?;
    let usage = token_usage(&active.prompt, &accounted_output);
    record_usage(
        &active.engine,
        &active.lease,
        &active.scope,
        &active.model,
        &active.prompt,
        &accounted_output,
        200,
    )
    .await?;
    Ok(usage)
}

async fn pull_text(active: &mut Active, output: &mut String) -> Result<Pull> {
    loop {
        if let Some(emission) = active.emissions.pop_front() {
            return emission_to_pull(active, output, emission);
        }
        if active.upstream_done {
            return Ok(Pull::Done);
        }
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
        let emissions = active.decoder.feed(&event)?;
        if event["type"] == "message_stop" {
            active.decoder.finish()?;
            active.upstream_done = true;
        }
        active.emissions.extend(emissions);
        if let Some(emission) = active.emissions.pop_front() {
            return emission_to_pull(active, output, emission);
        }
    }
}

fn emission_to_pull(
    active: &mut Active,
    output: &mut String,
    emission: WebEmission,
) -> Result<Pull> {
    match emission {
        WebEmission::Text(delta) => {
            if output
                .len()
                .saturating_add(active.native_bytes)
                .saturating_add(delta.len())
                > MAX_OUTPUT_BYTES
            {
                return Err(Fault::new(
                    502,
                    "api_error",
                    "upstream output exceeds limit",
                ));
            }
            output.push_str(&delta);
            Ok(Pull::Text(delta))
        }
        WebEmission::Citation(citation) => {
            let citation = crate::native_tools::sanitize(citation);
            let size = serde_json::to_vec(&citation)
                .map_err(|_| Fault::upstream())?
                .len();
            active.native_bytes = active.native_bytes.saturating_add(size);
            if output.len().saturating_add(active.native_bytes) > MAX_OUTPUT_BYTES {
                return Err(Fault::new(
                    502,
                    "api_error",
                    "upstream output exceeds limit",
                ));
            }
            active.citations.push(citation.clone());
            Ok(Pull::Citation(citation))
        }
        WebEmission::NativeStart(block) => {
            Ok(Pull::NativeStart(crate::native_tools::sanitize(block)))
        }
        WebEmission::NativeDelta(delta) => {
            Ok(Pull::NativeDelta(crate::native_tools::sanitize(delta)))
        }
        WebEmission::NativeStop(block) => {
            let decorated = crate::native_files::decorate(
                crate::native_tools::sanitize(block),
                &active.engine.config.gateway_key,
                &active.trace.token_id(),
                &active.lease.account_id,
                &active.target,
                3600,
            )?;
            active.file_until = active.file_until.max(decorated.expires_at.unwrap_or(0));
            let block = decorated.block;
            let size = serde_json::to_vec(&block)
                .map_err(|_| Fault::upstream())?
                .len();
            active.native_bytes = active.native_bytes.saturating_add(size);
            if output.len().saturating_add(active.native_bytes) > MAX_OUTPUT_BYTES {
                return Err(Fault::new(
                    502,
                    "api_error",
                    "upstream output exceeds limit",
                ));
            }
            active.native_blocks.push(block.clone());
            Ok(Pull::NativeStop(block))
        }
    }
}

fn accounting_output(text: &str, native: &[Value], citations: &[Value]) -> Result<String> {
    if native.is_empty() && citations.is_empty() {
        return Ok(text.to_owned());
    }
    let extension = serde_json::to_string(&json!({
        "claud2api_native": native,
        "claud2api_citations": citations,
    }))
    .map_err(|_| Fault::internal())?;
    Ok(format!("{text}{extension}"))
}

async fn fail_active(active: &mut Active, output: &str, fault: &Fault) {
    active.completion.take();
    active.trace.fail(fault.status);
    let accounted_output = accounting_output(output, &active.native_blocks, &active.citations)
        .unwrap_or_else(|_| output.to_owned());
    let _ = record_usage(
        &active.engine,
        &active.lease,
        &active.scope,
        &active.model,
        &active.prompt,
        &accounted_output,
        fault.status,
    )
    .await;
    if let Some(cleanup) = &mut active.cleanup {
        cleanup.run_with_lease(&active.lease).await;
    }
}

fn chat_chunk(active: &Active, delta: Value, finish_reason: Option<&str>) -> Value {
    let mut chunk = json!({
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
    });
    if finish_reason.is_some() {
        let object = chunk.as_object_mut().expect("chat chunk object");
        object.insert("claud2api_native".into(), json!(active.native_blocks));
        object.insert("claud2api_citations".into(), json!(active.citations));
    }
    chunk
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
        "claud2api_native": active.native_blocks,
        "claud2api_citations": active.citations,
    })
}

fn response_annotations(citations: &[Value], text: &str) -> Vec<Value> {
    citations
        .iter()
        .filter_map(|citation| {
            let url = citation.get("url")?.as_str()?;
            let title = citation.get("title")?.as_str()?;
            let cited_text = citation.get("cited_text")?.as_str()?;
            let start = citation.get("start_index")?.as_u64()? as usize;
            let end = citation.get("end_index")?.as_u64()? as usize;
            if cited_text.is_empty() || end < start || end - start != cited_text.chars().count() {
                return None;
            }
            let mut matches = text.match_indices(cited_text);
            let (byte_index, _) = matches.next()?;
            if matches.next().is_some() {
                return None;
            }
            let start_index = text[..byte_index].chars().count();
            let end_index = start_index + cited_text.chars().count();
            Some(json!({
                "type":"url_citation",
                "start_index":start_index,
                "end_index":end_index,
                "url":url,
                "title":title,
            }))
        })
        .collect()
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
            "annotations": response_annotations(&active.citations, text),
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
        "claud2api_native": active.native_blocks,
        "claud2api_citations": active.citations,
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

fn chat_extension(active: &Active, kind: &str, field: &str, value: Value) -> Bytes {
    let mut payload = json!({
        "id":active.response_id,
        "model":active.public_model,
    });
    payload
        .as_object_mut()
        .expect("chat extension object")
        .insert(field.to_owned(), value);
    sse_event(kind, &payload)
}

fn response_extension(
    active: &Active,
    kind: &str,
    sequence: usize,
    field: &str,
    value: Value,
) -> Value {
    let mut payload = json!({
        "response_id":active.response_id,
        "item_id":active.item_id,
    });
    payload
        .as_object_mut()
        .expect("response extension object")
        .insert(field.to_owned(), value);
    response_event(kind, sequence, payload)
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
                Ok(Pull::NativeStart(block)) => {
                    yield Ok(chat_extension(&active, "claud2api.native_start", "block", block));
                }
                Ok(Pull::NativeDelta(delta)) => {
                    yield Ok(chat_extension(&active, "claud2api.native_delta", "delta", delta));
                }
                Ok(Pull::NativeStop(block)) => {
                    yield Ok(chat_extension(&active, "claud2api.native_stop", "block", block));
                }
                Ok(Pull::Citation(citation)) => {
                    yield Ok(chat_extension(&active, "claud2api.citation", "citation", citation));
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
                                    "claud2api_native": active.native_blocks,
                                    "claud2api_citations": active.citations,
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
                Ok(Pull::NativeStart(block)) => {
                    let event = response_extension(
                        &active,
                        "claud2api.native_start",
                        sequence,
                        "block",
                        block,
                    );
                    yield Ok(sse_event("claud2api.native_start", &event));
                    sequence += 1;
                }
                Ok(Pull::NativeDelta(delta)) => {
                    let event = response_extension(
                        &active,
                        "claud2api.native_delta",
                        sequence,
                        "delta",
                        delta,
                    );
                    yield Ok(sse_event("claud2api.native_delta", &event));
                    sequence += 1;
                }
                Ok(Pull::NativeStop(block)) => {
                    let event = response_extension(
                        &active,
                        "claud2api.native_stop",
                        sequence,
                        "block",
                        block,
                    );
                    yield Ok(sse_event("claud2api.native_stop", &event));
                    sequence += 1;
                }
                Ok(Pull::Citation(citation)) => {
                    let event = response_extension(
                        &active,
                        "claud2api.citation",
                        sequence,
                        "citation",
                        citation,
                    );
                    yield Ok(sse_event("claud2api.citation", &event));
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
                                "part": {
                                    "type":"output_text",
                                    "text":output,
                                    "annotations":response_annotations(&active.citations, &output),
                                },
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
            Ok(
                Pull::Text(_)
                | Pull::NativeStart(_)
                | Pull::NativeDelta(_)
                | Pull::NativeStop(_)
                | Pull::Citation(_),
            ) => {}
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
    let base_key = trace.legacy_key(&conversation_id, &engine.config.gateway_key);
    let _lock = engine.legacy_lock(&base_key).await?;
    let candidate_keys = legacy_profile_keys(&base_key, &engine.config.gateway_key);
    let mappings: Vec<(String, String, String)> = {
        let legacy = engine.legacy.lock().await;
        candidate_keys
            .iter()
            .chain(
                legacy
                    .keys()
                    .filter(|candidate| candidate.starts_with(&format!("{base_key}:"))),
            )
            .filter_map(|key| {
                legacy
                    .get(key)
                    .cloned()
                    .map(|mapping| (key.clone(), mapping.0, mapping.1))
            })
            .collect()
    };
    if mappings.is_empty() {
        return Err(Fault::new(404, "not_found_error", "conversation not found"));
    }
    let settings = engine.store.read().await.settings;
    let scope = Scope::new(settings.request_timeout_seconds);
    for (key, account_id, upstream_id) in mappings {
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
                state.pending.retain(|job| {
                    job.account_id != account_id || job.conversation_id != upstream_id
                });
                Ok(())
            })
            .await?;
    }
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
    fn native_tool_extension_is_independent_of_client_tool_fields() {
        let raw = json!({
            "model":"claude-test",
            "claud2api_tools":["web_search","artifacts"],
            "messages":[{"role":"user","content":"search"}]
        });
        let projection = Projection::chat(&raw).expect("native tool groups are accepted");
        assert_eq!(
            projection.native_tools,
            Some(json!(["web_search", "artifacts"]))
        );
        assert!(!projection.prompt.contains("Do not use tools"));

        let response = json!({
            "model":"claude-test",
            "claud2api_tools":[],
            "input":"plain text"
        });
        let projection = Projection::responses(&response).expect("empty group list is accepted");
        assert_eq!(projection.native_tools, Some(json!([])));

        let client_function = json!({
            "model":"claude-test",
            "tools":[],
            "messages":[{"role":"user","content":"run"}]
        });
        assert_eq!(Projection::chat(&client_function).unwrap_err().status, 400);
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
    fn structured_native_results_are_included_in_output_accounting() {
        let text = "Answer";
        let native = vec![json!({
            "type":"tool_result",
            "tool_use_id":"call_1",
            "content":[{"type":"knowledge","title":"Guide","url":"https://example.com"}]
        })];
        let citations = vec![json!({"url":"https://example.com","cited_text":"Guide"})];
        let accounted = accounting_output(text, &native, &citations).unwrap();
        assert!(accounted.starts_with(text));
        assert!(accounted.contains("https://example.com"));
        assert!(protocol::estimate(&accounted) > protocol::estimate(text));
    }

    #[test]
    fn response_annotations_use_real_unique_unicode_cited_text() {
        let text = "开头 猫🌸 结尾";
        let citations = vec![json!({
            "url":"https://example.com/source",
            "title":"Source",
            "cited_text":"猫🌸",
            "start_index":0,
            "end_index":2,
            "text_block_index":7
        })];
        assert_eq!(
            response_annotations(&citations, text),
            vec![json!({
                "type":"url_citation",
                "start_index":3,
                "end_index":5,
                "url":"https://example.com/source",
                "title":"Source"
            })]
        );

        let ambiguous = vec![json!({
            "url":"https://example.com/source",
            "title":"Source",
            "cited_text":"猫🌸",
            "start_index":0,
            "end_index":2
        })];
        assert!(response_annotations(&ambiguous, "猫🌸 and 猫🌸").is_empty());
    }

    #[test]
    fn persistent_keys_change_when_native_tool_profile_changes() {
        let base = "opaque-session-key";
        let secret = "test-gateway-secret";
        let empty = profiled_legacy_key(base, secret, &[]);
        assert_eq!(project_legacy_key(base, secret, &[], &None), base);
        let search = profiled_legacy_key(base, secret, &["web_search".to_owned()]);
        assert_eq!(empty, profiled_legacy_key(base, secret, &[]));
        assert_ne!(empty, search);
        assert_eq!(legacy_profile_keys(base, secret).len(), 17);
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
