use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use claud2api::{
    config::{Config, Credential, Settings},
    control,
    engine::Engine,
    protocol,
    store::{self, Store},
    web,
};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, oneshot};
use tower::ServiceExt;

const MEDIA_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

fn media_image() -> Value {
    json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":MEDIA_PNG}})
}

fn media_request() -> Value {
    let mut request = base();
    request["messages"][0]["content"] =
        json!([media_image(),{"type":"text","text":"Read this image."}]);
    request
}

#[tokio::test]
async fn images_reach_upstream_as_binary_files_and_continue_without_reuploading_history() {
    let f = fixture(1).await;
    let mut request = media_request();
    let (status, headers, first) = call(
        &f.engine,
        "/v1/messages",
        request.clone(),
        Some("media-continuation"),
    )
    .await;
    assert_eq!(status, 200, "{first}");
    let id = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    {
        let uploaded = f.upstream.uploaded.lock().await;
        assert_eq!(uploaded.len(), 1);
        assert_eq!(
            uploaded[0]["data_hex"],
            hex::encode(STANDARD.decode(MEDIA_PNG).unwrap())
        );
        assert_eq!(uploaded[0]["media_type"], "image/png");
        assert_eq!(uploaded[0]["field_file"], true);
    }
    let messages = request["messages"].as_array_mut().unwrap();
    messages.push(json!({"role":"assistant","content":first["content"]}));
    messages.push(json!({"role":"user","content":"Continue using the previous image."}));
    reset_pacing(&f.engine).await;
    let (status, continued_headers, continued) =
        call(&f.engine, "/v1/messages", request, Some(&id)).await;
    assert_eq!(status, 200, "{continued}");
    assert_eq!(continued_headers["x-c2a-session-id"], id);
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 1);
    assert_eq!(f.upstream.uploaded.lock().await.len(), 1);
    let traces = f.upstream.traces.lock().await;
    let calls = completion_requests(&traces);
    assert_eq!(calls[0]["body"]["files"].as_array().unwrap().len(), 1);
    assert!(
        calls[0]["body"]["files"][0]
            .as_str()
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
    );
    assert_eq!(calls[1]["body"]["files"], json!([]));
    assert!(
        !calls[0]["body"]["prompt"]
            .as_str()
            .unwrap()
            .contains(MEDIA_PNG)
    );
    assert!(
        traces
            .iter()
            .filter(|trace| trace["uri"].as_str().unwrap().ends_with("/upload"))
            .all(|trace| trace["cookie"].as_str().unwrap().contains("fake-session-0"))
    );
    drop(traces);
    let logs = logs_for_request(&f.engine, headers["x-request-id"].to_str().unwrap()).await;
    assert!(
        logs.iter()
            .any(|log| log["operation"] == "file_upload" && log["status"] == 200)
    );
}

#[tokio::test]
async fn quota_migration_reuploads_image_history_using_the_new_account() {
    let f = fixture(2).await;
    let mut request = media_request();
    let (status, headers, first) = call(
        &f.engine,
        "/v1/messages",
        request.clone(),
        Some("media-migrate"),
    )
    .await;
    assert_eq!(status, 200, "{first}");
    let id = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    let old = f.engine.store.read().await.sessions[&id].clone();
    f.engine
        .pool
        .observe(&old.account_id, 429, None)
        .await
        .unwrap();
    request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":first["content"]}),
        json!({"role":"user","content":"Use the previous image again."}),
    ]);
    let (status, _, out) = call(&f.engine, "/v1/messages", request, Some(&id)).await;
    assert_eq!(status, 200, "{out}");
    assert_ne!(
        f.engine.store.read().await.sessions[&id].account_id,
        old.account_id
    );
    let traces = f.upstream.traces.lock().await;
    let uploads: Vec<_> = traces
        .iter()
        .filter(|trace| trace["uri"].as_str().unwrap().ends_with("/upload"))
        .collect();
    assert_eq!(uploads.len(), 2);
    assert_ne!(uploads[0]["cookie"], uploads[1]["cookie"]);
    let calls = completion_requests(&traces);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["body"]["files"].as_array().unwrap().len(), 1);
    assert_eq!(calls[1]["body"]["files"].as_array().unwrap().len(), 1);
    assert_ne!(calls[0]["body"]["files"], calls[1]["body"]["files"]);
}

#[tokio::test]
async fn pdf_and_tool_images_keep_file_ids_while_text_documents_are_inline_attachments() {
    let f = fixture(1).await;
    let image_id = "11111111-1111-4111-8111-111111111111";
    let pdf_id = "22222222-2222-4222-8222-222222222222";
    f.upstream.upload_replies.lock().await.extend([
        json!({"file_uuid":image_id,"file_kind":"image"}),
        json!({"file_uuid":pdf_id,"file_kind":"document","extracted_content":"PDF preview is not the visual file."})
    ]);
    let mut request = base();
    request["tools"] = json!([{"name":"Read","input_schema":{"type":"object"}}]);
    request["messages"] = json!([
        {"role":"user","content":"Read the image and documents."},
        {"role":"assistant","content":[{"type":"tool_use","id":"read-media","name":"Read","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"read-media","content":[
            media_image(),
            {"type":"document","title":"document.pdf","source":{"type":"base64","media_type":"application/pdf","data":STANDARD.encode(b"%PDF-1.4\nfixture content")}},
            {"type":"document","title":"notes.txt","source":{"type":"text","media_type":"text/plain","data":"PRIVATE_TEXT_DOCUMENT_1234"}}
        ]}]}
    ]);
    let (status, _, out) = call(&f.engine, "/v1/messages", request, None).await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(f.upstream.uploaded.lock().await.len(), 2);
    let traces = f.upstream.traces.lock().await;
    let calls = completion_requests(&traces);
    assert_eq!(calls[0]["body"]["files"], json!([image_id, pdf_id]));
    let attachments = calls[0]["body"]["attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(
        attachments[0]["extracted_content"],
        "PRIVATE_TEXT_DOCUMENT_1234"
    );
    assert!(
        attachments[0]["file_name"]
            .as_str()
            .unwrap()
            .starts_with("notes")
    );
    assert_eq!(attachments[0]["file_type"], "text/plain");
    assert!(
        !calls[0]["body"]["prompt"]
            .as_str()
            .unwrap()
            .contains("PRIVATE_TEXT_DOCUMENT_1234")
    );
}

#[tokio::test]
async fn invalid_upload_results_and_http_errors_never_start_generation() {
    for response in [
        json!({"type":"error","file_kind":"image","file_uuid":"11111111-1111-4111-8111-111111111111"}),
        json!({"file_kind":"image","file_uuid":"not-a-uuid"}),
        json!({"file_uuid":"11111111-1111-4111-8111-111111111111"}),
    ] {
        let f = fixture(1).await;
        f.upstream.upload_replies.lock().await.push_back(response);
        let (status, _, out) = call(&f.engine, "/v1/messages", media_request(), None).await;
        assert_eq!(status, 502, "{out}");
        assert!(completion_requests(&f.upstream.traces.lock().await).is_empty());
        assert!(!out.to_string().contains(MEDIA_PNG));
    }
    let f = fixture(1).await;
    f.upstream.upload_status.store(400, Ordering::SeqCst);
    let (status, _, out) = call(&f.engine, "/v1/messages", media_request(), None).await;
    assert_eq!(status, 400, "{out}");
    assert!(completion_requests(&f.upstream.traces.lock().await).is_empty());
}

#[tokio::test]
async fn media_reference_limits_fail_before_account_or_upstream_admission() {
    let f = fixture(1).await;
    let mut request = base();
    request["messages"][0]["content"] = json!((0..21).map(|index|json!({
        "type":"document","title":format!("file-{index}.txt"),"source":{"type":"text","data":"same content"}
    })).collect::<Vec<_>>());
    let (status, _, _) = call(&f.engine, "/v1/messages", request, None).await;
    assert_eq!(status, 400);
    assert!(f.upstream.traces.lock().await.is_empty());
    assert!(f.engine.store.read().await.sessions.is_empty());
}

struct Fixture {
    engine: Arc<Engine>,
    upstream: Arc<Upstream>,
    _dir: tempfile::TempDir,
    server: tokio::task::JoinHandle<()>,
}
#[derive(Default)]
struct Upstream {
    traces: Mutex<Vec<Value>>,
    replies: Mutex<VecDeque<Reply>>,
    automatic_summaries: AtomicBool,
    created: AtomicUsize,
    deleted: AtomicUsize,
    streams: Arc<AtomicUsize>,
    catalog_status: AtomicUsize,
    catalog_models: Mutex<Option<Vec<String>>>,
    stop_response_status: AtomicUsize,
    uploaded: Mutex<Vec<Value>>,
    upload_replies: Mutex<VecDeque<Value>>,
    upload_status: AtomicUsize,
    client_tool_waiters: Mutex<HashMap<String, oneshot::Sender<Value>>>,
}
enum Reply {
    Text(String),
    Http(u16),
    HttpRetry(u16, String),
    EarlyQuota,
    PartialQuota,
    Truncated,
    Slow,
    Summary(String),
    ClientTool { input: Value, final_text: String },
    Raw(String),
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn fixture(count: usize) -> Fixture {
    fixture_with(count, false).await
}
async fn fixture_with(count: usize, legacy: bool) -> Fixture {
    let upstream = Arc::new(Upstream::default());
    let app = Router::new()
        .fallback(any(mock))
        .with_state(upstream.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings {
        account_min_interval_seconds: 1,
        account_requests_per_minute: 60,
        request_timeout_seconds: 6,
        compaction_enabled: false,
        simulated_cache: true,
        ..Settings::default()
    };
    let config = Config {
        browser_profile: claud2api::config::BrowserProfile::Chrome146,
        public_bind: "127.0.0.1:8080".parse().unwrap(),
        control_bind: "127.0.0.1:18082".parse().unwrap(),
        gateway_key: "gateway-unit-test-key-32-characters-minimum".to_owned(),
        admin_key: "admin-unit-test-key-32-characters-minimum".to_owned(),
        bot_key: "bot-unit-test-key-32-characters-minimum".to_owned(),
        data_dir: dir.path().to_path_buf(),
        import_state: None,
        base_url: format!("http://{address}"),
        locale: "zh-TW".to_owned(),
        timezone: "Asia/Taipei".to_owned(),
        default_model: "test".to_owned(),
        effort: if legacy { "high" } else { "medium" }.to_owned(),
        legacy_thinking: if legacy { "off" } else { "auto" }.to_owned(),
        accounts: (0..count)
            .map(|i| Credential::parse(&format!("fake-session-{i}")).unwrap())
            .collect(),
        settings,
    };
    let engine = Engine::open(config).await.unwrap();
    if legacy {
        engine
            .store
            .mutate("", json!({}), |s| {
                s.settings.rate_limit_min_seconds = 0;
                Ok(())
            })
            .await
            .unwrap();
    }
    Fixture {
        engine,
        upstream,
        _dir: dir,
        server,
    }
}
fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}
fn text_events(text: &str) -> String {
    event(json!({"type":"message_start"}))
        + &event(
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        )
        + &event(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        )
        + &event(json!({"type":"content_block_stop","index":0}))
        + &event(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}))
        + &event(json!({"type":"message_stop"}))
}
async fn mock(State(state): State<Arc<Upstream>>, request: Request) -> Response {
    let method = request.method().to_string();
    let path = request.uri().to_string();
    let headers = request.headers().clone();
    let body = axum::body::to_bytes(request.into_body(), 8 << 20)
        .await
        .unwrap();
    let json_body = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    state.traces.lock().await.push(json!({"method":method,"uri":path,"body":json_body,"cookie":headers.get("cookie").and_then(|v|v.to_str().ok()),"headers":headers.iter().map(|(k,v)|(k.as_str().to_owned(),Value::String(v.to_str().unwrap_or("").to_owned()))).collect::<serde_json::Map<_,_>>()}));
    if path == "/api/organizations" {
        return axum::Json(json!([{"uuid":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}]))
            .into_response();
    }
    if path.starts_with("/edge-api/") {
        let status = state.catalog_status.load(Ordering::SeqCst);
        if status > 0 {
            return (
                StatusCode::from_u16(status as u16).unwrap(),
                axum::Json(json!({"error":"fixture catalog rejected"})),
            )
                .into_response();
        }
        let models = state
            .catalog_models
            .lock()
            .await
            .clone()
            .unwrap_or_else(|| {
                vec![
                    "test".to_owned(),
                    "claude-haiku-5-5".to_owned(),
                    "test".to_owned(),
                ]
            });
        return axum::Json(json!({"claude_ai_available_models":{"models":models.iter().map(|id|json!({"model_id":id})).collect::<Vec<_>>()}})).into_response();
    }
    if method == "POST" && path == "/api/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/upload" {
        let content_type = headers.get("content-type").unwrap().to_str().unwrap();
        let boundary = content_type.split("boundary=").nth(1).unwrap();
        let split = body
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .unwrap();
        let multipart_headers = std::str::from_utf8(&body[..split]).unwrap();
        let filename = multipart_headers
            .split("filename=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let media_type = multipart_headers
            .lines()
            .find_map(|line| line.strip_prefix("Content-Type: "))
            .unwrap();
        let ending = format!("\r\n--{boundary}--\r\n");
        assert!(body.ends_with(ending.as_bytes()));
        let data = &body[split + 4..body.len() - ending.len()];
        state.uploaded.lock().await.push(json!({
            "filename":filename,"media_type":media_type,"data_hex":hex::encode(data),
            "field_file":multipart_headers.contains("name=\"file\""),
        }));
        let status = state.upload_status.load(Ordering::SeqCst);
        if status != 0 {
            return StatusCode::from_u16(status as u16).unwrap().into_response();
        }
        let reply = state.upload_replies.lock().await.pop_front().unwrap_or_else(||json!({
            "file_uuid":store::uuid(),"file_kind":if media_type.starts_with("image/"){"image"}else{"document"},
            "file_name":filename,"file_type":media_type,"file_size":data.len()
        }));
        return axum::Json(reply).into_response();
    }
    if method == "DELETE" {
        state.deleted.fetch_add(1, Ordering::SeqCst);
        return StatusCode::NO_CONTENT.into_response();
    }
    if method == "PUT"
        && path.starts_with(
            "/api/organizations/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/chat_conversations/",
        )
    {
        return StatusCode::NO_CONTENT.into_response();
    }
    if method == "GET"
        && path
            .starts_with("/api/organizations/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/conversations/")
        && path.contains("/wiggle/download-file?")
    {
        return ([("content-type", "text/html")], "name,value\nexample,42\n").into_response();
    }
    if path.ends_with("/chat_conversations") {
        state.created.fetch_add(1, Ordering::SeqCst);
        return axum::Json(json!({"uuid":store::uuid()})).into_response();
    }
    if method == "POST" && path.ends_with("/tool_result") {
        let tool_id = json_body["tool_use_id"].as_str().unwrap_or_default();
        if let Some(sender) = state.client_tool_waiters.lock().await.remove(tool_id) {
            let _ = sender.send(json_body);
            return axum::Json(json!({"type":"ok"})).into_response();
        }
        return StatusCode::NOT_FOUND.into_response();
    }
    if method == "POST" && path.ends_with("/stop_response") {
        let status = state.stop_response_status.load(Ordering::SeqCst);
        return if status == 0 {
            StatusCode::NO_CONTENT.into_response()
        } else {
            StatusCode::from_u16(status as u16).unwrap().into_response()
        };
    }
    if path.ends_with("/completion") {
        let reply = state
            .replies
            .lock()
            .await
            .pop_front()
            .unwrap_or_else(|| {
                if state.automatic_summaries.load(Ordering::SeqCst)
                    && json_body["prompt"]
                        .as_str()
                        .is_some_and(|p| p.starts_with("[Claud2Api internal compaction]"))
                {
                    Reply::Summary(
                        json!({"goal":"continue fixture task","constraints":["preserve recent raw turns"],"completed":["older fixture rounds"],"facts":["synthetic summary only"],"open":[],"next":["continue current round"]}).to_string(),
                    )
                } else {
                    Reply::Text("hello".to_owned())
                }
            });
        let body = match reply {
            Reply::HttpRetry(status, retry) => {
                return (
                    StatusCode::from_u16(status).unwrap(),
                    [("retry-after", retry)],
                    axum::Json(json!({})),
                )
                    .into_response();
            }
            Reply::Http(status) => {
                return (
                    StatusCode::from_u16(status).unwrap(),
                    axum::Json(json!({"error":{"message":"private upstream detail"}})),
                )
                    .into_response();
            }
            Reply::EarlyQuota => {
                event(json!({"type":"message_start"}))
                    + &event(
                        json!({"type":"error","error":{"error_type":"rate_limit_error","message":"private detail"}}),
                    )
            }
            Reply::PartialQuota => {
                event(json!({"type":"content_block_delta","delta":{"text":"partial"}}))
                    + &event(json!({"type":"error","error":{"error_type":"rate_limit_error"}}))
            }
            Reply::Truncated => {
                event(json!({"type":"content_block_delta","delta":{"text":"partial"}}))
            }
            Reply::Text(text) | Reply::Summary(text) => text_events(&text),
            Reply::Raw(text) => text,
            Reply::ClientTool { input, final_text } => {
                let wire_name = json_body["tools"]
                    .as_array()
                    .and_then(|tools| {
                        tools.iter().find(|tool| {
                            tool["name"]
                                .as_str()
                                .is_some_and(|name| name.starts_with("c2a_client_"))
                        })
                    })
                    .and_then(|tool| tool["name"].as_str())
                    .unwrap_or("c2a_client_fixture")
                    .to_owned();
                let upstream_tool_id = "toolu_upstream_client_fixture".to_owned();
                let (sender, receiver) = oneshot::channel();
                state
                    .client_tool_waiters
                    .lock()
                    .await
                    .insert(upstream_tool_id.clone(), sender);
                let output = async_stream::stream! {
                    let events = [
                        json!({"type":"message_start"}),
                        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":upstream_tool_id,"name":wire_name,"input":{}}}),
                        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}),
                        json!({"type":"content_block_stop","index":0}),
                        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
                    ];
                    for value in events {
                        yield Ok::<_, std::convert::Infallible>(Bytes::from(event(value)));
                    }

                    // Claude's sidechannel POST releases this still-open completion.
                    let result = receiver.await.unwrap_or(Value::Null);
                    let result_content = result.get("content").cloned().unwrap_or_else(|| json!([]));
                    let is_error = result.get("is_error").cloned().unwrap_or(json!(false));
                    let continuation = [
                        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_result","tool_use_id":upstream_tool_id,"name":wire_name,"content":result_content,"is_error":is_error}}),
                        json!({"type":"content_block_stop","index":1}),
                        json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}),
                        json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":final_text}}),
                        json!({"type":"content_block_stop","index":2}),
                        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
                        json!({"type":"message_stop"}),
                    ];
                    for value in continuation {
                        yield Ok(Bytes::from(event(value)));
                    }
                };
                return (
                    [("content-type", "text/event-stream")],
                    Body::from_stream(output),
                )
                    .into_response();
            }
            Reply::Slow => {
                let streams = state.streams.clone();
                streams.fetch_add(1, Ordering::SeqCst);
                struct Guard(Arc<AtomicUsize>);
                impl Drop for Guard {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                let guard = Guard(streams);
                let output = async_stream::stream! {let _guard=guard;yield Ok::<_,std::convert::Infallible>(Bytes::from(event(json!({"type":"content_block_delta","delta":{"text":"first"}}))));tokio::time::sleep(Duration::from_secs(20)).await;yield Ok(Bytes::from(event(json!({"type":"message_stop"}))));};
                return (
                    [("content-type", "text/event-stream")],
                    Body::from_stream(output),
                )
                    .into_response();
            }
        };
        return ([("content-type", "text/event-stream")], body).into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}
fn base() -> Value {
    json!({"model":"test","max_tokens":128,"claud2api_native_output":"extensions","messages":[{"role":"user","content":"hello"}]})
}
fn default_native_descriptors() -> Value {
    let mut descriptors = vec![
        json!({"type":"web_search_v0","name":"web_search"}),
        json!({"type":"artifacts_v0","name":"artifacts"}),
        json!({"type":"repl_v0","name":"repl"}),
        json!({"type":"send_to_user_v0","name":"send_to_user"}),
    ];
    let widgets = [
        "weather_fetch",
        "display_stock_data",
        "recipe_display_v0",
        "places_search",
        "places_search_v1",
        "places_map_display_v0",
        "places_map_display_v1",
        "message_compose_v1",
        "writing_draft_v0",
        "ask_user_input_v0",
        "ask_user_free_form_input_v0",
        "fetch_sports_data",
        "recommend_claude_apps",
        "show_recommendation_cards",
        "quiz_display_v0",
        "storyboard_v0",
        "chart_display_v0",
        "options_card_display_v0",
        "step_card_display_v0",
        "itinerary_display_v0",
        "translation_display_v0",
        "comparison_card_display_v0",
        "featured_card_display_v0",
        "product_carousel_display_v0",
        "link_preview_display_v0",
        "places_list_display_v0",
        "starter_tasks_display_v0",
        "claude_code_install_display_v0",
        "role_select_display_v0",
        "approved_connectors_display_v0",
        "persona_cards_display_v0",
        "email_consent_prompt_v0",
        "insight_widget_v0",
        "prompts_card_display_v0",
        "source_card_display_v0",
        "offer_next_step_v0",
        "offer_schedule_v0",
    ];
    descriptors.extend(
        widgets
            .iter()
            .map(|name| json!({"type":"widget","name":name})),
    );
    json!(descriptors)
}
fn native_search_events() -> String {
    [
        json!({"type":"message_start"}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu-search-1","name":"web_search","input":{},"tool_origin":"first_party","approval_key":"synthetic-control-value"}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"que"}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"ry\":\"example\"}"}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"tool_use_block_update_delta","tool_use_id":"toolu-search-1","message":"Searching","display_content":"Searching"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_result","tool_use_id":"toolu-search-1","name":"web_search","content":[{"type":"knowledge","title":"Example","url":"https://example.com/source","metadata":{"type":"web","site_name":"Example"}}],"is_error":false}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"content_block_start","index":3,"content_block":{"type":"text","text":"","citations":[]}}),
        json!({"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"甲"}}),
        json!({"type":"content_block_delta","index":3,"delta":{"type":"citation_start_delta","citation":{"uuid":"citation-1","type":"web_search_result_location","title":"Example Source","url":"https://example.com/source","metadata":{"site_name":"Example"}}}}),
        json!({"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"乙丙"}}),
        json!({"type":"content_block_delta","index":3,"delta":{"type":"citation_end_delta","citation_uuid":"citation-1"}}),
        json!({"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"丁结论。"}}),
        json!({"type":"content_block_stop","index":3}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        json!({"type":"message_limit","message_limit":{"type":"turn"}}),
        json!({"type":"message_stop"}),
    ]
    .into_iter()
    .map(event)
    .collect::<Vec<_>>()
    .concat()
}
fn native_file_events() -> String {
    [
        json!({"type":"message_start"}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu-present-1","name":"present_files","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"files\":[\"/mnt/user-data/outputs/report.csv\"]}"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_result","tool_use_id":"toolu-present-1","name":"present_files","content":[{"type":"local_resource","file_path":"/mnt/user-data/outputs/report.csv","name":"report.csv","mime_type":"text/csv"}],"is_error":false}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        json!({"type":"message_stop"}),
    ]
    .into_iter()
    .map(event)
    .collect::<Vec<_>>()
    .concat()
}
fn single_native_search_descriptor() -> Value {
    json!([{"type":"web_search_v0","name":"web_search"}])
}
fn project_native_descriptors() -> Value {
    let mut descriptors = default_native_descriptors();
    descriptors
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"project_knowledge_search","name":"project_knowledge_search"}));
    descriptors
}
fn normalize_generated(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (k, v) in object {
                if matches!(k.as_str(), "created" | "created_at") {
                    *v = json!(0);
                } else if k == "request_id" {
                    *v = json!("req_GENERATED");
                } else if matches!(k.as_str(), "id" | "item_id") {
                    if let Some(id) = v.as_str() {
                        for prefix in ["chatcmpl-", "resp_", "msg_"] {
                            if id.starts_with(prefix) {
                                *v = json!(format!("{prefix}GENERATED"));
                                break;
                            }
                        }
                    }
                } else {
                    normalize_generated(v);
                }
            }
        }
        Value::Array(a) => {
            for v in a {
                normalize_generated(v);
            }
        }
        _ => {}
    }
}
fn sse_values(text: &str) -> Value {
    let mut values: Vec<Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| {
            if data == "[DONE]" {
                json!("[DONE]")
            } else {
                serde_json::from_str(data).unwrap()
            }
        })
        .collect();
    for v in &mut values {
        normalize_generated(v);
    }
    json!(values)
}
fn completion_requests(traces: &[Value]) -> Vec<Value> {
    traces
        .iter()
        .filter(|trace| trace["uri"].as_str().unwrap().ends_with("/completion"))
        .cloned()
        .collect()
}

fn assert_stop_before_replacement_completion_with_count(
    traces: &[Value],
    replacement_index: usize,
    expected_stop_count: usize,
) {
    let completion_indices: Vec<_> = traces
        .iter()
        .enumerate()
        .filter(|(_, trace)| {
            trace["uri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("/completion"))
        })
        .map(|(index, _)| index)
        .collect();
    assert!(completion_indices.len() > replacement_index);
    let stop_indices: Vec<_> = traces
        .iter()
        .enumerate()
        .filter(|(_, trace)| {
            trace["method"] == "POST"
                && trace["uri"]
                    .as_str()
                    .is_some_and(|uri| uri.ends_with("/stop_response"))
        })
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        stop_indices.len(),
        expected_stop_count,
        "fresh rebuild must stop its old stream the expected number of times"
    );
    let first_completion = traces[completion_indices[0]]["uri"]
        .as_str()
        .unwrap()
        .strip_suffix("/completion")
        .unwrap();
    for stop_index in &stop_indices {
        assert_eq!(
            traces[*stop_index]["uri"],
            format!("{first_completion}/stop_response"),
            "stop_response must target the original conversation"
        );
    }
    assert!(
        stop_indices
            .iter()
            .all(|index| *index < completion_indices[replacement_index]),
        "old stream must be stopped before starting the replacement completion"
    );
}

fn assert_stop_before_replacement_completion(traces: &[Value], replacement_index: usize) {
    assert_stop_before_replacement_completion_with_count(traces, replacement_index, 1);
}

fn assert_fresh_stop_before_rebuild(traces: &[Value]) {
    let completions = completion_requests(traces);
    assert_eq!(completions.len(), 2);
    assert_stop_before_replacement_completion(traces, 1);
}

#[tokio::test]
async fn native_tool_selection_forwards_descriptors_and_honors_admin_caps() {
    let f = fixture(1).await;
    let (_, settings) = admin(&f.engine, "GET", "/settings", Value::Null, None).await;
    for group in [
        "native_web_search",
        "native_artifacts",
        "native_repl",
        "native_widgets",
        "native_project_knowledge",
    ] {
        assert_eq!(settings["settings"][group], true, "{group}");
    }

    let mut selected = base();
    selected["claud2api_tools"] = json!(["web_search"]);
    let (status, _, body) =
        call(&f.engine, "/v1/messages", selected, Some("native-selected")).await;
    assert_eq!(status, 200, "{body}");
    let traces = f.upstream.traces.lock().await;
    assert_eq!(
        completion_requests(&traces)[0]["body"]["tools"],
        single_native_search_descriptor()
    );
    drop(traces);

    reset_pacing(&f.engine).await;
    let mut disabled = base();
    disabled["claud2api_tools"] = json!([]);
    let (status, _, body) =
        call(&f.engine, "/v1/messages", disabled, Some("native-disabled")).await;
    assert_eq!(status, 200, "{body}");
    let traces = f.upstream.traces.lock().await;
    assert_eq!(completion_requests(&traces)[1]["body"]["tools"], json!([]));

    let f = fixture(1).await;
    f.engine
        .store
        .mutate("", json!({}), |state| {
            state.settings.native_web_search = false;
            Ok(())
        })
        .await
        .unwrap();
    let mut forbidden = base();
    forbidden["claud2api_tools"] = json!(["web_search"]);
    let (status, _, body) = call(&f.engine, "/v1/messages", forbidden, None).await;
    assert_eq!(status, 400, "{body}");
    assert!(f.upstream.traces.lock().await.is_empty());
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 0);

    let f = fixture(1).await;
    let mut project_without_uuid = base();
    project_without_uuid["claud2api_tools"] = json!(["project_knowledge"]);
    let (status, _, body) = call(&f.engine, "/v1/messages", project_without_uuid, None).await;
    assert_eq!(status, 400, "{body}");
    assert!(f.upstream.traces.lock().await.is_empty());

    f.engine
        .store
        .mutate("", json!({}), |state| {
            state.settings.native_project_knowledge = false;
            Ok(())
        })
        .await
        .unwrap();
    let mut disabled_project = base();
    disabled_project["claud2api_project_uuid"] = json!("11111111-1111-4111-8111-111111111111");
    disabled_project["claud2api_tools"] = json!([]);
    let (status, _, body) = call(&f.engine, "/v1/messages", disabled_project, None).await;
    assert_eq!(status, 400, "{body}");
    assert!(f.upstream.traces.lock().await.is_empty());
}

fn synthetic_client_tool_request(choice: Value, stream: bool) -> Value {
    let mut request = base();
    request["stream"] = json!(stream);
    request["tool_choice"] = choice;
    request["tools"] = json!([{
        "name":"synthetic_echo",
        "description":"Echo a synthetic value.",
        "input_schema":{
            "type":"object",
            "properties":{"value":{"type":"string"}},
            "required":["value"],
            "additionalProperties":false
        }
    }]);
    request["messages"][0]["content"] = json!("Call synthetic_echo with the marker.");
    request
}

async fn start_synthetic_client_tool(
    f: &Fixture,
    request: Value,
    identity: &str,
) -> (String, Value) {
    let (status, headers, response) =
        call(&f.engine, "/v1/messages", request, Some(identity)).await;
    assert_eq!(status, 200, "{response}");
    let handle = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    let tool_call = response["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "tool_use")
        .expect("the initial generation should request a client tool")
        .clone();
    (handle, tool_call)
}

fn append_client_tool_result(
    request: &mut Value,
    tool_call: &Value,
    result_content: Value,
    following_user_text: Option<&str>,
) {
    let mut content = vec![json!({
        "type":"tool_result",
        "tool_use_id":tool_call["id"],
        "content":result_content
    })];
    if let Some(text) = following_user_text {
        content.push(json!({"type":"text","text":text}));
    }
    request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":[tool_call.clone()]}),
        json!({"role":"user","content":content}),
    ]);
}

