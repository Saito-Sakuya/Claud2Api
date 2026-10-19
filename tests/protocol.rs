use claud2api::{
    config::Settings,
    engine::{compaction_cut, normalize_summary},
    native_tools,
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
fn native(block: Value) -> Value {
    json!({"type":"claud2api_native","block":block})
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
fn native_web_blocks_and_citations_roundtrip_in_assistant_history() {
    let tool_use = json!({
        "type":"tool_use","id":"native-1","name":"web_search","input":{"query":"official docs"},
        "tool_origin":"first_party","message":"Searching the web"
    });
    let tool_result = json!({
        "type":"tool_result","tool_use_id":"native-1","is_error":false,
        "content":[{"type":"knowledge","title":"Official guide","url":"https://example.test/guide","metadata":{"site_name":"Example"}}],
        "structured_content":{"sources":1}
    });
    let citation = json!({
        "type":"citation",
        "citation":{"text_block_index":0,"start_index":2,"end_index":7,"cited_text":"世界","url":"https://example.test/guide"}
    });
    let mut raw = request();
    raw["messages"] = json!([
        {"role":"user","content":"查资料"},
        {"role":"assistant","content":[native(tool_use.clone()),native(tool_result.clone()),native(citation.clone())]},
        {"role":"user","content":"继续"}
    ]);

    let parsed = protocol::parse(&raw).unwrap();
    let serialized = serde_json::to_value(&parsed.turns).unwrap();
    assert_eq!(serialized[1]["content"][0]["block"], tool_use);
    assert_eq!(serialized[1]["content"][1]["block"], tool_result);
    assert_eq!(serialized[1]["content"][2]["block"], citation);
    assert_eq!(parsed.hashes("key")[1].len(), 64);
}
#[test]
fn native_blocks_are_assistant_only_bounded_and_paired_within_one_turn() {
    let call = native(json!({"type":"tool_use","id":"n1","name":"web_search","input":{}}));
    let result = native(json!({"type":"tool_result","tool_use_id":"n1","content":[]}));

    let mut valid = request();
    valid["messages"] = json!([
        {"role":"user","content":"one"},
        {"role":"assistant","content":[call.clone(),result.clone()]},
        {"role":"user","content":"two"}
    ]);
    assert!(protocol::parse(&valid).is_ok());

    for messages in [
        json!([
            {"role":"user","content":"one"},
            {"role":"assistant","content":[call.clone()]},
            {"role":"user","content":"two"}
        ]),
        json!([
            {"role":"user","content":"one"},
            {"role":"assistant","content":[result.clone()]},
            {"role":"user","content":"two"}
        ]),
        json!([
            {"role":"user","content":[call.clone()]}
        ]),
        json!([
            {"role":"user","content":"one"},
            {"role":"assistant","content":[call.clone()]},
            {"role":"assistant","content":[result.clone()]},
            {"role":"user","content":"two"}
        ]),
    ] {
        let mut raw = request();
        raw["messages"] = messages;
        assert!(protocol::parse(&raw).is_err());
    }

    let mut system_extension = request();
    system_extension["system"] = json!([native(json!({
        "type":"citation","citation":{"url":"https://example.test"}
    }))]);
    assert!(protocol::parse(&system_extension).is_err());

    let mut client_result_extension = request();
    client_result_extension["tools"] = tool_request()["tools"].clone();
    client_result_extension["messages"] = json!([
        {"role":"user","content":"run"},
        {"role":"assistant","content":[{"type":"tool_use","id":"client-1","name":"read","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"client-1","content":[native(json!({
            "type":"citation","citation":{"url":"https://example.test"}
        }))]}]}
    ]);
    assert!(protocol::parse(&client_result_extension).is_err());

    let mut duplicate = request();
    duplicate["messages"] = json!([
        {"role":"user","content":"one"},
        {"role":"assistant","content":[call.clone(),call,result]},
        {"role":"user","content":"two"}
    ]);
    assert!(protocol::parse(&duplicate).is_err());

    let oversized = native(json!({
        "type":"citation",
        "citation":{"data":"x".repeat((8 << 20) + 1)}
    }));
    let mut too_large = request();
    too_large["messages"] = json!([
        {"role":"user","content":"one"},
        {"role":"assistant","content":[oversized]},
        {"role":"user","content":"two"}
    ]);
    assert!(protocol::parse(&too_large).is_err());

    let mut nested = json!(null);
    for _ in 0..66 {
        nested = json!({"next":nested});
    }
    let too_deep = native(json!({"type":"citation","citation":{"nested":nested}}));
    let mut deep = request();
    deep["messages"] = json!([
        {"role":"user","content":"one"},
        {"role":"assistant","content":[too_deep]},
        {"role":"user","content":"two"}
    ]);
    assert!(protocol::parse(&deep).is_err());
}
#[test]
fn official_api_server_tool_result_requires_gateway_extension() {
    let mut raw = request();
    raw["messages"][0]["content"] = json!([{"type":"web_search_tool_result","content":[]}]);
    let err = protocol::parse(&raw).unwrap_err();
    assert!(err.message.contains("use a claud2api_native extension"));
}
#[test]
fn native_tool_settings_migrate_and_resolve_canonical_groups() {
    let settings = Settings::default();
    assert!(settings.native_web_search && settings.native_artifacts);
    assert!(settings.native_repl && settings.native_widgets);
    assert!(settings.native_project_knowledge);

    let mut old_snapshot = serde_json::to_value(&settings).unwrap();
    for key in [
        "native_web_search",
        "native_artifacts",
        "native_repl",
        "native_widgets",
        "native_project_knowledge",
    ] {
        old_snapshot.as_object_mut().unwrap().remove(key);
    }
    let migrated: Settings = serde_json::from_value(old_snapshot).unwrap();
    assert!(migrated.native_web_search && migrated.native_artifacts);
    assert!(migrated.native_repl && migrated.native_widgets);
    assert!(migrated.native_project_knowledge);

    assert_eq!(
        native_tools::resolve(&json!({}), &settings).unwrap(),
        vec![
            "web_search".to_owned(),
            "artifacts".to_owned(),
            "repl".to_owned(),
            "widgets".to_owned()
        ]
    );
    assert_eq!(
        native_tools::resolve(
            &json!({"claud2api_tools":["widgets","web_search"]}),
            &settings
        )
        .unwrap(),
        vec!["web_search".to_owned(), "widgets".to_owned()]
    );
    assert!(
        native_tools::resolve(
            &json!({"claud2api_tools":["web_search","web_search"]}),
            &settings
        )
        .is_err()
    );
    assert!(native_tools::resolve(&json!({"claud2api_tools":["connectors"]}), &settings).is_err());
    assert!(
        native_tools::resolve(&json!({"claud2api_tools":["project_knowledge"]}), &settings)
            .is_err()
    );
    assert!(native_tools::resolve(&json!({"claud2api_tools":null}), &settings).is_err());
    assert!(
        native_tools::resolve(&json!({"claud2api_tools":[]}), &settings)
            .unwrap()
            .is_empty()
    );

    let mut restricted = settings.clone();
    restricted.native_repl = false;
    assert_eq!(
        native_tools::resolve(&json!({}), &restricted).unwrap(),
        vec![
            "web_search".to_owned(),
            "artifacts".to_owned(),
            "widgets".to_owned()
        ]
    );
    assert!(native_tools::resolve(&json!({"claud2api_tools":["repl"]}), &restricted).is_err());
}

