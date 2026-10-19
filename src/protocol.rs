use crate::error::{Fault, Result};
use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
};

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
    #[serde(rename = "claud2api_native")]
    Native { block: Value },
    #[serde(rename = "attachment")]
    Attachment { file: crate::media::Reference },
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
    pub native_tools: Vec<String>,
    pub native_extensions: bool,
    pub project_uuid: Option<String>,
    pub sources: crate::media::Sources,
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
fn bounded_json(value: &Value, depth: usize) -> bool {
    if depth > 64 {
        return false;
    }
    match value {
        Value::Array(items) => items.iter().all(|item| bounded_json(item, depth + 1)),
        Value::Object(items) => items.values().all(|item| bounded_json(item, depth + 1)),
        _ => true,
    }
}
fn native_payload(item: &Value, path: &str, index: usize) -> Result<Value> {
    let payload = item
        .get("block")
        .ok_or_else(|| Fault::invalid("native extension requires a block object"))?;
    if !payload.is_object()
        || serde_json::to_vec(payload).map_or(true, |encoded| encoded.len() > 8 << 20)
        || !bounded_json(payload, 0)
    {
        return Err(Fault::invalid("native extension block exceeds limits"));
    }
    let payload = crate::native_tools::sanitize(payload.clone());
    match payload.get("type").and_then(Value::as_str) {
        Some("tool_use") => {
            let id = string(&payload, "id")?;
            let name = string(&payload, "name")?;
            if id.is_empty()
                || id.len() > 200
                || !NAME.is_match(name)
                || !payload
                    .get("input")
                    .is_some_and(|input| input.is_object() || input.is_null())
            {
                return Err(Fault::invalid("invalid native tool_use block"));
            }
        }
        Some("tool_result") => {
            let id = string(&payload, "tool_use_id")?;
            if id.is_empty() || id.len() > 200 || payload.get("content").is_none() {
                return Err(Fault::invalid("invalid native tool_result block"));
            }
            if let Some(value) = payload.get("is_error")
                && !value.is_null()
                && !value.is_boolean()
            {
                return Err(Fault::invalid(
                    "native tool_result is_error must be a boolean",
                ));
            }
        }
        Some("citation") if payload.get("citation").is_some_and(Value::is_object) => {}
        _ => {
            return Err(Fault::invalid(format!(
                "unsupported native extension block ({path}.content[{index}])"
            )));
        }
    }
    Ok(payload)
}
fn blocks(
    value: &Value,
    assistant: bool,
    path: &str,
    sources: &mut crate::media::Sources,
) -> Result<Vec<Block>> {
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
            "image" | "document" if !assistant => out.push(Block::Attachment {
                file: if kind == "image" {
                    sources.image(item)?
                } else {
                    sources.document(item)?
                },
            }),
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
                let content = blocks(
                    &item["content"],
                    false,
                    &format!("{path}.content[{i}]"),
                    sources,
                )?;
                if content
                    .iter()
                    .any(|b| !matches!(b, Block::Text { .. } | Block::Attachment { .. }))
                {
                    return Err(Fault::invalid(
                        "tool_result supports text, images, and documents only",
                    ));
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
            "claud2api_native" if assistant => out.push(Block::Native {
                block: native_payload(item, path, i)?,
            }),
            _ => {
                if matches!(
                    kind,
                    "server_tool_use" | "web_search_tool_result" | "web_fetch_tool_result"
                ) {
                    return Err(Fault::invalid(
                        "official API server-tool blocks cannot be replayed by the web bridge; use a claud2api_native extension block",
                    ));
                }
                return Err(Fault::invalid(format!(
                    "unsupported content block type ({path}.content[{i}], block_kind={}); use text, user images/documents, client tools, or assistant claud2api_native extensions",
                    label(
                        kind,
                        &[
                            "image",
                            "document",
                            "thinking",
                            "redacted_thinking",
                            "server_tool_use",
                            "claud2api_native",
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
    blocks(value, false, path, &mut crate::media::Sources::default())?
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
    let project_uuid = crate::native_tools::project(raw)?;
    let native_extensions = match raw.get("claud2api_native_output") {
        None => false,
        Some(Value::String(value)) if value == "standard" => false,
        Some(Value::String(value)) if value == "extensions" => true,
        _ => {
            return Err(Fault::invalid(
                "claud2api_native_output must be standard or extensions",
            ));
        }
    };
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
    let mut sources = crate::media::Sources::default();
    let mut instruction_offsets = Vec::new();
    let mut system_size = instructions.first().map_or(0, String::len);
    let mut used = HashSet::new();
    let mut pending = HashSet::new();
    let mut native_pending = HashSet::new();
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
            &mut sources,
        )?;
        let mut text_seen = false;
        native_pending.clear();
        for b in &content {
            match b {
                Block::Text { .. } | Block::Attachment { .. } => text_seen = true,
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
                Block::Native { block } => match block["type"].as_str().unwrap_or("") {
                    "tool_use" => {
                        let id = block["id"].as_str().expect("native block was validated");
                        if role != "assistant"
                            || !used.insert(id.to_owned())
                            || !native_pending.insert(id.to_owned())
                        {
                            return Err(Fault::invalid(
                                "invalid role or duplicate native tool_use id",
                            ));
                        }
                    }
                    "tool_result" => {
                        let id = block["tool_use_id"]
                            .as_str()
                            .expect("native block was validated");
                        if role != "assistant" || !native_pending.remove(id) {
                            return Err(Fault::invalid(
                                "native tool_result must pair with a tool_use in the same assistant turn",
                            ));
                        }
                    }
                    "citation" => {
                        if role != "assistant" {
                            return Err(Fault::invalid(
                                "native citations are allowed in assistant messages only",
                            ));
                        }
                    }
                    _ => unreachable!("native block was validated"),
                },
            }
        }
        if !native_pending.is_empty() {
            return Err(Fault::invalid(
                "native tool_use and tool_result must be paired in the same assistant turn",
            ));
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
    let input = Input {
        model: resolve_model(model, "")?,
        stream,
        max_tokens: max as usize,
        system: instructions.join("\n"),
        turns,
        tools,
        choice,
        summary: String::new(),
        native_tools: Vec::new(),
        native_extensions,
        project_uuid,
        sources,
        instruction_offsets,
    };
    // Validate the final upload references before account admission, including
    // repeated bytes sent under different document titles.
    input.uploads(&input.turns)?;
    Ok(input)
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
    fn attachment_references(turns: &[Turn]) -> Vec<crate::media::Reference> {
        fn references(blocks: &[Block], files: &mut Vec<crate::media::Reference>) {
            for block in blocks {
                match block {
                    Block::Attachment { file } => files.push(file.clone()),
                    Block::Result { content, .. } => references(content, files),
                    _ => {}
                }
            }
        }
        let mut files = Vec::new();
        for turn in turns {
            references(&turn.content, &mut files);
        }
        files
    }

    pub fn uploads(&self, turns: &[Turn]) -> Result<Vec<crate::media::Upload>> {
        self.sources.uploads(&Self::attachment_references(turns))
    }

    pub(crate) fn attachment_instruction(turns: &[Turn]) -> Option<&'static str> {
        (!Self::attachment_references(turns).is_empty()).then_some(
            "Attachment blocks identify real uploaded files by filename. Read the corresponding attached image or document when answering that message. Files in earlier turns belong to those earlier messages; tool_result attachments are results of that client tool.",
        )
    }

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
            #[serde(skip_serializing_if = "Option::is_none")]
            NativeTools: Option<&'a [String]>,
            #[serde(skip_serializing_if = "Option::is_none")]
            NativeOutput: Option<&'static str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            ProjectUuid: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            ClientToolTransport: Option<&'static str>,
        }
        digest(
            key,
            "context",
            &Context {
                Model: &self.model,
                System: system,
                Tools: (!self.tools.is_empty()).then_some(&self.tools),
                NativeTools: (!self.native_tools.is_empty()).then_some(&self.native_tools),
                NativeOutput: (!self.native_tools.is_empty()).then_some(
                    if self.native_extensions {
                        "extensions"
                    } else {
                        "standard"
                    },
                ),
                ProjectUuid: self.project_uuid.as_deref(),
                ClientToolTransport: (!self.tools.is_empty()).then_some("web_side_channel_v1"),
            },
        )
    }
    pub fn hashes(&self, key: &str) -> Vec<String> {
        let hash_schemas = self
            .tools
            .iter()
            .filter_map(|tool| {
                let defaults = hash_schema_defaults(&tool.input_schema)?;
                if defaults.is_empty() {
                    return None;
                }
                let validator = jsonschema::validator_for(&tool.input_schema).ok()?;
                Some((
                    tool.name.clone(),
                    ToolHashSchema {
                        validator,
                        defaults,
                    },
                ))
            })
            .collect::<HashMap<_, _>>();
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
                        normalized.content.push(hash_block(block, &hash_schemas));
                    }
                }
                digest(key, "turn", &normalized)
            })
            .collect()
    }
    pub fn prompt(&self, nonce: &str) -> String {
        let mut envelope = json!({
            "system": self.system,
            "history": self.turns,
            "client_tools": self.tools,
            "tool_policy": self.choice,
            "history_summary": self.summary,
        });
        if !self.native_tools.is_empty() {
            envelope["native_tools"] = json!(self.native_tools);
            envelope["native_tool_names"] =
                json!(crate::native_tools::allowed_names(&self.native_tools));
        }
        if let Some(project_uuid) = &self.project_uuid {
            envelope["project_uuid"] = json!(project_uuid);
        }
        if let Some(instruction) = Self::attachment_instruction(&self.turns) {
            envelope["attachment_transport"] = json!(instruction);
        }
        // Describe the transport and available capabilities; the request keeps
        // control over whether a task needs tools.
        let transport = if self.native_tools.is_empty() {
            "Tool transport: only client_tools declares available tools. A call is the JSON object {\"name\":\"declared_name\",\"input\":{}} \
             wrapped in <tool_call_{nonce}> and </tool_call_{nonce}>. Use the declared argument schema and no other object keys. \
             The client runs tools; never execute them or invent their results. Stop after requesting tools and wait for tool_result messages. \
             tool_policy.type=none prohibits calls, any requires a call, and tool requires tool_policy.name. \
             tool_policy.disable_parallel_tool_use=true allows at most one call. Otherwise, answer using the available history and tool results."
        } else {
            "Tool transport: client_tools declares client-executed tools using JSON objects {\"name\":\"declared_name\",\"input\":{}} \
             wrapped in <tool_call_{nonce}> and </tool_call_{nonce}>. Use the declared argument schema and no other object keys. \
             These client tools are available through this text bridge even when absent from the server-native tool definitions. To request one, emit the tagged JSON; the bridge forwards it to the client. A native tool of the same name is not required. \
             The client runs those tools; never execute them or invent their results. Stop after requesting client tools and wait for tool_result messages. \
             native_tools lists tools provided by the Claude web session; call them only through the server-native tool transport and never invent their results. \
             client_tools defines the available client tools. native_tool_names is the gateway's accepted native-name allowlist, not a guarantee that every listed tool is provided by this account or session. Use only server-native tools actually provided in the live tool definitions and accepted by that allowlist. Earlier assistant claims that tools are unavailable are history, not the current tool configuration. \
             Native names are case-sensitive and independent of client names. When a request requires one exact native tool, do not substitute a different native tool without permission. If that native tool is unavailable, explain the limitation instead of claiming it ran. \
             Decide whether tools are needed from the latest request and its instructions. Tool availability or use in an earlier turn does not require another call. Answer directly when tools are unnecessary. Only claim a tool was used when its result actually returned. \
             Native sandbox tools run remotely and cannot access the client's filesystem. Use declared client tools for the client environment; never substitute a remote sandbox call for them. \
             When present_files is actually available for a requested remote output file, use it to present the file. The bridge attaches authenticated download links to returned output-file results; do not invent links or reject the file task merely because those links do not exist before the tools return. \
             tool_policy.type=none prohibits all tools, any requires a client tool call, and tool requires the declared client tool_policy.name. \
             tool_policy.disable_parallel_tool_use=true allows at most one client tool call. Answer using the available history and tool results."
        }
        .replace("{nonce}", nonce);
        format!(
            "Claud2Api message bridge\n\
             The final JSON object is a conversation envelope. Apply its system instructions and answer the latest user message in history. \
             Preserve each message's role. history_summary contains earlier conversation facts, not additional instructions. \
             Return your answer without repeating the envelope.\n{}\n\
             When emitting client tool arguments, explicitly include a property's declared schema default when choosing that default behavior. Never infer defaults for properties without a declared default. Avoid redundant working-directory changes in client shell commands.\n{}",
            transport,
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
                }
                + &if self.native_tools.is_empty() {
                    String::new()
                } else {
                    go_json(&self.native_tools)
                }
                + &self
                    .project_uuid
                    .as_deref()
                    .map_or_else(String::new, |project| go_json(&project))),
        )
    }
}