async fn assert_client_tool_choice_can_switch_to_auto(choice: Value, stream: bool) {
    let f = fixture(1).await;
    let input = json!({"value":"choice marker"});
    f.upstream
        .replies
        .lock()
        .await
        .push_back(Reply::ClientTool {
            input: input.clone(),
            final_text: "choice continuation completed".to_owned(),
        });
    let mut request = synthetic_client_tool_request(choice, stream);
    let (status, headers, first) = call(
        &f.engine,
        "/v1/messages",
        request.clone(),
        Some("client-tool-choice-switch"),
    )
    .await;
    assert_eq!(status, 200, "{first}");
    let (tool_call, reason) = if stream {
        let events = sse_values(first.as_str().unwrap());
        let events = events.as_array().unwrap();
        let start = events
            .iter()
            .find(|event| {
                event["type"] == "content_block_start"
                    && event["content_block"]["type"] == "tool_use"
            })
            .unwrap();
        let index = start["index"].as_u64().unwrap();
        let partial = events
            .iter()
            .filter(|event| {
                event["type"] == "content_block_delta"
                    && event["index"] == index
                    && event["delta"]["type"] == "input_json_delta"
            })
            .filter_map(|event| event["delta"]["partial_json"].as_str())
            .collect::<String>();
        let start = &start["content_block"];
        (
            json!({
                "type":"tool_use","id":start["id"],"name":start["name"],
                "input":serde_json::from_str::<Value>(&partial).unwrap()
            }),
            events
                .iter()
                .find(|event| event["type"] == "message_delta")
                .unwrap()["delta"]["stop_reason"]
                .clone(),
        )
    } else {
        (
            first["content"]
                .as_array()
                .unwrap()
                .iter()
                .find(|block| block["type"] == "tool_use")
                .unwrap()
                .clone(),
            first["stop_reason"].clone(),
        )
    };
    assert_eq!(reason, "tool_use");
    assert!(tool_call["id"].as_str().unwrap().starts_with("toolu_c2a_"));
    assert_eq!(tool_call["name"], "synthetic_echo");
    assert_eq!(tool_call["input"], input);

    request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":[tool_call.clone()]}),
        json!({"role":"user","content":[{
            "type":"tool_result","tool_use_id":tool_call["id"],"content":"choice result"
        }]}),
    ]);
    request["tool_choice"] = json!({"type":"auto"});
    let handle = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    let (status, _, resumed) = call(&f.engine, "/v1/messages", request, Some(&handle)).await;
    assert_eq!(status, 200, "{resumed}");
    let text = if stream {
        sse_values(resumed.as_str().unwrap())
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|event| {
                (event["type"] == "content_block_delta")
                    .then(|| event["delta"]["text"].as_str())
                    .flatten()
            })
            .collect::<String>()
    } else {
        resumed["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<String>()
    };
    assert_eq!(text, "choice continuation completed");
    let traces = f.upstream.traces.lock().await;
    assert_eq!(completion_requests(&traces).len(), 1);
    assert_eq!(
        traces
            .iter()
            .filter(|trace| trace["uri"].as_str().unwrap().ends_with("/tool_result"))
            .count(),
        1
    );
}

#[tokio::test]
async fn client_tool_choice_any_and_named_tool_can_resume_with_auto() {
    assert_client_tool_choice_can_switch_to_auto(json!({"type":"any"}), false).await;
    assert_client_tool_choice_can_switch_to_auto(
        json!({"type":"tool","name":"synthetic_echo"}),
        true,
    )
    .await;
}

#[tokio::test]
async fn tool_choice_none_after_client_tool_starts_fresh_tool_free_completion() {
    let f = fixture(1).await;
    f.upstream.replies.lock().await.extend([
        Reply::ClientTool {
            input: json!({"value":"pending marker"}),
            final_text: "must not resume the pending completion".to_owned(),
        },
        Reply::Text("finished without tools".to_owned()),
    ]);

    let mut request = synthetic_client_tool_request(json!({"type":"auto"}), false);
    let (status, first_headers, first) = call(
        &f.engine,
        "/v1/messages",
        request.clone(),
        Some("tool-choice-none-after-pending"),
    )
    .await;
    assert_eq!(status, 200, "{first}");
    let handle = first_headers["x-c2a-session-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let first_tool = first["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "tool_use")
        .expect("first generation should request the declared client tool")
        .clone();
    assert_eq!(first_tool["name"], "synthetic_echo");
    assert_eq!(first_tool["input"], json!({"value":"pending marker"}));

    request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":[first_tool.clone()]}),
        json!({"role":"user","content":[{
            "type":"tool_result","tool_use_id":first_tool["id"],
            "content":"synthetic result must be preserved in history"
        }]}),
    ]);
    request["tool_choice"] = json!({"type":"none"});
    let (status, second_headers, second) =
        call(&f.engine, "/v1/messages", request, Some(&handle)).await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(second_headers["x-c2a-session-id"], handle);
    assert_eq!(second["stop_reason"], "end_turn");
    assert_eq!(second["content"].as_array().unwrap().len(), 1);
    assert_eq!(second["content"][0]["type"], "text");
    assert_eq!(second["content"][0]["text"], "finished without tools");
    assert!(
        second["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|block| { !matches!(block["type"].as_str(), Some("tool_use" | "tool_result")) })
    );

    let traces = f.upstream.traces.lock().await;
    let completions = completion_requests(&traces);
    assert_eq!(completions.len(), 2);
    assert_fresh_stop_before_rebuild(&traces);
    assert!(
        default_native_descriptors()
            .as_array()
            .unwrap()
            .iter()
            .all(|descriptor| completions[0]["body"]["tools"]
                .as_array()
                .unwrap()
                .contains(descriptor))
    );
    assert!(
        completions[0]["body"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("c2a_client_")))
    );
    assert_eq!(completions[1]["body"]["tools"], json!([]));
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 2);
    assert_eq!(
        traces
            .iter()
            .filter(|trace| trace["method"] == "POST"
                && trace["uri"].as_str().unwrap().ends_with("/tool_result"))
            .count(),
        0
    );
}

#[tokio::test]
async fn client_pdf_result_rebuilds_with_real_upload_and_preserves_validated_history() {
    let f = fixture(1).await;
    let pdf_bytes = b"%PDF-1.4\nsynthetic client tool PDF";
    let encoded_pdf = STANDARD.encode(pdf_bytes);
    let pdf_id = "33333333-3333-4333-8333-333333333333";
    f.upstream.upload_replies.lock().await.push_back(json!({
        "file_uuid":pdf_id,
        "file_kind":"document",
        "extracted_content":"Synthetic PDF preview; uploaded bytes remain authoritative."
    }));
    f.upstream.replies.lock().await.extend([
        Reply::ClientTool {
            input: json!({"value":"read the returned PDF"}),
            final_text: "must not resume the unsupported PDF result post".to_owned(),
        },
        Reply::Text("finished after reading the client PDF".to_owned()),
    ]);

    let mut request = synthetic_client_tool_request(json!({"type":"auto"}), false);
    request["messages"][0]["content"] = json!("Read the PDF returned by the client tool.");
    let (status, first_headers, first) = call(
        &f.engine,
        "/v1/messages",
        request.clone(),
        Some("client-pdf-tool-result-rebuild"),
    )
    .await;
    assert_eq!(status, 200, "{first}");
    let handle = first_headers["x-c2a-session-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let tool_call = first["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "tool_use")
        .expect("first generation should request the declared client tool")
        .clone();

    request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":[tool_call.clone()]}),
        json!({"role":"user","content":[{
            "type":"tool_result",
            "tool_use_id":tool_call["id"],
            "content":[
                {"type":"text","text":"The requested file is attached."},
                {"type":"document","title":"client-result.pdf","source":{
                    "type":"base64","media_type":"application/pdf","data":encoded_pdf
                }}
            ]
        }]}),
    ]);
    let (status, second_headers, second) =
        call(&f.engine, "/v1/messages", request, Some(&handle)).await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(second_headers["x-c2a-session-id"], handle);
    assert_eq!(second["stop_reason"], "end_turn");
    assert_eq!(second["content"].as_array().unwrap().len(), 1);
    assert_eq!(
        second["content"][0]["text"],
        "finished after reading the client PDF"
    );

    let uploaded = f.upstream.uploaded.lock().await;
    assert_eq!(uploaded.len(), 1);
    assert_eq!(uploaded[0]["media_type"], "application/pdf");
    let uploaded_filename = uploaded[0]["filename"].as_str().unwrap().to_owned();
    assert!(uploaded_filename.starts_with("client-result-"));
    assert!(uploaded_filename.ends_with(".pdf"));
    assert_eq!(uploaded[0]["data_hex"], hex::encode(pdf_bytes));
    drop(uploaded);

    let traces = f.upstream.traces.lock().await;
    let completions = completion_requests(&traces);
    assert_eq!(completions.len(), 2);
    assert_fresh_stop_before_rebuild(&traces);
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 2);
    assert_eq!(completions[0]["body"]["files"], json!([]));
    assert_eq!(completions[1]["body"]["files"], json!([pdf_id]));
    let prompt = completions[1]["body"]["prompt"].as_str().unwrap();
    let envelope = prompt
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|value| value.get("history").is_some())
        .expect("fresh completion should receive the full validated history envelope");
    let history = envelope["history"].as_array().unwrap();
    let returned_result = &history.last().unwrap()["content"][0];
    assert_eq!(returned_result["type"], "tool_result");
    assert_eq!(returned_result["content"][1]["type"], "attachment");
    assert_eq!(
        returned_result["content"][1]["file"]["filename"],
        uploaded_filename
    );
    assert_eq!(
        returned_result["content"][1]["file"]["media_type"],
        "application/pdf"
    );
    assert!(!prompt.contains(&STANDARD.encode(pdf_bytes)));
    assert_eq!(
        traces
            .iter()
            .filter(|trace| trace["method"] == "POST"
                && trace["uri"].as_str().unwrap().ends_with("/tool_result"))
            .count(),
        0
    );
}

#[tokio::test]
async fn dropping_client_tool_sse_retires_waiting_session_and_releases_account() {
    let f = fixture(1).await;
    f.upstream
        .replies
        .lock()
        .await
        .push_back(Reply::ClientTool {
            input: json!({"value":"cancel marker"}),
            final_text: "must not be delivered".to_owned(),
        });
    let request = synthetic_client_tool_request(json!({"type":"auto"}), true);
    let account_id = f
        .engine
        .store
        .read()
        .await
        .accounts
        .keys()
        .next()
        .unwrap()
        .clone();
    let response = web::public(f.engine.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("x-api-key", &f.engine.config.gateway_key)
                .header("x-c2a-session-id", "client-tool-aborted-stream")
                .header("content-type", "application/json")
                .body(Body::from(request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let handle = response
        .headers()
        .get("x-c2a-session-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let mut body = response.into_body();
    let mut saw_tool_use = false;
    tokio::time::timeout(Duration::from_secs(1), async {
        while let Some(frame) = body.frame().await {
            let frame = frame.unwrap();
            if let Ok(chunk) = frame.into_data() {
                let text = String::from_utf8_lossy(&chunk);
                for data in text.lines().filter_map(|line| line.strip_prefix("data: ")) {
                    let event: Value = serde_json::from_str(data).unwrap();
                    assert_ne!(event["type"], "message_stop");
                    if event["type"] == "content_block_start"
                        && event["content_block"]["type"] == "tool_use"
                    {
                        saw_tool_use = true;
                        break;
                    }
                }
                if saw_tool_use {
                    break;
                }
            }
        }
    })
    .await
    .expect("first client tool_use should arrive promptly");
    assert!(saw_tool_use);
    drop(body);

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let session_removed = !f.engine.store.read().await.sessions.contains_key(&handle);
            let account_released = !f.engine.pool.busy(&account_id).await;
            if session_removed && account_released {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropping the SSE should retire the wait and release the account");
    let traces = f.upstream.traces.lock().await;
    assert_eq!(completion_requests(&traces).len(), 1);
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 1);
    assert!(traces.iter().all(|trace| {
        !trace["uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("/tool_result"))
    }));
}

#[tokio::test]
async fn client_tool_roundtrip_resumes_the_same_completion_over_the_sidechannel() {
    let f = fixture(1).await;
    let input = json!({"value":"synthetic marker"});
    let result_text = "synthetic echo result marker";
    let final_text = "continued from the same completion";
    f.upstream
        .replies
        .lock()
        .await
        .push_back(Reply::ClientTool {
            input: input.clone(),
            final_text: final_text.to_owned(),
        });

    let mut request = base();
    request["stream"] = json!(true);
    request["tools"] = json!([{
        "name":"synthetic_echo",
        "description":"Echo the supplied synthetic value.",
        "input_schema":{
            "type":"object",
            "properties":{"value":{"type":"string"}},
            "required":["value"],
            "additionalProperties":false
        }
    }]);
    request["messages"][0]["content"] = json!("Call synthetic_echo with the marker.");

    let (status, headers, first) = call(
        &f.engine,
        "/v1/messages",
        request.clone(),
        Some("client-tool-roundtrip"),
    )
    .await;
    assert_eq!(status, 200, "{first}");
    let handle = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    let first_events = sse_values(first.as_str().expect("streaming response body"));
    let first_events = first_events.as_array().expect("SSE events");
    let tool_start = first_events
        .iter()
        .find(|event| {
            event["type"] == "content_block_start" && event["content_block"]["type"] == "tool_use"
        })
        .expect("downstream client tool_use");
    let tool_call = &tool_start["content_block"];
    let public_id = tool_call["id"].as_str().expect("public tool id");
    assert!(public_id.starts_with("toolu_c2a_"));
    assert_eq!(tool_call["name"], "synthetic_echo");
    let partial_json = first_events
        .iter()
        .filter(|event| event["type"] == "content_block_delta")
        .filter_map(|event| {
            (event["delta"]["type"] == "input_json_delta")
                .then(|| event["delta"]["partial_json"].as_str())
                .flatten()
        })
        .collect::<String>();
    assert_eq!(serde_json::from_str::<Value>(&partial_json).unwrap(), input);
    assert!(first_events.iter().any(|event| {
        event["type"] == "message_delta" && event["delta"]["stop_reason"] == "tool_use"
    }));

    request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":[{
            "type":"tool_use","id":public_id,"name":"synthetic_echo","input":input
        }]}),
        json!({"role":"user","content":[{
            "type":"tool_result","tool_use_id":public_id,"content":result_text,"is_error":false
        }]}),
    ]);
    request["tool_choice"] = json!({"type":"auto"});
    let resumed_request = request.clone();

    let mut changed_input = resumed_request.clone();
    changed_input["messages"][1]["content"][0]["input"]["value"] = json!("tampered marker");
    let (status, _, rejected) = call(&f.engine, "/v1/messages", changed_input, Some(&handle)).await;
    assert_eq!(
        status, 409,
        "changed tool arguments must be rejected: {rejected}"
    );

    let mut changed_id = resumed_request.clone();
    changed_id["messages"][1]["content"][0]["id"] = json!("toolu_c2a_wrong_id");
    changed_id["messages"][2]["content"][0]["tool_use_id"] = json!("toolu_c2a_wrong_id");
    let (status, _, rejected) = call(&f.engine, "/v1/messages", changed_id, Some(&handle)).await;
    assert_eq!(
        status, 409,
        "changed public tool id must be rejected: {rejected}"
    );

    let (_, other_token) = managed_token(&f.engine, 1).await;
    let (status, _, rejected) = call_key(
        &f.engine,
        "/v1/messages",
        resumed_request.clone(),
        Some(&handle),
        &other_token,
    )
    .await;
    assert_eq!(
        status, 403,
        "a different token must not resume the parked tool: {rejected}"
    );

    let (status, second_headers, second) =
        call(&f.engine, "/v1/messages", request, Some(&handle)).await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(second_headers["x-c2a-session-id"], handle);
    let second_events = sse_values(second.as_str().expect("streaming response body"));
    let continued_text = second_events
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|event| {
            (event["type"] == "content_block_delta")
                .then(|| event["delta"]["text"].as_str())
                .flatten()
        })
        .collect::<String>();
    assert_eq!(continued_text, final_text);
    assert!(second_events.as_array().unwrap().iter().any(|event| {
        event["type"] == "message_delta" && event["delta"]["stop_reason"] == "end_turn"
    }));

    let (status, _, replay) = call(&f.engine, "/v1/messages", resumed_request, Some(&handle)).await;
    assert_eq!(
        status, 409,
        "a completed client result must not be replayed: {replay}"
    );

    let traces = f.upstream.traces.lock().await;
    let completions = completion_requests(&traces);
    assert_eq!(completions.len(), 1, "continuation must not generate again");
    assert!(traces.iter().all(|trace| {
        !trace["uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("/stop_response"))
    }));
    let tool_descriptors = completions[0]["body"]["tools"].as_array().unwrap();
    let client_descriptor = tool_descriptors
        .iter()
        .find(|tool| {
            tool["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("c2a_client_"))
        })
        .expect("wire alias in upstream tools");
    assert_eq!(
        client_descriptor["input_schema"],
        json!({
            "type":"object",
            "properties":{"value":{"type":"string"}},
            "required":["value"],
            "additionalProperties":false
        })
    );

    let tool_results: Vec<_> = traces
        .iter()
        .filter(|trace| {
            trace["method"] == "POST" && trace["uri"].as_str().unwrap().ends_with("/tool_result")
        })
        .collect();
    assert_eq!(
        tool_results.len(),
        1,
        "one client result must be posted once"
    );
    let result = tool_results[0];
    assert_eq!(result["body"]["type"], "tool_result");
    assert_eq!(
        result["body"]["tool_use_id"],
        "toolu_upstream_client_fixture"
    );
    assert_eq!(result["body"]["is_error"], false);
    assert_eq!(
        result["body"]["content"],
        json!([{"type":"text","text":result_text}]),
        "the live completion receives only the caller's result content"
    );
    assert_eq!(
        result["body"]["content"][0],
        json!({"type":"text","text":result_text})
    );
    let completion_path = completions[0]["uri"]
        .as_str()
        .unwrap()
        .strip_suffix("/completion")
        .unwrap();
    assert_eq!(result["uri"], format!("{completion_path}/tool_result"));
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 1);
    assert!(f.upstream.client_tool_waiters.lock().await.is_empty());
    let session = f.engine.store.read().await.sessions[&handle].clone();
    assert!(!session.uncertain);
}

#[tokio::test]
async fn client_tool_result_followed_by_user_text_rebuilds_and_preserves_both_blocks() {
    let f = fixture(1).await;
    f.upstream.replies.lock().await.extend([
        Reply::ClientTool {
            input: json!({"value":"user text after result"}),
            final_text: "must not resume after a following user instruction".to_owned(),
        },
        Reply::Text("answered the follow-up".to_owned()),
    ]);
    let mut request = synthetic_client_tool_request(json!({"type":"auto"}), false);
    let (status, headers, first) = call(
        &f.engine,
        "/v1/messages",
        request.clone(),
        Some("client-tool-result-following-user-text"),
    )
    .await;
    assert_eq!(status, 200, "{first}");
    let handle = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    let tool_call = first["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "tool_use")
        .expect("first generation should request a client tool")
        .clone();

    let result_text = "original client result";
    let follow_up = "Now explain the result in one sentence.";
    request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":[tool_call.clone()]}),
        json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":tool_call["id"],"content":result_text},
            {"type":"text","text":follow_up}
        ]}),
    ]);
    let (status, resumed_headers, response) =
        call(&f.engine, "/v1/messages", request, Some(&handle)).await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(resumed_headers["x-c2a-session-id"], handle);
    assert_eq!(response["content"][0]["text"], "answered the follow-up");

    let traces = f.upstream.traces.lock().await;
    let completions = completion_requests(&traces);
    assert_eq!(completions.len(), 2);
    assert_fresh_stop_before_rebuild(&traces);
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 2);
    let prompt = completions[1]["body"]["prompt"].as_str().unwrap();
    let envelope = prompt
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|value| value.get("history").is_some())
        .expect("fresh completion should receive the full history");
    let history = envelope["history"].as_array().unwrap();
    let last = history.last().unwrap();
    assert_eq!(last["role"], "user");
    assert_eq!(last["content"].as_array().unwrap().len(), 2);
    assert_eq!(last["content"][0]["type"], "tool_result");
    assert_eq!(last["content"][0]["content"][0]["text"], result_text);
    assert_eq!(last["content"][1]["type"], "text");
    assert_eq!(last["content"][1]["text"], follow_up);
    assert_eq!(
        traces
            .iter()
            .filter(|trace| trace["method"] == "POST"
                && trace["uri"].as_str().unwrap().ends_with("/tool_result"))
            .count(),
        0
    );
}

#[tokio::test]
async fn client_tool_system_instruction_added_after_result_rebuilds_without_sidechannel_post() {
    let f = fixture(1).await;
    f.upstream.replies.lock().await.extend([
        Reply::ClientTool {
            input: json!({"value":"system instruction after result"}),
            final_text: "must not resume after an instruction change".to_owned(),
        },
        Reply::Text("followed the new instruction".to_owned()),
    ]);
    let mut request = synthetic_client_tool_request(json!({"type":"auto"}), false);
    let (status, headers, first) = call(
        &f.engine,
        "/v1/messages",
        request.clone(),
        Some("client-tool-system-instruction-rebuild"),
    )
    .await;
    assert_eq!(status, 200, "{first}");
    let handle = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    let tool_call = first["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "tool_use")
        .expect("first generation should request a client tool")
        .clone();

    let result_text = "original result remains separate";
    let instruction = "Use the returned result and answer concisely.";
    request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":[tool_call.clone()]}),
        json!({"role":"user","content":[{
            "type":"tool_result","tool_use_id":tool_call["id"],"content":result_text
        }]}),
        json!({"role":"system","content":instruction}),
    ]);
    let (status, resumed_headers, response) =
        call(&f.engine, "/v1/messages", request, Some(&handle)).await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(resumed_headers["x-c2a-session-id"], handle);
    assert_eq!(
        response["content"][0]["text"],
        "followed the new instruction"
    );

    let traces = f.upstream.traces.lock().await;
    let completions = completion_requests(&traces);
    assert_eq!(completions.len(), 2);
    assert_fresh_stop_before_rebuild(&traces);
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 2);
    let prompt = completions[1]["body"]["prompt"].as_str().unwrap();
    let envelope = prompt
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|value| value.get("history").is_some())
        .expect("fresh completion should receive the full history envelope");
    assert_eq!(envelope["system"], instruction);
    let history = envelope["history"].as_array().unwrap();
    let result = &history.last().unwrap()["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["content"][0]["text"], result_text);
    assert_eq!(
        traces
            .iter()
            .filter(|trace| trace["method"] == "POST"
                && trace["uri"].as_str().unwrap().ends_with("/tool_result"))
            .count(),
        0
    );
}

