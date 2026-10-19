//! Text rendering for Claude web native results sent to text-only clients.
//!
//! This module does not execute tools. The caller remains responsible for
//! retaining the complete structured blocks when its API supports them.

use serde_json::Value;
use std::collections::HashSet;

/// Renders a supported, already-sanitized native tool result or citation as
/// ordinary assistant text.
///
/// Incomplete `tool_use` blocks intentionally have no text representation.
/// Their raw structured form should be retained by the caller. The native
/// stream and protocol layers bound these values; this function never silently
/// truncates a result.
pub fn text(block: &Value) -> Option<String> {
    let rendered = match block.get("type").and_then(Value::as_str)? {
        "tool_use" => None,
        "tool_result" => Some(tool_result(block)),
        "citation" => citation(block),
        _ => None,
    }?;
    Some(format!("\n\n{rendered}\n\n"))
}

pub fn source_urls(block: &Value) -> HashSet<String> {
    fn collect(value: &Value, urls: &mut HashSet<String>, depth: usize) {
        if depth > 64 {
            return;
        }
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    if matches!(key.as_str(), "url" | "source_url" | "uri") {
                        if let Some(url) = value
                            .as_str()
                            .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
                        {
                            urls.insert(url.to_owned());
                        }
                    } else {
                        collect(value, urls, depth + 1);
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    collect(item, urls, depth + 1);
                }
            }
            _ => {}
        }
    }
    let mut urls = HashSet::new();
    collect(block, &mut urls, 0);
    urls
}

fn tool_result(block: &Value) -> String {
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("unknown");
    let is_error = match block.get("is_error") {
        Some(Value::Bool(true)) => "true",
        Some(Value::Bool(false)) => "false",
        _ => "unspecified",
    };

    let mut output = format!("[Native tool result: {name}; is_error={is_error}]");
    let mut parts = Vec::new();

    if let Some(content) = block.get("content") {
        render_content(content, &mut parts, 0);
    }
    if let Some(message) = block.get("message").and_then(Value::as_str)
        && !message.is_empty()
        && !parts.iter().any(|part| part == message)
    {
        parts.push(message.to_owned());
    }
    if let Some(structured) = block.get("structured_content")
        && !structured.is_null()
    {
        parts.push(format!("Structured result:\n{}", json_text(structured)));
    }
    if let Some(display) = block.get("display_content")
        && !display.is_null()
    {
        parts.push(format!("Display information:\n{}", json_text(display)));
    }

    if !parts.is_empty() {
        output.push('\n');
        output.push_str(&parts.join("\n"));
    }
    output
}

fn render_content(value: &Value, output: &mut Vec<String>, depth: usize) {
    if depth > 64 {
        output.push(format!("Structured result item:\n{}", json_text(value)));
        return;
    }

    match value {
        Value::String(text) => {
            if !text.is_empty() {
                output.push(text.to_owned());
            }
        }
        Value::Array(items) => {
            for item in items {
                render_content(item, output, depth + 1);
            }
        }
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = object.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    output.push(text.to_owned());
                }
            }
            Some("knowledge") | Some("source") => {
                let mut fields = Vec::new();
                push_field(&mut fields, "Source", object.get("title"));
                push_field(&mut fields, "URL", object.get("url"));
                for key in ["snippet", "text", "description"] {
                    push_field(&mut fields, key, object.get(key));
                }
                if fields.is_empty() {
                    output.push(format!("Structured result item:\n{}", json_text(value)));
                } else {
                    output.push(fields.join("\n"));
                }
            }
            Some("local_resource") => render_local_resource(object, output),
            Some("image") => render_image(object, output),
            _ => output.push(format!("Structured result item:\n{}", json_text(value))),
        },
        _ => output.push(format!("Structured result item:\n{}", json_text(value))),
    }
}