#[test]
fn project_binding_requires_canonical_uuid_and_gates_project_knowledge() {
    const PROJECT: &str = "123e4567-e89b-12d3-a456-426614174000";
    let settings = Settings::default();
    let mut raw = request();
    raw["claud2api_project_uuid"] = json!(PROJECT);
    let input = protocol::parse(&raw).unwrap();
    assert_eq!(input.project_uuid.as_deref(), Some(PROJECT));
    assert_ne!(
        input.context("key"),
        protocol::parse(&request()).unwrap().context("key")
    );
    assert!(
        input
            .prompt("n")
            .contains(&format!("\"project_uuid\":\"{PROJECT}\""))
    );
    assert_eq!(
        native_tools::resolve(&raw, &settings).unwrap(),
        vec![
            "web_search".to_owned(),
            "artifacts".to_owned(),
            "repl".to_owned(),
            "widgets".to_owned(),
            "project_knowledge".to_owned(),
        ]
    );
    assert_eq!(
        native_tools::resolve(
            &json!({"claud2api_project_uuid":PROJECT,"claud2api_tools":[]}),
            &settings
        )
        .unwrap(),
        Vec::<String>::new()
    );
    assert!(
        native_tools::resolve(
            &json!({"claud2api_project_uuid":PROJECT,"claud2api_tools":["project_knowledge"]}),
            &Settings {
                native_project_knowledge: false,
                ..settings.clone()
            }
        )
        .is_err()
    );

    for invalid in [
        json!(null),
        json!(123),
        json!("123E4567-E89B-12D3-A456-426614174000"),
        json!("123e4567-e89b-12d3-a456-426614174000/extra"),
    ] {
        let mut invalid_request = request();
        invalid_request["claud2api_project_uuid"] = invalid;
        assert!(protocol::parse(&invalid_request).is_err());
        assert!(native_tools::resolve(&invalid_request, &settings).is_err());
    }
}
#[test]
fn native_registry_descriptors_and_subtool_allowlist_are_bounded() {
    let groups = vec![
        "web_search".to_owned(),
        "artifacts".to_owned(),
        "repl".to_owned(),
        "widgets".to_owned(),
    ];
    let descriptors = native_tools::descriptors(&groups);
    assert_eq!(
        descriptors[0],
        json!({"type":"web_search_v0","name":"web_search"})
    );
    assert_eq!(
        descriptors[1],
        json!({"type":"artifacts_v0","name":"artifacts"})
    );
    assert_eq!(descriptors[2], json!({"type":"repl_v0","name":"repl"}));
    assert_eq!(
        descriptors[3],
        json!({"type":"send_to_user_v0","name":"send_to_user"})
    );
    assert_eq!(descriptors.as_array().unwrap().len(), 41);

    let names = native_tools::allowed_names(&groups);
    for name in [
        "web_fetch",
        "web_search_fast",
        "bash_tool",
        "create_file",
        "str_replace_editor",
        "file_search",
        "kill_bash",
        "Artifact",
        "weather_fetch",
        "chart_display_v0",
    ] {
        assert!(names.iter().any(|candidate| candidate == name), "{name}");
    }
    for name in [
        "send_to_user",
        "ask_user_input_v0",
        "message_compose_v1",
        "offer_schedule_v0",
        "approved_connectors_display_v0",
    ] {
        assert!(names.iter().any(|candidate| candidate == name), "{name}");
    }
    assert!(!names.iter().any(|name| name == "arbitrary_connector"));

    let project_groups = vec!["project_knowledge".to_owned()];
    assert_eq!(
        native_tools::descriptors(&project_groups),
        json!([{"type":"project_knowledge_search","name":"project_knowledge_search"}])
    );
    assert_eq!(
        native_tools::allowed_names(&project_groups),
        vec!["project_knowledge_search"]
    );
}
#[test]
fn native_sanitizer_removes_control_metadata_but_preserves_user_data() {
    let clean = native_tools::sanitize(json!({
        "type":"tool_use",
        "approval_key":"credential",
        "mcp_server_url":"private endpoint",
        "meta":{"approval_options":"control","label":"safe"},
        "input":{"approval_key":"user supplied","mcp_server_url":"user data"},
        "content":{"approval_options":"user content"}
    }));
    assert!(clean.get("approval_key").is_none());
    assert!(clean.get("mcp_server_url").is_none());
    assert!(clean["meta"].get("approval_options").is_none());
    assert_eq!(clean["meta"]["label"], "safe");
    assert_eq!(clean["input"]["approval_key"], "user supplied");
    assert_eq!(clean["input"]["mcp_server_url"], "user data");
    assert_eq!(clean["content"]["approval_options"], "user content");
}
#[test]
fn native_context_is_omitted_when_disabled_and_changes_when_enabled() {
    let mut input = protocol::parse(&request()).unwrap();
    let disabled = input.context("key");
    input.native_tools.clear();
    assert_eq!(input.context("key"), disabled);
    input.native_tools.push("web_search".to_owned());
    assert_ne!(input.context("key"), disabled);
    assert!(
        input
            .prompt("n")
            .contains("\"native_tools\":[\"web_search\"]")
    );
    assert!(
        input
            .prompt("n")
            .contains("tool_policy applies only to client_tools")
    );
    assert!(input.prompt("n").contains("<tool_call_n>"));
}
#[test]
fn current_native_capabilities_are_separate_from_client_tools_and_historical_denials() {
    let mut raw = request();
    raw["tools"] = json!([{
        "name":"WebSearch",
        "description":"Search using the client runtime.",
        "input_schema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}
    }]);
    raw["messages"] = json!([
        {"role":"user","content":"Search for the Rust website."},
        {"role":"assistant","content":"This conversation has no search tool."},
        {"role":"user","content":"Please actually search now."}
    ]);
    let mut input = protocol::parse(&raw).unwrap();
    input.native_tools = vec!["web_search".to_owned()];
    let prompt = input.prompt("current-capabilities");
    let envelope: Value = serde_json::from_str(prompt.lines().last().unwrap()).unwrap();
    assert_eq!(envelope["client_tools"][0]["name"], "WebSearch");
    let names = envelope["native_tool_names"].as_array().unwrap();
    assert!(names.contains(&json!("web_search")));
    assert!(names.contains(&json!("web_fetch")));
    assert!(!names.contains(&json!("WebSearch")));
    assert!(!names.contains(&json!("bash_tool")));
    assert_eq!(
        envelope["history"][1]["content"][0]["text"],
        "This conversation has no search tool."
    );
    input.native_tools.clear();
    let prompt = input.prompt("disabled-capabilities");
    let envelope: Value = serde_json::from_str(prompt.lines().last().unwrap()).unwrap();
    assert!(envelope.get("native_tool_names").is_none());
}
#[test]
fn tool_decoder_native_boundary_flushes_text_and_rejects_partial_frames() {
    let mut required = tool_request();
    required["tool_choice"] = json!({"type":"any"});
    let input = protocol::parse(&required).unwrap();
    let mut decoder = ToolDecoder::new(&input, "edge");
    assert_eq!(
        decoder.feed("before <tool_call").unwrap(),
        vec![Block::text("before ")]
    );
    assert_eq!(decoder.boundary().unwrap(), vec![Block::text("<tool_call")]);
    assert!(decoder.finish().is_err());

    let mut decoder = ToolDecoder::new(&input, "edge");
    decoder.feed("<tool_call_edge>{").unwrap();
    assert!(decoder.boundary().is_err());
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