#[tokio::test]
async fn client_tool_preflight_500_or_429_allows_only_the_exact_fresh_retry() {
    for preflight_status in [500, 429] {
        let f = fixture(1).await;
        let failed_reply = if preflight_status == 429 {
            Reply::HttpRetry(429, "0".to_owned())
        } else {
            Reply::Http(500)
        };
        f.upstream.replies.lock().await.extend([
            Reply::ClientTool {
                input: json!({"value":"first and only client call"}),
                final_text: "the original pending stream must be replaced".to_owned(),
            },
            failed_reply,
            Reply::Text("the exact retry completed".to_owned()),
        ]);
        let mut request = synthetic_client_tool_request(json!({"type":"auto"}), false);
        let (handle, tool_call) =
            start_synthetic_client_tool(&f, request.clone(), "client-tool-fresh-preflight-retry")
                .await;
        append_client_tool_result(
            &mut request,
            &tool_call,
            json!("original tool result"),
            Some("Answer this follow-up after the tool result."),
        );

        let (status, _, failure) =
            call(&f.engine, "/v1/messages", request.clone(), Some(&handle)).await;
        assert_ne!(status, 200, "preflight {preflight_status}: {failure}");
        let account_id = f.engine.store.read().await.sessions[&handle]
            .account_id
            .clone();
        tokio::time::timeout(Duration::from_secs(1), async {
            while f.engine.pool.busy(&account_id).await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed fresh preflight must release the account lease");
        {
            let traces = f.upstream.traces.lock().await;
            assert_eq!(completion_requests(&traces).len(), 2);
            assert_stop_before_replacement_completion(&traces, 1);
            assert_eq!(f.upstream.created.load(Ordering::SeqCst), 2);
            assert_eq!(
                traces
                    .iter()
                    .filter(|trace| trace["method"] == "POST"
                        && trace["uri"].as_str().unwrap().ends_with("/tool_result"))
                    .count(),
                0
            );
        }
        if preflight_status == 429 {
            f.engine
                .store
                .mutate("", json!({}), |state| {
                    for account in state.accounts.values_mut() {
                        account.cooldown_until = 0;
                        account.quota_until = 0;
                    }
                    Ok(())
                })
                .await
                .unwrap();
        }
        reset_pacing(&f.engine).await;
        let (status, headers, response) =
            call(&f.engine, "/v1/messages", request, Some(&handle)).await;
        assert_eq!(status, 200, "preflight {preflight_status}: {response}");
        assert_eq!(headers["x-c2a-session-id"], handle);
        assert_eq!(response["content"][0]["text"], "the exact retry completed");

        let traces = f.upstream.traces.lock().await;
        assert_eq!(completion_requests(&traces).len(), 3);
        assert_eq!(f.upstream.created.load(Ordering::SeqCst), 3);
        assert_stop_before_replacement_completion(&traces, 1);
        assert!(traces.iter().all(|trace| {
            !trace["uri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("/tool_result"))
        }));
        assert_eq!(f.upstream.client_tool_waiters.lock().await.len(), 1);
    }
}

#[tokio::test]
async fn failed_stop_response_must_succeed_before_retry_can_start_fresh() {
    let f = fixture(1).await;
    f.upstream.replies.lock().await.extend([
        Reply::ClientTool {
            input: json!({"value":"keep the first tool call"}),
            final_text: "the old completion remains open until explicitly stopped".to_owned(),
        },
        Reply::Text("retry after a successful stop".to_owned()),
    ]);
    let mut request = synthetic_client_tool_request(json!({"type":"auto"}), false);
    let (handle, tool_call) =
        start_synthetic_client_tool(&f, request.clone(), "client-tool-stop-failure-retry").await;
    append_client_tool_result(
        &mut request,
        &tool_call,
        json!("caller result"),
        Some("Please summarize the caller result."),
    );
    f.upstream.stop_response_status.store(500, Ordering::SeqCst);

    let (status, _, failure) =
        call(&f.engine, "/v1/messages", request.clone(), Some(&handle)).await;
    assert_eq!(status, 502, "{failure}");
    let account_id = f.engine.store.read().await.sessions[&handle]
        .account_id
        .clone();
    tokio::time::timeout(Duration::from_secs(1), async {
        while f.engine.pool.busy(&account_id).await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a failed stop request must release its account lease");
    {
        let traces = f.upstream.traces.lock().await;
        assert_eq!(completion_requests(&traces).len(), 1);
        assert_eq!(f.upstream.created.load(Ordering::SeqCst), 1);
        assert_eq!(
            traces
                .iter()
                .filter(|trace| trace["method"] == "POST"
                    && trace["uri"].as_str().unwrap().ends_with("/stop_response"))
                .count(),
            1
        );
        assert!(traces.iter().all(|trace| {
            !trace["uri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("/tool_result"))
        }));
    }

    f.upstream.stop_response_status.store(0, Ordering::SeqCst);
    reset_pacing(&f.engine).await;
    let (status, headers, response) = call(&f.engine, "/v1/messages", request, Some(&handle)).await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(headers["x-c2a-session-id"], handle);
    assert_eq!(
        response["content"][0]["text"],
        "retry after a successful stop"
    );
    let traces = f.upstream.traces.lock().await;
    assert_eq!(completion_requests(&traces).len(), 2);
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 2);
    assert_stop_before_replacement_completion_with_count(&traces, 1, 2);
    assert!(traces.iter().all(|trace| {
        !trace["uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("/tool_result"))
    }));
}

#[tokio::test]
async fn tampered_fresh_retry_result_is_rejected_without_consuming_the_receipt() {
    let f = fixture(1).await;
    f.upstream.replies.lock().await.extend([
        Reply::ClientTool {
            input: json!({"value":"preserve the result receipt"}),
            final_text: "the original pending stream is stopped".to_owned(),
        },
        Reply::Http(500),
        Reply::Text("the original result was retried".to_owned()),
    ]);
    let mut request = synthetic_client_tool_request(json!({"type":"auto"}), false);
    let (handle, tool_call) =
        start_synthetic_client_tool(&f, request.clone(), "client-tool-tampered-fresh-retry").await;
    append_client_tool_result(
        &mut request,
        &tool_call,
        json!("immutable tool result"),
        Some("Continue with the result."),
    );
    let (status, _, failure) =
        call(&f.engine, "/v1/messages", request.clone(), Some(&handle)).await;
    assert_ne!(status, 200, "{failure}");
    reset_pacing(&f.engine).await;
    let traces_before_tamper = f.upstream.traces.lock().await.len();

    let mut tampered = request.clone();
    tampered["messages"][2]["content"][0]["content"] = json!("modified tool result");
    let (status, _, rejected) = call(&f.engine, "/v1/messages", tampered, Some(&handle)).await;
    assert_eq!(status, 409, "{rejected}");
    assert_eq!(f.upstream.traces.lock().await.len(), traces_before_tamper);

    let (status, headers, response) = call(&f.engine, "/v1/messages", request, Some(&handle)).await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(headers["x-c2a-session-id"], handle);
    assert_eq!(
        response["content"][0]["text"],
        "the original result was retried"
    );
    let traces = f.upstream.traces.lock().await;
    assert_eq!(completion_requests(&traces).len(), 3);
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 3);
    assert_stop_before_replacement_completion(&traces, 1);
    assert!(traces.iter().all(|trace| {
        !trace["uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("/tool_result"))
    }));
}

#[tokio::test]
async fn fresh_retry_receipt_does_not_survive_restart_and_retires_old_target() {
    let f = fixture(1).await;
    f.upstream.replies.lock().await.extend([
        Reply::ClientTool {
            input: json!({"value":"result can only retry in this process"}),
            final_text: "the old web stream must be stopped".to_owned(),
        },
        Reply::Http(500),
    ]);
    let mut request = synthetic_client_tool_request(json!({"type":"auto"}), false);
    let (handle, tool_call) =
        start_synthetic_client_tool(&f, request.clone(), "client-tool-fresh-retry-restart").await;
    append_client_tool_result(
        &mut request,
        &tool_call,
        json!("preserve this result across only in-process retries"),
        Some("Continue from this result."),
    );
    let original_session = f.engine.store.read().await.sessions[&handle].clone();
    let old_target = original_session.conversation_id.clone();
    let (status, _, failure) =
        call(&f.engine, "/v1/messages", request.clone(), Some(&handle)).await;
    assert_ne!(status, 200, "{failure}");
    {
        let state = f.engine.store.read().await;
        let waiting = &state.sessions[&handle];
        assert!(waiting.uncertain);
        assert!(!waiting.active);
        assert!(waiting.expires_at <= store::now() + 600);
    }

    f.engine.store.flush_status().await.unwrap();
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&f.engine.store.db)
        .await
        .unwrap();
    let restart_dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        f.engine.config.data_dir.join("control.sqlite3"),
        restart_dir.path().join("control.sqlite3"),
    )
    .unwrap();
    let mut restarted_config = f.engine.config.clone();
    restarted_config.data_dir = restart_dir.path().to_owned();
    let restarted = Engine::open(restarted_config).await.unwrap();
    let restarted_state = restarted.store.read().await;
    assert!(!restarted_state.sessions.contains_key(&handle));
    assert!(restarted_state.pending.iter().any(|job| {
        job.account_id == original_session.account_id && job.conversation_id == old_target
    }));
    drop(restarted_state);

    let traces_before_lost_retry = f.upstream.traces.lock().await.len();
    let (status, _, lost) = call(&restarted, "/v1/messages", request, Some(&handle)).await;
    assert_eq!(
        status, 409,
        "a process-local receipt must not survive restart: {lost}"
    );
    assert_eq!(
        f.upstream.traces.lock().await.len(),
        traces_before_lost_retry
    );
    assert_eq!(
        completion_requests(&f.upstream.traces.lock().await).len(),
        2
    );
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn expired_fresh_retry_session_is_retired_and_queued_without_generation() {
    let f = fixture(1).await;
    f.upstream.replies.lock().await.extend([
        Reply::ClientTool {
            input: json!({"value":"expire the retry session"}),
            final_text: "the original response is stopped".to_owned(),
        },
        Reply::Http(500),
    ]);
    let mut request = synthetic_client_tool_request(json!({"type":"auto"}), false);
    let (handle, tool_call) =
        start_synthetic_client_tool(&f, request.clone(), "client-tool-expired-fresh-retry").await;
    append_client_tool_result(
        &mut request,
        &tool_call,
        json!("the result must not trigger another generation"),
        Some("Continue with this result."),
    );
    let original_session = f.engine.store.read().await.sessions[&handle].clone();
    let old_target = original_session.conversation_id.clone();
    let (status, _, failure) =
        call(&f.engine, "/v1/messages", request.clone(), Some(&handle)).await;
    assert_ne!(
        status, 200,
        "the first fresh generation should fail: {failure}"
    );

    f.engine
        .store
        .mutate("", json!({}), |state| {
            state
                .sessions
                .get_mut(&handle)
                .expect("the session exists before its test expiry")
                .expires_at = store::now() - 1;
            Ok(())
        })
        .await
        .unwrap();
    reset_pacing(&f.engine).await;
    let (status, _, expired) = call(&f.engine, "/v1/messages", request, Some(&handle)).await;
    assert_eq!(
        status, 409,
        "expired continuation must fail closed: {expired}"
    );

    let state = f.engine.store.read().await;
    assert!(!state.sessions.contains_key(&handle));
    assert!(state.pending.iter().any(|job| {
        job.account_id == original_session.account_id && job.conversation_id == old_target
    }));
    drop(state);
    let traces = f.upstream.traces.lock().await;
    assert_eq!(completion_requests(&traces).len(), 2);
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn fresh_retry_receipt_is_removed_after_real_upstream_content() {
    let f = fixture(1).await;
    let raw = [
        event(json!({"type":"message_start"})),
        event(json!({
            "type":"content_block_start","index":0,
            "content_block":{"type":"text","text":""}
        })),
        event(json!({
            "type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":"partial generated answer"}
        })),
        event(json!({"type":"content_block_stop","index":0})),
        event(json!({
            "type":"error",
            "error":{"error_type":"overloaded_error","message":"fixture failure after content"}
        })),
    ]
    .concat();
    f.upstream.replies.lock().await.extend([
        Reply::ClientTool {
            input: json!({"value":"the client tool was already called"}),
            final_text: "the old stream is stopped first".to_owned(),
        },
        Reply::Raw(raw),
    ]);
    let mut request = synthetic_client_tool_request(json!({"type":"auto"}), false);
    let (handle, tool_call) =
        start_synthetic_client_tool(&f, request.clone(), "client-tool-no-retry-after-content")
            .await;
    let original_session = f.engine.store.read().await.sessions[&handle].clone();
    let old_target = original_session.conversation_id.clone();
    append_client_tool_result(
        &mut request,
        &tool_call,
        json!("real tool result"),
        Some("Append this real user follow-up."),
    );
    let (status, _, failure) =
        call(&f.engine, "/v1/messages", request.clone(), Some(&handle)).await;
    assert_ne!(status, 200, "{failure}");
    {
        let state = f.engine.store.read().await;
        assert!(
            !state.sessions.contains_key(&handle),
            "failure after generated content must retire the uncertain session"
        );
        assert!(
            state.pending.iter().any(|job| {
                job.account_id == original_session.account_id && job.conversation_id == old_target
            }),
            "retiring the uncertain session must queue the stopped old target for cleanup"
        );
    }
    let traces_before_retry = f.upstream.traces.lock().await.len();
    let (status, _, rejected) = call(&f.engine, "/v1/messages", request, Some(&handle)).await;
    assert_eq!(status, 409, "{rejected}");
    assert_eq!(f.upstream.traces.lock().await.len(), traces_before_retry);
    let traces = f.upstream.traces.lock().await;
    assert_eq!(completion_requests(&traces).len(), 2);
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 2);
    assert_stop_before_replacement_completion(&traces, 1);
    assert!(traces.iter().all(|trace| {
        !trace["uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("/tool_result"))
    }));
}

#[tokio::test]
async fn client_tool_choice_change_to_any_or_named_rebuilds_with_new_policy() {
    for choice in [
        json!({"type":"any"}),
        json!({"type":"tool","name":"synthetic_echo"}),
    ] {
        let f = fixture(1).await;
        f.upstream.replies.lock().await.extend([
            Reply::ClientTool {
                input: json!({"value":"first call"}),
                final_text: "first pending stream must not be resumed".to_owned(),
            },
            Reply::ClientTool {
                input: json!({"value":"second call"}),
                final_text: "second pending stream is unused".to_owned(),
            },
        ]);
        let mut request = synthetic_client_tool_request(json!({"type":"auto"}), false);
        let (status, headers, first) = call(
            &f.engine,
            "/v1/messages",
            request.clone(),
            Some("client-tool-choice-change"),
        )
        .await;
        assert_eq!(status, 200, "{first}");
        let handle = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
        let first_tool = first["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|block| block["type"] == "tool_use")
            .expect("first generation should request a client tool")
            .clone();
        request["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":[first_tool.clone()]}),
            json!({"role":"user","content":[{
                "type":"tool_result","tool_use_id":first_tool["id"],
                "content":"first call completed by the caller"
            }]}),
        ]);
        request["tool_choice"] = choice.clone();

        let (status, resumed_headers, response) =
            call(&f.engine, "/v1/messages", request, Some(&handle)).await;
        assert_eq!(status, 200, "{response}");
        assert_eq!(resumed_headers["x-c2a-session-id"], handle);
        assert_eq!(response["stop_reason"], "tool_use");
        assert!(
            response["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|block| { block["type"] == "tool_use" && block["name"] == "synthetic_echo" })
        );

        let traces = f.upstream.traces.lock().await;
        let completions = completion_requests(&traces);
        assert_eq!(completions.len(), 2);
        assert_fresh_stop_before_rebuild(&traces);
        assert_eq!(f.upstream.created.load(Ordering::SeqCst), 2);
        let prompt = completions[1]["body"]["prompt"].as_str().unwrap();
        let envelope = prompt
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|value| value.get("history").is_some())
            .expect("fresh completion should receive the full history envelope");
        assert_eq!(envelope["tool_policy"]["type"], choice["type"]);
        if choice["type"] == "tool" {
            assert_eq!(envelope["tool_policy"]["name"], "synthetic_echo");
        }
        assert_eq!(
            envelope["history"].as_array().unwrap().last().unwrap()["content"][0]["type"],
            "tool_result"
        );
        assert!(
            completions[1]["body"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"]
                    .as_str()
                    .is_some_and(|name| name.starts_with("c2a_client_")))
        );
        assert_eq!(
            traces
                .iter()
                .filter(|trace| trace["method"] == "POST"
                    && trace["uri"].as_str().unwrap().ends_with("/tool_result"))
                .count(),
            0
        );
    }
}

#[tokio::test]
async fn lost_client_tool_continuation_after_restart_fails_closed_and_shutdown_releases_account() {
    let f = fixture(1).await;
    f.upstream
        .replies
        .lock()
        .await
        .push_back(Reply::ClientTool {
            input: json!({"value":"synthetic marker"}),
            final_text: "unused after restart".to_owned(),
        });
    let mut request = base();
    request["tools"] = json!([{
        "name":"synthetic_echo",
        "description":"Echo a synthetic value.",
        "input_schema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}
    }]);
    let (status, headers, first) = call(
        &f.engine,
        "/v1/messages",
        request.clone(),
        Some("client-tool-restart"),
    )
    .await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["stop_reason"], "tool_use");
    let handle = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    let tool_call = first["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "tool_use")
        .unwrap()
        .clone();
    let original_session = f.engine.store.read().await.sessions[&handle].clone();
    assert!(f.engine.pool.busy(&original_session.account_id).await);

    request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":[tool_call.clone()]}),
        json!({"role":"user","content":[{
            "type":"tool_result","tool_use_id":tool_call["id"],"content":"synthetic result"
        }]}),
    ]);
    let restart_dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        f.engine.config.data_dir.join("control.sqlite3"),
        restart_dir.path().join("control.sqlite3"),
    )
    .unwrap();
    let mut restarted_config = f.engine.config.clone();
    restarted_config.data_dir = restart_dir.path().to_owned();
    let restarted = Engine::open(restarted_config).await.unwrap();
    let traces_before = f.upstream.traces.lock().await.len();
    let (status, _, lost) = call(&restarted, "/v1/messages", request, Some(&handle)).await;
    assert_eq!(
        status, 409,
        "a restart-lost tool result must fail closed: {lost}"
    );
    assert_eq!(f.upstream.traces.lock().await.len(), traces_before);
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 1);
    assert_eq!(
        completion_requests(&f.upstream.traces.lock().await).len(),
        1
    );

    f.engine.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let session_removed = !f.engine.store.read().await.sessions.contains_key(&handle);
            let account_released = !f.engine.pool.busy(&original_session.account_id).await;
            if session_removed && account_released {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown should discard the parked completion and release its account");
}

#[tokio::test]
async fn default_native_output_uses_standard_blocks_and_preserves_continuation_for_json_and_sse() {
    let mut final_texts = Vec::new();
    for stream in [false, true] {
        let f = fixture(1).await;
        f.upstream
            .replies
            .lock()
            .await
            .push_back(Reply::Raw(native_search_events()));
        let mut request = base();
        request
            .as_object_mut()
            .unwrap()
            .remove("claud2api_native_output");
        request["stream"] = json!(stream);
        let (status, headers, output) =
            call(&f.engine, "/v1/messages", request.clone(), None).await;
        assert_eq!(status, 200, "{output}");
        let content = if stream {
            let values = sse_values(output.as_str().unwrap());
            assert!(!output.as_str().unwrap().contains("claud2api_native"));
            let text = values
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|event| event.pointer("/delta/text").and_then(Value::as_str))
                .collect::<String>();
            json!([{"type":"text","text":text}])
        } else {
            assert!(
                output["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|block| block["type"] == "text")
            );
            output["content"].clone()
        };
        let text = content
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<String>();
        assert!(text.contains("web_search"));
        assert!(text.contains("https://example.com/source"));
        assert_eq!(text.matches("https://example.com/source").count(), 1);
        assert!(text.contains("甲乙丙"));
        final_texts.push(text);
        let id = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
        let logs = logs_for_request(&f.engine, headers["x-request-id"].to_str().unwrap()).await;
        let downstream = logs
            .iter()
            .find(|row| row["direction"] == "downstream")
            .unwrap();
        assert_eq!(
            downstream["native_tools"],
            json!([{"name":"web_search","status":"completed"}])
        );
        assert_eq!(downstream["native_tools_complete"], true);
        assert!(
            !downstream["native_tools"]
                .to_string()
                .contains("toolu-search-1")
        );
        request["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":content}),
            json!({"role":"user","content":"Repeat the source without another search."}),
        ]);
        reset_pacing(&f.engine).await;
        let (status, _, out) = call(&f.engine, "/v1/messages", request, Some(&id)).await;
        assert_eq!(status, 200, "{out}");
        assert_eq!(f.upstream.created.load(Ordering::SeqCst), 1);
    }
    assert_eq!(final_texts[0], final_texts[1]);
}

#[tokio::test]
async fn truncated_native_stream_retains_calls_but_marks_the_audit_incomplete() {
    let f = fixture(1).await;
    let events = [
        json!({"type":"message_start"}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"private-truncated-id","name":"web_search","input":{"query":"fixture"}}}),
        json!({"type":"content_block_stop","index":0})
    ].into_iter().map(event).collect::<String>();
    f.upstream
        .replies
        .lock()
        .await
        .push_back(Reply::Raw(events));
    let (status, headers, _) = call(&f.engine, "/v1/messages", base(), None).await;
    assert_eq!(status, 502);
    let logs = logs_for_request(&f.engine, headers["x-request-id"].to_str().unwrap()).await;
    let downstream = logs
        .iter()
        .find(|row| row["direction"] == "downstream")
        .unwrap();
    assert_eq!(downstream["native_tools_complete"], false);
    assert_eq!(
        downstream["native_tools"],
        json!([{"name":"web_search","status":"started"}])
    );
    assert!(!downstream.to_string().contains("private-truncated-id"));
}

#[tokio::test]
async fn standard_native_errors_are_visible_in_both_json_and_streamed_output() {
    let mut texts = Vec::new();
    for stream in [false, true] {
        let f = fixture(1).await;
        let events = [
            json!({"type":"message_start"}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"failed-native","name":"web_search","input":{"query":"fixture"}}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_result","tool_use_id":"failed-native","content":[{"type":"text","text":"Fixture search failed; no results returned."}],"is_error":true}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
            json!({"type":"message_stop"})
        ].into_iter().map(event).collect::<String>();
        f.upstream
            .replies
            .lock()
            .await
            .push_back(Reply::Raw(events));
        let mut request = base();
        request
            .as_object_mut()
            .unwrap()
            .remove("claud2api_native_output");
        request["stream"] = json!(stream);
        let (status, headers, out) = call(&f.engine, "/v1/messages", request, None).await;
        assert_eq!(status, 200, "{out}");
        let text = if stream {
            sse_values(out.as_str().unwrap())
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|value| value.pointer("/delta/text").and_then(Value::as_str))
                .collect::<String>()
        } else {
            out["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<String>()
        };
        assert!(text.contains("web_search; is_error=true"));
        assert!(text.contains("no results returned"));
        assert!(!text.contains("unknown"));
        let logs = logs_for_request(&f.engine, headers["x-request-id"].to_str().unwrap()).await;
        assert_eq!(
            logs.iter()
                .find(|row| row["direction"] == "downstream")
                .unwrap()["native_tools"],
            json!([{"name":"web_search","status":"error"}])
        );
        texts.push(text);
    }
    assert_eq!(texts[0], texts[1]);
}

#[tokio::test]
async fn tool_choice_none_removes_native_descriptors_before_upstream_generation() {
    let f = fixture(1).await;
    let mut request = base();
    request["tool_choice"] = json!({"type":"none"});
    let (status, headers, out) = call(&f.engine, "/v1/messages", request, None).await;
    assert_eq!(status, 200, "{out}");
    let traces = f.upstream.traces.lock().await;
    let calls = completion_requests(&traces);
    assert_eq!(calls[0]["body"]["tools"], json!([]));
    assert!(
        !calls[0]["body"]["prompt"]
            .as_str()
            .unwrap()
            .contains("\"native_tools\"")
    );
    drop(traces);
    let logs = logs_for_request(&f.engine, headers["x-request-id"].to_str().unwrap()).await;
    assert_eq!(
        logs.iter()
            .find(|row| row["direction"] == "downstream")
            .unwrap()["native_tools"],
        json!([])
    );
}

#[tokio::test]
async fn messages_native_blocks_preserve_lifecycle_and_stream_the_same_final_blocks() {
    let json_fixture = fixture(1).await;
    json_fixture
        .upstream
        .replies
        .lock()
        .await
        .push_back(Reply::Raw(native_search_events()));
    let (status, headers, body) = call(
        &json_fixture.engine,
        "/v1/messages",
        base(),
        Some("native-json"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["content"][0]["type"], "claud2api_native");
    assert_eq!(body["content"][0]["block"]["type"], "tool_use");
    assert_eq!(
        body["content"][0]["block"]["input"],
        json!({"query":"example"})
    );
    assert_eq!(body["content"][0]["block"]["message"], "Searching");
    assert_eq!(body["content"][1]["block"]["type"], "tool_result");
    assert_eq!(
        body["content"][1]["block"]["content"][0]["type"],
        "knowledge"
    );
    assert_eq!(body["content"][2]["type"], "text");
    assert_eq!(body["content"][2]["text"], "甲乙丙");
    assert_eq!(body["content"][3]["type"], "claud2api_native");
    assert_eq!(body["content"][3]["block"]["type"], "citation");
    assert_eq!(body["content"][3]["block"]["citation"]["start_index"], 1);
    assert_eq!(body["content"][3]["block"]["citation"]["end_index"], 3);
    assert_eq!(
        body["content"][3]["block"]["citation"]["cited_text"],
        "乙丙"
    );
    assert_eq!(body["content"][4]["type"], "text");
    assert_eq!(body["content"][4]["text"], "丁结论。");
    assert!(
        body["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|block| { !matches!(block["type"].as_str(), Some("tool_use" | "tool_result")) })
    );
    assert!(!body.to_string().contains("synthetic-control-value"));
    let traces = json_fixture.upstream.traces.lock().await;
    let upstream_calls = completion_requests(&traces);
    assert_eq!(
        upstream_calls[0]["body"]["tools"],
        default_native_descriptors()
    );
    let prompt = upstream_calls[0]["body"]["prompt"].as_str().unwrap();
    let envelope: Value = serde_json::from_str(prompt.lines().last().unwrap()).unwrap();
    assert_eq!(envelope["client_tools"], json!([]));
    assert_eq!(
        envelope["native_tools"],
        json!(["web_search", "artifacts", "repl", "widgets"])
    );
    drop(traces);
    let session_id = headers["x-c2a-session-id"].to_str().unwrap();
    let session = json_fixture
        .engine
        .store
        .read()
        .await
        .sessions
        .get(session_id)
        .unwrap()
        .clone();
    assert_eq!(session.history.len(), 2);

    let stream_fixture = fixture(1).await;
    stream_fixture
        .upstream
        .replies
        .lock()
        .await
        .push_back(Reply::Raw(native_search_events()));
    let mut request = base();
    request["stream"] = json!(true);
    request["claud2api_tools"] = json!(["web_search"]);
    let (status, _, wire) = call(
        &stream_fixture.engine,
        "/v1/messages",
        request,
        Some("native-stream"),
    )
    .await;
    assert_eq!(status, 200);
    let events = sse_values(wire.as_str().unwrap());
    let events = events.as_array().unwrap();
    let tool_start = events
        .iter()
        .find(|event| {
            event["type"] == "content_block_start"
                && event["content_block"]["type"] == "claud2api_native"
                && event["content_block"]["block"]["type"] == "tool_use"
        })
        .unwrap();
    assert_eq!(tool_start["content_block"]["block"]["input"], json!({}));
    assert!(events.iter().any(|event| {
        event["type"] == "content_block_delta"
            && event["delta"]["type"] == "claud2api_native_delta"
            && event["delta"]["delta"]["type"] == "input_json_delta"
    }));
    let tool_stop = events
        .iter()
        .find(|event| {
            event["type"] == "content_block_stop"
                && event["claud2api_block"]["block"]["type"] == "tool_use"
        })
        .unwrap();
    assert_eq!(tool_stop["claud2api_block"], body["content"][0]);
    let result_stop = events
        .iter()
        .find(|event| {
            event["type"] == "content_block_stop"
                && event["claud2api_block"]["block"]["type"] == "tool_result"
        })
        .unwrap();
    assert_eq!(result_stop["claud2api_block"], body["content"][1]);
    let text_block_indexes: Vec<_> = events
        .iter()
        .filter(|event| {
            event["type"] == "content_block_start" && event["content_block"]["type"] == "text"
        })
        .map(|event| event["index"].as_u64().unwrap())
        .collect();
    let text_deltas = events
        .iter()
        .filter(|event| {
            event["type"] == "content_block_delta" && event["delta"]["type"] == "text_delta"
        })
        .filter_map(|event| event["delta"]["text"].as_str())
        .collect::<String>();
    assert_eq!(text_block_indexes, vec![2, 4]);
    assert_eq!(text_deltas, "甲乙丙丁结论。");
    assert!(events.iter().any(|event| {
        event["type"] == "content_block_start"
            && event["content_block"]["block"]["type"] == "citation"
    }));
}

#[tokio::test]
async fn compat_adapters_return_native_extensions_and_stream_extension_events() {
    for responses in [false, true] {
        for stream in [false, true] {
            let f = fixture(1).await;
            f.upstream
                .replies
                .lock()
                .await
                .push_back(Reply::Raw(native_search_events()));
            let path = if responses {
                "/v1/responses"
            } else {
                "/v1/chat/completions"
            };
            let mut request =
                json!({"model":"test","stream":stream,"claud2api_tools":["web_search"]});
            if responses {
                request["input"] = json!("请用搜索验证这个来源。");
            } else {
                request["messages"] = json!([{"role":"user","content":"请用搜索验证这个来源。"}]);
            }
            let (status, _, output) = call(&f.engine, path, request, None).await;
            assert_eq!(status, 200, "{path} stream={stream}: {output}");
            let traces = f.upstream.traces.lock().await;
            assert_eq!(
                completion_requests(&traces)[0]["body"]["tools"],
                single_native_search_descriptor()
            );
            drop(traces);

            if !stream {
                assert_eq!(output["claud2api_native"].as_array().unwrap().len(), 2);
                assert_eq!(output["claud2api_native"][0]["type"], "tool_use");
                assert_eq!(output["claud2api_native"][1]["type"], "tool_result");
                assert_eq!(output["claud2api_citations"][0]["cited_text"], "乙丙");
                if responses {
                    assert_eq!(output["output"][0]["content"][0]["text"], "甲乙丙丁结论。");
                    assert_eq!(
                        output["output"][0]["content"][0]["annotations"][0]["start_index"],
                        1
                    );
                    assert_eq!(
                        output["output"][0]["content"][0]["annotations"][0]["end_index"],
                        3
                    );
                } else {
                    assert_eq!(output["choices"][0]["message"]["content"], "甲乙丙丁结论。");
                }
            } else {
                let wire = output.as_str().unwrap();
                assert!(wire.contains("event: claud2api.native_start"));
                assert!(wire.contains("event: claud2api.native_delta"));
                assert!(wire.contains("event: claud2api.native_stop"));
                assert!(wire.contains("event: claud2api.citation"));
                let events = sse_values(wire);
                let events = events.as_array().unwrap();
                if responses {
                    assert!(
                        events
                            .iter()
                            .any(|event| event["type"] == "claud2api.native_start")
                    );
                    let completed = events
                        .iter()
                        .find(|event| event["type"] == "response.completed")
                        .unwrap();
                    assert_eq!(
                        completed["response"]["claud2api_native"]
                            .as_array()
                            .unwrap()
                            .len(),
                        2
                    );
                    assert_eq!(
                        completed["response"]["claud2api_citations"][0]["cited_text"],
                        "乙丙"
                    );
                    assert_eq!(
                        completed["response"]["output"][0]["content"][0]["annotations"][0]["url"],
                        "https://example.com/source"
                    );
                } else {
                    let final_chunk = events
                        .iter()
                        .find(|event| event["choices"][0]["finish_reason"] == "stop")
                        .unwrap();
                    assert_eq!(final_chunk["claud2api_native"].as_array().unwrap().len(), 2);
                    assert_eq!(final_chunk["claud2api_citations"][0]["cited_text"], "乙丙");
                }
            }
        }
    }
}

#[tokio::test]
async fn native_blocks_roundtrip_in_messages_history_without_recreating_conversation() {
    let f = fixture(1).await;
    f.upstream.replies.lock().await.extend([
        Reply::Raw(native_search_events()),
        Reply::Text("continued".to_owned()),
    ]);
    let (status, headers, first) =
        call(&f.engine, "/v1/messages", base(), Some("native-history")).await;
    assert_eq!(status, 200, "{first}");
    let session_id = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    reset_pacing(&f.engine).await;
    let mut next = base();
    next["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":first["content"]}),
        json!({"role":"user","content":"continue after the native search"}),
    ]);
    let (status, response_headers, second) =
        call(&f.engine, "/v1/messages", next.clone(), Some(&session_id)).await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(response_headers["x-c2a-session-id"], session_id);
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 1);
    next["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role":"assistant","content":second["content"]}));
    next["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role":"user","content":"history hash sentinel"}));
    let expected_history = protocol::parse(&next)
        .unwrap()
        .hashes(&f.engine.config.gateway_key);
    let state = f.engine.store.read().await;
    let session = state.sessions.get(&session_id).unwrap();
    assert_eq!(
        session.history,
        expected_history[..expected_history.len() - 1]
    );
    assert_eq!(session.history.len(), 4);
    drop(state);
    let traces = f.upstream.traces.lock().await;
    let calls = completion_requests(&traces);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["uri"], calls[1]["uri"]);
    assert_eq!(
        calls[1]["body"]["parent_message_uuid"],
        calls[0]["body"]["turn_message_uuids"]["assistant_message_uuid"]
    );
    assert_eq!(calls[0]["body"]["tools"], default_native_descriptors());
    assert_eq!(calls[1]["body"]["tools"], default_native_descriptors());
    assert!(
        calls[1]["body"]["prompt"]
            .as_str()
            .unwrap()
            .contains("continue after the native search")
    );
    assert!(
        !calls[1]["body"]["prompt"]
            .as_str()
            .unwrap()
            .contains("toolu-search-1")
    );
}

#[tokio::test]
async fn explicit_project_is_bound_before_completion_and_part_of_session_context() {
    let f = fixture(1).await;
    f.upstream.replies.lock().await.extend([
        Reply::Text("first project answer".to_owned()),
        Reply::Text("same project continuation".to_owned()),
        Reply::Text("changed project answer".to_owned()),
    ]);
    let project_a = "11111111-1111-4111-8111-111111111111";
    let project_b = "22222222-2222-4222-8222-222222222222";
    let mut first_request = base();
    first_request["claud2api_project_uuid"] = json!(project_a);
    let (status, headers, first) = call(
        &f.engine,
        "/v1/messages",
        first_request,
        Some("project-context"),
    )
    .await;
    assert_eq!(status, 200, "{first}");
    let handle = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    let first_context = f.engine.store.read().await.sessions[&handle]
        .context_hash
        .clone();

    reset_pacing(&f.engine).await;
    let mut same_project = base();
    same_project["claud2api_project_uuid"] = json!(project_a);
    same_project["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":first["content"]}),
        json!({"role":"user","content":"continue with this project"}),
    ]);
    let (status, _, second) = call(
        &f.engine,
        "/v1/messages",
        same_project.clone(),
        Some(&handle),
    )
    .await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.engine.store.read().await.sessions[&handle].context_hash,
        first_context
    );

    reset_pacing(&f.engine).await;
    same_project["claud2api_project_uuid"] = json!(project_b);
    same_project["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":second["content"]}),
        json!({"role":"user","content":"switch the project"}),
    ]);
    let (status, _, third) = call(&f.engine, "/v1/messages", same_project, Some(&handle)).await;
    assert_eq!(status, 200, "{third}");
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 2);
    assert_ne!(
        f.engine.store.read().await.sessions[&handle].context_hash,
        first_context
    );

    let traces = f.upstream.traces.lock().await;
    let bindings: Vec<_> = traces
        .iter()
        .enumerate()
        .filter(|(_, trace)| {
            trace["method"] == "PUT"
                && trace["uri"].as_str().is_some_and(|uri| {
                    uri.contains("/chat_conversations/") && uri.contains("rendering_mode=raw")
                })
        })
        .collect();
    let calls: Vec<_> = completion_requests(&traces);
    assert_eq!(bindings.len(), 2);
    assert_eq!(calls.len(), 3);
    assert_eq!(bindings[0].1["body"]["project_uuid"], project_a);
    assert_eq!(bindings[1].1["body"]["project_uuid"], project_b);
    for (binding, completion_index) in bindings.iter().zip([0, 2]) {
        assert!(
            binding.0
                < traces
                    .iter()
                    .position(|trace| trace["uri"] == calls[completion_index]["uri"])
                    .unwrap()
        );
        let bound_conversation = binding.1["uri"]
            .as_str()
            .unwrap()
            .split('?')
            .next()
            .unwrap();
        let completed_conversation = calls[completion_index]["uri"]
            .as_str()
            .unwrap()
            .strip_suffix("/completion")
            .unwrap();
        assert_eq!(bound_conversation, completed_conversation);
    }
    assert_eq!(calls[0]["body"]["tools"], project_native_descriptors());
    assert_eq!(calls[1]["body"]["tools"], project_native_descriptors());
    assert_eq!(calls[2]["body"]["tools"], project_native_descriptors());
    let first_prompt = calls[0]["body"]["prompt"].as_str().unwrap();
    let first_envelope: Value = serde_json::from_str(first_prompt.lines().last().unwrap()).unwrap();
    assert_eq!(first_envelope["project_uuid"], project_a);
    assert_eq!(
        first_envelope["native_tools"],
        json!([
            "web_search",
            "artifacts",
            "repl",
            "widgets",
            "project_knowledge"
        ])
    );
    assert_eq!(calls[0]["uri"], calls[1]["uri"]);
    assert_ne!(calls[1]["uri"], calls[2]["uri"]);
}

