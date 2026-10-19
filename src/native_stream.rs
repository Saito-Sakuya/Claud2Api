use crate::error::{Fault, Result};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

const MAX_PARTIAL_JSON: usize = 1 << 20;
const MAX_STREAM_BYTES: usize = 8 << 20;
const MAX_BLOCKS: usize = 256;
const MAX_CALLS: usize = 128;
const MAX_CITATIONS: usize = 256;
const MAX_ID_BYTES: usize = 200;

#[derive(Clone, Debug, PartialEq)]
pub enum WebEmission {
    Text(String),
    Citation(Value),
    NativeStart(Value),
    NativeDelta(Value),
    NativeStop(Value),
}

pub struct WebBlockDecoder {
    allowed_names: HashSet<String>,
    active: Option<ActiveBlock>,
    next_index: usize,
    total_bytes: usize,
    structured: bool,
    legacy: bool,
    ended: bool,
    calls: HashMap<String, CallState>,
    citation_count: usize,
    citation_text_bytes: usize,
}

enum ActiveBlock {
    Text {
        index: usize,
        text: String,
        chars: usize,
        open_citations: HashMap<String, PendingCitation>,
    },
    Thinking {
        index: usize,
    },
    ToolUse(ToolUseBlock),
    ToolResult(ToolResultBlock),
}

impl ActiveBlock {
    fn index(&self) -> usize {
        match self {
            Self::Text { index, .. } | Self::Thinking { index } => *index,
            Self::ToolUse(block) => block.index,
            Self::ToolResult(block) => block.index,
        }
    }
}

struct ToolUseBlock {
    index: usize,
    id: String,
    block: Value,
    initial_input: Value,
    partial_json: String,
    saw_delta: bool,
}

struct ToolResultBlock {
    index: usize,
    tool_use_id: String,
    block: Value,
    content_seen: bool,
    snapshot: Option<String>,
}

struct CallState {
    name: String,
    result_started: bool,
    result_complete: bool,
}

struct PendingCitation {
    citation: Value,
    start_index: usize,
}

impl WebBlockDecoder {
    pub fn new(allowed_names: impl IntoIterator<Item = String>) -> Self {
        Self {
            allowed_names: allowed_names.into_iter().collect(),
            active: None,
            next_index: 0,
            total_bytes: 0,
            structured: false,
            legacy: false,
            ended: false,
            calls: HashMap::new(),
            citation_count: 0,
            citation_text_bytes: 0,
        }
    }

    pub fn feed(&mut self, event: &Value) -> Result<Vec<WebEmission>> {
        if !event.is_object() || self.ended {
            return Err(Fault::upstream());
        }
        let serialized = serde_json::to_vec(event).map_err(|_| Fault::upstream())?;
        self.total_bytes = self.total_bytes.saturating_add(serialized.len());
        if self.total_bytes > MAX_STREAM_BYTES {
            return Err(Fault::upstream());
        }

        if event.get("error").is_some_and(|value| !value.is_null())
            || event.get("type").and_then(Value::as_str) == Some("error")
        {
            return Err(Fault::upstream());
        }
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(Fault::upstream)?;

        match kind {
            "message_start" | "ping" | "message_delta" | "message_limit" => Ok(Vec::new()),
            "message_stop" => {
                if self.active.is_some() {
                    return Err(Fault::upstream());
                }
                self.ended = true;
                Ok(Vec::new())
            }
            "content_block_start" => self.start_block(event),
            "content_block_delta" => self.delta(event),
            "content_block_stop" => self.stop_block(event),
            "delta" if self.active.is_none() => self.legacy_text_delta(event),
            _ => Err(Fault::upstream()),
        }
    }

    pub fn finish(&self) -> Result<()> {
        if self.active.is_some() || self.calls.values().any(|call| !call.result_complete) {
            return Err(Fault::upstream());
        }
        Ok(())
    }

