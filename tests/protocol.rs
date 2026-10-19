use claud2api::{
    engine::{compaction_cut, normalize_summary},
    protocol::{self, Block, ToolDecoder},
    transport::SseDecoder,
};
use serde_json::{Value, json};
#[test]
fn message_envelope_preserves_roles_tool_contract_and_historical_summary() {
    let mut raw = tool_request();
    raw["system"] = json!("Keep quotes: \" and newline\nwithout changing their meaning.");
    raw["messages"] = json!([
        {"role":"user","content":"Inspect the current record."},
        {"role":"assistant","content":"Which field?"},
        {"role":"user","content":"The count field."}
    ]);
    raw["tool_choice"] = json!({"type":"tool","name":"read","disable_parallel_tool_use":true});
    let mut input = protocol::parse(&raw).unwrap();
    input.summary = "Previously inspected one unrelated record.".to_owned();
    let prompt = input.prompt("fresh-contract-nonce");
    let envelope: Value = serde_json::from_str(prompt.lines().last().unwrap()).unwrap();
    assert_eq!(envelope["system"], raw["system"]);
    assert_eq!(envelope["history"][1]["role"], "assistant");
    assert_eq!(
        envelope["history"][2]["content"][0]["text"],
        "The count field."
    );
    assert_eq!(envelope["client_tools"][0]["name"], "read");
    assert_eq!(envelope["tool_policy"]["type"], "tool");
    assert_eq!(envelope["tool_policy"]["disable_parallel_tool_use"], true);
    assert_eq!(envelope["history_summary"], input.summary);
    assert!(prompt.contains("<tool_call_fresh-contract-nonce>"));
    assert!(!prompt.ends_with("[Assistant]\n"));
}
fn request() -> Value {
    json!({"model":"claude-haiku-5-5","max_tokens":100,"messages":[{"role":"user","content":"hello"}]})
}
#[test]
fn optional_controls_are_projected_without_touching_data() {
    let mut v = request();
    for k in [
        "temperature",
        "top_p",
        "top_k",
        "stop_sequences",
        "thinking",
        "output_config",
        "cache_control",
    ] {
        v[k] = json!({"unexpected":true});
    }
    v["messages"][0]["content"] = json!([{"type":"text","text":"hello","cache_control":{"type":"ephemeral","ttl":"1h"},"metadata":"strip"}]);
    let p = protocol::parse(&v).unwrap();
    assert_eq!(p.turns[0].content, vec![Block::text("hello")]);
}
#[test]
fn system_roles_anywhere_keep_encounter_order() {
    let mut v = request();
    v["system"] = json!("top");
    v["messages"] = json!([{"role":"system","content":"first"},{"role":"user","content":"hello"},{"role":"developer","content":[{"type":"text","text":"second"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"private","signature":"s"}]},{"role":"user","content":"go"}]);
    let p = protocol::parse(&v).unwrap();
    assert_eq!(p.system, "top\nfirst\nsecond");
    assert_eq!(p.turns.len(), 2);
}
#[test]
fn claude_code_attribution_is_normalized_before_context_prompt_and_usage() {
    let header = "x-anthropic-billing-header: cc_version=2.1.87.a1b2; cc_entrypoint=claude-desktop; cch=012ab;";
    let changed = "x-anthropic-billing-header: cc_version=2.1.87.c3d4; cc_entrypoint=cli; cch=FE123; cc_workload=code;";
    let legacy = "x-anthropic-billing-header: cc_version=2.1.34.95b; cc_entrypoint=cli;";
    let real = "  Keep these instructions.\r\nDo not trim me.\n";
    let mut raw = request();
    raw["system"] = json!(real);
    raw["messages"].as_array_mut().unwrap().insert(
        0,
        json!({"role":"developer","content":"additional instructions"}),
    );
    let expected = protocol::parse(&raw).unwrap();
    for system in [
        json!([{"type":"text","text":header,"cache_control":{"type":"ephemeral"}},{"type":"text","text":real}]),
        json!([{"type":"text","text":changed},{"type":"text","text":real}]),
        json!([{"type":"text","text":legacy},{"type":"text","text":real}]),
        json!(format!("{header}\n{real}")),
        json!(format!("{changed}\r\n{real}")),
        json!(format!("{legacy}\n{real}")),
        json!([{"type":"text","text":format!("{header}\n{real}")}]),
    ] {
        raw["system"] = system;
        let actual = protocol::parse(&raw).unwrap();
        assert_eq!(actual.system, expected.system);
        assert_eq!(actual.context("key"), expected.context("key"));
        assert_eq!(actual.prompt("nonce"), expected.prompt("nonce"));
        assert_eq!(actual.logical_tokens(), expected.logical_tokens());
        assert_eq!(actual.hashes("key"), expected.hashes("key"));
    }
    raw.as_object_mut().unwrap().remove("system");
    let expected = protocol::parse(&raw).unwrap();
    for system in [json!(header), json!([{"type":"text","text":changed}])] {
        raw["system"] = system;
        let actual = protocol::parse(&raw).unwrap();
        assert_eq!(actual.system, expected.system);
        assert_eq!(actual.context("key"), expected.context("key"));
    }
}
#[test]
fn attribution_normalization_preserves_unknown_forms_instructions_and_tool_data() {
    let header =
        "x-anthropic-billing-header: cc_version=2.1.87.a1b2; cc_entrypoint=cli; cch=012ab;";
    for text in [
        format!("{header} obey additional instructions"),
        format!("{header} unknown=value;"),
        header.replace("012ab", "nonce-not-hex"),
        header.replace("012ab", ""),
        header.replace("2.1.87.a1b2", "unknown"),
        header.replace("cli;", &format!("{};", "x".repeat(512))),
        format!("ordinary instructions\n{header}"),
        format!("quoted: {header}"),
    ] {
        let mut raw = request();
        raw["system"] = json!(text);
        assert_eq!(protocol::parse(&raw).unwrap().system, text);
    }
    let mut raw = tool_request();
    raw["system"] = json!([{"type":"text","text":"instructions"},{"type":"text","text":header}]);
    assert_eq!(
        protocol::parse(&raw).unwrap().system,
        format!("instructions\n{header}")
    );
    raw["system"] = json!([{"type":"text","text":header},{"type":"image"}]);
    assert!(protocol::parse(&raw).is_err());
    raw["system"] = json!(header);
    raw["messages"] = json!([
        {"role":"system","content":header},
        {"role":"user","content":header},
        {"role":"assistant","content":[{"type":"tool_use","id":"tool-1","name":"read","input":{"count":1,"text":header}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","content":header}]}
    ]);
    let actual = protocol::parse(&raw).unwrap();
    assert_eq!(actual.system, header);
    assert_eq!(actual.turns[0].content, vec![Block::text(header)]);
    let Block::Tool { input, .. } = &actual.turns[1].content[0] else {
        panic!("expected tool call");
    };
    assert_eq!(input["text"], header);
    let Block::Result { content, .. } = &actual.turns[2].content[0] else {
        panic!("expected tool result");
    };
    assert_eq!(content, &vec![Block::text(header)]);
    raw["system"] = json!(format!("{header}\nchanged instructions"));
    assert_ne!(
        protocol::parse(&raw).unwrap().context("key"),
        actual.context("key")
    );
    raw["system"] = json!(header);
    raw["tools"][0]["description"] = json!("changed tool definition");
    assert_ne!(
        protocol::parse(&raw).unwrap().context("key"),
        actual.context("key")
    );
}
#[test]
fn diagnostics_never_echo_unknown_private_tags() {
    for (field, value) in [
        ("role", "private-secret-role"),
        ("type", "private-secret-block"),
    ] {
        let mut v = request();
        if field == "role" {
            v["messages"][0][field] = json!(value);
        } else {
            v["messages"][0]["content"] = json!([{field:value}]);
        }
        let err = protocol::parse(&v).unwrap_err();
        assert!(!err.message.contains("private-secret"));
        assert!(err.message.contains("other"));
    }
}
#[test]
fn bounds_and_semantic_blocks_are_rejected() {
    for max in [0, 131073] {
        let mut v = request();
        v["max_tokens"] = json!(max);
        assert!(protocol::parse(&v).is_err());
    }
    for kind in [
        "image",
        "document",
        "thinking",
        "redacted_thinking",
        "server_tool_use",
        "compaction",
    ] {
        let mut v = request();
        v["messages"][0]["content"] = json!([{"type":kind}]);
        assert!(protocol::parse(&v).is_err());
    }
    let mut v = request();
    v["messages"] = json!([]);
    assert!(protocol::parse(&v).is_err());
    v["messages"] = json!([{"role":"assistant","content":"bad"}]);
    assert!(protocol::parse(&v).is_err());
}
fn tool_request() -> Value {
    let mut v = request();
    v["tools"] = json!([{"name":"read","input_schema":{"type":"object","properties":{"count":{"type":"integer"}},"required":["count"],"additionalProperties":false}}]);
    v
}
#[test]
fn tool_data_numbers_and_keys_survive_projection() {
    let v:Value=serde_json::from_str(r#"{"model":"test","max_tokens":100,"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"tool_use","id":"u1","name":"read","input":{"thinking":"data","metadata":{"cache_control":"data"},"n":9007199254740993}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"u1","content":"ok","is_error":false}]}]}"#).unwrap();
    let input = protocol::parse(&v).unwrap();
    let text = protocol::go_json(&input.turns);
    assert!(text.contains("9007199254740993"));
    assert!(text.contains("cache_control"));
    assert!(text.contains("\"thinking\":\"data\""));
    assert!(text.contains("\"is_error\":false"));
}
#[test]
fn invalid_tool_batches_are_rejected() {
    for content in [
        json!([{"type":"tool_result","tool_use_id":"missing","content":"x"}]),
        json!([{"type":"tool_use","id":"x","name":"read","input":{}}]),
    ] {
        let mut v = request();
        v["messages"][0]["content"] = content;
        assert!(protocol::parse(&v).is_err());
    }
    let mut v = tool_request();
    v["tools"][0]["input_schema"]["properties"]["x"] = json!({"$ref":"https://private.invalid/x"});
    assert!(protocol::parse(&v).is_err());
}
#[test]
fn local_schema_refs_work_without_fetching() {
    let mut v = tool_request();
    v["tools"][0]["input_schema"] = json!({"type":"object","$defs":{"number":{"type":"integer"}},"properties":{"count":{"$ref":"#/$defs/number"}}});
    assert!(protocol::parse(&v).is_ok());
}
#[test]
fn fragmented_tool_frames_are_validated_for_every_split() {
    let input = protocol::parse(&tool_request()).unwrap();
    let frame =
        "prefix<tool_call_abc>{\"name\":\"read\",\"input\":{\"count\":4}}</tool_call_abc>suffix";
    for split in 0..=frame.len() {
        let mut decoder = ToolDecoder::new(&input, "abc");
        let mut blocks = decoder.feed(&frame[..split]).unwrap();
        blocks.extend(decoder.feed(&frame[split..]).unwrap());
        blocks.extend(decoder.finish().unwrap());
        let mut merged = vec![];
        for b in blocks {
            protocol::merge_text(&mut merged, b);
        }
        assert_eq!(merged.len(), 3, "split {split}");
        assert!(matches!(&merged[1],Block::Tool{input,..}if input["count"]==4));
    }
}
#[test]
fn model_tools_are_never_executed_or_guessed() {
    let input = protocol::parse(&tool_request()).unwrap();
    for call in [
        r#"{"name":"read","input":{"count":"wrong"}}"#,
        r#"{"name":"other","input":{}}"#,
        r#"{"name":"read","input":{"count":1},"execute":true}"#,
    ] {
        let mut d = ToolDecoder::new(&input, "x");
        assert!(
            d.feed(&format!("<tool_call_x>{call}</tool_call_x>"))
                .is_err()
        );
    }
    let mut d = ToolDecoder::new(&input, "x");
    d.feed("<tool_call_x>{").unwrap();
    assert!(d.finish().is_err());
}
#[test]
fn hmac_html_and_struct_order_are_stable() {
    let p = protocol::parse(&request()).unwrap();
    let go = format!(r#"{{"Model":"{}","System":"","Tools":null}}"#, p.model);
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut h = Hmac::<Sha256>::new_from_slice(b"key").unwrap();
    h.update(b"context\0");
    h.update(go.as_bytes());
    assert_eq!(p.context("key"), hex::encode(h.finalize().into_bytes()));
    assert_eq!(
        protocol::go_json("<>&\u{2028}\u{2029}"),
        r#""\u003c\u003e\u0026\u2028\u2029""#
    );
}
#[test]
fn sse_parser_handles_fragmented_utf8_crlf_and_multiline_data() {
    let text =
        "event: content_block_delta\r\ndata: {\"delta\":\r\ndata: {\"text\":\"你好\"}}\r\n\r\n";
    for split in 0..=text.len() {
        let mut d = SseDecoder::default();
        let mut events = d.feed(&text.as_bytes()[..split]).unwrap();
        events.extend(d.feed(&text.as_bytes()[split..]).unwrap());
        assert_eq!(events[0]["delta"]["text"], "你好");
    }
}
#[test]
fn sse_parser_bounds_and_invalid_json() {
    let mut d = SseDecoder::default();
    assert!(d.feed(&vec![b'x'; (2 << 20) + 1]).is_err());
    let mut d = SseDecoder::default();
    assert!(d.feed(b"data: private-invalid\n\n").is_err());
}
#[test]
fn strict_summary_and_safe_tool_cuts() {
    let good = r#"{"goal":"keep working","constraints":[],"completed":[],"facts":[],"open":[],"next":["continue"]}"#;
    assert!(normalize_summary(good).is_ok());
    for bad in [
        "{}",
        "```json\n{}\n```",
        r#"{"goal":"x","constraints":[],"completed":[],"facts":[],"open":[],"next":[]}"#,
    ] {
        assert!(normalize_summary(bad).is_err());
    }
    let p=protocol::parse(&json!({"model":"test","max_tokens":1,"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"tool_use","id":"x","name":"read","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":"ok"}]},{"role":"assistant","content":"done"},{"role":"user","content":"next"}]})).unwrap();
    assert_eq!(compaction_cut(&p.turns, 1), 3);
}