#[tokio::test]
async fn native_tool_errors_after_generation_do_not_commit_or_retry() {
    let cases = [
        (
            "malformed_input",
            vec![
                json!({"type":"message_start"}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu-bad-json","name":"web_search","input":{}}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"not json"}}),
                json!({"type":"content_block_stop","index":0}),
            ],
            json!(["web_search"]),
        ),
        (
            "unallowed_tool",
            vec![
                json!({"type":"message_start"}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu-disabled","name":"repl","input":{}}}),
            ],
            json!(["web_search"]),
        ),
        (
            "truncated_tool",
            vec![
                json!({"type":"message_start"}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu-truncated","name":"web_search","input":{}}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"example\"}"}}),
            ],
            json!(["web_search"]),
        ),
    ];
    for (case, events, groups) in cases {
        let f = fixture(2).await;
        let wire = events.into_iter().map(event).collect::<Vec<_>>().concat();
        f.upstream.replies.lock().await.extend([
            Reply::Raw(wire),
            Reply::Text("must not be replayed".to_owned()),
        ]);
        let mut request = base();
        request["claud2api_tools"] = groups;
        let (status, headers, body) = call(&f.engine, "/v1/messages", request, Some(case)).await;
        assert_eq!(status, 502, "{case}: {body}");
        assert_eq!(f.upstream.created.load(Ordering::SeqCst), 1, "{case}");
        assert_eq!(f.upstream.replies.lock().await.len(), 1, "{case}");
        let state = f.engine.store.read().await;
        assert!(
            state
                .sessions
                .values()
                .all(|session| session.history.is_empty() && !session.active),
            "{case}"
        );
        assert!(
            state
                .pending
                .iter()
                .any(|item| !item.conversation_id.is_empty()),
            "{case}"
        );
        assert!(headers.get("x-c2a-session-id").is_none(), "{case}");
    }
}

#[tokio::test]
async fn native_file_links_are_token_scoped_and_return_sandboxed_attachments() {
    let f = fixture(1).await;
    let (_, owner_key) = managed_token(&f.engine, 3).await;
    let (_, other_key) = managed_token(&f.engine, 3).await;
    f.upstream
        .replies
        .lock()
        .await
        .push_back(Reply::Raw(native_file_events()));
    let mut request = base();
    request["claud2api_tools"] = json!(["repl"]);
    let (status, _, output) = call_key(&f.engine, "/v1/messages", request, None, &owner_key).await;
    assert_eq!(status, 200, "{output}");
    let file_result = output["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| {
            block["type"] == "claud2api_native" && block["block"]["type"] == "tool_result"
        })
        .unwrap();
    let file = &file_result["block"]["content"][0];
    let download_url = file["claud2api_download_url"].as_str().unwrap();
    assert!(download_url.starts_with("/v1/native-files/v1."));
    let download_path = download_url.split('?').next().unwrap();

    reset_pacing(&f.engine).await;
    let downloaded = public_raw(&f.engine, "GET", download_path, &owner_key, Value::Null).await;
    assert_eq!(downloaded.status(), 200);
    assert_eq!(
        downloaded.headers()["content-type"],
        "application/octet-stream"
    );
    assert_eq!(
        downloaded.headers()["content-security-policy"],
        "sandbox; default-src 'none'"
    );
    assert_eq!(downloaded.headers()["x-content-type-options"], "nosniff");
    assert!(
        downloaded.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .contains("attachment;")
    );
    assert_eq!(
        downloaded.into_body().collect().await.unwrap().to_bytes(),
        Bytes::from_static(b"name,value\nexample,42\n")
    );

    let requests_before_rejection = f.upstream.traces.lock().await.len();
    let rejected = public_raw(&f.engine, "GET", download_path, &other_key, Value::Null).await;
    assert_eq!(rejected.status(), 403);
    assert_eq!(
        f.upstream.traces.lock().await.len(),
        requests_before_rejection
    );

    let anonymous = web::public(f.engine.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(download_path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);
    assert_eq!(
        f.upstream.traces.lock().await.len(),
        requests_before_rejection
    );

    let mut tampered_path = download_path.to_owned();
    let last = tampered_path.pop().unwrap();
    tampered_path.push(if last == '0' { '1' } else { '0' });
    let tampered = public_raw(&f.engine, "GET", &tampered_path, &owner_key, Value::Null).await;
    assert_eq!(tampered.status(), 404);
    assert_eq!(
        f.upstream.traces.lock().await.len(),
        requests_before_rejection
    );
}

#[tokio::test]
async fn transient_compat_file_output_defers_cleanup_until_its_link_expires() {
    let f = fixture(1).await;
    f.upstream
        .replies
        .lock()
        .await
        .push_back(Reply::Raw(native_file_events()));
    let request = json!({
        "model":"test",
        "claud2api_tools":["repl"],
        "messages":[{"role":"user","content":"prepare a CSV file"}]
    });
    let (status, _, output) = call(&f.engine, "/v1/chat/completions", request, None).await;
    assert_eq!(status, 200, "{output}");
    assert_eq!(output["choices"][0]["message"]["content"], "");
    let file = output["claud2api_native"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "tool_result")
        .unwrap()["content"][0]
        .clone();
    assert!(file["claud2api_download_url"].as_str().is_some());
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 1);
    assert_eq!(f.upstream.deleted.load(Ordering::SeqCst), 0);

    let state = f.engine.store.read().await;
    assert_eq!(state.pending.len(), 1);
    let pending = state.pending[0].clone();
    assert!(pending.next_attempt >= store::now() + 3500);
    drop(state);

    f.engine.sweep().await.unwrap();
    assert_eq!(f.upstream.deleted.load(Ordering::SeqCst), 0);
    assert!(f.engine.store.read().await.pending.contains(&pending));
}

#[tokio::test]
async fn messages_file_cleanup_waits_after_a_context_rebuild() {
    let f = fixture(1).await;
    f.upstream.replies.lock().await.extend([
        Reply::Raw(native_file_events()),
        Reply::Text("new context response".to_owned()),
    ]);
    let mut first_request = base();
    first_request["claud2api_tools"] = json!(["repl"]);
    let (status, headers, first) = call(
        &f.engine,
        "/v1/messages",
        first_request,
        Some("messages-file-context"),
    )
    .await;
    assert_eq!(status, 200, "{first}");
    assert!(first["content"].as_array().unwrap().iter().any(|block| {
        block["type"] == "claud2api_native"
            && block["block"]["type"] == "tool_result"
            && block["block"]["content"][0]["claud2api_download_url"]
                .as_str()
                .is_some()
    }));
    let handle = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    let old_conversation = f.engine.store.read().await.sessions[&handle]
        .conversation_id
        .clone();
    let old_pending = f
        .engine
        .store
        .read()
        .await
        .pending
        .iter()
        .find(|item| item.conversation_id == old_conversation)
        .cloned()
        .expect("file conversation cleanup job");
    assert!(old_pending.next_attempt >= store::now() + 3500);

    reset_pacing(&f.engine).await;
    let mut rebuilt = base();
    rebuilt["system"] = json!("A changed system context.");
    rebuilt["claud2api_tools"] = json!(["repl"]);
    rebuilt["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":first["content"]}),
        json!({"role":"user","content":"continue in the updated context"}),
    ]);
    let (status, _, second) = call(&f.engine, "/v1/messages", rebuilt, Some(&handle)).await;
    assert_eq!(status, 200, "{second}");
    let state = f.engine.store.read().await;
    assert_ne!(state.sessions[&handle].conversation_id, old_conversation);
    assert!(state.pending.contains(&old_pending));
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 2);
    drop(state);

    f.engine.sweep().await.unwrap();
    assert_eq!(f.upstream.deleted.load(Ordering::SeqCst), 0);
    assert!(f.engine.store.read().await.pending.contains(&old_pending));
}

#[tokio::test]
async fn live_legacy_file_cleanup_does_not_starve_later_due_cleanup_jobs() {
    let f = fixture(1).await;
    f.upstream
        .replies
        .lock()
        .await
        .push_back(Reply::Raw(native_file_events()));
    let request = json!({
        "model":"test",
        "conversation_id":"legacy-file-with-download",
        "claud2api_tools":["repl"],
        "messages":[{"role":"user","content":"create a linked file"}]
    });
    let (status, _, output) = call(&f.engine, "/v1/chat/completions", request, None).await;
    assert_eq!(status, 200, "{output}");
    let live_target = f
        .engine
        .legacy
        .lock()
        .await
        .values()
        .next()
        .unwrap()
        .1
        .clone();
    let account = f.engine.config.accounts[0].id();
    let other_target = "33333333-3333-4333-8333-333333333333";
    f.engine
        .store
        .mutate("", json!({}), |state| {
            let live = state
                .pending
                .iter_mut()
                .find(|job| job.conversation_id == live_target)
                .unwrap();
            live.next_attempt = 0;
            state.queue(&account, other_target);
            Ok(())
        })
        .await
        .unwrap();

    reset_pacing(&f.engine).await;
    f.engine.sweep().await.unwrap();
    assert_eq!(f.upstream.deleted.load(Ordering::SeqCst), 1);
    let pending = f.engine.store.read().await.pending.clone();
    assert!(pending.iter().any(|job| job.conversation_id == live_target));
    assert!(
        !pending
            .iter()
            .any(|job| job.conversation_id == other_target)
    );
    let deletes: Vec<_> = f
        .upstream
        .traces
        .lock()
        .await
        .iter()
        .filter(|trace| trace["method"] == "DELETE")
        .map(|trace| trace["uri"].clone())
        .collect();
    assert_eq!(deletes.len(), 1);
    assert!(deletes[0].as_str().unwrap().contains(other_target));
    assert!(!deletes[0].as_str().unwrap().contains(&live_target));
}

#[tokio::test]
async fn legacy_conversation_id_rejects_a_native_profile_change_before_upstream() {
    let f = fixture(1).await;
    f.upstream.replies.lock().await.extend([
        Reply::Text("first profile".to_owned()),
        Reply::Text("continued".to_owned()),
    ]);
    let mut first = json!({"model":"test","conversation_id":"native-profile","messages":[{"role":"user","content":"first"}]});
    assert_eq!(
        call(&f.engine, "/v1/chat/completions", first.clone(), None)
            .await
            .0,
        200
    );
    reset_pacing(&f.engine).await;
    first["claud2api_tools"] = json!(["web_search"]);
    let before = f.upstream.created.load(Ordering::SeqCst);
    let (status, _, error) = call(&f.engine, "/v1/chat/completions", first.clone(), None).await;
    assert_eq!(status, 409, "{error}");
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), before);
    assert_eq!(
        completion_requests(&f.upstream.traces.lock().await).len(),
        1
    );
    reset_pacing(&f.engine).await;
    first.as_object_mut().unwrap().remove("claud2api_tools");
    first["messages"] = json!([{"role":"user","content":"continue original profile"}]);
    let (status, _, output) = call(&f.engine, "/v1/chat/completions", first, None).await;
    assert_eq!(status, 200, "{output}");
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), before);
    let calls = completion_requests(&f.upstream.traces.lock().await);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["uri"], calls[1]["uri"]);
}

#[tokio::test]
async fn compat_project_profile_is_sticky_and_rejects_project_switches() {
    let project_a = "11111111-1111-4111-8111-111111111111";
    let project_b = "22222222-2222-4222-8222-222222222222";
    for responses in [false, true] {
        let f = fixture(1).await;
        f.upstream.replies.lock().await.extend([
            Reply::Text("first project response".to_owned()),
            Reply::Text("continued project response".to_owned()),
        ]);
        let path = if responses {
            "/v1/responses"
        } else {
            "/v1/chat/completions"
        };
        let mut request = json!({"model":"test","conversation_id":"compat-project"});
        request["claud2api_project_uuid"] = json!(project_a);
        if responses {
            request["input"] = json!("first project prompt");
        } else {
            request["messages"] = json!([{"role":"user","content":"first project prompt"}]);
        }
        let (status, _, first) = call(&f.engine, path, request.clone(), None).await;
        assert_eq!(status, 200, "{path}: {first}");

        reset_pacing(&f.engine).await;
        if responses {
            request["input"] = json!("same project continuation");
        } else {
            request["messages"] = json!([{"role":"user","content":"same project continuation"}]);
        }
        let (status, _, second) = call(&f.engine, path, request.clone(), None).await;
        assert_eq!(status, 200, "{path}: {second}");

        reset_pacing(&f.engine).await;
        request["claud2api_project_uuid"] = json!(project_b);
        if responses {
            request["input"] = json!("attempt project switch");
        } else {
            request["messages"] = json!([{"role":"user","content":"attempt project switch"}]);
        }
        let (status, _, rejected) = call(&f.engine, path, request, None).await;
        assert_eq!(status, 409, "{path}: {rejected}");

        let traces = f.upstream.traces.lock().await;
        let calls = completion_requests(&traces);
        assert_eq!(calls.len(), 2, "{path}");
        assert_eq!(calls[0]["uri"], calls[1]["uri"]);
        assert_eq!(
            calls[1]["body"]["parent_message_uuid"],
            calls[0]["body"]["turn_message_uuids"]["assistant_message_uuid"]
        );
        assert_eq!(calls[0]["body"]["tools"], project_native_descriptors());
        assert_eq!(calls[1]["body"]["tools"], project_native_descriptors());
        assert_eq!(f.upstream.created.load(Ordering::SeqCst), 1, "{path}");
        let bindings: Vec<_> = traces
            .iter()
            .filter(|trace| trace["method"] == "PUT")
            .collect();
        assert_eq!(bindings.len(), 1, "{path}");
        assert_eq!(bindings[0]["body"]["project_uuid"], project_a);
    }
}

#[tokio::test]
async fn text_adapters_validate_success_failure_usage_and_upstream_tool_isolation() {
    for responses in [false, true] {
        for stream in [false, true] {
            for case in ["text", "empty", "quota", "partial_error", "truncated"] {
                let f = fixture_with(1, true).await;
                let text = "Unicode answer: \u{4f60}\u{597d} \u{1f642}\n";
                let reply = match case {
                    "text" => Reply::Text(text.to_owned()),
                    "empty" => Reply::Text(String::new()),
                    "quota" => Reply::HttpRetry(429, "9".to_owned()),
                    "partial_error" => Reply::PartialQuota,
                    _ => Reply::Truncated,
                };
                f.upstream.replies.lock().await.push_back(reply);
                let payload = if responses {
                    json!({"model":"test","stream":stream,"instructions":"Keep a literal quote: \".","input":[{"role":"developer","content":"Use brief answers."},{"role":"assistant","content":"Earlier answer."},{"role":"user","content":[{"type":"input_text","text":"Continue this record."}]}]})
                } else {
                    json!({"model":"test","stream":stream,"stream_options":{"include_usage":true},"messages":[{"role":"system","content":"Use brief answers."},{"role":"assistant","content":"Earlier answer."},{"role":"user","content":"Continue this record."}]})
                };
                let (status, headers, output) = call(
                    &f.engine,
                    if responses {
                        "/v1/responses"
                    } else {
                        "/v1/chat/completions"
                    },
                    payload,
                    None,
                )
                .await;
                let success = matches!(case, "text" | "empty");
                let traces = f.upstream.traces.lock().await;
                let request = traces
                    .iter()
                    .find(|v| v["uri"].as_str().unwrap().ends_with("/completion"))
                    .unwrap();
                assert_eq!(request["body"]["tools"], default_native_descriptors());
                assert!(request["body"].get("create_conversation_params").is_none());
                let prompt = request["body"]["prompt"].as_str().unwrap();
                let transcript: Value =
                    serde_json::from_str(prompt.lines().last().unwrap()).unwrap();
                assert!(transcript.to_string().contains("Earlier answer."));
                assert!(transcript.to_string().contains("Continue this record."));
                let expected_input = protocol::estimate(prompt);
                assert!(expected_input > 0);
                drop(traces);
                if !stream {
                    if !success {
                        assert!(status >= 400, "{responses}/{case}: {output}");
                        assert!(output["error"].is_object());
                        continue;
                    }
                    assert_eq!(status, 200, "{output}");
                    let answer = if responses {
                        &output["output"][0]["content"][0]["text"]
                    } else {
                        &output["choices"][0]["message"]["content"]
                    };
                    assert_eq!(answer, if case == "text" { text } else { "" });
                    let usage = &output["usage"];
                    let input = if responses {
                        "input_tokens"
                    } else {
                        "prompt_tokens"
                    };
                    let completion = if responses {
                        "output_tokens"
                    } else {
                        "completion_tokens"
                    };
                    assert_eq!(usage[input], expected_input);
                    assert_eq!(
                        usage[completion],
                        protocol::estimate(answer.as_str().unwrap())
                    );
                    assert_eq!(
                        usage["total_tokens"].as_u64().unwrap(),
                        usage[input].as_u64().unwrap() + usage[completion].as_u64().unwrap()
                    );
                } else {
                    assert_eq!(status, 200);
                    assert_eq!(headers["content-type"], "text/event-stream");
                    let wire = output.as_str().unwrap();
                    let events = sse_values(wire);
                    let events = events.as_array().unwrap();
                    if responses {
                        let terminal = events.last().unwrap();
                        assert_eq!(
                            terminal["type"],
                            if success {
                                "response.completed"
                            } else {
                                "response.failed"
                            }
                        );
                        for (index, event) in events.iter().enumerate() {
                            assert_eq!(event["sequence_number"].as_u64().unwrap(), index as u64);
                        }
                        if success {
                            assert!(
                                events
                                    .iter()
                                    .any(|event| event["type"] == "response.output_text.done")
                            );
                            assert_eq!(
                                terminal["response"]["usage"]["input_tokens"],
                                expected_input
                            );
                        } else {
                            assert!(terminal["response"]["error"].is_object());
                            assert!(
                                !events
                                    .iter()
                                    .any(|event| event["type"] == "response.completed")
                            );
                        }
                    } else if success {
                        assert_eq!(events.last().unwrap(), "[DONE]");
                        let usage_event = &events[events.len() - 2];
                        assert_eq!(usage_event["choices"], json!([]));
                        assert_eq!(usage_event["usage"]["prompt_tokens"], expected_input);
                    } else {
                        assert!(events.last().unwrap()["error"].is_object());
                        assert!(
                            !events
                                .iter()
                                .any(|event| event["choices"][0]["finish_reason"] == "stop")
                        );
                        assert!(!wire.contains("[DONE]"));
                    }
                    assert!(!wire.contains("[error:"));
                    assert!(!wire.contains("private detail"));
                }
            }
        }
    }
}
#[tokio::test]
async fn text_adapter_conversation_extension_appends_incremental_messages() {
    for responses in [false, true] {
        let f = fixture(1).await;
        let path = if responses {
            "/v1/responses"
        } else {
            "/v1/chat/completions"
        };
        for text in ["First incremental record.", "Second incremental record."] {
            reset_pacing(&f.engine).await;
            let mut payload = json!({"model":"test","conversation_id":"incremental-record"});
            if responses {
                payload["input"] = json!(text);
            } else {
                payload["messages"] = json!([{"role":"user","content":text}]);
            }
            let (status, _, body) = call(&f.engine, path, payload, None).await;
            assert_eq!(status, 200, "{body}");
        }
        assert_eq!(f.upstream.created.load(Ordering::SeqCst), 1);
        let traces = f.upstream.traces.lock().await;
        let calls: Vec<_> = traces
            .iter()
            .filter(|trace| trace["uri"].as_str().unwrap().ends_with("/completion"))
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["uri"], calls[1]["uri"]);
        assert_eq!(
            calls[1]["body"]["parent_message_uuid"],
            calls[0]["body"]["turn_message_uuids"]["assistant_message_uuid"]
        );
        assert!(
            !calls[1]["body"]["prompt"]
                .as_str()
                .unwrap()
                .contains("First incremental record.")
        );
        assert!(
            calls[1]["body"]["prompt"]
                .as_str()
                .unwrap()
                .contains("Second incremental record.")
        );
    }
}

