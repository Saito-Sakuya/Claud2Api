use crate::{
    error::Fault,
    store::{Store, nonce, now},
};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{
    Arc, LazyLock, Mutex, Weak,
    atomic::{AtomicBool, AtomicU16, Ordering},
};
use std::time::Instant;

tokio::task_local! {pub static REQUEST_ID: String;}

#[derive(Clone, Serialize)]
pub struct TokenUsage {
    pub input_tokens: usize,
    pub output_tokens: Option<usize>,
    pub cache_creation_input_tokens: Option<usize>,
    pub cache_read_input_tokens: Option<usize>,
    pub estimated: bool,
    pub complete: bool,
    pub basis: &'static str,
}
impl TokenUsage {
    fn logical(input: usize, output: Option<usize>, cache: bool, read: usize) -> Self {
        Self {
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: cache.then_some(input - read.min(input)),
            cache_read_input_tokens: cache.then_some(read.min(input)),
            estimated: true,
            complete: output.is_some(),
            basis: "logical_request",
        }
    }
    fn upstream(input: usize) -> Self {
        Self {
            input_tokens: input,
            output_tokens: None,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            estimated: true,
            complete: false,
            basis: "sent_prompt",
        }
    }
}

#[derive(Clone, Serialize)]
pub struct StatusRecord {
    pub id: String,
    pub at: i64,
    pub request_id: String,
    pub account_id: String,
    pub model: Option<String>,
    pub token_id: Option<String>,
    pub token_name: Option<String>,
    pub usage: Option<TokenUsage>,
    pub direction: &'static str,
    pub mode: &'static str,
    pub route: &'static str,
    pub operation: &'static str,
    pub phase: &'static str,
    pub level: &'static str,
    pub status: u16,
    pub elapsed_ms: u64,
    pub first_byte_ms: Option<u64>,
    pub bytes: u64,
    pub chunks: u64,
    pub finished_at: Option<i64>,
    /// Observed native tool names and statuses. `null` is unknown; an empty
    /// list is meaningful only with `native_tools_complete: true`.
    pub native_tools: Option<Vec<NativeToolAudit>>,
    /// `true` means the native stream was fully decoded. `false` means the
    /// recorded call list may be partial or truncated. `null` means unknown.
    pub native_tools_complete: Option<bool>,
}

const MAX_NATIVE_TOOL_AUDIT: usize = 128;
const NATIVE_TOOL_GROUPS: [&str; 5] = [
    "web_search",
    "artifacts",
    "repl",
    "widgets",
    "project_knowledge",
];
static NATIVE_TOOL_NAMES: LazyLock<std::collections::HashSet<String>> = LazyLock::new(|| {
    let groups = NATIVE_TOOL_GROUPS.map(str::to_owned);
    crate::native_tools::allowed_names(&groups)
        .into_iter()
        .collect()
});

#[derive(Clone, Serialize)]
pub struct NativeToolAudit {
    pub name: String,
    /// `started` has no matching result, `completed` returned a result block,
    /// and `error` returned a result marked with `is_error: true`.
    pub status: &'static str,
}

#[derive(Default)]
struct NativeToolState {
    observation_complete: bool,
    truncated: bool,
    indices: HashMap<String, usize>,
    calls: Vec<NativeToolAudit>,
}
impl NativeToolState {
    fn start(&mut self, id: &str, name: &str) {
        if id.is_empty()
            || id.len() > 200
            || id.chars().any(char::is_control)
            || !NATIVE_TOOL_NAMES.contains(name)
            || self.indices.contains_key(id)
        {
            return;
        }
        if self.calls.len() >= MAX_NATIVE_TOOL_AUDIT {
            self.truncated = true;
            return;
        }
        let index = self.calls.len();
        self.indices.insert(id.to_owned(), index);
        self.calls.push(NativeToolAudit {
            name: name.to_owned(),
            status: "started",
        });
    }