fn render_local_resource(object: &serde_json::Map<String, Value>, output: &mut Vec<String>) {
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("unnamed file");
    let mut fields = vec![format!("Remote sandbox file: {name}")];
    push_field(&mut fields, "Type", object.get("mime_type"));
    if let Some(url) = object
        .get("claud2api_download_url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
    {
        fields.push(format!("Download: {url}"));
    } else {
        push_field(&mut fields, "Remote path", object.get("file_path"));
    }
    output.push(fields.join("\n"));
}

fn render_image(object: &serde_json::Map<String, Value>, output: &mut Vec<String>) {
    let url = ["url", "source_url", "image_url"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .filter(|url| !url.is_empty());
    let alt = ["alt", "title", "name"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .filter(|alt| !alt.is_empty());

    let mut fields = vec![match alt {
        Some(alt) => format!("Image result: {alt}"),
        None => "Image result".to_owned(),
    }];
    if let Some(url) = url {
        fields.push(format!("Source: {url}"));
    } else {
        fields.push("Image result has no downloadable URL.".to_owned());
    }
    output.push(fields.join("\n"));
}

fn citation(block: &Value) -> Option<String> {
    let citation = block
        .get("citation")
        .filter(|value| value.is_object())
        .unwrap_or(block);
    let title = ["title", "document_title", "name"]
        .iter()
        .find_map(|key| citation.get(*key).and_then(Value::as_str))
        .filter(|title| !title.is_empty());
    let url = ["url", "source_url", "uri"]
        .iter()
        .find_map(|key| citation.get(*key).and_then(Value::as_str))
        .filter(|url| !url.is_empty());

    match (title, url) {
        (Some(title), Some(url)) if title != url => Some(format!("Source: {title} — {url}")),
        (Some(title), _) => Some(format!("Source: {title}")),
        (_, Some(url)) => Some(format!("Source: {url}")),
        (None, None) => Some(format!("Source: {}", json_text(citation))),
    }
}

fn push_field(fields: &mut Vec<String>, label: &str, value: Option<&Value>) {
    if let Some(value) = value
        && let Some(value) = value.as_str()
        && !value.is_empty()
    {
        fields.push(format!("{label}: {value}"));
    }
}

fn json_text(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "[structured result unavailable]".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn omits_incomplete_tool_use_and_does_not_execute_client_tools() {
        assert_eq!(
            text(&json!({"type":"tool_use","name":"web_search","input":{"query":"x"}})),
            None
        );
        assert_eq!(
            text(&json!({"type":"client_tool_use","name":"Bash","input":{"command":"touch x"}})),
            None
        );
    }

    #[test]
    fn renders_tool_result_with_name_status_text_and_real_urls() {
        let rendered = text(&json!({
            "type":"tool_result",
            "name":"web_search",
            "is_error":false,
            "content":[
                {"type":"text","text":"Found one official result."},
                {"type":"knowledge","title":"Release notes","url":"https://example.test/releases","snippet":"Version details."}
            ]
        })).unwrap();

        assert!(rendered.contains("Native tool result: web_search; is_error=false"));
        assert!(rendered.contains("Found one official result."));
        assert!(rendered.contains("Release notes"));
        assert!(rendered.contains("https://example.test/releases"));
        assert!(rendered.contains("Version details."));
    }

    #[test]
    fn renders_citation_title_and_source_once_for_parent_deduplication() {
        let rendered = text(&json!({
            "type":"citation",
            "citation":{
                "title":"Official documentation",
                "url":"https://docs.example.test/guide",
                "source_url":"https://docs.example.test/guide",
                "cited_text":"A short excerpt"
            }
        }))
        .unwrap();

        assert_eq!(
            rendered.trim(),
            "Source: Official documentation — https://docs.example.test/guide"
        );
    }

    #[test]
    fn renders_downloadable_files_as_remote_sandbox_files() {
        let rendered = text(&json!({
            "type":"tool_result",
            "name":"present_files",
            "is_error":false,
            "content":[{
                "type":"local_resource",
                "name":"report.csv",
                "mime_type":"text/csv",
                "file_path":"/mnt/user-data/outputs/report.csv",
                "claud2api_download_url":"/v1/native-files/v1.opaque"
            }]
        }))
        .unwrap();

        assert!(rendered.contains("Remote sandbox file: report.csv"));
        assert!(rendered.contains("Type: text/csv"));
        assert!(rendered.contains("Download: /v1/native-files/v1.opaque"));
        assert!(!rendered.contains("Local file"));
    }

    #[test]
    fn retains_widget_structured_result_and_error_signal() {
        let rendered = text(&json!({
            "type":"tool_result",
            "name":"weather_fetch",
            "is_error":true,
            "content":[{"type":"text","text":"Weather provider failed."}],
            "structured_content":{
                "widget_type":"weather_fetch",
                "temperature_c":18,
                "condition":"cloudy",
                "source_url":"https://weather.example.test/tokyo"
            }
        }))
        .unwrap();

        assert!(rendered.contains("Native tool result: weather_fetch; is_error=true"));
        assert!(rendered.contains("Weather provider failed."));
        assert!(rendered.contains("\"temperature_c\": 18"));
        assert!(rendered.contains("https://weather.example.test/tokyo"));
    }

    #[test]
    fn sanitized_control_metadata_is_not_rendered() {
        let block = crate::native_tools::sanitize(json!({
            "type":"tool_result",
            "name":"web_search",
            "approval_key":"secret approval key",
            "approval_options":["secret option"],
            "mcp_server_url":"https://private-mcp.example.test",
            "meta":{"approval_key":"secret nested key","label":"safe"},
            "is_error":false,
            "content":[{"type":"text","text":"Public result."}]
        }));
        let rendered = text(&block).unwrap();
        assert!(rendered.contains("Public result."));
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("private-mcp"));
    }

    #[test]
    fn preserves_unspecified_error_status_and_unknown_structured_content() {
        let rendered = text(&json!({
            "type":"tool_result",
            "name":"custom_widget",
            "content":[{"type":"widget_payload","some_value":true,"url":"https://example.test"}]
        }))
        .unwrap();
        assert!(rendered.contains("is_error=unspecified"));
        assert!(rendered.contains("widget_payload"));
        assert!(rendered.contains("https://example.test"));
    }
}