#[tokio::test]
async fn text_adapters_reject_unsupported_inputs_without_contacting_upstream() {
    let f = fixture(1).await;
    for (path, payload) in [
        (
            "/v1/chat/completions",
            json!({"model":"test","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"private-image"}}]}]}),
        ),
        (
            "/v1/chat/completions",
            json!({"model":"test","stream":"yes","messages":[{"role":"user","content":"hello"}]}),
        ),
        (
            "/v1/chat/completions",
            json!({"model":"test","tools":[{"type":"function","function":{"name":"read"}}],"messages":[{"role":"user","content":"hello"}]}),
        ),
        (
            "/v1/responses",
            json!({"model":"test","input":[{"type":"function_call","name":"read","arguments":"{}"}]}),
        ),
        (
            "/v1/responses",
            json!({"model":"test","input":[{"type":42,"role":"user","content":"hello"}]}),
        ),
        (
            "/v1/responses",
            json!({"model":"test","instructions":false,"input":"hello"}),
        ),
    ] {
        let (status, _, output) = call(&f.engine, path, payload, None).await;
        assert_eq!(status, 400, "{path}: {output}");
        assert!(!output.to_string().contains("private-image"));
    }
    assert!(f.upstream.traces.lock().await.is_empty());
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn text_adapter_disconnect_releases_reader_and_retains_cleanup_ownership() {
    for responses in [false, true] {
        let f = fixture(1).await;
        let account = f.engine.config.accounts[0].id();
        f.upstream.replies.lock().await.push_back(Reply::Slow);
        let path = if responses {
            "/v1/responses"
        } else {
            "/v1/chat/completions"
        };
        let mut payload =
            json!({"model":"test","stream":true,"conversation_id":"interrupted-record"});
        if responses {
            payload["input"] = json!("Continue the record.");
        } else {
            payload["messages"] = json!([{"role":"user","content":"Continue the record."}]);
        }
        let response = public_raw(
            &f.engine,
            "POST",
            path,
            &f.engine.config.gateway_key,
            payload,
        )
        .await;
        assert_eq!(response.status(), 200);
        let mut body = response.into_body();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let frame = body.frame().await.unwrap().unwrap();
                if frame
                    .data_ref()
                    .is_some_and(|data| String::from_utf8_lossy(data).contains("first"))
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert!(f.engine.pool.busy(&account).await);
        f.engine.sweep().await.unwrap();
        assert_eq!(f.upstream.deleted.load(Ordering::SeqCst), 0);
        drop(body);
        tokio::time::timeout(Duration::from_secs(3), async {
            while f.engine.pool.busy(&account).await
                || f.upstream.streams.load(Ordering::SeqCst) > 0
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(f.engine.legacy.lock().await.is_empty());
        assert_eq!(f.engine.store.read().await.pending.len(), 1);
        reset_pacing(&f.engine).await;
        f.engine.sweep().await.unwrap();
        assert_eq!(f.upstream.deleted.load(Ordering::SeqCst), 1);
        assert!(f.engine.store.read().await.pending.is_empty());
    }
}

#[tokio::test]
async fn version_one_state_import_preserves_binding_parent_and_cooldown() {
    let f = fixture(1).await;
    let secret = f.engine.config.gateway_key.clone();
    let nonce = "1234567890abcdef1234567890abcdef";
    let mut payload = json!({"model":"test","max_tokens":128,"messages":[
        {"role":"user","content":"Record one."},
        {"role":"assistant","content":"Stored."},
        {"role":"user","content":"Resume the record."}
    ]});
    payload["claud2api_tools"] = json!([]);
    let projected = protocol::parse(&payload).unwrap();
    let context_hash = projected.context(&secret);
    let history = projected.hashes(&secret);
    let account = f.engine.config.accounts[0].id();
    let logical = store::uuid();
    let physical = store::uuid();
    let parent = store::uuid();
    let key = protocol::digest(&secret, "session", "restored-client");
    let future = time::OffsetDateTime::from_unix_timestamp(store::now() + 3600)
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let disk = json!({"version":1,"sessions":[{"id":logical,"key":key,"account_id":account,"conversation_id":physical,"parent_uuid":parent,"nonce":nonce,"context_hash":context_hash,"history":history[..2],"expires_at":future}],"pending":[]});
    let path = f._dir.path().join("state-v1.json");
    std::fs::write(&path, disk.to_string()).unwrap();
    let mut config = f.engine.config.clone();
    config.gateway_key = secret.clone();
    config.import_state = Some(path);
    config.data_dir = f._dir.path().join("imported");
    let engine = Engine::open(config).await.unwrap();
    let (status, headers, body) =
        call(&engine, "/v1/messages", payload, Some("restored-client")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(headers["x-c2a-session-id"], logical);
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 0);
    let traces = f.upstream.traces.lock().await;
    let request = traces
        .iter()
        .find(|v| v["uri"].as_str().unwrap().ends_with("/completion"))
        .unwrap();
    assert!(request["uri"].as_str().unwrap().contains(&physical));
    assert_eq!(request["body"]["parent_message_uuid"], parent);
    drop(traces);
    engine.pool.observe(&account, 429, Some("0")).await.unwrap();
    let until = engine.store.read().await.accounts[&account].quota_until;
    drop(engine);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let engine = Engine::open({
        let mut c = f.engine.config.clone();
        c.gateway_key = secret.clone();
        c.data_dir = f._dir.path().join("imported");
        c
    })
    .await
    .unwrap();
    assert_eq!(
        engine.store.read().await.accounts[&account].quota_until,
        until
    );
}
#[tokio::test]
async fn committed_session_on_quota_cooldown_moves_on_the_next_turn() {
    let f = fixture(2).await;
    let (_, headers, first) = call(&f.engine, "/v1/messages", base(), Some("quota-next")).await;
    let handle = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    let old = f.engine.store.read().await.sessions[&handle].clone();
    f.engine
        .pool
        .observe(&old.account_id, 429, None)
        .await
        .unwrap();
    let mut v = base();
    v["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":first["content"]}),
        json!({"role":"user","content":"next"}),
    ]);
    let (status, h, body) = call(&f.engine, "/v1/messages", v, Some(&handle)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(h["x-c2a-session-id"], handle);
    assert_ne!(
        f.engine.store.read().await.sessions[&handle].account_id,
        old.account_id
    );
    let logs = logs_for_request(&f.engine, h["x-request-id"].to_str().unwrap()).await;
    let downstream = logs
        .iter()
        .find(|r| r["direction"] == "downstream")
        .unwrap();
    assert_eq!(
        downstream["account_id"],
        f.engine.store.read().await.sessions[&handle].account_id
    );
    assert_ne!(downstream["account_id"], old.account_id);
    assert_eq!(
        downstream["usage"]["output_tokens"],
        body["usage"]["output_tokens"]
    );
}
async fn call(
    engine: &Arc<Engine>,
    path: &str,
    payload: Value,
    session: Option<&str>,
) -> (u16, HeaderMap, Value) {
    call_key(engine, path, payload, session, &engine.config.gateway_key).await
}
async fn call_key(
    engine: &Arc<Engine>,
    path: &str,
    payload: Value,
    session: Option<&str>,
    key: &str,
) -> (u16, HeaderMap, Value) {
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("x-api-key", key)
        .header("content-type", "application/json");
    if let Some(id) = session {
        request = request.header("x-c2a-session-id", id);
    }
    let response = web::public(engine.clone())
        .oneshot(request.body(Body::from(payload.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)));
    (status, headers, value)
}
async fn admin(
    engine: &Arc<Engine>,
    method: &str,
    path: &str,
    value: Value,
    key: Option<&str>,
) -> (u16, Value) {
    let request = axum::http::Request::builder()
        .method(method)
        .uri(format!("/control/v1{path}"))
        .header(
            "authorization",
            format!("Bearer {}", key.unwrap_or(&engine.config.admin_key)),
        )
        .header("content-type", "application/json")
        .body(Body::from(if value.is_null() {
            String::new()
        } else {
            value.to_string()
        }))
        .unwrap();
    let response = control::router(engine.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn reset_pacing(e: &Engine) {
    e.store
        .mutate("", json!({}), |s| {
            for a in s.accounts.values_mut() {
                a.last_start = 0;
                a.starts.clear();
            }
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn profile_changes_preserve_slot_session_cache_and_remote_parent() {
    use claud2api::browser::BrowserProfile;
    let f = fixture(1).await;
    let (_, headers, first) = call(
        &f.engine,
        "/v1/messages",
        base(),
        Some("profile-continuity"),
    )
    .await;
    let handle = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    let original = f.engine.store.read().await.sessions[&handle].clone();
    let mut payload = base();
    payload["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role":"assistant","content":first["content"]}));
    payload["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role":"user","content":"second"}));
    reset_pacing(&f.engine).await;
    let request = f
        .engine
        .reserve(protocol::parse(&payload).unwrap(), handle.clone())
        .await
        .unwrap();
    let revision = f.engine.store.read().await.revision;
    let (status, _) = admin(
        &f.engine,
        "PATCH",
        &format!("/accounts/{}", original.account_id),
        json!({"expected_revision":revision,"browser_profile":"firefox_151_windows"}),
        None,
    )
    .await;
    assert_eq!(
        status, 409,
        "transport cannot replace an active account gate"
    );
    drop(request);
    for profile in [
        BrowserProfile::Firefox151,
        BrowserProfile::Chrome152,
        BrowserProfile::Firefox144,
    ] {
        let before = f.engine.store.read().await;
        let (status, body) = admin(
            &f.engine,
            "PATCH",
            &format!("/accounts/{}", original.account_id),
            json!({"expected_revision":before.revision,"browser_profile":profile.id()}),
            None,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let after = f.engine.store.read().await;
        assert_eq!(
            serde_json::to_value(&before.sessions[&handle]).unwrap(),
            serde_json::to_value(&after.sessions[&handle]).unwrap()
        );
        reset_pacing(&f.engine).await;
        let prior = after.sessions[&handle].clone();
        let (status, headers, response) =
            call(&f.engine, "/v1/messages", payload.clone(), Some(&handle)).await;
        assert_eq!(status, 200, "{response}");
        assert_eq!(headers["x-c2a-session-id"], handle);
        assert!(
            response["usage"]["cache_read_input_tokens"]
                .as_u64()
                .unwrap()
                > 0
        );
        let after = f.engine.store.read().await;
        let session = &after.sessions[&handle];
        assert_eq!(session.conversation_id, original.conversation_id);
        assert_eq!(session.nonce, original.nonce);
        let traces = f.upstream.traces.lock().await;
        let completion = traces
            .iter()
            .rev()
            .find(|t| t["uri"].as_str().unwrap().ends_with("/completion"))
            .unwrap();
        assert_eq!(completion["body"]["parent_message_uuid"], prior.parent_uuid);
        let headers = &completion["headers"];
        for name in ["upgrade-insecure-requests", "sec-fetch-user", "priority"] {
            assert!(headers.get(name).is_none());
        }
        assert_eq!(headers["sec-fetch-mode"], "cors");
        assert_eq!(headers["sec-fetch-dest"], "empty");
        let ua = headers["user-agent"].as_str().unwrap();
        assert!(ua.contains(&format!("{}/{}", profile.family(), profile.version())));
        if profile.family() == "Firefox" {
            assert!(headers.get("sec-ch-ua").is_none());
            assert!(!ua.contains("Chrome"));
        } else {
            assert!(
                headers["sec-ch-ua"]
                    .as_str()
                    .unwrap()
                    .contains(&profile.version().to_string())
            );
        }
        assert!(!headers["accept-encoding"].as_str().unwrap_or("").is_empty());
        drop(traces);
        payload["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"assistant","content":response["content"]}));
        payload["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"user","content":format!("next {}", profile.version())}));
    }
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn account_proxy_is_write_only_validated_and_encrypted() {
    let f = fixture(2).await;
    let before = f.engine.store.read().await;
    let id = before.accounts.keys().next().unwrap().clone();
    let proxy = "socks5h://private-user:private-password@127.0.0.1:1080";
    let (status, body) = admin(&f.engine, "PATCH", &format!("/accounts/{id}"), json!({"expected_revision":before.revision,"proxy_url":proxy,"browser_profile":"firefox_150_windows"}), None).await;
    assert_eq!(status, 200, "{body}");
    let (_, accounts) = admin(&f.engine, "GET", "/accounts", Value::Null, None).await;
    let text = accounts.to_string();
    assert!(!text.contains("private-user") && !text.contains("private-password"));
    let row = accounts["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == id)
        .unwrap();
    assert_eq!(row["proxy"]["scheme"], "socks5h");
    assert_eq!(row["proxy"]["authenticated"], true);
    assert_eq!(row["browser_profile"], "firefox_150_windows");
    assert_eq!(
        accounts["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["proxy"]["configured"] == true)
            .count(),
        1
    );
    let revision = f.engine.store.read().await.revision;
    let (status, error) = admin(&f.engine, "PATCH", &format!("/accounts/{id}"), json!({"expected_revision":revision,"proxy_url":"http://private-password@proxy.example/path"}), None).await;
    assert_eq!(status, 400);
    assert!(!error.to_string().contains("private-password"));
    assert_eq!(f.engine.store.read().await.revision, revision);
    let (_, events) = admin(&f.engine, "GET", "/events", Value::Null, None).await;
    assert!(!events.to_string().contains("private-password"));
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&f.engine.store.db)
        .await
        .unwrap();
    let bytes = std::fs::read(f.engine.config.data_dir.join("control.sqlite3")).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("private-password"));
    let (status, _) = admin(
        &f.engine,
        "PATCH",
        &format!("/accounts/{id}"),
        json!({"expected_revision":revision,"proxy_url":""}),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        f.engine.store.read().await.accounts[&id]
            .transport
            .proxy_url
            .is_none()
    );
}

#[tokio::test]
async fn cancelled_transport_commit_repairs_using_the_original_account_gate() {
    use claud2api::{browser::BrowserProfile, guard::Scope};
    let f = fixture(1).await;
    let scope = Scope::new(2);
    let first = f
        .engine
        .pool
        .acquire(None, &Default::default(), &scope, false)
        .await
        .unwrap();
    let gate = first.slot.gate.clone();
    let id = first.account_id.clone();
    drop(first);
    f.engine
        .store
        .mutate("test_interrupted_transport", json!({}), |s| {
            let account = s.accounts.get_mut(&id).unwrap();
            account.transport.browser_profile = Some(BrowserProfile::Firefox151);
            account.transport_revision += 1;
            account.last_start = 0;
            account.starts.clear();
            Ok(())
        })
        .await
        .unwrap();
    let repaired = tokio::time::timeout(
        Duration::from_millis(500),
        f.engine
            .pool
            .acquire(None, &Default::default(), &scope, false),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(repaired.slot.revision, 1);
    assert!(Arc::ptr_eq(&gate, &repaired.slot.gate));
    assert!(
        f.engine
            .pool
            .acquire(None, &Default::default(), &scope, false)
            .await
            .is_err()
    );
    let models = repaired
        .slot
        .client
        .models(&repaired, &scope, true)
        .await
        .unwrap();
    assert!(models.contains(&"test".to_owned()));
    assert!(
        f.upstream.traces.lock().await.last().unwrap()["headers"]["user-agent"]
            .as_str()
            .unwrap()
            .contains("Firefox/151")
    );
}

#[tokio::test]
async fn dropped_terminal_log_is_unknown_and_log_storage_failure_is_isolated() {
    use claud2api::observability::RequestTrace;
    let f = fixture(1).await;
    let trace = RequestTrace::new(&f.engine.store, "messages");
    let mut span = trace.span("downstream", "json", "client_request", "");
    f.engine.store.flush_status().await.unwrap();
    for _ in 0..1024 {
        span.headers(200);
    }
    span.finish(200);
    f.engine.store.flush_status().await.unwrap();
    assert!(f.engine.store.status_dropped.load(Ordering::Relaxed) > 0);
    let records = f
        .engine
        .store
        .status_logs("all", "", None, 100)
        .await
        .unwrap();
    let row = records["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["request_id"] == trace.id)
        .unwrap();
    assert_eq!(row["phase"], "unknown");
    assert!(row["finished_at"].as_i64().is_some());
    assert!(f.engine.store.healthy());
    sqlx::query("DROP TABLE protocol_logs")
        .execute(&f.engine.store.db)
        .await
        .unwrap();
    let trace = RequestTrace::new(&f.engine.store, "messages");
    trace
        .span("downstream", "json", "client_request", "")
        .finish(200);
    assert!(f.engine.store.flush_status().await.is_err());
    assert!(!f.engine.store.status_healthy());
    assert!(f.engine.store.healthy());
    assert_eq!(
        call(&f.engine, "/v1/messages", base(), Some("logging-failed"))
            .await
            .0,
        200
    );
}

#[tokio::test]
async fn protocol_logs_record_both_directions_filter_and_never_store_content() {
    let f = fixture(1).await;
    let mut payload = base();
    payload["messages"][0]["content"] = json!("private-user-message-canary");
    let (status, headers, _) = call(
        &f.engine,
        "/v1/messages",
        payload,
        Some("private-fingerprint-canary"),
    )
    .await;
    assert_eq!(status, 200);
    let request_id = headers["x-request-id"].to_str().unwrap();
    reset_pacing(&f.engine).await;
    f.upstream.replies.lock().await.push_back(Reply::Truncated);
    let mut payload = base();
    payload["stream"] = json!(true);
    let (status, error_headers, body) =
        call(&f.engine, "/v1/messages", payload, Some("failed-stream")).await;
    assert_eq!(status, 200);
    assert!(body.as_str().unwrap().contains("event: error"));
    let error = body
        .as_str()
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|v| v["type"] == "error")
        .unwrap();
    assert_eq!(
        error["request_id"],
        error_headers["x-request-id"].to_str().unwrap()
    );
    f.engine.store.flush_status().await.unwrap();
    let (_, all) = admin(&f.engine, "GET", "/logs", Value::Null, None).await;
    let records = all["records"].as_array().unwrap();
    assert!(records.iter().any(|r| r["request_id"] == request_id
        && r["direction"] == "upstream"
        && r["operation"] == "completion"
        && r["phase"] == "completed"));
    assert!(records.iter().any(|r| r["request_id"] == request_id
        && r["direction"] == "downstream"
        && r["mode"] == "json"
        && r["bytes"].as_u64().unwrap() > 0));
    assert!(records.iter().any(|r| r["direction"] == "downstream"
        && r["mode"] == "sse"
        && r["status"] == 502
        && r["level"] == "error"));
    for marker in [
        "private-user-message-canary",
        "private-fingerprint-canary",
        "fake-session",
        "partial",
        "private upstream detail",
    ] {
        assert!(!all.to_string().contains(marker));
    }
    let id = f
        .engine
        .store
        .read()
        .await
        .accounts
        .keys()
        .next()
        .unwrap()
        .clone();
    for level in ["normal", "error"] {
        let (status, filtered) = admin(
            &f.engine,
            "GET",
            &format!("/logs?level={level}&account_id={id}&limit=2"),
            Value::Null,
            None,
        )
        .await;
        assert_eq!(status, 200);
        assert!(!filtered["records"].as_array().unwrap().is_empty());
        assert!(
            filtered["records"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["level"] == level && r["account_id"] == id)
        );
    }
    assert_eq!(
        admin(&f.engine, "GET", "/logs?level=secret", Value::Null, None)
            .await
            .0,
        400
    );
    assert_eq!(
        admin(
            &f.engine,
            "GET",
            "/logs",
            Value::Null,
            Some(&f.engine.config.bot_key)
        )
        .await
        .0,
        403
    );
}

#[tokio::test]
async fn protocol_logs_keep_requested_models_for_json_stream_aliases_and_rejections() {
    let requested = "c2a-claude-haiku-5-5";
    for path in ["/v1/messages", "/v1/chat/completions", "/v1/responses"] {
        for stream in [false, true] {
            let f = fixture(1).await;
            let mut payload = base();
            payload["model"] = json!(requested);
            payload["stream"] = json!(stream);
            if path == "/v1/responses" {
                payload["input"] = json!("hello");
            }
            let (status, headers, response) = call(&f.engine, path, payload, None).await;
            assert_eq!(status, 200, "{path}: {response}");
            f.engine.store.flush_status().await.unwrap();
            let (status, logs) = admin(&f.engine, "GET", "/logs", Value::Null, None).await;
            assert_eq!(status, 200);
            let records: Vec<_> = logs["records"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["request_id"] == headers["x-request-id"].to_str().unwrap())
                .collect();
            assert!(
                records
                    .iter()
                    .any(|r| r["direction"] == "upstream" && r["operation"] == "completion")
            );
            assert!(records.iter().any(|r| r["direction"] == "downstream"));
            assert!(records.iter().all(|r| r["model"] == requested));
            assert!(records.iter().any(|r| r["direction"] == "downstream"
                && r["mode"] == if stream { "sse" } else { "json" }));
            let downstream = records
                .iter()
                .find(|r| r["direction"] == "downstream")
                .unwrap();
            let ledger: String =
                sqlx::query_scalar("SELECT data FROM usage ORDER BY seq DESC LIMIT 1")
                    .fetch_one(&f.engine.store.db)
                    .await
                    .unwrap();
            let usage: Value = serde_json::from_str(&ledger).unwrap();
            assert_eq!(downstream["usage"]["input_tokens"], usage["input_estimate"]);
            assert_eq!(
                downstream["usage"]["output_tokens"],
                usage["output_estimate"]
            );
            assert_eq!(downstream["usage"]["estimated"], true);
            assert_eq!(downstream["usage"]["complete"], true);
            let upstream = records
                .iter()
                .find(|r| r["operation"] == "completion")
                .unwrap();
            assert_eq!(upstream["usage"]["output_tokens"], 2); // "hello", five Unicode characters.
            assert_eq!(upstream["usage"]["complete"], true);
            assert_eq!(downstream["account_id"], upstream["account_id"]);
            assert!(
                records
                    .iter()
                    .filter(|r| r["direction"] == "upstream" && r["operation"] != "completion")
                    .all(|r| r["usage"].is_null())
            );
            let traces = f.upstream.traces.lock().await;
            let prompt = traces
                .iter()
                .find(|r| r["uri"].as_str().unwrap().ends_with("/completion"))
                .unwrap()["body"]["prompt"]
                .as_str()
                .unwrap();
            assert_eq!(
                upstream["usage"]["input_tokens"],
                protocol::estimate(prompt)
            );
            assert_eq!(
                traces
                    .iter()
                    .find(|r| r["uri"].as_str().unwrap().ends_with("/completion"))
                    .unwrap()["body"]["model"],
                "claude-haiku-5-5"
            );
        }
    }
    let f = fixture(1).await;
    let mut payload = base();
    payload["model"] = json!(requested);
    payload["messages"] = json!([]);
    let (status, headers, _) = call(&f.engine, "/v1/messages", payload, None).await;
    assert_eq!(status, 400);
    f.engine.store.flush_status().await.unwrap();
    let (_, logs) = admin(&f.engine, "GET", "/logs?level=error", Value::Null, None).await;
    let record = logs["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["request_id"] == headers["x-request-id"].to_str().unwrap())
        .unwrap();
    assert_eq!(record["model"], requested);
    assert!(f.upstream.traces.lock().await.is_empty());
    let mut historic = record.clone();
    historic.as_object_mut().unwrap().remove("model");
    historic.as_object_mut().unwrap().remove("usage");
    historic["request_id"] = json!("req_historic_without_model");
    sqlx::query(
        "INSERT INTO protocol_logs(id,at,account_id,level,finished_at,data) VALUES(?,?,?,?,?,?)",
    )
    .bind("historic-without-model")
    .bind(store::now())
    .bind("")
    .bind("error")
    .bind(store::now())
    .bind(historic.to_string())
    .execute(&f.engine.store.db)
    .await
    .unwrap();
    let (_, logs) = admin(&f.engine, "GET", "/logs", Value::Null, None).await;
    assert!(
        logs["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["request_id"] == "req_historic_without_model")
            .unwrap()["model"]
            .is_null()
    );
    assert!(
        logs["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["request_id"] == "req_historic_without_model")
            .unwrap()["usage"]
            .is_null()
    );
}

async fn logs_for_request(e: &Arc<Engine>, request_id: &str) -> Vec<Value> {
    e.store.flush_status().await.unwrap();
    let (_, logs) = admin(e, "GET", "/logs?limit=200", Value::Null, None).await;
    logs["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["request_id"] == request_id)
        .cloned()
        .collect()
}

#[tokio::test]
async fn log_tokens_count_unicode_once_and_preserve_unknown_partial_and_empty_outputs() {
    let f = fixture(1).await;
    let mut events = event(json!({"type":"message_start"}));
    for text in ["你", "好", "a", "b"] {
        if text == "你" {
            events += &event(
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            );
        }
        events += &event(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        );
    }
    events += &event(json!({"type":"content_block_stop","index":0}));
    events += &event(
        json!({"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}),
    );
    events += &event(
        json!({"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"思"}}),
    );
    events += &event(json!({"type":"content_block_stop","index":1}));
    events += &event(
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu-token-count","name":"web_search","input":{}}}),
    );
    events += &event(
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
    );
    events += &event(json!({"type":"content_block_stop","index":2}));
    events += &event(
        json!({"type":"content_block_start","index":3,"content_block":{"type":"tool_result","tool_use_id":"toolu-token-count","content":[],"is_error":false}}),
    );
    events += &event(json!({"type":"content_block_stop","index":3}));
    events += &event(json!({"type":"message_stop"}));
    f.upstream
        .replies
        .lock()
        .await
        .push_back(Reply::Raw(events));
    let (status, headers, body) = call(&f.engine, "/v1/messages", base(), None).await;
    assert_eq!(status, 200, "{body}");
    let logs = logs_for_request(&f.engine, headers["x-request-id"].to_str().unwrap()).await;
    let upstream = logs
        .iter()
        .find(|r| r["operation"] == "completion")
        .unwrap();
    assert_eq!(upstream["usage"]["output_tokens"], 2); // ceil((4 text+1 thinking+2 JSON chars)/4), not per-delta rounding or bytes.
    let downstream = logs
        .iter()
        .find(|r| r["direction"] == "downstream")
        .unwrap();
    assert_eq!(
        downstream["usage"]["output_tokens"],
        body["usage"]["output_tokens"]
    );
    assert!(!serde_json::to_string(&logs).unwrap().contains("你好ab"));

    for (reply, expected, complete) in [
        (Reply::Http(500), Value::Null, false),
        (Reply::Truncated, json!(2), false),
        (
            Reply::Raw(
                event(json!({"type":"message_start"})) + &event(json!({"type":"message_stop"})),
            ),
            json!(0),
            true,
        ),
    ] {
        let f = fixture(1).await;
        f.upstream.replies.lock().await.push_back(reply);
        let (_, headers, _) = call(&f.engine, "/v1/messages", base(), None).await;
        let logs = logs_for_request(&f.engine, headers["x-request-id"].to_str().unwrap()).await;
        let upstream = logs
            .iter()
            .find(|r| r["operation"] == "completion")
            .unwrap();
        assert!(upstream["usage"]["input_tokens"].as_u64().unwrap() > 0);
        assert_eq!(upstream["usage"]["output_tokens"], expected);
        assert_eq!(upstream["usage"]["complete"], complete);
        let downstream = logs
            .iter()
            .find(|r| r["direction"] == "downstream")
            .unwrap();
        assert!(downstream["status"].as_u64().unwrap() >= 400);
        assert!(downstream["usage"]["output_tokens"].is_null());
        assert_eq!(downstream["usage"]["complete"], false);
    }
}

#[tokio::test]
async fn log_tokens_match_cache_creation_read_and_disabled_usage_and_survive_restart() {
    let f = fixture(1).await;
    let (_, headers, first) = call(&f.engine, "/v1/messages", base(), Some("log-cache")).await;
    let first_logs = logs_for_request(&f.engine, headers["x-request-id"].to_str().unwrap()).await;
    let first_usage = &first_logs
        .iter()
        .find(|r| r["direction"] == "downstream")
        .unwrap()["usage"];
    assert_eq!(
        first_usage["cache_creation_input_tokens"],
        first["usage"]["cache_creation_input_tokens"]
    );
    assert_eq!(first_usage["cache_read_input_tokens"], 0);
    assert_eq!(
        first_usage["input_tokens"],
        first["usage"]["cache_creation_input_tokens"]
    );
    reset_pacing(&f.engine).await;
    let mut follow = base();
    follow["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":first["content"]}),
        json!({"role":"user","content":"next"}),
    ]);
    let (_, headers, second) = call(&f.engine, "/v1/messages", follow, Some("log-cache")).await;
    let second_logs = logs_for_request(&f.engine, headers["x-request-id"].to_str().unwrap()).await;
    let second_usage = &second_logs
        .iter()
        .find(|r| r["direction"] == "downstream")
        .unwrap()["usage"];
    assert!(second_usage["cache_read_input_tokens"].as_u64().unwrap() > 0);
    assert_eq!(
        second_usage["input_tokens"].as_u64().unwrap(),
        second["usage"]["input_tokens"].as_u64().unwrap()
            + second["usage"]["cache_creation_input_tokens"]
                .as_u64()
                .unwrap()
            + second["usage"]["cache_read_input_tokens"].as_u64().unwrap()
    );
    let upstream = second_logs
        .iter()
        .find(|r| r["operation"] == "completion")
        .unwrap();
    assert!(
        upstream["usage"]["input_tokens"].as_u64().unwrap()
            < second_usage["input_tokens"].as_u64().unwrap()
    ); // Continuation sends only the delta.
    f.engine
        .store
        .mutate("", json!({}), |s| {
            s.settings.simulated_cache = false;
            Ok(())
        })
        .await
        .unwrap();
    reset_pacing(&f.engine).await;
    let (_, headers, uncached) =
        call(&f.engine, "/v1/messages", base(), Some("log-cache-off")).await;
    let request_id = headers["x-request-id"].to_str().unwrap();
    let records = logs_for_request(&f.engine, request_id).await;
    let final_usage = &records
        .iter()
        .find(|r| r["direction"] == "downstream")
        .unwrap()["usage"];
    assert_eq!(
        final_usage["input_tokens"],
        uncached["usage"]["input_tokens"]
    );
    assert!(final_usage["cache_creation_input_tokens"].is_null());
    assert!(final_usage["cache_read_input_tokens"].is_null());
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&f.engine.store.db)
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        f.engine.config.data_dir.join("control.sqlite3"),
        dir.path().join("control.sqlite3"),
    )
    .unwrap();
    let mut config = f.engine.config.clone();
    config.data_dir = dir.path().to_owned();
    let restarted = Engine::open(config).await.unwrap();
    let restored = logs_for_request(&restarted, request_id).await;
    assert_eq!(
        restored
            .iter()
            .find(|r| r["direction"] == "downstream")
            .unwrap()["usage"],
        *final_usage
    );
}

#[tokio::test]
async fn dropped_json_delivery_does_not_publish_completed_downstream_token_usage() {
    let f = fixture(1).await;
    let response = public_raw(
        &f.engine,
        "POST",
        "/v1/messages",
        &f.engine.config.gateway_key,
        base(),
    )
    .await;
    assert_eq!(response.status(), 200);
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    drop(response);
    let records = logs_for_request(&f.engine, &request_id).await;
    let upstream = records
        .iter()
        .find(|r| r["operation"] == "completion")
        .unwrap();
    assert_eq!(upstream["usage"]["complete"], true);
    let downstream = records
        .iter()
        .find(|r| r["direction"] == "downstream")
        .unwrap();
    assert_eq!(downstream["status"], 499);
    assert!(downstream["usage"]["input_tokens"].as_u64().unwrap() > 0);
    assert!(downstream["usage"]["output_tokens"].is_null());
    assert_eq!(downstream["usage"]["complete"], false);
}

#[tokio::test]
async fn encrypted_previous_format_upgrades_without_resetting_sessions() {
    use aes_gcm::{
        Aes256Gcm, KeyInit, Nonce,
        aead::{Aead, Payload},
    };
    use sha2::{Digest, Sha256};
    let f = fixture(1).await;
    let (_, headers, _) = call(&f.engine, "/v1/messages", base(), Some("old-vault")).await;
    let handle = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    let snapshot = f.engine.store.read().await;
    let mut plain = serde_json::to_value(&snapshot).unwrap();
    for account in plain["accounts"].as_object_mut().unwrap().values_mut() {
        account.as_object_mut().unwrap().remove("transport");
        account
            .as_object_mut()
            .unwrap()
            .remove("transport_revision");
    }
    plain["settings"]["simulated_cache"] = json!(false);
    let mut hash = Sha256::new();
    hash.update(b"ClaudAPI durable vault v1\0");
    hash.update(f.engine.config.gateway_key.as_bytes());
    let cipher = Aes256Gcm::new(&hash.finalize());
    let nonce = [7u8; 12];
    let encrypted = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &serde_json::to_vec(&plain).unwrap(),
                aad: b"ClaudAPI/snapshot/1",
            },
        )
        .unwrap();
    let mut payload = nonce.to_vec();
    payload.extend(encrypted);
    let dir = tempfile::tempdir().unwrap();
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(dir.path().join("control.sqlite3"))
                .create_if_missing(true),
        )
        .await
        .unwrap();
    sqlx::query("CREATE TABLE vault(id INTEGER PRIMARY KEY,payload BLOB NOT NULL)")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO vault VALUES(1,?)")
        .bind(&payload)
        .execute(&db)
        .await
        .unwrap();
    db.close().await;
    let mut config = f.engine.config.clone();
    config.data_dir = dir.path().to_owned();
    config.browser_profile = claud2api::browser::BrowserProfile::Chrome152;
    let upgraded = Engine::open(config).await.unwrap();
    let state = upgraded.store.read().await;
    assert!(
        !state.settings.simulated_cache,
        "persisted user choices survive new defaults"
    );
    assert_eq!(
        serde_json::to_value(&snapshot.sessions[&handle]).unwrap(),
        serde_json::to_value(&state.sessions[&handle]).unwrap()
    );
    assert!(
        state
            .accounts
            .values()
            .all(|a| a.transport.browser_profile.is_none())
    );
    let (_, accounts) = admin(&upgraded, "GET", "/accounts", Value::Null, None).await;
    assert_eq!(
        accounts["accounts"][0]["browser_profile"],
        "chrome_152_windows"
    );
    let rewritten: Vec<u8> = sqlx::query_scalar("SELECT payload FROM vault WHERE id=1")
        .fetch_one(&upgraded.store.db)
        .await
        .unwrap();
    assert!(
        cipher
            .decrypt(
                Nonce::from_slice(&rewritten[..12]),
                Payload {
                    msg: &rewritten[12..],
                    aad: b"ClaudAPI/snapshot/1"
                }
            )
            .is_err()
    );
    let mut hash = Sha256::new();
    hash.update(b"Claud2Api durable vault v1\0");
    hash.update(upgraded.config.gateway_key.as_bytes());
    Aes256Gcm::new(&hash.finalize())
        .decrypt(
            Nonce::from_slice(&rewritten[..12]),
            Payload {
                msg: &rewritten[12..],
                aad: b"Claud2Api/snapshot/1",
            },
        )
        .unwrap();
}

#[tokio::test]
async fn messages_json_stream_and_exact_browser_headers() {
    let f = fixture(1).await;
    let (status, headers, body) = call(
        &f.engine,
        "/v1/messages",
        base(),
        Some("client-fingerprint"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["content"][0]["text"], "hello");
    assert_eq!(body["usage"]["cache_read_input_tokens"], 0);
    assert_eq!(headers["x-claud2api-cache"], "simulated-1h");
    let traces = f.upstream.traces.lock().await;
    let request = traces
        .iter()
        .find(|v| v["uri"].as_str().unwrap().ends_with("/completion"))
        .unwrap();
    assert_eq!(request["body"]["tools"], default_native_descriptors());
    assert_eq!(request["body"]["thinking_mode"], "off");
    for (name, value) in [
        ("sec-ch-ua-platform", "\"Windows\""),
        ("accept", "text/event-stream"),
        ("authorization", ""),
        ("x-api-key", ""),
    ] {
        assert_eq!(request["headers"][name].as_str().unwrap_or(""), value);
    }
    drop(traces);
    reset_pacing(&f.engine).await;
    let mut payload = base();
    payload["stream"] = json!(true);
    let (status, _, body) = call(
        &f.engine,
        "/v1/messages",
        payload,
        Some("other-fingerprint"),
    )
    .await;
    assert_eq!(status, 200);
    let stream = body.as_str().unwrap();
    assert!(stream.contains("event: message_start"));
    assert!(stream.contains("event: message_stop"));
}
#[tokio::test]
async fn continuation_sends_delta_reuses_parent_and_simulates_cache() {
    let f = fixture(1).await;
    let (_, headers, first) = call(&f.engine, "/v1/messages", base(), Some("one")).await;
    let handle = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    let before = f.engine.store.read().await.sessions[&handle].clone();
    reset_pacing(&f.engine).await;
    let mut v = base();
    v["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role":"assistant","content":first["content"]}));
    v["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role":"user","content":"second"}));
    let (status, h, second) = call(&f.engine, "/v1/messages", v, Some(&handle)).await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(h["x-c2a-session-id"], handle);
    assert!(second["usage"]["cache_read_input_tokens"].as_u64().unwrap() > 0);
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 1);
    let traces = f.upstream.traces.lock().await;
    let last = traces
        .iter()
        .rev()
        .find(|v| v["uri"].as_str().unwrap().ends_with("/completion"))
        .unwrap();
    assert_eq!(last["body"]["parent_message_uuid"], before.parent_uuid);
    let prompt = last["body"]["prompt"].as_str().unwrap();
    assert!(prompt.contains("second"));
    assert!(!prompt.contains("\"text\":\"hello\""));
}
#[tokio::test]
async fn client_optional_default_replay_continues_but_real_tool_argument_edits_rebuild() {
    let f = fixture(1).await;
    let mut request = base();
    request["tools"] = json!([{"name":"Edit","input_schema":{
        "type":"object","properties":{
            "file_path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"},
            "replace_all":{"type":"boolean","default":false}
        },"required":["file_path","old_string","new_string"],"additionalProperties":false
    }}]);
    let (status, headers, first) = call(
        &f.engine,
        "/v1/messages",
        request.clone(),
        Some("default-replay"),
    )
    .await;
    assert_eq!(status, 200);
    let handle = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    let nonce = f.engine.store.read().await.sessions[&handle].nonce.clone();
    request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":first["content"]}),
        json!({"role":"user","content":"Edit the fixture."}),
    ]);
    f.upstream.replies.lock().await.push_back(Reply::Text(format!(
        "<tool_call_{nonce}>{{\"name\":\"Edit\",\"input\":{{\"file_path\":\"fixture.txt\",\"old_string\":\"one\",\"new_string\":\"two\"}}}}</tool_call_{nonce}>"
    )));
    reset_pacing(&f.engine).await;
    let (status, _, tool_response) =
        call(&f.engine, "/v1/messages", request.clone(), Some(&handle)).await;
    assert_eq!(status, 200, "{tool_response}");
    assert_eq!(tool_response["content"][0]["type"], "tool_use");
    assert!(
        tool_response["content"][0]["input"]
            .get("replace_all")
            .is_none()
    );
    let tool_id = tool_response["content"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let before = f.engine.store.read().await.sessions[&handle].clone();
    let mut replay = tool_response["content"].clone();
    replay[0]["input"]["replace_all"] = json!(false);
    request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":replay}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":tool_id,"content":"fixture edited"}]})
    ]);
    reset_pacing(&f.engine).await;
    let (status, _, result) = call(&f.engine, "/v1/messages", request.clone(), Some(&handle)).await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.engine.store.read().await.sessions[&handle].conversation_id,
        before.conversation_id
    );
    request["messages"][3]["content"][0]["input"]["old_string"] = json!("actual history edit");
    request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":result["content"]}),
        json!({"role":"user","content":"Continue."}),
    ]);
    reset_pacing(&f.engine).await;
    let (status, _, result) = call(&f.engine, "/v1/messages", request, Some(&handle)).await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 2);
    assert_ne!(
        f.engine.store.read().await.sessions[&handle].conversation_id,
        before.conversation_id
    );
}
#[tokio::test]
async fn claude_code_attribution_changes_reuse_conversation_but_real_context_changes_reset() {
    for stream in [false, true] {
        let f = fixture(1).await;
        let mut payload = base();
        payload["stream"] = json!(stream);
        payload["tools"] =
            json!([{"name":"read","description":"read a file","input_schema":{"type":"object"}}]);
        let mut previous = None;
        for round in 0..5 {
            reset_pacing(&f.engine).await;
            let instructions = if round < 3 {
                "stable instructions"
            } else {
                "changed instructions"
            };
            payload["system"] = json!([
                {"type":"text","text":format!("x-anthropic-billing-header: cc_version=2.1.87.{round:04x}; cc_entrypoint=claude-desktop; cch={round:05x};")},
                {"type":"text","text":instructions}
            ]);
            if round == 4 {
                payload["tools"][0]["description"] = json!("changed tool definition");
            }
            let (status, headers, body) = call(
                &f.engine,
                "/v1/messages",
                payload.clone(),
                Some("desktop-attribution"),
            )
            .await;
            assert_eq!(status, 200, "round={round}, stream={stream}: {body}");
            let (content, usage) = if stream {
                let events = sse_values(body.as_str().unwrap());
                let events = events.as_array().unwrap();
                assert!(events.iter().any(|e| e["type"] == "message_stop"));
                let start = events
                    .iter()
                    .find(|e| e["type"] == "message_start")
                    .unwrap();
                let text = events
                    .iter()
                    .filter_map(|e| e["delta"]["text"].as_str())
                    .collect::<String>();
                (
                    json!([{"type":"text","text":text}]),
                    start["message"]["usage"].clone(),
                )
            } else {
                (body["content"].clone(), body["usage"].clone())
            };
            let handle = headers["x-c2a-session-id"].to_str().unwrap();
            let session = f.engine.store.read().await.sessions[handle].clone();
            let traces = f.upstream.traces.lock().await;
            let last = traces
                .iter()
                .rev()
                .find(|v| v["uri"].as_str().unwrap().ends_with("/completion"))
                .unwrap();
            let prompt = last["body"]["prompt"].as_str().unwrap();
            assert!(!prompt.contains("x-anthropic-billing-header"));
            assert!(!prompt.contains("cc_version="));
            if (1..3).contains(&round) {
                let old: &store::Session = previous.as_ref().unwrap();
                assert_eq!(session.conversation_id, old.conversation_id);
                assert_eq!(last["body"]["parent_message_uuid"], old.parent_uuid);
                assert!(usage["cache_read_input_tokens"].as_u64().unwrap() > 0);
                assert!(!prompt.contains("\"text\":\"hello\""));
            } else {
                assert_eq!(usage["cache_read_input_tokens"], 0);
                if let Some(old) = &previous {
                    assert_ne!(session.conversation_id, old.conversation_id);
                }
                assert!(prompt.contains(instructions));
            }
            assert_eq!(
                f.upstream.created.load(Ordering::SeqCst),
                if round < 3 { 1 } else { round - 1 }
            );
            drop(traces);
            previous = Some(session);
            payload["messages"].as_array_mut().unwrap().extend([
                json!({"role":"assistant","content":content}),
                json!({"role":"user","content":format!("round {}", round + 1)}),
            ]);
        }
    }
}
#[tokio::test]
async fn appended_message_instructions_continue_but_edits_to_committed_context_reset() {
    for (stream, explicit) in [(false, true), (true, false)] {
        let f = fixture(1).await;
        let restart_dir = tempfile::tempdir().unwrap();
        let mut engine = f.engine.clone();
        let mut payload = base();
        payload["stream"] = json!(stream);
        payload["system"] = json!("stable top-level instructions 你好");
        payload["messages"]
            .as_array_mut()
            .unwrap()
            .insert(0, json!({"role":"system","content":"initial rules 初始"}));
        let mut prior: Option<store::Session> = None;
        let mut handle = String::new();
        for round in 0..6 {
            if round == 2 {
                engine.store.flush_status().await.unwrap();
                sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
                    .execute(&engine.store.db)
                    .await
                    .unwrap();
                std::fs::copy(
                    engine.config.data_dir.join("control.sqlite3"),
                    restart_dir.path().join("control.sqlite3"),
                )
                .unwrap();
                let mut config = engine.config.clone();
                config.data_dir = restart_dir.path().to_owned();
                engine = Engine::open(config).await.unwrap();
            }
            reset_pacing(&engine).await;
            if round == 3 {
                payload["messages"][0]["content"] = json!("edited historical instructions");
            }
            if round == 4 {
                payload["system"] = json!("edited global instructions");
            }
            if round == 5 {
                payload["messages"].as_array_mut().unwrap().remove(0);
            }
            let (status, headers, body) = call(
                &engine,
                "/v1/messages",
                payload.clone(),
                explicit.then_some("appended-instructions"),
            )
            .await;
            assert_eq!(status, 200, "round={round}, stream={stream}: {body}");
            let next_handle = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
            if round < 3 && round > 0 {
                assert_eq!(handle, next_handle);
            }
            handle = next_handle;
            let content = if stream {
                let events = sse_values(body.as_str().unwrap());
                assert!(
                    events
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|e| e["type"] == "message_stop")
                );
                let text = events
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|e| e["delta"]["text"].as_str())
                    .collect::<String>();
                json!([{"type":"text","text":text}])
            } else {
                body["content"].clone()
            };
            let session = engine.store.read().await.sessions[&handle].clone();
            let traces = f.upstream.traces.lock().await;
            let last = traces
                .iter()
                .rev()
                .find(|v| v["uri"].as_str().unwrap().ends_with("/completion"))
                .unwrap();
            let prompt = last["body"]["prompt"].as_str().unwrap();
            assert!(prompt.contains(payload["system"].as_str().unwrap()));
            if round > 0 {
                assert!(prompt.contains(&format!("new instruction {round}")));
            }
            if (1..3).contains(&round) {
                let old = prior.as_ref().unwrap();
                assert_eq!(session.conversation_id, old.conversation_id);
                assert_eq!(last["body"]["parent_message_uuid"], old.parent_uuid);
                assert_ne!(session.context_hash, old.context_hash);
                assert!(!prompt.contains("\"text\":\"hello\""));
            } else if round > 0 {
                assert_ne!(
                    session.conversation_id,
                    prior.as_ref().unwrap().conversation_id
                );
                assert!(prompt.contains("\"text\":\"hello\""));
            }
            assert_eq!(
                f.upstream.created.load(Ordering::SeqCst),
                if round < 3 { 1 } else { round - 1 }
            );
            drop(traces);
            prior = Some(session);
            payload["messages"].as_array_mut().unwrap().extend([
                json!({"role":"assistant","content":content}),
                json!({"role":if round % 2 == 0 {"system"} else {"developer"},"content":[{"type":"text","text":format!("new instruction {}",round+1)}]}),
                json!({"role":"user","content":format!("next round {}",round+1)}),
            ]);
        }
    }
}
#[tokio::test]
async fn inserted_instructions_in_completed_history_cannot_reuse_a_conversation() {
    let f = fixture(1).await;
    let (_, headers, first) =
        call(&f.engine, "/v1/messages", base(), Some("instruction-edit")).await;
    let id = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    let before = f.engine.store.read().await.sessions[&id].clone();
    reset_pacing(&f.engine).await;
    let mut payload = base();
    payload["messages"].as_array_mut().unwrap().extend([
        json!({"role":"system","content":"inserted before the old assistant"}),
        json!({"role":"assistant","content":first["content"]}),
        json!({"role":"user","content":"next"}),
    ]);
    let (status, _, second) = call(&f.engine, "/v1/messages", payload, Some(&id)).await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 2);
    assert_ne!(
        f.engine.store.read().await.sessions[&id].conversation_id,
        before.conversation_id
    );
    assert_eq!(second["usage"]["cache_read_input_tokens"], 0);
}
#[tokio::test]
async fn appended_instructions_do_not_rebase_a_checkpoint_from_the_old_context() {
    let f = fixture(1).await;
    let (id, mut payload) = compaction_input(&f).await;
    f.upstream.automatic_summaries.store(true, Ordering::SeqCst);
    let (status, _, out) = call(&f.engine, "/v1/messages", payload.clone(), Some(&id)).await;
    assert_eq!(status, 200, "{out}");
    let before = f.engine.store.read().await.sessions[&id].clone();
    assert!(before.checkpoint.is_some());
    f.engine
        .store
        .mutate("", json!({}), |s| {
            s.settings.compaction_enabled = false;
            Ok(())
        })
        .await
        .unwrap();
    payload["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":out["content"]}),
        json!({"role":"system","content":"new instructions must remain visible"}),
        json!({"role":"user","content":"after instruction update"}),
    ]);
    reset_pacing(&f.engine).await;
    let created = f.upstream.created.load(Ordering::SeqCst);
    let (status, _, out) = call(&f.engine, "/v1/messages", payload.clone(), Some(&id)).await;
    assert_eq!(status, 200, "{out}");
    let current = f.engine.store.read().await.sessions[&id].clone();
    assert_eq!(current.conversation_id, before.conversation_id);
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), created);
    assert!(current.checkpoint.is_none());
    assert_ne!(current.context_hash, before.context_hash);
    let mut parsed = protocol::parse(&payload).unwrap();
    parsed.native_tools =
        claud2api::native_tools::resolve(&payload, &f.engine.store.read().await.settings).unwrap();
    assert_eq!(
        current.context_hash,
        parsed.context(&f.engine.config.gateway_key)
    );
    let traces = f.upstream.traces.lock().await;
    let last = traces
        .iter()
        .rev()
        .find(|v| v["uri"].as_str().unwrap().ends_with("/completion"))
        .unwrap();
    assert_eq!(last["body"]["parent_message_uuid"], before.parent_uuid);
    assert!(
        last["body"]["prompt"]
            .as_str()
            .unwrap()
            .contains("new instructions must remain visible")
    );
}
#[tokio::test]
async fn quota_before_generation_migrates_but_partial_never_replays() {
    for early in [true, false] {
        let f = fixture(2).await;
        f.upstream.replies.lock().await.extend(if early {
            vec![Reply::EarlyQuota, Reply::Text("migrated".to_owned())]
        } else {
            vec![Reply::PartialQuota, Reply::Text("must-not-run".to_owned())]
        });
        let (status, _, v) = call(&f.engine, "/v1/messages", base(), Some("quota")).await;
        if early {
            assert_eq!(status, 200, "{v}");
            assert_eq!(v["content"][0]["text"], "migrated");
            assert_eq!(f.upstream.created.load(Ordering::SeqCst), 2);
            let state = f.engine.store.read().await;
            assert_eq!(
                state
                    .accounts
                    .values()
                    .filter(|a| a.cooldown_until > store::now())
                    .count(),
                1
            );
            assert_eq!(state.pending.len(), 1);
        } else {
            assert_eq!(status, 429);
            assert_eq!(f.upstream.created.load(Ordering::SeqCst), 1);
            assert!(f.upstream.replies.lock().await.len() == 1);
        }
    }
}
#[tokio::test]
async fn http_quota_migration_and_pause_are_durable() {
    let f = fixture(2).await;
    f.upstream
        .replies
        .lock()
        .await
        .extend([Reply::Http(429), Reply::Text("moved".to_owned())]);
    let (status, _, v) = call(&f.engine, "/v1/messages", base(), Some("httpquota")).await;
    assert_eq!(status, 200, "{v}");
    let state = f.engine.store.read().await;
    assert!(
        state
            .accounts
            .values()
            .any(|a| a.cooldown_until > store::now() + 17000)
    );
    let path = f.engine.config.data_dir.join("control.sqlite3");
    let bytes = std::fs::read(path).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("fake-session"));
    let duplicate = Store::open(&f.engine.config).await;
    assert!(duplicate.is_err());
}
#[tokio::test]
async fn truncated_stream_returns_error_without_committed_history() {
    let f = fixture(1).await;
    f.upstream.replies.lock().await.push_back(Reply::Truncated);
    let (status, _, v) = call(&f.engine, "/v1/messages", base(), Some("broken")).await;
    assert_eq!(status, 502, "{v}");
    let s = f.engine.store.read().await;
    assert!(
        s.sessions
            .values()
            .all(|s| s.history.is_empty() && !s.active)
    );
    assert_eq!(s.pending.len(), 1);
}
#[tokio::test]
async fn strict_one_request_per_account_and_distinct_account_capacity() {
    let f = fixture(2).await;
    let p = protocol::parse(&base()).unwrap();
    let one = f
        .engine
        .reserve(p.clone(), "identity-one".to_owned())
        .await
        .unwrap();
    let two = f
        .engine
        .reserve(p.clone(), "identity-two".to_owned())
        .await
        .unwrap();
    assert_ne!(
        one.lease.as_ref().unwrap().account_id,
        two.lease.as_ref().unwrap().account_id
    );
    assert!(
        f.engine
            .reserve(p.clone(), "identity-three".to_owned())
            .await
            .is_err()
    );
    assert!(
        f.engine
            .reserve(p, "identity-one".to_owned())
            .await
            .is_err()
    );
    drop((one, two));
}
#[tokio::test]
async fn account_names_named_import_and_atomic_proxy_batches_preserve_guards() {
    let f = fixture(2).await;
    let revision = f.engine.store.read().await.revision;
    let (status, result) = admin(
        &f.engine,
        "POST",
        "/accounts/import",
        json!({"expected_revision":revision,"accounts":[
            {"credential":"工作账号:fake-named-session"},
            {"credential":"sessionKey=fake-cookie-session; marker=http://example.test"},
            {"credential":"ignored:fake-explicit-session","label":"显式名称"},
            {"credential":"another:fake-named-session"}
        ]}),
        None,
    )
    .await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["added"], 3);
    let before = f.engine.store.read().await;
    let named = before
        .accounts
        .values()
        .find(|a| a.label == "工作账号")
        .unwrap();
    assert_eq!(named.credential.session_key, "fake-named-session");
    assert!(before.accounts.values().any(|a| a.label == "显式名称"));
    assert!(
        before
            .accounts
            .values()
            .any(|a| a.credential.cookie.contains("http://example.test"))
    );
    let ids = before.accounts.keys().take(2).cloned().collect::<Vec<_>>();
    let scope = claud2api::guard::Scope::new(5);
    let held = f
        .engine
        .pool
        .acquire(Some(&ids[1]), &Default::default(), &scope, false)
        .await
        .unwrap();
    let (status,_) = admin(&f.engine,"PATCH","/accounts/proxy",json!({"expected_revision":before.revision,"account_ids":ids,"proxy_url":"http://user:private-proxy-password@127.0.0.1:9"}),None).await;
    assert_eq!(status, 409);
    let after = f.engine.store.read().await;
    assert_eq!(after.revision, before.revision);
    for id in &ids {
        assert_eq!(
            serde_json::to_value(&after.accounts[id].transport).unwrap(),
            serde_json::to_value(&before.accounts[id].transport).unwrap()
        );
    }
    let (status, _) = admin(
        &f.engine,
        "PATCH",
        &format!("/accounts/{}", ids[1]),
        json!({"expected_revision":after.revision,"label":"使用中仍可改名"}),
        None,
    )
    .await;
    assert_eq!(status, 200);
    drop(held);
    let revision = f.engine.store.read().await.revision;
    for list in [json!([]), json!([ids[0], ids[0]])] {
        assert_eq!(
            admin(
                &f.engine,
                "PATCH",
                "/accounts/proxy",
                json!({"expected_revision":revision,"account_ids":list,"proxy_url":""}),
                None
            )
            .await
            .0,
            400
        );
    }
    assert_eq!(
        admin(
            &f.engine,
            "PATCH",
            "/accounts/proxy",
            json!({"expected_revision":revision,"account_ids":ids,"proxy_url":"file:///invalid"}),
            None
        )
        .await
        .0,
        400
    );
    let (status,result)=admin(&f.engine,"PATCH","/accounts/proxy",json!({"expected_revision":revision,"account_ids":ids,"proxy_url":"http://user:private-proxy-password@127.0.0.1:9"}),None).await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["updated"], 2);
    let (_, accounts) = admin(&f.engine, "GET", "/accounts", Value::Null, None).await;
    assert!(!accounts.to_string().contains("private-proxy-password"));
    let changed = f.engine.store.read().await;
    for id in &ids {
        assert_eq!(
            changed.accounts[id].transport_revision,
            before.accounts[id].transport_revision + 1
        );
        assert_eq!(
            changed.accounts[id].transport.browser_profile,
            before.accounts[id].transport.browser_profile
        );
    }
    assert_eq!(
        admin(
            &f.engine,
            "PATCH",
            "/accounts/proxy",
            json!({"expected_revision":revision,"account_ids":ids,"proxy_url":""}),
            None
        )
        .await
        .0,
        409
    );
    assert_eq!(
        admin(
            &f.engine,
            "PATCH",
            "/accounts/proxy",
            json!({"expected_revision":changed.revision,"account_ids":ids,"proxy_url":""}),
            Some(&f.engine.config.bot_key)
        )
        .await
        .0,
        403
    );
    assert_eq!(
        admin(
            &f.engine,
            "PATCH",
            "/accounts/proxy",
            json!({"expected_revision":changed.revision,"account_ids":ids,"proxy_url":""}),
            None
        )
        .await
        .0,
        200
    );
    assert!(
        f.engine.store.read().await.accounts[&ids[0]]
            .transport
            .proxy_url
            .is_none()
    );
}
#[tokio::test]
async fn token_metadata_edits_preserve_secret_and_live_admissions() {
    let f = fixture(0).await;
    let revision = f.engine.store.read().await.revision;
    let (_,created)=admin(&f.engine,"POST","/tokens",json!({"expected_revision":revision,"label":"before","max_concurrency":2,"expires_at":null}),None).await;
    let secret = created["token"].as_str().unwrap();
    let id = created["id"].as_str().unwrap();
    let old = f.engine.store.read().await.tokens[id].clone();
    let first = f.engine.tokens.admit(secret).await.unwrap();
    let second = f.engine.tokens.admit(secret).await.unwrap();
    let path = format!("/tokens/{id}");
    let revision = f.engine.store.read().await.revision;
    let patch = json!({"expected_revision":revision,"label":"after","max_concurrency":1,"expires_at":store::now()+3600});
    assert_eq!(
        admin(
            &f.engine,
            "PATCH",
            &path,
            patch.clone(),
            Some(&f.engine.config.bot_key)
        )
        .await
        .0,
        403
    );
    let (status, result) = admin(&f.engine, "PATCH", &path, patch.clone(), None).await;
    assert_eq!(status, 200, "{result}");
    assert!(result.get("token").is_none());
    assert_eq!(first.label, "before");
    assert_eq!(f.engine.tokens.busy(id, 1).await, 2);
    assert!(f.engine.tokens.admit(secret).await.is_err());
    drop(first);
    assert!(f.engine.tokens.admit(secret).await.is_err());
    drop(second);
    let third = f.engine.tokens.admit(secret).await.unwrap();
    assert_eq!(third.label, "after");
    assert_eq!(admin(&f.engine, "PATCH", &path, patch, None).await.0, 409);
    let revision = f.engine.store.read().await.revision;
    for (cap, expiration) in [
        (0, Value::Null),
        (513, Value::Null),
        (2, json!(store::now() - 1)),
    ] {
        assert_eq!(admin(&f.engine,"PATCH",&path,json!({"expected_revision":revision,"label":"valid","max_concurrency":cap,"expires_at":expiration}),None).await.0,400);
    }
    assert_eq!(admin(&f.engine,"PATCH",&path,json!({"expected_revision":revision,"label":"raised","max_concurrency":3,"expires_at":null}),None).await.0,200);
    let fourth = f.engine.tokens.admit(secret).await.unwrap();
    let fifth = f.engine.tokens.admit(secret).await.unwrap();
    assert!(f.engine.tokens.admit(secret).await.is_err());
    let current = f.engine.store.read().await.tokens[id].clone();
    assert_eq!(current.digest, old.digest);
    assert_eq!(current.created_at, old.created_at);
    assert_eq!(current.id, old.id);
    assert_eq!(f.engine.tokens.busy(id, 3).await, 3);
    drop((third, fourth, fifth));
    f.engine.store.flush_status().await.unwrap();
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&f.engine.store.db)
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        f.engine.config.data_dir.join("control.sqlite3"),
        dir.path().join("control.sqlite3"),
    )
    .unwrap();
    let mut config = f.engine.config.clone();
    config.data_dir = dir.path().to_owned();
    let restarted = Engine::open(config).await.unwrap();
    assert_eq!(
        restarted.tokens.admit(secret).await.unwrap().label,
        "raised"
    );
    assert_eq!(restarted.store.read().await.tokens[id].digest, old.digest);
    let (_, list) = admin(&restarted, "GET", "/tokens", Value::Null, None).await;
    assert!(!list.to_string().contains(secret));
    assert!(!list.to_string().contains(&old.digest));
}
#[tokio::test]
async fn account_cycle_api_reads_the_full_ledger_and_tracks_generation_start() {
    let f = fixture(1).await;
    let (status, _, _) = call(&f.engine, "/v1/messages", base(), Some("cycle-start")).await;
    assert_eq!(status, 200);
    let id = f
        .engine
        .store
        .read()
        .await
        .accounts
        .keys()
        .next()
        .unwrap()
        .clone();
    let raw: String = sqlx::query_scalar("SELECT data FROM usage ORDER BY seq DESC LIMIT 1")
        .fetch_one(&f.engine.store.db)
        .await
        .unwrap();
    let mut u: store::Usage = serde_json::from_str(&raw).unwrap();
    assert!(u.started_at.is_some_and(|v| v <= store::now()));
    let (_, empty) = admin(&f.engine, "GET", "/accounts", Value::Null, None).await;
    assert_eq!(
        empty["accounts"][0]["usage_cycles"]["current"]["totals"]["requests"],
        1
    );
    u.started_at = Some(store::now() - 18001);
    u.input_estimate = 3;
    u.output_estimate = 2;
    let mut tx = f.engine.store.db.begin().await.unwrap();
    sqlx::query("DELETE FROM usage")
        .execute(&mut *tx)
        .await
        .unwrap();
    for _ in 0..10005 {
        sqlx::query("INSERT INTO usage(at,data) VALUES(?,?)")
            .bind(store::now() - 18001)
            .bind(serde_json::to_string(&u).unwrap())
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    u.started_at = Some(store::now());
    sqlx::query("INSERT INTO usage(at,data) VALUES(?,?)")
        .bind(store::now())
        .bind(serde_json::to_string(&u).unwrap())
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let cycles = f
        .engine
        .store
        .account_cycles(vec![id], store::now())
        .await
        .unwrap();
    let cycle = cycles.values().next().unwrap();
    assert_eq!(cycle["previous"]["totals"]["requests"], 10005);
    assert_eq!(cycle["previous"]["totals"]["input_estimate"], 30015);
    assert_eq!(cycle["current"]["totals"]["requests"], 1);
}
#[tokio::test]
async fn admin_scopes_revisions_import_disable_and_cleanup() {
    let f = fixture(1).await;
    let (status, _) = admin(
        &f.engine,
        "GET",
        "/info",
        Value::Null,
        Some(&f.engine.config.bot_key),
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = admin(
        &f.engine,
        "GET",
        "/accounts",
        Value::Null,
        Some(&f.engine.config.bot_key),
    )
    .await;
    assert_eq!(status, 403);
    let (status, _) = admin(
        &f.engine,
        "GET",
        "/info",
        Value::Null,
        Some(&f.engine.config.gateway_key),
    )
    .await;
    assert_eq!(status, 401);
    let (_, settings) = admin(&f.engine, "GET", "/settings", Value::Null, None).await;
    let revision = settings["revision"].as_u64().unwrap();
    let(status,import)=admin(&f.engine,"POST","/accounts/import",json!({"expected_revision":revision,"accounts":[{"credential":"fake-added-account"},{"credential":"fake-added-account"}]}),None).await;
    assert_eq!(status, 200, "{import}");
    assert_eq!(import["added"], 1);
    let (status, _) = admin(
        &f.engine,
        "PUT",
        "/settings",
        json!({"expected_revision":revision,"settings":settings["settings"]}),
        None,
    )
    .await;
    assert_eq!(status, 409);
    let state = f.engine.store.read().await;
    let id = state.accounts.keys().next().unwrap();
    let (status, _) = admin(
        &f.engine,
        "PATCH",
        &format!("/accounts/{id}"),
        json!({"expected_revision":state.revision,"enabled":false}),
        None,
    )
    .await;
    assert_eq!(status, 200);
    let (_, accounts) = admin(&f.engine, "GET", "/accounts", Value::Null, None).await;
    assert!(!accounts.to_string().contains("fake-added-account"));
}
#[tokio::test]
async fn expiry_and_manual_cleanup_respect_active_and_account_guards() {
    let f = fixture(1).await;
    let (_, headers, _) = call(&f.engine, "/v1/messages", base(), Some("expiry")).await;
    let id = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    f.engine
        .store
        .mutate("", json!({}), |s| {
            let current = s.sessions.get_mut(&id).unwrap();
            current.expires_at = store::now() - 1;
            current.active = true;
            Ok(())
        })
        .await
        .unwrap();
    let revision = f.engine.store.read().await.revision;
    let (status, _) = admin(
        &f.engine,
        "DELETE",
        &format!("/sessions/{id}"),
        json!({"expected_revision":revision}),
        None,
    )
    .await;
    assert_eq!(status, 409);
    f.engine.sweep().await.unwrap();
    assert!(f.engine.store.read().await.sessions.contains_key(&id));
    f.engine
        .store
        .mutate("", json!({}), |s| {
            s.sessions.get_mut(&id).unwrap().active = false;
            for a in s.accounts.values_mut() {
                a.paused = true;
            }
            Ok(())
        })
        .await
        .unwrap();
    f.engine.sweep().await.unwrap();
    assert!(f.engine.store.read().await.sessions.is_empty());
    assert_eq!(f.upstream.deleted.load(Ordering::SeqCst), 0);
    f.engine
        .store
        .mutate("", json!({}), |s| {
            for a in s.accounts.values_mut() {
                a.paused = false;
                a.last_start = 0;
                a.starts.clear();
            }
            Ok(())
        })
        .await
        .unwrap();
    f.engine.sweep().await.unwrap();
    assert_eq!(f.upstream.deleted.load(Ordering::SeqCst), 1);
    assert!(f.engine.store.read().await.pending.is_empty());
}
#[tokio::test]
async fn cross_origin_and_gateway_cookie_are_rejected() {
    let f = fixture(1).await;
    let request = axum::http::Request::builder()
        .method("PUT")
        .uri("/control/v1/settings")
        .header(
            "authorization",
            format!("Bearer {}", f.engine.config.admin_key),
        )
        .header("host", "127.0.0.1:18082")
        .header("origin", "https://evil.invalid")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(
        control::router(f.engine.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let request = axum::http::Request::builder()
        .uri("/v1/models")
        .header(
            "cookie",
            format!("sessionKey={}", f.engine.config.gateway_key),
        )
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        web::public(f.engine.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn disconnect_cancels_reader_before_releasing_account() {
    let f = fixture(1).await;
    f.upstream.replies.lock().await.push_back(Reply::Slow);
    let mut p = base();
    p["stream"] = json!(true);
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", &f.engine.config.gateway_key)
        .body(Body::from(p.to_string()))
        .unwrap();
    let response = web::public(f.engine.clone())
        .oneshot(request)
        .await
        .unwrap();
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let mut body = response.into_body();
    let first = body.frame().await.unwrap().unwrap();
    assert!(!first.data_ref().unwrap().is_empty());
    drop(body);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let mut all_released = true;
            for id in f.engine.store.read().await.accounts.keys() {
                all_released &= !f.engine.pool.busy(id).await;
            }
            if f.upstream.streams.load(Ordering::SeqCst) == 0
                && all_released
                && f.engine
                    .store
                    .read()
                    .await
                    .sessions
                    .values()
                    .all(|s| !s.active)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    for id in f.engine.store.read().await.accounts.keys() {
        assert!(!f.engine.pool.busy(id).await);
    }
    f.engine.store.flush_status().await.unwrap();
    let records = f
        .engine
        .store
        .status_logs("error", "", None, 100)
        .await
        .unwrap();
    for direction in ["upstream", "downstream"] {
        assert!(
            records["records"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["request_id"] == request_id
                    && r["direction"] == direction
                    && r["status"] == 499)
        );
    }
}

#[tokio::test]
async fn compacted_checkpoint_and_cache_survive_profile_change_and_restart() {
    let f = fixture(1).await;
    let (id, mut payload) = compaction_input(&f).await;
    f.upstream.replies.lock().await.extend([Reply::Summary(r#"{"goal":"continue","constraints":[],"completed":[],"facts":["checkpoint-marker"],"open":[],"next":["continue"]}"#.to_owned()),Reply::Text("continued".to_owned())]);
    let (status, _, out) = call(&f.engine, "/v1/messages", payload.clone(), Some(&id)).await;
    assert_eq!(status, 200, "{out}");
    let before = f.engine.store.read().await;
    let session = before.sessions[&id].clone();
    assert!(session.checkpoint.is_some());
    let status = admin(
        &f.engine,
        "PATCH",
        &format!("/accounts/{}", session.account_id),
        json!({"expected_revision":before.revision,"browser_profile":"firefox_144_windows"}),
        None,
    )
    .await
    .0;
    assert_eq!(status, 200);
    let after = f.engine.store.read().await;
    assert_eq!(
        serde_json::to_value(&session).unwrap(),
        serde_json::to_value(&after.sessions[&id]).unwrap()
    );
    f.engine.store.flush_status().await.unwrap();
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&f.engine.store.db)
        .await
        .unwrap();
    let copy = tempfile::tempdir().unwrap();
    std::fs::copy(
        f.engine.config.data_dir.join("control.sqlite3"),
        copy.path().join("control.sqlite3"),
    )
    .unwrap();
    let mut config = f.engine.config.clone();
    config.data_dir = copy.path().to_owned();
    let restarted = Engine::open(config).await.unwrap();
    assert_eq!(
        serde_json::to_value(&session).unwrap(),
        serde_json::to_value(&restarted.store.read().await.sessions[&id]).unwrap()
    );
    restarted
        .store
        .mutate("", json!({}), |s| {
            s.settings.compaction_enabled = false;
            Ok(())
        })
        .await
        .unwrap();
    reset_pacing(&restarted).await;
    payload["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":out["content"]}),
        json!({"role":"user","content":"after-restart"}),
    ]);
    let (status, _, out) = call(&restarted, "/v1/messages", payload, Some(&id)).await;
    assert_eq!(status, 200, "{out}");
    assert!(out["usage"]["cache_read_input_tokens"].as_u64().unwrap() > 0);
    let state = restarted.store.read().await;
    assert_eq!(state.sessions[&id].conversation_id, session.conversation_id);
    assert_eq!(
        serde_json::to_value(&state.sessions[&id].checkpoint).unwrap(),
        serde_json::to_value(&session.checkpoint).unwrap()
    );
    let traces = f.upstream.traces.lock().await;
    let completion = traces
        .iter()
        .rev()
        .find(|v| v["uri"].as_str().unwrap().ends_with("/completion"))
        .unwrap();
    assert_eq!(
        completion["body"]["parent_message_uuid"],
        session.parent_uuid
    );
    assert!(
        completion["headers"]["user-agent"]
            .as_str()
            .unwrap()
            .contains("Firefox/144")
    );
}
#[derive(Debug, serde::Serialize)]
struct CompactionComparison {
    first_compaction_round: usize,
    summaries: usize,
    business_generations: usize,
}

const COMPACTION_COMPARISON_ROUNDS: usize = 112;

async fn exercise_compaction_policy(previous_defaults: bool) -> CompactionComparison {
    let f = fixture(1).await;
    f.upstream.automatic_summaries.store(true, Ordering::SeqCst);
    f.engine
        .store
        .mutate("", json!({}), |s| {
            s.settings.compaction_enabled = true;
            if previous_defaults {
                s.settings.context_budget = 32768;
                s.settings.compaction_trigger_percent = 60;
                s.settings.keep_recent_rounds = 4;
            }
            Ok(())
        })
        .await
        .unwrap();
    let keep = f.engine.store.read().await.settings.keep_recent_rounds;
    let mut history = Vec::new();
    let mut markers = Vec::new();
    let mut handle = "policy-comparison".to_owned();
    let mut first_compaction = None;
    let mut covered = 0;
    for round in 0..COMPACTION_COMPARISON_ROUNDS {
        let marker = format!("raw-round-{round:02}-marker");
        markers.push(marker.clone());
        history.push(
            json!({"role":"user","content":format!("{marker}: {}", "fixture-data ".repeat(150))}),
        );
        reset_pacing(&f.engine).await;
        let (status, headers, out) = call(
            &f.engine,
            "/v1/messages",
            json!({"model":"test","max_tokens":8192,"messages":history}),
            Some(&handle),
        )
        .await;
        assert_eq!(status, 200, "round {round}: {out}");
        let returned = headers["x-c2a-session-id"].to_str().unwrap();
        if round == 0 {
            handle = returned.to_owned();
        } else {
            assert_eq!(returned, handle);
        }
        let state = f.engine.store.read().await;
        let session = &state.sessions[&handle];
        assert_eq!(session.history.len(), 2 * (round + 1));
        if let Some(checkpoint) = &session.checkpoint
            && checkpoint.covered > covered
        {
            first_compaction.get_or_insert(round + 1);
            // All six (or the previous four) completed pairs remain raw after rotation.
            assert_eq!(checkpoint.covered, 2 * (round - keep));
            let traces = f.upstream.traces.lock().await;
            let prompt = traces
                .iter()
                .rev()
                .find(|v| v["uri"].as_str().unwrap().ends_with("/completion"))
                .unwrap()["body"]["prompt"]
                .as_str()
                .unwrap();
            for recent in &markers[round - keep..=round] {
                assert!(prompt.contains(recent), "missing retained input {recent}");
            }
            assert!(!prompt.contains(&markers[round - keep - 1]));
            covered = checkpoint.covered;
        }
        history.push(json!({"role":"assistant","content":out["content"]}));
    }
    let traces = f.upstream.traces.lock().await;
    let prompts: Vec<_> = traces
        .iter()
        .filter(|v| v["uri"].as_str().unwrap().ends_with("/completion"))
        .map(|v| v["body"]["prompt"].as_str().unwrap())
        .collect();
    let summaries = prompts
        .iter()
        .filter(|p| p.starts_with("[Claud2Api internal compaction]"))
        .count();
    CompactionComparison {
        first_compaction_round: first_compaction.expect("long fixture should compact"),
        summaries,
        business_generations: prompts.len() - summaries,
    }
}

#[tokio::test]
async fn larger_compaction_defaults_delay_summaries_and_preserve_recent_raw_rounds() {
    let previous = exercise_compaction_policy(true).await;
    let current = exercise_compaction_policy(false).await;
    assert_eq!(previous.business_generations, COMPACTION_COMPARISON_ROUNDS);
    assert_eq!(current.business_generations, COMPACTION_COMPARISON_ROUNDS);
    assert!(current.first_compaction_round > previous.first_compaction_round);
    assert!(current.summaries < previous.summaries);
    eprintln!(
        "compaction_policy_comparison={}",
        json!({"previous":previous,"current":current})
    );
}

#[tokio::test]
async fn saved_compaction_policy_survives_restart_until_explicit_control_update() {
    let f = fixture(0).await;
    let initial = f.engine.store.read().await.settings;
    assert_eq!(
        (
            initial.context_budget,
            initial.compaction_trigger_percent,
            initial.keep_recent_rounds
        ),
        (128000, 75, 6)
    );
    let mut previous = initial.clone();
    previous.context_budget = 32768;
    previous.compaction_trigger_percent = 60;
    previous.keep_recent_rounds = 4;
    let mut intermediate = initial.clone();
    intermediate.context_budget = 65536;
    let mut previous_large = initial.clone();
    previous_large.context_budget = 200000;
    let mut engine = f.engine.clone();
    let mut copies = Vec::new();
    for policy in [previous, intermediate, previous_large, initial] {
        let revision = engine.store.read().await.revision;
        let (status, out) = admin(
            &engine,
            "PUT",
            "/settings",
            json!({"expected_revision":revision,"settings":policy}),
            None,
        )
        .await;
        assert_eq!(status, 200, "{out}");
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&engine.store.db)
            .await
            .unwrap();
        let copy = tempfile::tempdir().unwrap();
        std::fs::copy(
            engine.config.data_dir.join("control.sqlite3"),
            copy.path().join("control.sqlite3"),
        )
        .unwrap();
        let mut config = engine.config.clone();
        config.data_dir = copy.path().to_owned();
        config.settings = Settings::default();
        let restarted = Engine::open(config).await.unwrap();
        let (status, read) = admin(&restarted, "GET", "/settings", Value::Null, None).await;
        assert_eq!(status, 200);
        assert_eq!(read["settings"], serde_json::to_value(&policy).unwrap());
        assert_eq!(read["revision"], revision + 1);
        engine = restarted;
        copies.push(copy);
    }
}

async fn compaction_input(f: &Fixture) -> (String, Value) {
    compaction_input_with(f, base()).await
}
async fn compaction_input_with(f: &Fixture, mut v: Value) -> (String, Value) {
    let (_, headers, first) = call(&f.engine, "/v1/messages", v.clone(), Some("compact")).await;
    let id = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    let messages = v["messages"].as_array_mut().unwrap();
    messages.push(json!({"role":"assistant","content":first["content"]}));
    for _ in 0..8 {
        messages.push(json!({"role":"user","content":"x".repeat(600)}));
        messages.push(json!({"role":"assistant","content":"y".repeat(600)}));
    }
    messages.push(json!({"role":"user","content":"continue"}));
    let mut input = protocol::parse(&v).unwrap();
    input.native_tools =
        claud2api::native_tools::resolve(&v, &f.engine.store.read().await.settings).unwrap();
    let hashes = input.hashes(&f.engine.config.gateway_key);
    f.engine
        .store
        .mutate("", json!({}), |s| {
            s.settings.compaction_enabled = true;
            s.settings.context_budget = 4096;
            s.settings.compaction_trigger_percent = 60;
            s.settings.keep_recent_rounds = 1;
            let session = s.sessions.get_mut(&id).unwrap();
            session.history = hashes[..hashes.len() - 1].to_vec();
            session.context_estimate = 18000;
            Ok(())
        })
        .await
        .unwrap();
    reset_pacing(&f.engine).await;
    (id, v)
}
#[tokio::test]
async fn compaction_uploads_covered_images_to_summary_without_readding_them_to_recent_turns() {
    let f = fixture(1).await;
    let (id, v) = compaction_input_with(&f, media_request()).await;
    f.upstream.replies.lock().await.extend([
        Reply::Summary(r#"{"goal":"continue","constraints":[],"completed":[],"facts":["image contents preserved"],"open":[],"next":["continue"]}"#.to_owned()),
        Reply::Text("continued".to_owned()),
    ]);
    let (status, _, out) = call(&f.engine, "/v1/messages", v, Some(&id)).await;
    assert_eq!(status, 200, "{out}");
    assert!(
        f.engine.store.read().await.sessions[&id]
            .checkpoint
            .is_some()
    );
    assert_eq!(f.upstream.uploaded.lock().await.len(), 2);
    let traces = f.upstream.traces.lock().await;
    let calls = completion_requests(&traces);
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0]["body"]["files"].as_array().unwrap().len(), 1);
    assert_eq!(calls[1]["body"]["files"].as_array().unwrap().len(), 1);
    assert!(
        calls[1]["body"]["prompt"]
            .as_str()
            .unwrap()
            .contains("Read the corresponding attached image or document")
    );
    assert_eq!(calls[2]["body"]["files"], json!([]));
    assert!(
        calls[2]["body"]["prompt"]
            .as_str()
            .unwrap()
            .contains("image contents preserved")
    );
    assert!(
        calls
            .iter()
            .all(|call| !call["body"]["prompt"].as_str().unwrap().contains(MEDIA_PNG))
    );
}
#[tokio::test]
async fn compaction_creates_checkpoint_and_rotates_after_success() {
    let f = fixture(1).await;
    let (id, v) = compaction_input(&f).await;
    f.upstream.replies.lock().await.extend([Reply::Summary(r#"{"goal":"continue","constraints":[],"completed":["old work"],"facts":[],"open":[],"next":["next task"]}"#.to_owned()),Reply::Text("continued".to_owned())]);
    let old = f.engine.store.read().await.sessions[&id]
        .conversation_id
        .clone();
    let (status, headers, out) = call(&f.engine, "/v1/messages", v, Some(&id)).await;
    assert_eq!(status, 200, "{out}");
    let state = f.engine.store.read().await;
    let current = &state.sessions[&id];
    assert_ne!(current.conversation_id, old);
    assert!(current.checkpoint.is_some());
    assert_eq!(out["usage"]["cache_read_input_tokens"], 0);
    assert!(state.pending.iter().any(|j| j.conversation_id == old));
    let traces = f.upstream.traces.lock().await;
    let upstream_calls = completion_requests(&traces);
    assert_eq!(upstream_calls.len(), 3);
    assert_eq!(upstream_calls[1]["body"]["tools"], json!([]));
    assert_eq!(
        upstream_calls[2]["body"]["tools"],
        default_native_descriptors()
    );
    assert!(
        upstream_calls[1]["body"]["prompt"]
            .as_str()
            .unwrap()
            .starts_with("[Claud2Api internal compaction]")
    );
    drop(traces);
    let usage = f.engine.store.usage(store::now() - 18000).await.unwrap();
    assert_eq!(usage["requests"], 3);
    let logs = logs_for_request(&f.engine, headers["x-request-id"].to_str().unwrap()).await;
    let completions: Vec<_> = logs
        .iter()
        .filter(|r| r["operation"] == "completion")
        .collect();
    assert_eq!(completions.len(), 2); // Internal handoff and business generation remain separate.
    assert_ne!(
        completions[0]["usage"]["output_tokens"],
        completions[1]["usage"]["output_tokens"]
    );
    let downstream = logs
        .iter()
        .find(|r| r["direction"] == "downstream")
        .unwrap();
    assert_eq!(
        downstream["usage"]["output_tokens"],
        out["usage"]["output_tokens"]
    );
}
#[tokio::test]
async fn invalid_summaries_keep_the_known_context_and_apply_backoff() {
    for summary in ["not JSON".to_owned(), "x".repeat(8193)] {
        let f = fixture(1).await;
        let (id, input) = compaction_input(&f).await;
        let old = f.engine.store.read().await.sessions[&id]
            .conversation_id
            .clone();
        f.upstream
            .replies
            .lock()
            .await
            .extend([Reply::Summary(summary), Reply::Text("continued".to_owned())]);
        let (status, _, out) = call(&f.engine, "/v1/messages", input, Some(&id)).await;
        assert_eq!(status, 200, "{out}");
        let state = f.engine.store.read().await;
        let session = &state.sessions[&id];
        assert_eq!(session.conversation_id, old);
        assert!(session.checkpoint.is_none());
        assert!(session.compaction_retry_after > store::now());
        assert!(!state.pending.iter().any(|j| j.conversation_id == old));
    }
}
#[tokio::test]
async fn truncated_internal_generation_cannot_start_business_or_retire_known_context() {
    let f = fixture(1).await;
    let (id, input) = compaction_input(&f).await;
    let old = f.engine.store.read().await.sessions[&id]
        .conversation_id
        .clone();
    f.upstream
        .replies
        .lock()
        .await
        .extend([Reply::Truncated, Reply::Text("must not run".to_owned())]);
    let (status, _, out) = call(&f.engine, "/v1/messages", input, Some(&id)).await;
    assert_eq!(status, 502, "{out}");
    assert_eq!(f.upstream.replies.lock().await.len(), 1);
    let state = f.engine.store.read().await;
    assert_eq!(state.sessions[&id].conversation_id, old);
    assert!(!state.pending.iter().any(|j| j.conversation_id == old));
}
#[tokio::test]
async fn model_reads_consume_account_admission_pacing() {
    let f = fixture(1).await;
    f.engine
        .store
        .mutate("", json!({}), |state| {
            state.settings.account_min_interval_seconds = 300;
            Ok(())
        })
        .await
        .unwrap();
    for expected in [200, 429] {
        let response = web::public(f.engine.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/models")
                    .header("x-api-key", &f.engine.config.gateway_key)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
    }
    assert_eq!(
        f.engine
            .store
            .read()
            .await
            .accounts
            .values()
            .next()
            .unwrap()
            .starts
            .len(),
        1
    );
}
#[tokio::test]
async fn slow_upload_times_out_before_account_admission() {
    let f = fixture(1).await;
    f.engine
        .store
        .mutate("", json!({}), |s| {
            s.settings.request_timeout_seconds = 1;
            Ok(())
        })
        .await
        .unwrap();
    let body = Body::from_stream(futures_util::stream::pending::<
        Result<Bytes, std::convert::Infallible>,
    >());
    let response = tokio::time::timeout(
        Duration::from_secs(3),
        web::public(f.engine.clone()).oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("x-api-key", &f.engine.config.gateway_key)
                .body(body)
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(f.upstream.traces.lock().await.is_empty());
    assert!(
        f.engine
            .store
            .read()
            .await
            .accounts
            .values()
            .all(|a| a.starts.is_empty())
    );
}
#[tokio::test]
async fn runtime_ownership_does_not_rewrite_vault_but_named_events_remain_durable() {
    let f = fixture(1).await;
    let (_, headers, _) = call(&f.engine, "/v1/messages", base(), Some("runtime-only")).await;
    let id = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    let before: Vec<u8> = sqlx::query_scalar("SELECT payload FROM vault WHERE id=1")
        .fetch_one(&f.engine.store.db)
        .await
        .unwrap();
    f.engine
        .store
        .mutate("", json!({}), |s| {
            let session = s.sessions.get_mut(&id).unwrap();
            session.active = true;
            session.active_ticket = "runtime-ticket".to_owned();
            Ok(())
        })
        .await
        .unwrap();
    assert!(f.engine.store.read().await.sessions[&id].active);
    let after: Vec<u8> = sqlx::query_scalar("SELECT payload FROM vault WHERE id=1")
        .fetch_one(&f.engine.store.db)
        .await
        .unwrap();
    assert_eq!(before, after);
    f.engine
        .store
        .mutate(
            "fixture_named_event",
            json!({"reason":"unchanged state"}),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let after: Vec<u8> = sqlx::query_scalar("SELECT payload FROM vault WHERE id=1")
        .fetch_one(&f.engine.store.db)
        .await
        .unwrap();
    assert_ne!(before, after);
    assert!(
        f.engine.store.events(0, 200).await.unwrap()["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "fixture_named_event")
    );
}
#[tokio::test]
async fn durable_failure_latches_health_and_prevents_upstream_work() {
    let f = fixture(1).await;
    sqlx::query("DROP TABLE vault")
        .execute(&f.engine.store.db)
        .await
        .unwrap();
    let (status, _, body) = call(&f.engine, "/v1/messages", base(), Some("disk-fault")).await;
    assert_eq!(status, 503, "{body}");
    assert!(!f.engine.store.healthy());
    assert!(f.upstream.traces.lock().await.is_empty());
    let response = web::public(f.engine.clone())
        .oneshot(
            axum::http::Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}
#[tokio::test]
async fn unknown_event_before_quota_fails_after_generation_and_is_never_replayed() {
    let f = fixture(2).await;
    f.upstream.replies.lock().await.extend([
        Reply::Raw(
            event(json!({"type":"unknown_future_event"}))
                + &event(json!({"type":"error","error":{"type":"rate_limit_error"}})),
        ),
        Reply::Text("must not run".to_owned()),
    ]);
    let (status, _, _) = call(&f.engine, "/v1/messages", base(), Some("unknown-event")).await;
    assert_eq!(status, 502);
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 1);
    assert_eq!(f.upstream.replies.lock().await.len(), 1);
}
#[tokio::test]
async fn no_supported_content_fails_and_stop_sequence_survives() {
    let f = fixture(1).await;
    f.upstream
        .replies
        .lock()
        .await
        .push_back(Reply::Raw(event(json!({"type":"message_stop"}))));
    let (status, _, _) = call(&f.engine, "/v1/messages", base(), Some("empty")).await;
    assert_eq!(status, 502);
    reset_pacing(&f.engine).await;
    f.upstream.replies.lock().await.push_back(Reply::Raw(event(json!({"type":"content_block_delta","delta":{"text":"done"}}))+&event(json!({"type":"message_delta","delta":{"stop_reason":"stop_sequence","stop_sequence":"END"}}))+&event(json!({"type":"message_stop"}))));
    let (status, _, v) = call(&f.engine, "/v1/messages", base(), Some("stop")).await;
    assert_eq!(status, 200);
    assert_eq!(v["stop_sequence"], "END");
}
#[tokio::test]
async fn disabling_an_account_drains_active_work_without_releasing_its_slot() {
    let f = fixture(1).await;
    let reservation = f
        .engine
        .reserve(
            protocol::parse(&base()).unwrap(),
            "active-disable".to_owned(),
        )
        .await
        .unwrap();
    let id = reservation.lease.as_ref().unwrap().account_id.clone();
    let revision = f.engine.store.read().await.revision;
    let (status, _) = admin(
        &f.engine,
        "PATCH",
        &format!("/accounts/{id}"),
        json!({"expected_revision":revision,"enabled":false}),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert!(f.engine.pool.busy(&id).await);
    reservation.lease.as_ref().unwrap().check().await.unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let task = tokio::spawn(f.engine.clone().run(reservation, tx));
    let mut done = false;
    while let Some(output) = rx.recv().await {
        match output {
            claud2api::engine::Output::Done { .. } => done = true,
            claud2api::engine::Output::Error(e) => panic!("{e}"),
            _ => {}
        }
    }
    task.await.unwrap();
    assert!(done);
    assert!(!f.engine.pool.busy(&id).await);
    let (status, _, _) = call(&f.engine, "/v1/messages", base(), Some("new-on-disabled")).await;
    assert_eq!(status, 429);
}
#[tokio::test]
async fn runtime_settings_revision_range_and_events_are_consistent() {
    let f = fixture(1).await;
    let (_, settings) = admin(&f.engine, "GET", "/settings", Value::Null, None).await;
    let original_revision = settings["revision"].as_u64().unwrap();
    let original_settings = settings["settings"].clone();
    let mut incomplete = original_settings.clone();
    incomplete
        .as_object_mut()
        .unwrap()
        .remove("native_web_search");
    let (status, _) = admin(
        &f.engine,
        "PUT",
        "/settings",
        json!({"expected_revision":original_revision,"settings":incomplete}),
        None,
    )
    .await;
    assert_eq!(status, 400);
    let (_, after_incomplete) = admin(&f.engine, "GET", "/settings", Value::Null, None).await;
    assert_eq!(after_incomplete["revision"], original_revision);
    assert_eq!(after_incomplete["settings"], original_settings);

    let mut invalid = settings["settings"].clone();
    invalid["account_min_interval_seconds"] = json!(0);
    let (status, _) = admin(
        &f.engine,
        "PUT",
        "/settings",
        json!({"expected_revision":settings["revision"],"settings":invalid}),
        None,
    )
    .await;
    assert_eq!(status, 400);
    let (status, updated) = admin(
        &f.engine,
        "PUT",
        "/settings",
        json!({"expected_revision":settings["revision"],"settings":settings["settings"]}),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert!(updated["revision"].as_u64().unwrap() > settings["revision"].as_u64().unwrap());
    let (_, events) = admin(
        &f.engine,
        "GET",
        "/events",
        Value::Null,
        Some(&f.engine.config.bot_key),
    )
    .await;
    let cursor = events["cursor"].as_i64().unwrap();
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "settings_updated")
    );
    let (_, next) = admin(
        &f.engine,
        "GET",
        &format!("/events?after={cursor}"),
        Value::Null,
        Some(&f.engine.config.bot_key),
    )
    .await;
    assert!(next["events"].as_array().unwrap().is_empty());
}
#[tokio::test]
async fn corruption_and_key_change_fail_closed_without_overwriting_vault() {
    let f = fixture(1).await;
    let config = f.engine.config.clone();
    let path = config.data_dir.clone();
    f.engine
        .store
        .mutate("", json!({}), |s| {
            s.settings.simulated_cache = true;
            Ok(())
        })
        .await
        .unwrap();
    let second = path.join("corrupt-copy");
    std::fs::create_dir_all(&second).unwrap();
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&f.engine.store.db)
        .await
        .unwrap();
    std::fs::copy(path.join("control.sqlite3"), second.join("control.sqlite3")).unwrap();
    let mut bad = config.clone();
    bad.data_dir = second;
    bad.gateway_key = "changed-gateway-key-that-cannot-decrypt-existing-state".to_owned();
    assert!(Store::open(&bad).await.is_err());
    let third = path.join("corrupt-schema");
    std::fs::create_dir_all(&third).unwrap();
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(third.join("control.sqlite3"))
                .create_if_missing(true),
        )
        .await
        .unwrap();
    sqlx::query("PRAGMA user_version=999")
        .execute(&db)
        .await
        .unwrap();
    db.close().await;
    let mut bad = config;
    bad.data_dir = third;
    assert!(Store::open(&bad).await.is_err());
}
#[tokio::test]
async fn abandoned_prepared_reservation_releases_logical_and_physical_ownership() {
    let f = fixture(1).await;
    let request = f
        .engine
        .reserve(protocol::parse(&base()).unwrap(), "abandoned".to_owned())
        .await
        .unwrap();
    let id = request.session.id.clone();
    assert!(f.engine.store.read().await.sessions[&id].active);
    drop(request);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !f.engine.store.read().await.sessions.contains_key(&id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    reset_pacing(&f.engine).await;
    assert!(
        f.engine
            .reserve(
                protocol::parse(&base()).unwrap(),
                "after-abandon".to_owned()
            )
            .await
            .is_ok()
    );
}
#[tokio::test]
async fn migration_wait_is_cancellable_without_leaving_a_busy_session() {
    let f = fixture(2).await;
    let (_, headers, first) = call(&f.engine, "/v1/messages", base(), Some("waiting")).await;
    let id = headers["x-c2a-session-id"].to_str().unwrap().to_owned();
    let old = f.engine.store.read().await.sessions[&id].account_id.clone();
    f.engine.pool.observe(&old, 429, None).await.unwrap();
    let held = f
        .engine
        .reserve(protocol::parse(&base()).unwrap(), "hold-other".to_owned())
        .await
        .unwrap();
    let mut next = base();
    next["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":first["content"]}),
        json!({"role":"user","content":"next"}),
    ]);
    let request = f
        .engine
        .reserve(protocol::parse(&next).unwrap(), id.clone())
        .await
        .unwrap();
    assert!(request.lease.is_none());
    let cancel = request.scope.cancel.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let task = tokio::spawn(f.engine.clone().run(request, tx));
    assert!(matches!(
        rx.recv().await,
        Some(claud2api::engine::Output::Ping)
    ));
    cancel.cancel();
    task.await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !f.engine.store.read().await.sessions[&id].active {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.engine.store.read().await.sessions[&id].account_id, old);
    drop(held);
}
#[tokio::test]
async fn access_failure_pauses_account_and_cannot_trigger_failover_or_cleanup() {
    let f = fixture(2).await;
    f.upstream.replies.lock().await.extend([
        Reply::Http(403),
        Reply::Text("should remain queued".to_owned()),
    ]);
    let (status, _, body) = call(&f.engine, "/v1/messages", base(), Some("access-failure")).await;
    assert_eq!(status, 502, "{body}");
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 1);
    assert_eq!(f.upstream.replies.lock().await.len(), 1);
    let snapshot = f.engine.store.read().await;
    let paused = snapshot.accounts.values().find(|a| a.paused).unwrap();
    assert_eq!(snapshot.pending[0].account_id, paused.id);
    f.engine.sweep().await.unwrap();
    assert_eq!(f.upstream.deleted.load(Ordering::SeqCst), 0);
}

async fn managed_token(e: &Arc<Engine>, limit: usize) -> (String, String) {
    let revision = e.store.read().await.revision;
    let (status, result) = admin(e, "POST", "/tokens", json!({"expected_revision":revision,"label":"fixture token","max_concurrency":limit,"expires_at":null}), None).await;
    assert_eq!(status, 200, "{result}");
    (
        result["id"].as_str().unwrap().to_owned(),
        result["token"].as_str().unwrap().to_owned(),
    )
}
async fn public_raw(
    e: &Arc<Engine>,
    method: &str,
    path: &str,
    key: &str,
    payload: Value,
) -> Response {
    web::public(e.clone())
        .oneshot(
            axum::http::Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(if payload.is_null() {
                    String::new()
                } else {
                    payload.to_string()
                }))
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn model_update(e: &Arc<Engine>, models: Value) -> (u16, Value) {
    admin(
        e,
        "PUT",
        "/models",
        json!({"expected_revision":e.store.read().await.revision,"models":models}),
        None,
    )
    .await
}

#[tokio::test]
async fn model_configuration_controls_catalog_and_all_six_response_presentations() {
    let f = fixture(1).await;
    let config = json!({"enabled":true,"entries":[
        {"id":"work","display_name":"工作模型","upstream_model":"claude-haiku-5-5","enabled":true},
        {"id":"c2a-claude-haiku-5-5","display_name":"兼容模型","upstream_model":"claude-haiku-5-5","enabled":true},
        {"id":"hidden","display_name":"停用","upstream_model":"test","enabled":false},
        {"id":"missing","display_name":"无权限","upstream_model":"not-in-catalog","enabled":true}
    ]});
    assert_eq!(model_update(&f.engine, config).await.0, 200);
    let response = public_raw(
        &f.engine,
        "GET",
        "/v1/models",
        &f.engine.config.gateway_key,
        Value::Null,
    )
    .await;
    assert_eq!(response.status(), 200);
    let list: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(list["data"].as_array().unwrap().len(), 2);
    assert_eq!(list["data"][0]["id"], "work");
    assert_eq!(list["data"][0]["display_name"], "工作模型");
    for path in ["/v1/messages", "/v1/chat/completions", "/v1/responses"] {
        for stream in [false, true] {
            reset_pacing(&f.engine).await;
            let mut payload = base();
            payload["model"] = json!("work");
            payload["stream"] = json!(stream);
            payload["input"] = json!("hello");
            let (status, _, body) = call(&f.engine, path, payload, None).await;
            assert_eq!(status, 200, "{path}: {body}");
            let values = if stream {
                sse_values(body.as_str().unwrap())
            } else {
                json!([body])
            };
            fn assert_model(value: &Value) -> usize {
                match value {
                    Value::Array(items) => items.iter().map(assert_model).sum(),
                    Value::Object(items) => items
                        .iter()
                        .map(|(k, v)| {
                            if k == "model" {
                                assert_eq!(v, "work");
                                1
                            } else {
                                assert_model(v)
                            }
                        })
                        .sum(),
                    _ => 0,
                }
            }
            assert!(assert_model(&values) > 0, "{values}");
        }
    }
    reset_pacing(&f.engine).await;
    let mut payload = base();
    payload["model"] = json!("c2a-claude-haiku-5-5");
    let (status, _, body) = call(&f.engine, "/v1/messages", payload, None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["model"], "c2a-claude-haiku-5-5");
    assert!(
        f.upstream
            .traces
            .lock()
            .await
            .iter()
            .filter(|v| v["uri"].as_str().unwrap().ends_with("/completion"))
            .all(|v| v["body"]["model"] == "claude-haiku-5-5")
    );
    let created = f.upstream.created.load(Ordering::SeqCst);
    for model in ["hidden", "missing", "test", "claude-haiku-5-5"] {
        reset_pacing(&f.engine).await;
        let mut payload = base();
        payload["model"] = json!(model);
        assert_eq!(call(&f.engine, "/v1/messages", payload, None).await.0, 400);
    }
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), created);
    f.engine.store.flush_status().await.unwrap();
    let (_, logs) = admin(&f.engine, "GET", "/logs?limit=100", Value::Null, None).await;
    assert!(
        logs["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["model"] == "work")
    );
}

#[tokio::test]
async fn model_mutations_validate_and_preserve_native_defaults_and_revision() {
    let f = fixture(1).await;
    let (_, original) = admin(&f.engine, "GET", "/models", Value::Null, None).await;
    assert_eq!(original["models"]["enabled"], false);
    let entry = json!({"id":"work","display_name":"Name","upstream_model":"test","enabled":true});
    for entries in [
        json!([entry.clone(), entry.clone()]),
        json!([{ "id":"bad id","display_name":"X","upstream_model":"test","enabled":true}]),
        json!([{ "id":"work","display_name":"X","upstream_model":"c2a-claude-haiku-5-5","enabled":true}]),
    ] {
        assert_eq!(
            model_update(&f.engine, json!({"enabled":true,"entries":entries}))
                .await
                .0,
            400
        );
    }
    assert_eq!(
        model_update(&f.engine, json!({"enabled":true,"entries":[entry]}))
            .await
            .0,
        200
    );
    assert_eq!(
        admin(
            &f.engine,
            "PUT",
            "/models",
            json!({"expected_revision":original["revision"],"models":original["models"]}),
            None
        )
        .await
        .0,
        409
    );
    for path in ["/models", "/tokens"] {
        assert_eq!(
            admin(
                &f.engine,
                "GET",
                path,
                Value::Null,
                Some(&f.engine.config.bot_key)
            )
            .await
            .0,
            403
        );
    }
    // Turning off management preserves the stored draft and native prefix behavior.
    let mut config = f.engine.store.read().await.models;
    config.enabled = false;
    assert_eq!(
        model_update(&f.engine, serde_json::to_value(config).unwrap())
            .await
            .0,
        200
    );
    let mut payload = base();
    payload["model"] = json!("c2a-claude-haiku-5-5");
    assert_eq!(
        call(&f.engine, "/v1/messages", payload, None).await.2["model"],
        "claude-haiku-5-5"
    );
}

#[tokio::test]
async fn managed_tokens_are_one_time_scoped_hashed_and_revision_checked() {
    let f = fixture(1).await;
    for (limit, expiry) in [
        (0, Value::Null),
        (513, Value::Null),
        (1, json!(store::now() - 1)),
    ] {
        let revision = f.engine.store.read().await.revision;
        assert_eq!(admin(&f.engine,"POST","/tokens",json!({"expected_revision":revision,"label":"x","max_concurrency":limit,"expires_at":expiry}),None).await.0,400);
    }
    let old_revision = f.engine.store.read().await.revision;
    let (id, secret) = managed_token(&f.engine, 2).await;
    assert_eq!(secret.len(), 72);
    assert!(secret.starts_with("c2a_tok_"));
    let (_, list) = admin(&f.engine, "GET", "/tokens", Value::Null, None).await;
    assert_eq!(list["tokens"][0]["id"], id);
    assert_eq!(list["tokens"][0]["max_concurrency"], 2);
    assert_eq!(list["environment_key_enabled"], true);
    assert!(list["tokens"][0].get("digest").is_none());
    assert!(!list.to_string().contains(&secret));
    assert_eq!(
        admin(&f.engine, "GET", "/tokens", Value::Null, Some(&secret))
            .await
            .0,
        401
    );
    assert_eq!(
        admin(
            &f.engine,
            "DELETE",
            &format!("/tokens/{id}"),
            json!({"expected_revision":old_revision}),
            None
        )
        .await
        .0,
        409
    );
    assert_eq!(
        call_key(&f.engine, "/v1/messages", base(), None, &secret)
            .await
            .0,
        200
    );
    let (_, events) = admin(&f.engine, "GET", "/events?limit=100", Value::Null, None).await;
    assert!(!events.to_string().contains(&secret));
    let snapshot = f.engine.store.read().await;
    assert_ne!(snapshot.tokens[&id].digest, secret);
    assert_eq!(
        snapshot.tokens[&id].digest,
        claud2api::tokens::digest(&f.engine.config.gateway_key, &secret)
    );
    assert!(!serde_json::to_string(&snapshot).unwrap().contains(&secret));
    let bytes = std::fs::read(f.engine.config.data_dir.join("control.sqlite3")).unwrap();
    assert!(!bytes.windows(secret.len()).any(|v| v == secret.as_bytes()));
    f.engine.store.flush_status().await.unwrap();
    let (_, logs) = admin(&f.engine, "GET", "/logs?limit=100", Value::Null, None).await;
    assert!(
        logs["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["token_id"] == id)
    );
    assert!(!logs.to_string().contains(&secret));
}

#[tokio::test]
async fn token_limit_covers_unread_json_and_stream_drop_and_is_independent_per_token() {
    let f = fixture(1).await;
    let (id, key) = managed_token(&f.engine, 1).await;
    let (_, other) = managed_token(&f.engine, 1).await;
    let first = public_raw(&f.engine, "GET", "/v1/models", &key, Value::Null).await;
    assert_eq!(first.status(), 200);
    assert_eq!(f.engine.tokens.busy(&id, 1).await, 1);
    let second = public_raw(&f.engine, "GET", "/v1/models", &key, Value::Null).await;
    assert_eq!(second.status(), 429);
    reset_pacing(&f.engine).await;
    assert_eq!(
        public_raw(&f.engine, "GET", "/v1/models", &other, Value::Null)
            .await
            .status(),
        200
    );
    drop(first);
    assert_eq!(f.engine.tokens.busy(&id, 1).await, 0);
    reset_pacing(&f.engine).await;
    let mut payload = base();
    payload["stream"] = json!(true);
    let stream = public_raw(&f.engine, "POST", "/v1/messages", &key, payload).await;
    assert_eq!(stream.status(), 200);
    assert_eq!(f.engine.tokens.busy(&id, 1).await, 1);
    assert_eq!(f.engine.tokens.admit(&key).await.err().unwrap().status, 429);
    drop(stream);
    assert_eq!(f.engine.tokens.busy(&id, 1).await, 0);
    let guard = f.engine.tokens.admit(&key).await.unwrap();
    drop(guard);
    let root = f
        .engine
        .tokens
        .admit(&f.engine.config.gateway_key)
        .await
        .unwrap();
    assert!(root.id.is_none());
}

#[tokio::test]
async fn token_capacity_expiry_and_deletion_drain_without_revoking_active_work() {
    let f = fixture(1).await;
    let (id, key) = managed_token(&f.engine, 2).await;
    let one = f.engine.tokens.admit(&key).await.unwrap();
    let two = f.engine.tokens.admit(&key).await.unwrap();
    assert_eq!(f.engine.tokens.admit(&key).await.err().unwrap().status, 429);
    drop(two);
    f.engine
        .store
        .mutate("", json!({}), |s| {
            s.tokens.get_mut(&id).unwrap().expires_at = Some(store::now() - 1);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(f.engine.tokens.admit(&key).await.err().unwrap().status, 401);
    assert_eq!(f.engine.tokens.busy(&id, 2).await, 1);
    drop(one);
    let (id, key) = managed_token(&f.engine, 1).await;
    reset_pacing(&f.engine).await;
    let mut payload = base();
    payload["stream"] = json!(true);
    let response = public_raw(&f.engine, "POST", "/v1/messages", &key, payload).await;
    assert_eq!(response.status(), 200);
    let revision = f.engine.store.read().await.revision;
    assert_eq!(
        admin(
            &f.engine,
            "DELETE",
            &format!("/tokens/{id}"),
            json!({"expected_revision":revision}),
            None
        )
        .await
        .0,
        200
    );
    assert_eq!(f.engine.tokens.admit(&key).await.err().unwrap().status, 401);
    let text = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(text.contains("message_stop"), "{text}");
    assert_eq!(f.engine.tokens.busy(&id, 1).await, 0);
}

#[tokio::test]
async fn managed_tokens_isolate_fingerprints_handles_and_legacy_continuations_and_delete() {
    let f = fixture(1).await;
    let (_, a) = managed_token(&f.engine, 1).await;
    let (_, b) = managed_token(&f.engine, 1).await;
    let (status, headers, _) =
        call_key(&f.engine, "/v1/messages", base(), Some("same-client"), &a).await;
    assert_eq!(status, 200);
    let handle = headers["x-c2a-session-id"].to_str().unwrap();
    reset_pacing(&f.engine).await;
    let (status, headers, _) =
        call_key(&f.engine, "/v1/messages", base(), Some("same-client"), &b).await;
    assert_eq!(status, 200);
    assert_ne!(headers["x-c2a-session-id"], handle);
    assert_eq!(
        call_key(&f.engine, "/v1/messages", base(), Some(handle), &b)
            .await
            .0,
        403
    );
    assert_eq!(
        call(&f.engine, "/v1/messages", base(), Some(handle))
            .await
            .0,
        403
    );
    let mut payload = base();
    payload["conversation_id"] = json!("shared-id");
    let before = f.upstream.created.load(Ordering::SeqCst);
    for token in [&a, &b, &f.engine.config.gateway_key] {
        reset_pacing(&f.engine).await;
        assert_eq!(
            call_key(
                &f.engine,
                "/v1/chat/completions",
                payload.clone(),
                None,
                token
            )
            .await
            .0,
            200
        );
    }
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), before + 3);
    reset_pacing(&f.engine).await;
    assert_eq!(
        call_key(&f.engine, "/v1/chat/completions", payload.clone(), None, &a)
            .await
            .0,
        200
    );
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), before + 3);
    reset_pacing(&f.engine).await;
    let deleted = public_raw(
        &f.engine,
        "DELETE",
        "/v1/conversations/shared-id",
        &a,
        Value::Null,
    )
    .await;
    assert_eq!(deleted.status(), 200);
    drop(deleted);
    assert_eq!(
        public_raw(
            &f.engine,
            "DELETE",
            "/v1/conversations/shared-id",
            &a,
            Value::Null
        )
        .await
        .status(),
        404
    );
    reset_pacing(&f.engine).await;
    assert_eq!(
        call_key(&f.engine, "/v1/chat/completions", payload, None, &b)
            .await
            .0,
        200
    );
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), before + 3);
    assert_eq!(f.engine.legacy.lock().await.len(), 2);
}

#[tokio::test]
async fn models_and_token_hashes_survive_restart_and_old_snapshots_have_safe_defaults() {
    let f = fixture(1).await;
    let (id, key) = managed_token(&f.engine, 3).await;
    assert_eq!(model_update(&f.engine,json!({"enabled":true,"entries":[{"id":"work","display_name":"Working","upstream_model":"test","enabled":true}]})).await.0,200);
    let copy = tempfile::tempdir().unwrap();
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&f.engine.store.db)
        .await
        .unwrap();
    std::fs::copy(
        f.engine.config.data_dir.join("control.sqlite3"),
        copy.path().join("control.sqlite3"),
    )
    .unwrap();
    let mut config = f.engine.config.clone();
    config.data_dir = copy.path().to_owned();
    let restarted = Engine::open(config).await.unwrap();
    assert_eq!(
        restarted.tokens.admit(&key).await.unwrap().id.as_deref(),
        Some(id.as_str())
    );
    assert_eq!(
        restarted
            .store
            .read()
            .await
            .models
            .resolve("work", "")
            .unwrap()
            .upstream,
        "test"
    );
    let mut old = serde_json::to_value(f.engine.store.read().await).unwrap();
    old.as_object_mut().unwrap().remove("models");
    old.as_object_mut().unwrap().remove("tokens");
    let restored: store::Snapshot = serde_json::from_value(old).unwrap();
    assert!(!restored.models.enabled);
    assert!(restored.tokens.is_empty());
}

#[tokio::test]
async fn token_auth_rejects_conflicting_or_duplicate_headers_and_releases_failed_admission() {
    let f = fixture(1).await;
    let (id, key) = managed_token(&f.engine, 1).await;
    for (name, value) in [
        (
            "authorization",
            format!("Bearer {}", f.engine.config.gateway_key),
        ),
        ("x-api-key", key.clone()),
    ] {
        let response = web::public(f.engine.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/v1/models")
                    .header("x-api-key", &key)
                    .header(name, value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
    }
    let result = public_raw(
        &f.engine,
        "POST",
        "/v1/messages",
        &key,
        json!({"model":"test"}),
    )
    .await;
    assert_eq!(result.status(), 400);
    assert_eq!(f.engine.tokens.busy(&id, 1).await, 1);
    drop(result);
    assert_eq!(f.engine.tokens.busy(&id, 1).await, 0);
}

#[tokio::test]
async fn token_limits_cannot_expand_account_capacity_and_capture_inflight_model_selection() {
    let f = fixture(1).await;
    let (_, a) = managed_token(&f.engine, 3).await;
    let (_, b) = managed_token(&f.engine, 3).await;
    assert_eq!(model_update(&f.engine,json!({"enabled":true,"entries":[{"id":"work","display_name":"Original","upstream_model":"test","enabled":true}]})).await.0,200);
    f.upstream.replies.lock().await.push_back(Reply::Slow);
    let mut payload = base();
    payload["model"] = json!("work");
    payload["stream"] = json!(true);
    let response = public_raw(&f.engine, "POST", "/v1/messages", &a, payload).await;
    assert_eq!(response.status(), 200);
    reset_pacing(&f.engine).await;
    let mut next = base();
    next["model"] = json!("work");
    assert_eq!(
        call_key(&f.engine, "/v1/messages", next, None, &b).await.0,
        429
    );
    assert_eq!(f.upstream.streams.load(Ordering::SeqCst), 1);
    // An edit after admission must not rewrite an existing SSE response's ID.
    assert_eq!(
        model_update(&f.engine, json!({"enabled":true,"entries":[]}))
            .await
            .0,
        200
    );
    let mut body = response.into_body().into_data_stream();
    let chunk = body.next().await.unwrap().unwrap();
    let text = String::from_utf8(chunk.to_vec()).unwrap();
    assert!(text.contains("\"model\":\"work\""), "{text}");
    drop(body);
    tokio::time::timeout(Duration::from_secs(2), async {
        while f.upstream.streams.load(Ordering::SeqCst) > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn exact_model_entries_override_normalized_aliases_even_when_disabled() {
    let f = fixture(1).await;
    let native = json!({"id":"claude-haiku-5-5","display_name":"Native","upstream_model":"claude-haiku-5-5","enabled":true});
    let bridge = json!({"id":"c2a-claude-haiku-5-5","display_name":"Override","upstream_model":"test","enabled":true});
    let mut config = json!({"enabled":true,"entries":[native.clone(),bridge]});
    assert_eq!(model_update(&f.engine, config.clone()).await.0, 200);
    let mut payload = base();
    payload["model"] = json!("c2a-claude-haiku-5-5");
    assert_eq!(
        call(&f.engine, "/v1/messages", payload.clone(), None)
            .await
            .2["model"],
        "c2a-claude-haiku-5-5"
    );
    assert_eq!(
        f.upstream
            .traces
            .lock()
            .await
            .iter()
            .rev()
            .find(|v| v["uri"].as_str().unwrap().ends_with("/completion"))
            .unwrap()["body"]["model"],
        "test"
    );
    config["entries"][1]["enabled"] = json!(false);
    assert_eq!(model_update(&f.engine, config).await.0, 200);
    assert_eq!(
        call(&f.engine, "/v1/messages", payload.clone(), None)
            .await
            .0,
        400
    );
    assert_eq!(
        model_update(&f.engine, json!({"enabled":true,"entries":[native]}))
            .await
            .0,
        200
    );
    reset_pacing(&f.engine).await;
    assert_eq!(
        call(&f.engine, "/v1/messages", payload, None).await.2["model"],
        "claude-haiku-5-5"
    );
}

#[tokio::test]
async fn token_name_logs_and_frozen_cache_costs_survive_token_deletion() {
    let f = fixture(1).await;
    let (id, key) = managed_token(&f.engine, 1).await;
    let mut payload = base();
    payload["model"] = json!("claude-haiku-5-5");
    let (status, headers, first) = call_key(
        &f.engine,
        "/v1/messages",
        payload.clone(),
        Some("cost-session"),
        &key,
    )
    .await;
    assert_eq!(status, 200);
    let handle = headers["x-c2a-session-id"].to_str().unwrap();
    payload["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":first["content"]}),
        json!({"role":"user","content":"continue"}),
    ]);
    reset_pacing(&f.engine).await;
    assert_eq!(
        call_key(
            &f.engine,
            "/v1/messages",
            payload.clone(),
            Some(handle),
            &key
        )
        .await
        .0,
        200
    );
    // Disable cache for the next request, without repricing the two saved rows.
    f.engine
        .store
        .mutate("", json!({}), |s| {
            s.settings.simulated_cache = false;
            Ok(())
        })
        .await
        .unwrap();
    reset_pacing(&f.engine).await;
    assert_eq!(
        call_key(
            &f.engine,
            "/v1/messages",
            {
                let mut p = base();
                p["model"] = json!("claude-haiku-5-5");
                p
            },
            Some("cache-off"),
            &key
        )
        .await
        .0,
        200
    );
    let rows: Vec<String> = sqlx::query_scalar("SELECT data FROM usage ORDER BY seq")
        .fetch_all(&f.engine.store.db)
        .await
        .unwrap();
    let records: Vec<store::Usage> = rows
        .iter()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(records.len(), 3);
    assert!(
        records.iter().all(|r| r.token_id.as_deref() == Some(&id)
            && r.token_name.as_deref() == Some("fixture token"))
    );
    assert_eq!(records[0].cache_read_simulated, 0);
    assert!(records[1].cache_read_simulated > 0);
    assert_eq!(records[2].simulated_cache, Some(false));
    assert_eq!(records[2].charge.as_ref().unwrap().cache_write_nano_usd, 0);
    for r in &records {
        assert_eq!(
            r.charge.as_ref().unwrap().total_nano_usd,
            claud2api::usage::quote(r).unwrap().total_nano_usd
        );
    }
    let expected: u64 = records
        .iter()
        .map(|r| r.charge.as_ref().unwrap().total_nano_usd)
        .sum();
    let rev = f.engine.store.read().await.revision;
    assert_eq!(
        admin(
            &f.engine,
            "DELETE",
            &format!("/tokens/{id}"),
            json!({"expected_revision":rev}),
            None
        )
        .await
        .0,
        200
    );
    f.engine.store.flush_status().await.unwrap();
    let (_, logs) = admin(&f.engine, "GET", "/logs?limit=100", Value::Null, None).await;
    assert!(
        logs["records"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["token_id"] == id)
            .all(|r| r["token_name"] == "fixture token")
    );
    let (status, stats) = admin(
        &f.engine,
        "GET",
        "/analytics?range=all&granularity=day",
        Value::Null,
        None,
    )
    .await;
    assert_eq!(status, 200, "{stats}");
    assert_eq!(stats["totals"]["records"], 3);
    assert_eq!(stats["totals"]["total_nano_usd"], expected);
    assert_eq!(stats["tokens"][0]["name"], "fixture token");
    assert_eq!(stats["series"][0]["points"].as_array().unwrap().len(), 7);
    assert!(!stats.to_string().contains(&key));
    assert!(!logs.to_string().contains(&key));
    assert_eq!(
        admin(
            &f.engine,
            "GET",
            "/analytics",
            Value::Null,
            Some(&f.engine.config.bot_key)
        )
        .await
        .0,
        403
    );
    assert_eq!(
        admin(&f.engine, "GET", "/analytics?range=bad", Value::Null, None)
            .await
            .0,
        400
    );
}

#[tokio::test]
async fn analytics_streams_past_ten_thousand_rows_with_unknown_history_and_restart() {
    let f = fixture(1).await;
    let now = store::now();
    let old = json!({"input_estimate":10,"output_estimate":2,"physical_estimate":20,"cache_read_simulated":0,"cache_creation_simulated":0,"internal":false,"status":200,"account_id":"old","model":"test"});
    sqlx::query("INSERT INTO usage(at,data) VALUES(?,?)")
        .bind(now - 70 * 86400)
        .bind(old.to_string())
        .execute(&f.engine.store.db)
        .await
        .unwrap();
    let mut u = store::Usage {
        accounting_version: 1,
        token_id: Some(store::uuid()),
        token_name: Some("kept label".to_owned()),
        input_estimate: 1000,
        output_estimate: 100,
        simulated_cache: Some(false),
        status: 200,
        model: "claude-sonnet-5-5".to_owned(),
        ..store::Usage::default()
    };
    u.charge = claud2api::usage::quote(&u);
    sqlx::query("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10001) INSERT INTO usage(at,data) SELECT ?,? FROM n").bind(now).bind(serde_json::to_string(&u).unwrap()).execute(&f.engine.store.db).await.unwrap();
    let stats = f
        .engine
        .store
        .analytics("all", "hour", "Asia/Taipei", now)
        .await
        .unwrap();
    assert_eq!(stats["totals"]["records"], 10002);
    assert_eq!(stats["totals"]["input_estimate"], 10_001_010);
    assert_eq!(stats["totals"]["priced_requests"], 10001);
    assert_eq!(stats["totals"]["unpriced_requests"], 1);
    assert_eq!(stats["totals"]["legacy_records"], 1);
    assert_eq!(stats["truncated"], false);
    assert!(
        stats["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "unassigned")
    );
    let recent = f
        .engine
        .store
        .analytics("24h", "hour", "Asia/Taipei", now)
        .await
        .unwrap();
    assert_eq!(recent["totals"]["records"], 10001);
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&f.engine.store.db)
        .await
        .unwrap();
    let copy = tempfile::tempdir().unwrap();
    std::fs::copy(
        f.engine.config.data_dir.join("control.sqlite3"),
        copy.path().join("control.sqlite3"),
    )
    .unwrap();
    let mut config = f.engine.config.clone();
    config.data_dir = copy.path().to_owned();
    let restarted = Engine::open(config).await.unwrap();
    assert_eq!(
        restarted
            .store
            .analytics("all", "hour", "Asia/Taipei", now)
            .await
            .unwrap()["totals"],
        stats["totals"]
    );
}

#[tokio::test]
async fn analytics_captures_legacy_root_internal_and_failed_usage_without_invented_costs() {
    let f = fixture(1).await;
    let (_, key) = managed_token(&f.engine, 1).await;
    let mut p = base();
    p["model"] = json!("claude-haiku-5-5");
    assert_eq!(
        call_key(&f.engine, "/v1/chat/completions", p.clone(), None, &key)
            .await
            .0,
        200
    );
    reset_pacing(&f.engine).await;
    assert_eq!(
        call(&f.engine, "/v1/messages", p.clone(), None).await.0,
        200
    );
    reset_pacing(&f.engine).await;
    f.upstream.replies.lock().await.push_back(Reply::Http(500));
    assert_eq!(
        call_key(&f.engine, "/v1/messages", p, None, &key).await.0,
        502
    );
    let trace = claud2api::observability::RequestTrace::new(&f.engine.store, "messages");
    trace.bind_token_name("internal owner");
    let mut scope = claud2api::guard::Scope::new(5);
    scope.trace = Some(trace);
    f.engine
        .store
        .record_scoped(
            store::Usage {
                input_estimate: 100,
                output_estimate: 20,
                internal: true,
                status: 200,
                model: "claude-sonnet-5-5".to_owned(),
                ..store::Usage::default()
            },
            &scope,
            false,
        )
        .await
        .unwrap();
    let stats = f
        .engine
        .store
        .analytics("24h", "hour", "Asia/Taipei", store::now())
        .await
        .unwrap();
    assert_eq!(stats["totals"]["records"], 4);
    assert_eq!(stats["totals"]["requests"], 3);
    assert_eq!(stats["totals"]["internal_requests"], 1);
    assert_eq!(stats["totals"]["unmetered_requests"], 1);
    assert_eq!(stats["totals"]["priced_requests"], 3);
    let rows: Vec<String> = sqlx::query_scalar("SELECT data FROM usage ORDER BY seq")
        .fetch_all(&f.engine.store.db)
        .await
        .unwrap();
    let rows: Vec<Value> = rows
        .iter()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(rows[0]["simulated_cache"], false);
    assert_eq!(rows[1]["token_name"], "环境网关密钥");
    assert!(rows[2]["charge"].is_null());
    assert_eq!(rows[3]["simulated_cache"], false);
}

#[tokio::test]
async fn analytics_calendar_boundaries_and_busy_token_log_names_are_explicit() {
    let f = fixture(1).await;
    let now = store::now();
    let (window, _) =
        claud2api::usage::windows("last_month", "hour", "Asia/Taipei", now, 0).unwrap();
    let u = store::Usage {
        accounting_version: 1,
        input_estimate: 1,
        simulated_cache: Some(false),
        status: 200,
        model: "unpriced".to_owned(),
        ..store::Usage::default()
    };
    for at in [window.start - 1, window.start, window.end - 1, window.end] {
        sqlx::query("INSERT INTO usage(at,data) VALUES(?,?)")
            .bind(at)
            .bind(serde_json::to_string(&u).unwrap())
            .execute(&f.engine.store.db)
            .await
            .unwrap();
    }
    let stats = f
        .engine
        .store
        .analytics("last_month", "hour", "Asia/Taipei", now)
        .await
        .unwrap();
    assert_eq!(stats["totals"]["records"], 2);
    assert_eq!(stats["totals"]["unpriced_requests"], 2);
    assert_eq!(stats["totals"]["total_nano_usd"], 0); // UI must render "unpriced", not free.
    let (id, key) = managed_token(&f.engine, 1).await;
    let held = f.engine.tokens.admit(&key).await.unwrap();
    let response = public_raw(&f.engine, "GET", "/v1/models", &key, Value::Null).await;
    assert_eq!(response.status(), 429);
    let _ = response.into_body().collect().await.unwrap();
    drop(held);
    f.engine.store.flush_status().await.unwrap();
    let (_, logs) = admin(&f.engine, "GET", "/logs", Value::Null, None).await;
    assert!(
        logs["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["status"] == 429
                && r["token_id"] == id
                && r["token_name"] == "fixture token")
    );
}

#[tokio::test]
async fn complete_dynamic_catalog_supports_native_and_bridge_names_without_family_allowlists() {
    let f = fixture(1).await;
    // Deliberately synthetic IDs, including future names absent from pricing and source tables.
    let ids: Vec<String> = (1..=18)
        .map(|i| format!("claude-catalog-fixture-{i}"))
        .collect();
    let mut advertised = ids.clone();
    advertised.push(ids[0].clone()); // Upstream duplicates must not hide or duplicate entries.
    *f.upstream.catalog_models.lock().await = Some(advertised);
    let response = public_raw(
        &f.engine,
        "GET",
        "/v1/models",
        &f.engine.config.gateway_key,
        Value::Null,
    )
    .await;
    assert_eq!(response.status(), 200);
    let list: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        ids
    );
    reset_pacing(&f.engine).await;
    let (status, observed) = admin(&f.engine, "POST", "/models/upstream", json!({}), None).await;
    assert_eq!(status, 200, "{observed}");
    assert_eq!(observed["upstream_catalog"]["models"], json!(ids));
    assert_eq!(
        observed["default_models"].as_array().unwrap().len(),
        ids.len()
    );
    assert!(
        observed["default_models"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["enabled"] == true)
    );
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 0);
    assert_eq!(
        model_update(
            &f.engine,
            json!({"enabled":true,"entries":observed["default_models"]})
        )
        .await
        .0,
        200
    );

    for native in &ids {
        for stream in [false, true] {
            reset_pacing(&f.engine).await;
            let mut payload = base();
            payload["model"] = json!(if stream {
                format!("c2a-{native}")
            } else {
                native.clone()
            });
            payload["stream"] = json!(stream);
            let (status, _, body) = call(&f.engine, "/v1/messages", payload, None).await;
            assert_eq!(status, 200, "{native}: {body}");
            if stream {
                let events = sse_values(body.as_str().unwrap());
                let start = events
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["type"] == "message_start")
                    .unwrap();
                assert_eq!(start["message"]["model"], *native);
            } else {
                assert_eq!(body["model"], *native);
            }
            let traces = f.upstream.traces.lock().await;
            let completion = traces
                .iter()
                .rev()
                .find(|t| t["uri"].as_str().unwrap().ends_with("/completion"))
                .unwrap();
            assert_eq!(completion["body"]["model"], *native);
        }
    }
    let created = f.upstream.created.load(Ordering::SeqCst);
    reset_pacing(&f.engine).await;
    let mut unavailable = base();
    unavailable["model"] = json!("claude-not-advertised");
    assert_eq!(
        call(&f.engine, "/v1/messages", unavailable, None).await.0,
        400
    );
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), created);
    // Unknown prices must not prevent a supported native model from generating.
    let stats = f
        .engine
        .store
        .analytics("all", "hour", "Asia/Taipei", store::now())
        .await
        .unwrap();
    assert_eq!(stats["totals"]["records"], ids.len() * 2);
    assert_eq!(stats["totals"]["unpriced_requests"], ids.len() * 2);
}