    fn start_block(&mut self, event: &Value) -> Result<Vec<WebEmission>> {
        if self.active.is_some() || self.legacy || self.next_index >= MAX_BLOCKS {
            return Err(Fault::upstream());
        }
        let index = event_index(event)?;
        if index != self.next_index {
            return Err(Fault::upstream());
        }
        let mut block = event
            .get("content_block")
            .filter(|value| value.is_object())
            .cloned()
            .ok_or_else(Fault::upstream)?;
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(Fault::upstream)?;
        if !bounded_json_depth(&block, 0) {
            return Err(Fault::upstream());
        }
        self.structured = true;
        self.next_index += 1;

        let mut emissions = Vec::new();
        let active = match block_type.as_str() {
            "text" => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(Fault::upstream)?;
                let chars = text.chars().count();
                if !text.is_empty() {
                    emissions.push(WebEmission::Text(text.to_owned()));
                }
                let open_citations = HashMap::new();
                if let Some(citations) = block.get("citations") {
                    let citations = citations.as_array().ok_or_else(Fault::upstream)?;
                    for citation in citations {
                        self.register_citation(citation)?;
                        let start = citation
                            .get("start_index")
                            .and_then(Value::as_u64)
                            .and_then(|value| usize::try_from(value).ok())
                            .ok_or_else(Fault::upstream)?;
                        let end = citation
                            .get("end_index")
                            .and_then(Value::as_u64)
                            .and_then(|value| usize::try_from(value).ok())
                            .ok_or_else(Fault::upstream)?;
                        if start > end || end > chars {
                            return Err(Fault::upstream());
                        }
                        let cited_text = unicode_slice(text, start, end)?;
                        self.reserve_cited_text(&cited_text)?;
                        let mut citation = citation.clone();
                        let object = citation.as_object_mut().ok_or_else(Fault::upstream)?;
                        object.insert("text_block_index".to_owned(), json!(index));
                        object.insert("cited_text".to_owned(), json!(cited_text));
                        emissions.push(WebEmission::Citation(citation));
                    }
                }
                ActiveBlock::Text {
                    index,
                    text: text.to_owned(),
                    chars,
                    open_citations,
                }
            }
            "thinking" => ActiveBlock::Thinking { index },
            "tool_use" => {
                let id = bounded_id(block.get("id"))?;
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty() && name.len() <= 128)
                    .map(str::to_owned)
                    .ok_or_else(Fault::upstream)?;
                if !self.allowed_names.contains(&name)
                    || self.calls.contains_key(&id)
                    || self.calls.len() >= MAX_CALLS
                {
                    return Err(Fault::upstream());
                }
                let initial_input = match block.get("input") {
                    Some(value) if value.is_null() => json!({}),
                    Some(value) if value.is_object() => value.clone(),
                    _ => return Err(Fault::upstream()),
                };
                block
                    .as_object_mut()
                    .ok_or_else(Fault::upstream)?
                    .insert("input".to_owned(), initial_input.clone());
                emissions.push(WebEmission::NativeStart(block.clone()));
                ActiveBlock::ToolUse(ToolUseBlock {
                    index,
                    id,
                    block,
                    initial_input,
                    partial_json: String::new(),
                    saw_delta: false,
                })
            }
            "tool_result" => {
                let tool_use_id = bounded_id(block.get("tool_use_id"))?;
                if block
                    .get("is_error")
                    .is_some_and(|value| !value.is_boolean())
                {
                    return Err(Fault::upstream());
                }
                let call = self
                    .calls
                    .get_mut(&tool_use_id)
                    .ok_or_else(Fault::upstream)?;
                if call.result_started
                    || block
                        .get("name")
                        .is_some_and(|name| name.as_str() != Some(call.name.as_str()))
                {
                    return Err(Fault::upstream());
                }
                call.result_started = true;
                let content_seen = block.get("content").is_some();
                emissions.push(WebEmission::NativeStart(block.clone()));
                ActiveBlock::ToolResult(ToolResultBlock {
                    index,
                    tool_use_id,
                    block,
                    content_seen,
                    snapshot: None,
                })
            }
            _ => return Err(Fault::upstream()),
        };
        self.active = Some(active);
        Ok(emissions)
    }

    fn delta(&mut self, event: &Value) -> Result<Vec<WebEmission>> {
        if self.active.is_none() {
            return self.legacy_text_delta(event);
        }
        let index = event_index(event)?;
        if self.active.as_ref().map(ActiveBlock::index) != Some(index) {
            return Err(Fault::upstream());
        }
        let delta = event
            .get("delta")
            .filter(|value| value.is_object())
            .ok_or_else(Fault::upstream)?;
        let kind = delta
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(Fault::upstream)?;

        let mut active = self.active.take().ok_or_else(Fault::upstream)?;
        let result = match &mut active {
            ActiveBlock::Text {
                index,
                text: accumulated_text,
                chars,
                open_citations,
            } => match kind {
                "text_delta" => {
                    let text = delta
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(Fault::upstream)?;
                    *chars = chars.saturating_add(text.chars().count());
                    accumulated_text.push_str(text);
                    Ok(vec![WebEmission::Text(text.to_owned())])
                }
                "citation_start_delta" => {
                    let citation = delta
                        .get("citation")
                        .filter(|value| value.is_object())
                        .cloned()
                        .ok_or_else(Fault::upstream)?;
                    let id = self.register_citation(&citation)?;
                    if open_citations
                        .insert(
                            id,
                            PendingCitation {
                                citation,
                                start_index: *chars,
                            },
                        )
                        .is_some()
                    {
                        return Err(Fault::upstream());
                    }
                    Ok(Vec::new())
                }
                "citation_end_delta" => {
                    let id = delta
                        .get("citation_uuid")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES)
                        .ok_or_else(Fault::upstream)?
                        .to_owned();
                    let pending = open_citations.remove(&id).ok_or_else(Fault::upstream)?;
                    if *chars < pending.start_index {
                        return Err(Fault::upstream());
                    }
                    let cited_text = unicode_slice(accumulated_text, pending.start_index, *chars)?;
                    self.reserve_cited_text(&cited_text)?;
                    let mut citation = pending.citation;
                    let object = citation.as_object_mut().ok_or_else(Fault::upstream)?;
                    object.insert("start_index".to_owned(), json!(pending.start_index));
                    object.insert("end_index".to_owned(), json!(*chars));
                    object.insert("text_block_index".to_owned(), json!(*index));
                    object.insert("cited_text".to_owned(), json!(cited_text));
                    Ok(vec![WebEmission::Citation(citation)])
                }
                _ => Err(Fault::upstream()),
            },
            ActiveBlock::Thinking { .. } => match kind {
                "thinking_delta"
                | "signature_delta"
                | "thinking_summary_delta"
                | "thinking_cut_off_delta" => Ok(Vec::new()),
                _ => Err(Fault::upstream()),
            },
            ActiveBlock::ToolUse(block) => {
                if kind == "tool_use_block_update_delta" {
                    if delta
                        .get("tool_use_id")
                        .is_some_and(|id| id.as_str() != Some(block.id.as_str()))
                    {
                        return Err(Fault::upstream());
                    }
                    let object = block.block.as_object_mut().ok_or_else(Fault::upstream)?;
                    for field in ["message", "display_content"] {
                        if let Some(value) = delta.get(field) {
                            object.insert(field.to_owned(), value.clone());
                        }
                    }
                    Ok(vec![WebEmission::NativeDelta(delta.clone())])
                } else if kind != "input_json_delta" {
                    Err(Fault::upstream())
                } else {
                    let partial = delta
                        .get("partial_json")
                        .and_then(Value::as_str)
                        .ok_or_else(Fault::upstream)?;
                    if block.partial_json.len().saturating_add(partial.len()) > MAX_PARTIAL_JSON {
                        return Err(Fault::upstream());
                    }
                    block.partial_json.push_str(partial);
                    block.saw_delta = true;
                    Ok(vec![WebEmission::NativeDelta(delta.clone())])
                }
            }
            ActiveBlock::ToolResult(block) => {
                if kind != "input_json_delta" {
                    return Err(Fault::upstream());
                }
                let snapshot = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .filter(|snapshot| snapshot.len() <= MAX_PARTIAL_JSON)
                    .ok_or_else(Fault::upstream)?;
                block.snapshot = Some(snapshot.to_owned());
                block.content_seen = true;
                Ok(vec![WebEmission::NativeDelta(delta.clone())])
            }
        };
        if result.is_ok() {
            self.active = Some(active);
        }
        result
    }

    fn stop_block(&mut self, event: &Value) -> Result<Vec<WebEmission>> {
        let index = event_index(event)?;
        if self.active.as_ref().map(ActiveBlock::index) != Some(index) {
            return Err(Fault::upstream());
        }
        let active = self.active.take().ok_or_else(Fault::upstream)?;
        match active {
            ActiveBlock::Text { open_citations, .. } => {
                if !open_citations.is_empty() {
                    return Err(Fault::upstream());
                }
                Ok(Vec::new())
            }
            ActiveBlock::Thinking { .. } => Ok(Vec::new()),
            ActiveBlock::ToolUse(block) => {
                let buffered = match event.get("buffered_input") {
                    Some(value) => Some(
                        value
                            .as_str()
                            .filter(|value| value.len() <= MAX_PARTIAL_JSON)
                            .ok_or_else(Fault::upstream)?,
                    ),
                    None => None,
                };
                let partial = if block.saw_delta {
                    Some(parse_input_object(&block.partial_json)?)
                } else {
                    None
                };
                let buffered = buffered.map(parse_input_object).transpose()?;
                let input = match (partial, buffered) {
                    (Some(partial), Some(buffered)) if partial != buffered => {
                        return Err(Fault::upstream());
                    }
                    (Some(partial), _) => partial,
                    (None, Some(buffered)) => buffered,
                    (None, None) => block.initial_input,
                };
                let mut final_block = block.block;
                final_block
                    .as_object_mut()
                    .ok_or_else(Fault::upstream)?
                    .insert("input".to_owned(), input);
                if let Some(timestamp) = event.get("stop_timestamp") {
                    final_block["stop_timestamp"] = timestamp.clone();
                }
                if !bounded_json_depth(&final_block, 0) {
                    return Err(Fault::upstream());
                }
                self.calls.insert(
                    block.id,
                    CallState {
                        name: final_block["name"]
                            .as_str()
                            .ok_or_else(Fault::upstream)?
                            .to_owned(),
                        result_started: false,
                        result_complete: false,
                    },
                );
                Ok(vec![WebEmission::NativeStop(final_block)])
            }
            ActiveBlock::ToolResult(block) => {
                if !block.content_seen {
                    return Err(Fault::upstream());
                }
                let mut final_block = block.block;
                if let Some(timestamp) = event.get("stop_timestamp") {
                    final_block["stop_timestamp"] = timestamp.clone();
                }
                if let Some(snapshot) = block.snapshot {
                    let content = serde_json::from_str::<Value>(&snapshot)
                        .ok()
                        .and_then(|value| value.as_array().cloned())
                        .filter(|items| {
                            !items.is_empty()
                                && items
                                    .iter()
                                    .all(|item| item.is_object() && item.get("type").is_some())
                        })
                        .map(Value::Array)
                        .unwrap_or_else(|| json!([{"type":"text","text":snapshot}]));
                    final_block
                        .as_object_mut()
                        .ok_or_else(Fault::upstream)?
                        .insert("content".to_owned(), content);
                }
                let call = self
                    .calls
                    .get_mut(&block.tool_use_id)
                    .ok_or_else(Fault::upstream)?;
                if !call.result_started || call.result_complete {
                    return Err(Fault::upstream());
                }
                call.result_complete = true;
                if !bounded_json_depth(&final_block, 0) {
                    return Err(Fault::upstream());
                }
                Ok(vec![WebEmission::NativeStop(final_block)])
            }
        }
    }

    fn legacy_text_delta(&mut self, event: &Value) -> Result<Vec<WebEmission>> {
        if self.active.is_some() || self.structured {
            return Err(Fault::upstream());
        }
        let delta = event
            .get("delta")
            .filter(|value| value.is_object())
            .ok_or_else(Fault::upstream)?;
        if delta
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind != "text_delta")
        {
            return Err(Fault::upstream());
        }
        if event.get("index").is_some() && event_index(event)? != 0 {
            return Err(Fault::upstream());
        }
        let text = delta
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(Fault::upstream)?;
        self.legacy = true;
        Ok(vec![WebEmission::Text(text.to_owned())])
    }

    fn register_citation(&mut self, citation: &Value) -> Result<String> {
        let id = citation
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES)
            .ok_or_else(Fault::upstream)?
            .to_owned();
        if self.citation_count >= MAX_CITATIONS {
            return Err(Fault::upstream());
        }
        self.citation_count += 1;
        Ok(id)
    }

    fn reserve_cited_text(&mut self, text: &str) -> Result<()> {
        self.citation_text_bytes = self.citation_text_bytes.saturating_add(text.len());
        if self.citation_text_bytes > MAX_STREAM_BYTES {
            return Err(Fault::upstream());
        }
        Ok(())
    }
}