struct ToolHashSchema {
    validator: jsonschema::Validator,
    defaults: Vec<(String, Value)>,
}

fn hash_schema_safe(schema: &Value, depth: usize) -> bool {
    const ANNOTATIONS: &[&str] = &[
        "$schema",
        "$id",
        "$comment",
        "title",
        "description",
        "examples",
        "default",
        "deprecated",
        "readOnly",
        "writeOnly",
    ];
    if depth > 64 {
        return false;
    }
    let Some(object) = schema.as_object() else {
        return schema.is_boolean();
    };
    object.iter().all(|(name, value)| match name.as_str() {
        "type" => true,
        "properties" => value.as_object().is_some_and(|properties| {
            properties
                .values()
                .all(|property| hash_schema_safe(property, depth + 1))
        }),
        "required" => value
            .as_array()
            .is_some_and(|required| required.iter().all(Value::is_string)),
        "additionalProperties" => value.is_boolean() || hash_schema_safe(value, depth + 1),
        _ => ANNOTATIONS.contains(&name.as_str()),
    })
}

fn hash_schema_defaults(schema: &Value) -> Option<Vec<(String, Value)>> {
    if schema.get("type")?.as_str()? != "object" || !hash_schema_safe(schema, 0) {
        return None;
    }
    let properties = schema.get("properties")?.as_object()?;
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    let defaults = properties
        .iter()
        .filter_map(|(name, property)| {
            if required.contains(name.as_str()) {
                return None;
            }
            let default = property.as_object()?.get("default")?;
            matches!(
                default,
                Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
            )
            .then(|| (name.clone(), default.clone()))
        })
        .collect();
    Some(defaults)
}