#[tokio::test]
async fn admin_upstream_catalog_is_explicit_native_durable_and_does_not_replace_mappings() {
    let f = fixture(1).await;
    let (_, initial) = admin(&f.engine, "GET", "/models", Value::Null, None).await;
    assert!(initial["upstream_catalog"].is_null());
    assert_eq!(initial["default_models"], json!([]));
    assert!(f.upstream.traces.lock().await.is_empty());
    assert_eq!(model_update(&f.engine,json!({"enabled":true,"entries":[{"id":"work","display_name":"Kept","upstream_model":"test","enabled":true}]})).await.0,200);
    let revision = f.engine.store.read().await.revision;
    let id = f
        .engine
        .store
        .read()
        .await
        .accounts
        .keys()
        .next()
        .unwrap()
        .clone();
    let (status, result) = admin(
        &f.engine,
        "POST",
        "/models/upstream",
        json!({"account_id":id}),
        None,
    )
    .await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["revision"], revision);
    assert_eq!(result["upstream_catalog"]["account_id"], id);
    assert_eq!(
        result["upstream_catalog"]["models"],
        json!(["test", "claude-haiku-5-5"])
    );
    assert_eq!(
        result["default_models"][1]["upstream_model"],
        "claude-haiku-5-5"
    );
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 0);
    let (_, saved) = admin(&f.engine, "GET", "/models", Value::Null, None).await;
    assert_eq!(saved["models"]["entries"][0]["id"], "work");
    assert_eq!(saved["default_models"], result["default_models"]);
    assert_eq!(call(&f.engine, "/v1/messages", base(), None).await.0, 400); // Still only exposes work.
    let before = f.upstream.traces.lock().await.len();
    for _ in 0..3 {
        admin(&f.engine, "GET", "/models", Value::Null, None).await;
    }
    assert_eq!(f.upstream.traces.lock().await.len(), before);
    // The observed defaults survive restart independently from configured mappings.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&f.engine.store.db)
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        f.engine.config.data_dir.join("control.sqlite3"),
        dir.path().join("control.sqlite3"),
    )
    .unwrap();
    let mut config = f.engine.config.clone();
    config.data_dir = dir.path().to_owned();
    let restarted = Engine::open(config).await.unwrap();
    let (_, restored) = admin(&restarted, "GET", "/models", Value::Null, None).await;
    assert_eq!(restored["upstream_catalog"], saved["upstream_catalog"]);
    assert_eq!(restored["models"], saved["models"]);
}

