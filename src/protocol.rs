use crate::error::{Fault, Result};
use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{collections::HashSet, sync::LazyLock};

static NAME: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^[A-Za-z0-9_-]{1,128}$").expect("constant regex"));
static OLD_ID: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^user_[a-fA-F0-9]{64}_account_[a-fA-F0-9-]*_session_([a-fA-F0-9-]{36})$")
        .expect("constant regex")
});
static CODE_ATTRIBUTION: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"\Ax-anthropic-billing-header: cc_version=[0-9]+\.[0-9]+\.[0-9]+(?:\.[A-Za-z0-9_-]+)?; cc_entrypoint=[A-Za-z0-9_.-]+;(?: cch=[A-Fa-f0-9]{5};)?(?: cc_workload=[A-Za-z0-9_.-]+;)?\z",
    )
    .expect("constant regex")
});

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "type")]
pub enum Block {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    Tool {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    Result {
        tool_use_id: String,
        content: Vec<Block>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}
impl Block {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Turn {
    pub role: String,
    pub content: Vec<Block>,
}
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub input_schema: Value,
}
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Choice {
    #[serde(rename = "type")]
    pub mode: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(
        default,
        rename = "disable_parallel_tool_use",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub single: bool,
}
impl Default for Choice {
    fn default() -> Self {
        Self {
            mode: "auto".to_owned(),
            name: String::new(),
            single: false,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Input {
    pub model: String,
    pub stream: bool,
    pub max_tokens: usize,
    pub system: String,
    pub turns: Vec<Turn>,
    pub tools: Vec<Tool>,
    pub choice: Choice,
    pub summary: String,
    instruction_offsets: Vec<(usize, usize)>,
}
fn string<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| Fault::invalid("required string field is missing or invalid"))
}
fn label(value: &str, allowed: &[&str]) -> String {
    if value.is_empty() {
        "empty".to_owned()
    } else if allowed.contains(&value) {
        value.to_owned()
    } else {
        "other".to_owned()
    }
}
fn blocks(value: &Value, assistant: bool, path: &str) -> Result<Vec<Block>> {
    if let Some(text) = value.as_str() {
        return Ok(vec![Block::text(text)]);
    }
    let raw = value
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| Fault::invalid("content must be a string or a nonempty block array"))?;
    let mut out = Vec::new();
    for (i, item) in raw.iter().enumerate() {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "thinking" | "redacted_thinking" if assistant => {}
            "text" => out.push(Block::text(string(item, "text")?)),
            "tool_use" => {
                let id = string(item, "id")?;
                let name = string(item, "name")?;
                if id.is_empty()
                    || id.len() > 200
                    || !NAME.is_match(name)
                    || !item["input"].is_object()
                {
                    return Err(Fault::invalid("invalid tool_use block"));
                }
                out.push(Block::Tool {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    input: item["input"].clone(),
                });
            }
            "tool_result" => {
                let id = string(item, "tool_use_id")?;
                if id.is_empty() {
                    return Err(Fault::invalid("invalid tool_result block; use tool_use_id"));
                }
                let content = blocks(&item["content"], false, &format!("{path}.content[{i}]"))?;
                if content.iter().any(|b| !matches!(b, Block::Text { .. })) {
                    return Err(Fault::invalid("tool_result supports text content only"));
                }
                let is_error = match item.get("is_error") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(
                        v.as_bool()
                            .ok_or_else(|| Fault::invalid("is_error must be a boolean"))?,
                    ),
                };
                out.push(Block::Result {
                    tool_use_id: id.to_owned(),
                    content,
                    is_error,
                });
            }
            _ => {
                return Err(Fault::invalid(format!(
                    "unsupported content block type ({path}.content[{i}], block_kind={}); only text and client tools are supported",
                    label(
                        kind,
                        &[
                            "image",
                            "document",
                            "thinking",
                            "redacted_thinking",
                            "server_tool_use",
                            "web_search_tool_result",
                            "web_fetch_tool_result",
                            "tool_reference",
                            "search_result",
                            "container_upload",
                            "compaction"
                        ]
                    )
                )));
            }
        }
    }
    Ok(out)
}
fn instruction(value: &Value, path: &str) -> Result<Vec<String>> {
    blocks(value, false, path)?
        .into_iter()
        .map(|b| {
            if let Block::Text { text } = b {
                Ok(text)
            } else {
                Err(Fault::invalid("system supports text only"))
            }
        })
        .collect::<Result<Vec<_>>>()
}
fn strip_code_attribution(parts: &mut Vec<String>) {
    let Some(first) = parts.first() else {
        return;
    };
    let (line, rest) = first
        .split_once('\n')
        .map_or((first.as_str(), None), |(line, rest)| (line, Some(rest)));
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.len() > 512 || !CODE_ATTRIBUTION.is_match(line) {
        return;
    }
    // This web bridge reshapes system content, so the API's positional strip
    // cannot apply. Remove only known leading client metadata, never the rest
    // of a merged block or a matching string in actual conversation/tool data.
    if let Some(rest) = rest.filter(|rest| !rest.is_empty()) {
        parts[0] = rest.to_owned();
    } else {
        parts.remove(0);
    }
}
fn local_schema(schema: &Value, depth: usize) -> Result<()> {
    if depth > 64 {
        return Err(Fault::invalid("schema nesting exceeds limit"));
    }
    match schema {
        Value::Object(m) => {
            for (k, v) in m {
                if (k == "$ref" || k == "$dynamicRef" || k == "$recursiveRef")
                    && !v.as_str().is_some_and(|s| s.starts_with('#'))
                {
                    return Err(Fault::invalid("external schema references are disabled"));
                }
                local_schema(v, depth + 1)?;
            }
        }
        Value::Array(a) => {
            for v in a {
                local_schema(v, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}
pub fn parse(raw: &Value) -> Result<Input> {
    let model = string(raw, "model")?;
    if model.trim().is_empty() || model.len() > 150 {
        return Err(Fault::invalid("model is required"));
    }
    let max = raw["max_tokens"]
        .as_u64()
        .filter(|v| (1..=131072).contains(v))
        .ok_or_else(|| {
            Fault::invalid(
                "max_tokens must be between 1 and 131072; the web upstream budget remains advisory",
            )
        })?;
    let stream = match raw.get("stream") {
        None | Some(Value::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| Fault::invalid("stream must be boolean"))?,
    };
    let messages = raw["messages"]
        .as_array()
        .filter(|m| !m.is_empty() && m.len() <= 2000)
        .ok_or_else(|| Fault::invalid("messages must contain between 1 and 2000 turns"))?;
    let mut instructions = Vec::new();
    if let Some(system) = raw.get("system") {
        let mut parts = instruction(system, "system")?;
        strip_code_attribution(&mut parts);
        if !parts.is_empty() {
            instructions.push(parts.join("\n"));
        }
    }
    let mut turns = Vec::new();
    let mut instruction_offsets = Vec::new();
    let mut system_size = instructions.first().map_or(0, String::len);
    let mut used = HashSet::new();
    let mut pending = HashSet::new();
    let mut last = "";
    for (i, m) in messages.iter().enumerate() {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        if matches!(role, "system" | "developer") {
            let text = instruction(&m["content"], &format!("messages[{i}]"))?.join("\n");
            instruction_offsets.push((turns.len(), system_size));
            system_size += usize::from(!instructions.is_empty()) + text.len();
            instructions.push(text);
            continue;
        }
        if !matches!(role, "user" | "assistant") {
            return Err(Fault::invalid(format!(
                "message role must be user or assistant (messages[{i}], role_kind={})",
                label(role, &["system", "developer", "tool", "function", "model"])
            )));
        }
        if last.is_empty() && role != "user" {
            return Err(Fault::invalid("first message must have user role"));
        }
        if !pending.is_empty() && role != "user" {
            return Err(Fault::invalid(
                "tool calls require a following user tool_result",
            ));
        }
        last = role;
        let content = blocks(
            &m["content"],
            role == "assistant",
            &format!("messages[{i}]"),
        )?;
        let mut text_seen = false;
        for b in &content {
            match b {
                Block::Text { .. } => text_seen = true,
                Block::Tool { id, .. } => {
                    if role != "assistant" || !used.insert(id.clone()) {
                        return Err(Fault::invalid("invalid role or duplicate tool_use id"));
                    }
                    pending.insert(id.clone());
                }
                Block::Result { tool_use_id, .. } => {
                    if role != "user" || text_seen || !pending.remove(tool_use_id) {
                        return Err(Fault::invalid(
                            "tool_result must precede text and reference an outstanding tool_use_id",
                        ));
                    }
                }
            }
        }
        if role == "user" && !pending.is_empty() {
            return Err(Fault::invalid(
                "missing tool_result for an outstanding tool call",
            ));
        }
        if !content.is_empty() {
            turns.push(Turn {
                role: role.to_owned(),
                content,
            });
        }
    }
    if last != "user" || !pending.is_empty() {
        return Err(Fault::invalid(
            "last turn must be a user turn with all tool results supplied",
        ));
    }
    let mut tools = Vec::new();
    let mut names = HashSet::new();
    if let Some(rawtools) = raw.get("tools").filter(|v| !v.is_null()) {
        let a = rawtools
            .as_array()
            .filter(|a| a.len() <= 128)
            .ok_or_else(|| Fault::invalid("invalid or too many tools"))?;
        for t in a {
            let name = string(t, "name")?;
            if !NAME.is_match(name) || !names.insert(name.to_owned()) {
                return Err(Fault::invalid("invalid or duplicate tool name"));
            }
            if t.get("type")
                .and_then(Value::as_str)
                .is_some_and(|k| !matches!(k, "" | "custom"))
            {
                return Err(Fault::invalid("server tools are unsupported"));
            }
            let schema = t["input_schema"].clone();
            if !schema.is_object() || schema["type"] != "object" {
                return Err(Fault::invalid("tool input_schema must have type object"));
            }
            local_schema(&schema, 0)?;
            jsonschema::validator_for(&schema)
                .map_err(|_| Fault::invalid("invalid tool schema or external reference"))?;
            let description = t
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            tools.push(Tool {
                name: name.to_owned(),
                description,
                input_schema: schema,
            });
        }
    }
    let choice = match raw.get("tool_choice").filter(|v| !v.is_null()) {
        None => Choice::default(),
        Some(v) => serde_json::from_value::<Choice>(v.clone())
            .map_err(|_| Fault::invalid("invalid tool_choice"))?,
    };
    match choice.mode.as_str() {
        "auto" | "none" if choice.name.is_empty() => {}
        "any" if !names.is_empty() && choice.name.is_empty() => {}
        "tool" if names.contains(&choice.name) => {}
        _ => return Err(Fault::invalid("invalid or unsupported tool_choice")),
    }
    Ok(Input {
        model: resolve_model(model, "")?,
        stream,
        max_tokens: max as usize,
        system: instructions.join("\n"),
        turns,
        tools,
        choice,
        summary: String::new(),
        instruction_offsets,
    })
}
pub fn resolve_model(requested: &str, fallback: &str) -> Result<String> {
    let requested = if requested.is_empty() {
        fallback
    } else {
        requested
    };
    if requested.trim().is_empty() || requested.len() > 150 {
        return Err(Fault::invalid("model is required"));
    }
    let model = requested.strip_prefix("c2a-").unwrap_or(requested);
    if requested.starts_with("c2a-") && (!model.starts_with("claude-") || model == "claude-") {
        return Err(Fault::invalid(
            "bridge model must be c2a- followed by an exact claude-* web model ID",
        ));
    }
    Ok(model.to_owned())
}

// Go v1 state uses struct field order and HTML-escaped JSON, including null tools.
pub fn go_json<T: Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value)
        .expect("serializable protocol")
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}
pub fn digest<T: Serialize + ?Sized>(key: &str, kind: &str, value: &T) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC accepts any length");
    mac.update(kind.as_bytes());
    mac.update(&[0]);
    mac.update(go_json(value).as_bytes());
    hex::encode(mac.finalize().into_bytes())
}
impl Input {
    pub fn context(&self, key: &str) -> String {
        self.context_for_system(key, &self.system)
    }
    pub(crate) fn context_before(&self, key: &str, completed_turns: usize) -> Option<String> {
        // Only instructions appended after the committed transcript can extend
        // its context. All prior instructions, the model and tools must match.
        self.instruction_offsets
            .iter()
            .find(|(turn, _)| *turn >= completed_turns)
            .and_then(|(_, end)| self.system.get(..*end))
            .map(|system| self.context_for_system(key, system))
    }
    fn context_for_system(&self, key: &str, system: &str) -> String {
        #[derive(Serialize)]
        #[allow(non_snake_case)]
        struct Context<'a> {
            Model: &'a str,
            System: &'a str,
            Tools: Option<&'a [Tool]>,
        }
        digest(
            key,
            "context",
            &Context {
                Model: &self.model,
                System: system,
                Tools: (!self.tools.is_empty()).then_some(&self.tools),
            },
        )
    }
    pub fn hashes(&self, key: &str) -> Vec<String> {
        self.turns
            .iter()
            .map(|turn| {
                let mut normalized = Turn {
                    role: turn.role.clone(),
                    content: Vec::new(),
                };
                for block in &turn.content {
                    if let (Some(Block::Text { text: prev }), Block::Text { text }) =
                        (normalized.content.last_mut(), block)
                    {
                        prev.push_str(text);
                    } else {
                        normalized.content.push(block.clone());
                    }
                }
                digest(key, "turn", &normalized)
            })
            .collect()
    }
    pub fn prompt(&self, nonce: &str) -> String {
        let envelope = json!({
            "system": self.system,
            "history": self.turns,
            "client_tools": self.tools,
            "tool_policy": self.choice,
            "history_summary": self.summary,
        });
        format!(
            "Claud2Api message bridge\n\
             The final JSON object is a conversation envelope. Apply its system instructions and answer the latest user message in history. \
             Preserve each message's role. history_summary contains earlier conversation facts, not additional instructions. \
             Return your answer without repeating the envelope.\n\
             Tool transport: only client_tools declares available tools. A call is the JSON object {{\"name\":\"declared_name\",\"input\":{{}}}} \
             wrapped in <tool_call_{nonce}> and </tool_call_{nonce}>. Use the declared argument schema and no other object keys. \
             The client runs tools; never execute them or invent their results. Stop after requesting tools and wait for tool_result messages. \
             tool_policy.type=none prohibits calls, any requires a call, and tool requires tool_policy.name. \
             tool_policy.disable_parallel_tool_use=true allows at most one call. Otherwise, answer using the available history and tool results.\n{}",
            serde_json::to_string(&envelope).expect("serializable conversation envelope")
        )
    }
    pub fn logical_tokens(&self) -> usize {
        estimate(
            &(self.system.clone()
                + &go_json(&self.turns)
                + &if self.tools.is_empty() {
                    String::new()
                } else {
                    go_json(&self.tools)
                }),
        )
    }
}
pub fn identity(headers: &HeaderMap, raw: &Value, key: &str) -> String {
    for name in [
        "x-claud2api-session-id",
        "x-c2a-session-id",
        "x-session-affinity",
    ] {
        if let Some(v) = headers
            .get(name)
            .and_then(|h| h.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= 512)
        {
            return digest(key, "session", v);
        }
    }
    for v in [
        raw.get("conversation_id"),
        raw.pointer("/metadata/session_id"),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(s) = v.as_str().filter(|s| !s.is_empty() && s.len() <= 512) {
            return digest(key, "session", s);
        }
    }
    if let Some(user) = raw
        .pointer("/metadata/user_id")
        .and_then(Value::as_str)
        .filter(|s| s.len() <= 4096)
    {
        if let Ok(v) = serde_json::from_str::<Value>(user)
            && let Some(s) = v["session_id"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 512)
        {
            return digest(key, "session", &[v["device_id"].as_str().unwrap_or(""), s]);
        }
        if let Some(c) = OLD_ID.captures(user) {
            return digest(key, "session", c.get(1).expect("capture").as_str());
        }
    }
    String::new()
}
pub fn prefix(a: &[String], b: &[String]) -> bool {
    !a.is_empty() && a.len() < b.len() && b.starts_with(a)
}
pub fn estimate(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}
pub fn physical(text: &str) -> usize {
    text.len().div_ceil(2)
}

pub struct ToolDecoder {
    open: String,
    close: String,
    buffer: String,
    inside: bool,
    calls: usize,
    choice: Choice,
    tools: Vec<Tool>,
}
impl ToolDecoder {
    pub fn new(input: &Input, nonce: &str) -> Self {
        Self {
            open: format!("<tool_call_{nonce}>"),
            close: format!("</tool_call_{nonce}>"),
            buffer: String::new(),
            inside: false,
            calls: 0,
            choice: input.choice.clone(),
            tools: input.tools.clone(),
        }
    }
    pub fn feed(&mut self, text: &str) -> Result<Vec<Block>> {
        self.buffer.push_str(text);
        let mut out = Vec::new();
        loop {
            if self.inside {
                let Some(end) = self.buffer.find(&self.close) else {
                    if self.buffer.len() > 1 << 20 {
                        return Err(Fault::upstream());
                    }
                    break;
                };
                if end > 1 << 20 {
                    return Err(Fault::upstream());
                }
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Call {
                    name: String,
                    input: Value,
                }
                let call: Call = serde_json::from_str(&self.buffer[..end])
                    .map_err(|_| Fault::new(502, "api_error", "invalid model tool frame"))?;
                if !call.input.is_object()
                    || self.choice.mode == "none"
                    || (self.choice.mode == "tool" && self.choice.name != call.name)
                {
                    return Err(Fault::new(502, "api_error", "model violated tool_choice"));
                }
                let tool = self
                    .tools
                    .iter()
                    .find(|t| t.name == call.name)
                    .ok_or_else(|| {
                        Fault::new(502, "api_error", "model requested an undeclared tool")
                    })?;
                let schema =
                    jsonschema::validator_for(&tool.input_schema).map_err(|_| Fault::internal())?;
                if !schema.is_valid(&call.input) {
                    return Err(Fault::new(
                        502,
                        "api_error",
                        "model tool arguments do not match input_schema",
                    ));
                }
                self.calls += 1;
                if self.calls > 128 || (self.choice.single && self.calls > 1) {
                    return Err(Fault::new(
                        502,
                        "api_error",
                        "model exceeded tool call limit",
                    ));
                }
                out.push(Block::Tool {
                    id: format!("toolu_{}", uuid::Uuid::new_v4().simple()),
                    name: call.name,
                    input: call.input,
                });
                self.buffer.drain(..end + self.close.len());
                self.inside = false;
            } else if let Some(start) = self.buffer.find(&self.open) {
                if start > 0 {
                    out.push(Block::text(&self.buffer[..start]));
                }
                self.buffer.drain(..start + self.open.len());
                self.inside = true;
            } else {
                let keep = (1..self.open.len().min(self.buffer.len() + 1))
                    .filter(|&n| self.buffer.ends_with(&self.open[..n]))
                    .max()
                    .unwrap_or(0);
                let ready = self.buffer.len() - keep;
                if ready > 0 {
                    out.push(Block::text(&self.buffer[..ready]));
                    self.buffer.drain(..ready);
                }
                break;
            }
        }
        Ok(out)
    }
    pub fn finish(&mut self) -> Result<Vec<Block>> {
        if self.inside {
            return Err(Fault::new(502, "api_error", "incomplete model tool frame"));
        }
        if matches!(self.choice.mode.as_str(), "any" | "tool") && self.calls == 0 {
            return Err(Fault::new(
                502,
                "api_error",
                "model did not produce the required tool call",
            ));
        }
        Ok(if self.buffer.is_empty() {
            vec![]
        } else {
            vec![Block::text(std::mem::take(&mut self.buffer))]
        })
    }
}
pub fn merge_text(out: &mut Vec<Block>, b: Block) {
    if let (Some(Block::Text { text: prev }), Block::Text { text }) = (out.last_mut(), &b) {
        prev.push_str(text);
    } else {
        out.push(b);
    }
}
pub fn usage(input: usize, output: usize, cache: bool, read: usize) -> Value {
    if cache {
        let read = read.min(input);
        json!({"input_tokens":0,"output_tokens":output,"cache_read_input_tokens":read,"cache_creation_input_tokens":input-read,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":input-read}})
    } else {
        json!({"input_tokens":input,"output_tokens":output})
    }
}