fn event_index(event: &Value) -> Result<usize> {
    event
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(Fault::upstream)
}

fn bounded_json_depth(value: &Value, depth: usize) -> bool {
    if depth > 64 {
        return false;
    }
    match value {
        Value::Array(items) => items.iter().all(|item| bounded_json_depth(item, depth + 1)),
        Value::Object(items) => items
            .values()
            .all(|item| bounded_json_depth(item, depth + 1)),
        _ => true,
    }
}

fn bounded_id(value: Option<&Value>) -> Result<String> {
    value
        .and_then(Value::as_str)
        .filter(|id| {
            !id.is_empty() && id.len() <= MAX_ID_BYTES && !id.chars().any(char::is_control)
        })
        .map(str::to_owned)
        .ok_or_else(Fault::upstream)
}

fn unicode_slice(text: &str, start: usize, end: usize) -> Result<String> {
    if start > end || end > text.chars().count() {
        return Err(Fault::upstream());
    }
    Ok(text.chars().skip(start).take(end - start).collect())
}

fn parse_input_object(json: &str) -> Result<Value> {
    if json.len() > MAX_PARTIAL_JSON {
        return Err(Fault::upstream());
    }
    let value = serde_json::from_str::<Value>(json).map_err(|_| Fault::upstream())?;
    if !value.is_object() {
        return Err(Fault::upstream());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{WebBlockDecoder, WebEmission};
    use serde_json::{Value, json};

    fn decoder() -> WebBlockDecoder {
        WebBlockDecoder::new(["lookup".to_owned()])
    }

    #[test]
    fn repeated_citation_sources_and_optional_tool_presentation_updates_are_valid() {
        let mut d = decoder();
        d.feed(&start(0, json!({"type":"text","text":""}))).unwrap();
        for _ in 0..2 {
            d.feed(&delta(0,json!({"type":"citation_start_delta","citation":{"uuid":"same-source","url":"https://example.com"}}))).unwrap();
            d.feed(&delta(0, json!({"type":"text_delta","text":"文"})))
                .unwrap();
            d.feed(&delta(
                0,
                json!({"type":"citation_end_delta","citation_uuid":"same-source"}),
            ))
            .unwrap();
        }
        d.feed(&stop(0)).unwrap();
        d.feed(&start(
            1,
            json!({"type":"tool_use","id":"t1","name":"lookup","input":{}}),
        ))
        .unwrap();
        d.feed(&delta(
            1,
            json!({"type":"tool_use_block_update_delta","message":"Working"}),
        ))
        .unwrap();
        let final_block = d.feed(&stop(1)).unwrap();
        assert!(
            matches!(&final_block[0],WebEmission::NativeStop(block) if block["message"]=="Working")
        );
    }

    fn start(index: usize, block: Value) -> Value {
        json!({"type":"content_block_start","index":index,"content_block":block})
    }

    fn delta(index: usize, delta: Value) -> Value {
        json!({"type":"content_block_delta","index":index,"delta":delta})
    }

    fn stop(index: usize) -> Value {
        json!({"type":"content_block_stop","index":index})
    }

    fn complete_call(decoder: &mut WebBlockDecoder, id: &str) {
        decoder
            .feed(&start(
                0,
                json!({"type":"tool_use","id":id,"name":"lookup","input":{}}),
            ))
            .unwrap();
        decoder.feed(&stop(0)).unwrap();
    }

    #[test]
    fn accumulates_json_one_byte_at_a_time_until_stop() {
        let mut decoder = decoder();
        decoder
            .feed(&start(
                0,
                json!({"type":"tool_use","id":"call_1","name":"lookup","input":{}}),
            ))
            .unwrap();
        let input = br#"{"query":"hello","limit":3}"#;
        for byte in input {
            decoder
                .feed(&delta(
                    0,
                    json!({"type":"input_json_delta","partial_json":String::from_utf8(vec![*byte]).unwrap()}),
                ))
                .unwrap();
        }
        let emissions = decoder.feed(&stop(0)).unwrap();
        assert!(matches!(
            &emissions[0],
            WebEmission::NativeStop(block)
                if block["input"] == json!({"query":"hello","limit":3})
        ));
    }

    #[test]
    fn result_delta_is_a_replacing_content_snapshot() {
        let mut decoder = decoder();
        complete_call(&mut decoder, "call_1");
        decoder
            .feed(&start(
                1,
                json!({"type":"tool_result","tool_use_id":"call_1","content":[{"type":"text","text":"initial"}],"is_error":false}),
            ))
            .unwrap();
        let first = json!({"type":"input_json_delta","partial_json":"[{\"type\":\"text\",\"text\":\"first\"}]"});
        let last = json!({"type":"input_json_delta","partial_json":"[{\"type\":\"text\",\"text\":\"final\"}]"});
        let first_out = decoder.feed(&delta(1, first.clone())).unwrap();
        assert_eq!(first_out, vec![WebEmission::NativeDelta(first)]);
        decoder.feed(&delta(1, last)).unwrap();
        let stop = decoder.feed(&stop(1)).unwrap();
        assert!(matches!(
            &stop[0],
            WebEmission::NativeStop(block)
                if block["content"] == json!([{"type":"text","text":"final"}])
                    && block["is_error"] == false
        ));
        decoder.finish().unwrap();
    }

    #[test]
    fn citation_offsets_count_unicode_codepoints() {
        let mut decoder = decoder();
        let initial = decoder
            .feed(&start(0, json!({"type":"text","text":"猫🌸"})))
            .unwrap();
        assert_eq!(initial, vec![WebEmission::Text("猫🌸".to_owned())]);
        decoder
            .feed(&delta(
                0,
                json!({"type":"citation_start_delta","citation":{"uuid":"cite-1","source":"web"}}),
            ))
            .unwrap();
        decoder
            .feed(&delta(0, json!({"type":"text_delta","text":"語"})))
            .unwrap();
        let citation = decoder
            .feed(&delta(
                0,
                json!({"type":"citation_end_delta","citation_uuid":"cite-1"}),
            ))
            .unwrap();
        assert_eq!(
            citation,
            vec![WebEmission::Citation(json!({
                "uuid":"cite-1","source":"web","start_index":2,"end_index":3,
                "text_block_index":0,"cited_text":"語"
            }))]
        );
        decoder.feed(&stop(0)).unwrap();
        decoder.finish().unwrap();
    }

    #[test]
    fn rejects_mismatched_order_truncation_unknown_calls_and_reused_ids() {
        let mut active = decoder();
        active
            .feed(&start(0, json!({"type":"text","text":"x"})))
            .unwrap();
        assert!(active.feed(&stop(1)).is_err());
        assert!(active.finish().is_err());

        let mut unknown_call = decoder();
        assert!(
            unknown_call
                .feed(&start(
                    0,
                    json!({"type":"tool_result","tool_use_id":"missing","content":[]}),
                ))
                .is_err()
        );

        let mut duplicate = decoder();
        complete_call(&mut duplicate, "same");
        assert!(
            duplicate
                .feed(&start(
                    1,
                    json!({"type":"tool_use","id":"same","name":"lookup","input":{}}),
                ))
                .is_err()
        );
    }

    #[test]
    fn rejects_unknown_tools_and_incomplete_or_oversized_calls() {
        let mut unknown_tool = decoder();
        assert!(
            unknown_tool
                .feed(&start(
                    0,
                    json!({"type":"tool_use","id":"call_1","name":"other","input":{}}),
                ))
                .is_err()
        );

        let mut incomplete = decoder();
        incomplete
            .feed(&start(
                0,
                json!({"type":"tool_use","id":"call_1","name":"lookup","input":{}}),
            ))
            .unwrap();
        assert!(incomplete.finish().is_err());

        let mut oversized = decoder();
        oversized
            .feed(&start(
                0,
                json!({"type":"tool_use","id":"call_1","name":"lookup","input":{}}),
            ))
            .unwrap();
        assert!(
            oversized
                .feed(&delta(
                    0,
                    json!({"type":"input_json_delta","partial_json":"x".repeat((1 << 20) + 1)}),
                ))
                .is_err()
        );
    }

    #[test]
    fn legacy_text_without_block_start_remains_supported() {
        let mut decoder = decoder();
        assert_eq!(
            decoder
                .feed(&json!({"type":"content_block_delta","delta":{"text":"old stream"}}))
                .unwrap(),
            vec![WebEmission::Text("old stream".to_owned())]
        );
        decoder.feed(&json!({"type":"message_stop"})).unwrap();
        decoder.finish().unwrap();
    }

    #[test]
    fn normalizes_null_tool_input_and_applies_tool_use_updates() {
        let mut decoder = WebBlockDecoder::new(["web_fetch".to_owned()]);
        let start_events = decoder
            .feed(&start(
                0,
                json!({"type":"tool_use","id":"call_search","name":"web_fetch","input":null}),
            ))
            .unwrap();
        assert!(matches!(
            &start_events[0],
            WebEmission::NativeStart(block) if block["input"] == json!({})
        ));

        let update = json!({
            "type":"tool_use_block_update_delta",
            "tool_use_id":"call_search",
            "message":"Fetching source",
            "display_content":{"url":"https://example.test"}
        });
        assert_eq!(
            decoder.feed(&delta(0, update.clone())).unwrap(),
            vec![WebEmission::NativeDelta(update)]
        );
        let stopped = decoder.feed(&stop(0)).unwrap();
        assert!(matches!(
            &stopped[0],
            WebEmission::NativeStop(block)
                if block["input"] == json!({})
                    && block["message"] == "Fetching source"
                    && block["display_content"]["url"] == "https://example.test"
        ));

        let knowledge = json!([
            {"type":"text","text":"Found a page"},
            {"type":"knowledge","title":"Reference","url":"https://example.test"}
        ]);
        decoder
            .feed(&start(
                1,
                json!({"type":"tool_result","tool_use_id":"call_search","content":knowledge}),
            ))
            .unwrap();
        let result = decoder.feed(&stop(1)).unwrap();
        assert!(matches!(
            &result[0],
            WebEmission::NativeStop(block) if block["content"] == knowledge
        ));
        decoder.finish().unwrap();
    }

    #[test]
    fn ignores_message_limit_and_empty_thinking_blocks() {
        let mut decoder = decoder();
        assert!(
            decoder
                .feed(&json!({"type":"message_limit","limit":"max_tokens"}))
                .unwrap()
                .is_empty()
        );
        assert!(
            decoder
                .feed(&start(0, json!({"type":"thinking","thinking":""})))
                .unwrap()
                .is_empty()
        );
        assert!(decoder.feed(&stop(0)).unwrap().is_empty());
        decoder.finish().unwrap();
    }

    #[test]
    fn initial_citations_get_source_index_and_unicode_cited_text() {
        let mut decoder = decoder();
        let emissions = decoder
            .feed(&start(
                0,
                json!({
                    "type":"text",
                    "text":"a🙂b",
                    "citations":[{
                        "uuid":"cite-initial",
                        "source":"web",
                        "start_index":1,
                        "end_index":2
                    }]
                }),
            ))
            .unwrap();
        assert_eq!(
            emissions,
            vec![
                WebEmission::Text("a🙂b".to_owned()),
                WebEmission::Citation(json!({
                    "uuid":"cite-initial",
                    "source":"web",
                    "start_index":1,
                    "end_index":2,
                    "text_block_index":0,
                    "cited_text":"🙂"
                }))
            ]
        );
        decoder.feed(&stop(0)).unwrap();
        decoder.finish().unwrap();
    }
}