    fn result(&mut self, id: &str, is_error: Option<bool>) {
        let Some(index) = self.indices.get(id).copied() else {
            return;
        };
        if self.calls[index].status != "started" {
            return;
        }
        self.calls[index].status = if is_error == Some(true) {
            "error"
        } else {
            "completed"
        };
    }

    fn snapshot(&self) -> (Option<Vec<NativeToolAudit>>, Option<bool>) {
        let complete = if self.observation_complete {
            Some(!self.truncated)
        } else if !self.calls.is_empty() || self.truncated {
            Some(false)
        } else {
            None
        };
        let calls = if self.observation_complete || !self.calls.is_empty() || self.truncated {
            Some(self.calls.clone())
        } else {
            None
        };
        (calls, complete)
    }
}

pub struct RequestTrace {
    pub id: String,
    store: Weak<Store>,
    route: &'static str,
    account: Mutex<String>,
    model: Mutex<Option<String>>,
    token_id: Mutex<Option<String>>,
    token_name: Mutex<Option<String>>,
    usage: Mutex<Option<TokenUsage>>,
    stream: AtomicBool,
    failure: AtomicU16,
    generation_starts: Mutex<std::collections::HashMap<String, i64>>,
    native_tools: Mutex<NativeToolState>,
}
impl RequestTrace {
    pub fn new(store: &Arc<Store>, route: &'static str) -> Arc<Self> {
        Arc::new(Self {
            id: format!("req_{}", nonce()),
            store: Arc::downgrade(store),
            route,
            account: Mutex::new(String::new()),
            model: Mutex::new(None),
            token_id: Mutex::new(None),
            token_name: Mutex::new(None),
            usage: Mutex::new(None),
            stream: AtomicBool::new(false),
            failure: AtomicU16::new(0),
            generation_starts: Mutex::new(std::collections::HashMap::new()),
            native_tools: Mutex::new(NativeToolState::default()),
        })
    }
    pub fn bind_account(&self, id: &str) {
        *self.account.lock().expect("trace account lock") = id.to_owned();
    }
    pub fn generation_started(&self, id: &str) -> Option<i64> {
        self.generation_starts
            .lock()
            .expect("trace generation lock")
            .get(id)
            .copied()
    }
    /// Records a validated native tool start. The ID is retained only in memory
    /// to pair its result and is never included in status-log output.
    pub fn native_start(&self, id: &str, name: &str) {
        self.native_tools
            .lock()
            .expect("trace native tool lock")
            .start(id, name);
    }
    /// Records completion without retaining any tool input or result content.
    pub fn native_result(&self, id: &str, is_error: Option<bool>) {
        self.native_tools
            .lock()
            .expect("trace native tool lock")
            .result(id, is_error);
    }
    /// Marks that the native stream was fully decoded, distinguishing an
    /// observed zero-call turn from a request whose tool activity is unknown.
    pub fn native_observation_complete(&self) {
        self.native_tools
            .lock()
            .expect("trace native tool lock")
            .observation_complete = true;
    }
    fn native_tool_summary(&self) -> (Option<Vec<NativeToolAudit>>, Option<bool>) {
        self.native_tools
            .lock()
            .expect("trace native tool lock")
            .snapshot()
    }
    pub fn requested_model(&self, value: &str) {
        if !value.is_empty() && value.len() <= 150 && !value.chars().any(char::is_control) {
            *self.model.lock().expect("trace model lock") = Some(value.to_owned());
        }
    }
    pub fn bind_token(&self, id: Option<String>) {
        *self.token_id.lock().expect("trace token lock") = id;
    }
    pub fn token_id(&self) -> Option<String> {
        self.token_id.lock().expect("trace token lock").clone()
    }
    pub fn bind_token_name(&self, name: &str) {
        *self.token_name.lock().expect("trace token name lock") = Some(name.to_owned());
    }
    pub fn token_name(&self) -> Option<String> {
        self.token_name
            .lock()
            .expect("trace token name lock")
            .clone()
    }
    pub fn begin_usage(&self, input: usize) {
        *self.usage.lock().expect("trace usage lock") =
            Some(TokenUsage::logical(input, None, false, 0));
    }
    pub fn complete_usage(&self, input: usize, output: usize, cache: bool, read: usize) {
        *self.usage.lock().expect("trace usage lock") =
            Some(TokenUsage::logical(input, Some(output), cache, read));
    }
    pub fn legacy_key(&self, raw: &str, key: &str) -> String {
        if raw.is_empty() {
            String::new()
        } else {
            crate::protocol::digest(key, "legacy-token-session", &(self.token_id(), raw))
        }
    }
    pub fn streaming(&self, value: bool) {
        self.stream.store(value, Ordering::Release);
    }
    pub fn fail(&self, status: u16) {
        self.failure.store(status, Ordering::Release);
    }
    pub fn outcome(&self) -> u16 {
        self.failure.load(Ordering::Acquire)
    }
    pub fn mode(&self) -> &'static str {
        if self.stream.load(Ordering::Acquire) {
            "sse"
        } else {
            "json"
        }
    }
    pub fn span(
        self: &Arc<Self>,
        direction: &'static str,
        mode: &'static str,
        operation: &'static str,
        account: &str,
    ) -> Span {
        if direction == "upstream" && operation == "completion" {
            self.generation_starts
                .lock()
                .expect("trace generation lock")
                .insert(account.to_owned(), now());
        }
        let span = Span {
            trace: self.clone(),
            started: Instant::now(),
            published: Instant::now(),
            done: false,
            record: StatusRecord {
                id: nonce(),
                at: now(),
                request_id: self.id.clone(),
                account_id: account.to_owned(),
                model: None,
                token_id: None,
                token_name: None,
                usage: None,
                direction,
                mode,
                route: self.route,
                operation,
                phase: "processing",
                level: "normal",
                status: 0,
                elapsed_ms: 0,
                first_byte_ms: None,
                bytes: 0,
                chunks: 0,
                finished_at: None,
                native_tools: None,
                native_tools_complete: None,
            },
        };
        span.publish();
        span
    }
}
pub struct Span {
    trace: Arc<RequestTrace>,
    started: Instant,
    published: Instant,
    done: bool,
    record: StatusRecord,
}
impl Span {
    fn publish(&self) {
        if let Some(store) = self.trace.store.upgrade() {
            let mut record = self.record.clone();
            record.model = self.trace.model.lock().expect("trace model lock").clone();
            record.token_id = self.trace.token_id();
            record.token_name = self.trace.token_name();
            if record.direction == "downstream" {
                if self.done {
                    (record.native_tools, record.native_tools_complete) =
                        self.trace.native_tool_summary();
                }
                record.usage = self.trace.usage.lock().expect("trace usage lock").clone();
                if (!self.done || record.status >= 400)
                    && let Some(usage) = &mut record.usage
                {
                    usage.output_tokens = None;
                    usage.cache_creation_input_tokens = None;
                    usage.cache_read_input_tokens = None;
                    usage.complete = false;
                }
            }
            store.enqueue_status(record);
        }
    }
    pub fn headers(&mut self, status: u16) {
        self.record.status = status;
        if self.record.direction == "downstream" {
            self.record.account_id = self
                .trace
                .account
                .lock()
                .expect("trace account lock")
                .clone();
            self.record.mode = self.trace.mode();
            self.record.phase = "sending";
        } else {
            self.record.phase = "receiving";
        }
        self.record.elapsed_ms = self.started.elapsed().as_millis() as u64;
        self.publish();
    }
    pub fn chunk(&mut self, size: usize) {
        if self.record.first_byte_ms.is_none() {
            self.record.first_byte_ms = Some(self.started.elapsed().as_millis() as u64);
        }
        self.record.bytes = self.record.bytes.saturating_add(size as u64);
        self.record.chunks = self.record.chunks.saturating_add(1);
        self.progress();
    }
    fn progress(&mut self) {
        if self.published.elapsed().as_secs() >= 1 {
            self.record.elapsed_ms = self.started.elapsed().as_millis() as u64;
            self.publish();
            self.published = Instant::now();
        }
    }
    pub fn input_tokens(&mut self, input: usize) {
        self.record.usage = Some(TokenUsage::upstream(input));
        self.publish();
    }
    pub fn output_tokens(&mut self, output: usize, complete: bool) {
        if let Some(usage) = &mut self.record.usage {
            usage.output_tokens = Some(output);
            usage.complete = complete;
        }
        self.progress();
    }
    pub fn finish(&mut self, status: u16) {
        if self.done {
            return;
        }
        self.done = true;
        self.record.status = status;
        self.record.phase = if status == 499 {
            "cancelled"
        } else if status >= 400 {
            "error"
        } else {
            "completed"
        };
        self.record.level = if status >= 400 { "error" } else { "normal" };
        self.record.elapsed_ms = self.started.elapsed().as_millis() as u64;
        self.record.finished_at = Some(now());
        if self.record.direction == "downstream" {
            self.record.account_id = self
                .trace
                .account
                .lock()
                .expect("trace account lock")
                .clone();
            self.record.mode = self.trace.mode();
        }
        self.publish();
    }
    pub fn failed(&mut self, fault: &Fault) {
        self.finish(fault.status);
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        if !self.done {
            self.finish(499);
        }
    }
}