#[tokio::test]
async fn admin_catalog_uses_account_guard_and_retains_last_observation_on_access_failure() {
    let f = fixture(1).await;
    // Keep the rate-limit assertion independent of a wall-clock second boundary.
    f.engine
        .store
        .mutate("", json!({}), |state| {
            state.settings.account_min_interval_seconds = 300;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        admin(
            &f.engine,
            "POST",
            "/models/upstream",
            json!({}),
            Some(&f.engine.config.bot_key)
        )
        .await
        .0,
        403
    );
    assert_eq!(
        admin(
            &f.engine,
            "POST",
            "/models/upstream",
            json!({}),
            Some(&f.engine.config.gateway_key)
        )
        .await
        .0,
        401
    );
    assert_eq!(
        admin(
            &f.engine,
            "POST",
            "/models/upstream",
            json!({"account_id":"missing"}),
            None
        )
        .await
        .0,
        404
    );
    let held = f
        .engine
        .reserve(protocol::parse(&base()).unwrap(), "held-account".to_owned())
        .await
        .unwrap();
    assert_eq!(
        admin(&f.engine, "POST", "/models/upstream", json!({}), None)
            .await
            .0,
        429
    );
    assert_eq!(f.upstream.created.load(Ordering::SeqCst), 0);
    drop(held);
    reset_pacing(&f.engine).await;
    assert_eq!(
        admin(&f.engine, "POST", "/models/upstream", json!({}), None)
            .await
            .0,
        200
    );
    let previous = f
        .engine
        .store
        .read()
        .await
        .upstream_catalog
        .unwrap()
        .fetched_at;
    assert_eq!(
        admin(&f.engine, "POST", "/models/upstream", json!({}), None)
            .await
            .0,
        429
    );
    reset_pacing(&f.engine).await;
    f.upstream.catalog_status.store(403, Ordering::SeqCst);
    assert_eq!(
        admin(&f.engine, "POST", "/models/upstream", json!({}), None)
            .await
            .0,
        502
    );
    let state = f.engine.store.read().await;
    assert_eq!(state.upstream_catalog.unwrap().fetched_at, previous);
    assert!(state.accounts.values().next().unwrap().paused);
    assert_eq!(
        admin(&f.engine, "POST", "/models/upstream", json!({}), None)
            .await
            .0,
        503
    );
}
