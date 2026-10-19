use axum::{
    Json,
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct Fault {
    pub status: u16,
    pub kind: &'static str,
    pub message: String,
    pub retry_after: Option<u64>,
    pub quota: bool,
}
pub type Result<T> = std::result::Result<T, Fault>;
impl Fault {
    pub fn new(status: u16, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            retry_after: None,
            quota: false,
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request_error", message)
    }
    pub fn internal() -> Self {
        Self::new(503, "api_error", "durable state is unavailable")
    }
    pub fn busy(message: &str, delay: u64) -> Self {
        let mut f = Self::new(429, "rate_limit_error", message);
        f.retry_after = Some(delay.max(1));
        f
    }
    pub fn upstream() -> Self {
        Self::new(
            502,
            "api_error",
            "upstream response or tool protocol failed",
        )
    }
    pub fn cancelled() -> Self {
        Self::new(499, "api_error", "request cancelled")
    }
    pub fn timeout() -> Self {
        Self::new(504, "api_error", "request deadline exceeded")
    }
    pub fn envelope(&self, request_id: &str) -> Value {
        json!({"type":"error","error":{"type":self.kind,"message":self.message},"request_id":request_id})
    }
}
impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Fault {}
impl IntoResponse for Fault {
    fn into_response(self) -> Response {
        let id = crate::observability::REQUEST_ID
            .try_with(Clone::clone)
            .unwrap_or_else(|_| format!("req_{}", uuid::Uuid::new_v4().simple()));
        let mut response = (
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(self.envelope(&id)),
        )
            .into_response();
        if let Some(seconds) = self.retry_after {
            response.headers_mut().insert(
                "retry-after",
                HeaderValue::from_str(&seconds.to_string()).expect("integer header"),
            );
        }
        response.headers_mut().insert(
            "x-request-id",
            HeaderValue::from_str(&id).expect("generated id"),
        );
        response
    }
}