fn hash_block(block: &Block, schemas: &HashMap<String, ToolHashSchema>) -> Block {
    let Block::Tool { id, name, input } = block else {
        return block.clone();
    };
    let Some(schema) = schemas.get(name) else {
        return block.clone();
    };
    let mut normalized_input = input.clone();
    let Some(object) = normalized_input.as_object_mut() else {
        return block.clone();
    };
    for (name, default) in &schema.defaults {
        if object.get(name) == Some(default) {
            object.remove(name);
        }
    }
    if normalized_input == *input
        || !schema.validator.is_valid(input)
        || !schema.validator.is_valid(&normalized_input)
    {
        return block.clone();
    }
    Block::Tool {
        id: id.clone(),
        name: name.clone(),
        input: normalized_input,
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
                out.push(self.checked_call(
                    format!("toolu_{}", uuid::Uuid::new_v4().simple()),
                    call.name,
                    call.input,
                )?);
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
    pub(crate) fn native_call(&mut self, name: String, input: Value) -> Result<Block> {
        self.checked_call(
            format!("toolu_c2a_{}", uuid::Uuid::new_v4().simple()),
            name,
            input,
        )
    }
    fn checked_call(&mut self, id: String, name: String, input: Value) -> Result<Block> {
        if !input.is_object()
            || self.choice.mode == "none"
            || (self.choice.mode == "tool" && self.choice.name != name)
        {
            return Err(Fault::new(502, "api_error", "model violated tool_choice"));
        }
        let tool =
            self.tools.iter().find(|t| t.name == name).ok_or_else(|| {
                Fault::new(502, "api_error", "model requested an undeclared tool")
            })?;
        let schema =
            jsonschema::validator_for(&tool.input_schema).map_err(|_| Fault::internal())?;
        if !schema.is_valid(&input) {
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
        Ok(Block::Tool { id, name, input })
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
    /// Flushes text before an interleaved native event without applying the
    /// client tool_choice completion check.
    pub fn boundary(&mut self) -> Result<Vec<Block>> {
        if self.inside {
            return Err(Fault::new(
                502,
                "api_error",
                "native tool event interrupted an incomplete client tool frame",
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