pub fn operation(path: &str, method: &str) -> &'static str {
    if method == "POST" && path.ends_with("/stop_response") {
        return "completion_stop";
    }
    if path.ends_with("/completion") {
        "completion"
    } else if method == "POST" && path.starts_with("/api/") && path.ends_with("/upload") {
        "file_upload"
    } else if path == "/api/organizations" {
        "organizations"
    } else if path.starts_with("/edge-api/") {
        "model_catalog"
    } else if path.contains("/wiggle/download-file?") {
        "native_file_download"
    } else if method == "PUT" && path.ends_with("?rendering_mode=raw") {
        "project_bind"
    } else if method == "DELETE" {
        "conversation_delete"
    } else {
        "conversation_create"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_audit_distinguishes_unknown_empty_and_interrupted_observations() {
        let mut state = NativeToolState::default();
        let (calls, complete) = state.snapshot();
        assert!(calls.is_none());
        assert_eq!(complete, None);
        state.start("private-call-id", "web_search");
        state.result("private-call-id", Some(false));
        let (calls, complete) = state.snapshot();
        assert_eq!(complete, Some(false));
        assert_eq!(
            serde_json::to_value(calls).unwrap(),
            json!([
                {"name":"web_search","status":"completed"}
            ])
        );
        state.observation_complete = true;
        assert_eq!(state.snapshot().1, Some(true));
        let empty = NativeToolState {
            observation_complete: true,
            ..Default::default()
        };
        assert!(empty.snapshot().0.unwrap().is_empty());
        assert_eq!(empty.snapshot().1, Some(true));
    }

    #[test]
    fn native_audit_capacity_is_bounded_and_cannot_be_marked_complete_after_truncation() {
        let mut state = NativeToolState::default();
        for index in 0..MAX_NATIVE_TOOL_AUDIT {
            state.start(&format!("call-{index}"), "bash_tool");
        }
        state.start("call-0", "bash_tool");
        assert!(!state.truncated, "a duplicate ID must not consume capacity");
        state.start("one-call-too-many", "web_search");
        state.observation_complete = true;
        let (calls, complete) = state.snapshot();
        assert_eq!(calls.unwrap().len(), MAX_NATIVE_TOOL_AUDIT);
        assert_eq!(complete, Some(false));
        assert_eq!(state.indices.len(), MAX_NATIVE_TOOL_AUDIT);
    }
}
