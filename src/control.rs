use crate::{
    config::{Credential, Settings},
    engine::Engine,
    error::{Fault, Result},
    guard::available,
    store::{Account, now},
    web::equal,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderValue, Method},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, patch, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone, Copy)]
enum Access {
    Admin,
    Bot,
}
async fn auth(State(e): State<Arc<Engine>>, mut req: Request, next: Next) -> Response {
    let value = req
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    if req.headers().get_all("authorization").iter().count() != 1 {
        return Fault::new(401, "authentication_error", "control key required").into_response();
    }
    let scope = if equal(value, &e.config.admin_key) {
        Access::Admin
    } else if !e.config.bot_key.is_empty() && equal(value, &e.config.bot_key) {
        Access::Bot
    } else {
        return Fault::new(401, "authentication_error", "invalid control key").into_response();
    };
    let path = req.uri().path();
    let info = matches!(path, "/info" | "/usage" | "/events");
    if matches!(scope, Access::Bot) && (!info || req.method() != Method::GET) {
        return Fault::new(
            403,
            "permission_error",
            "read-only information scope required",
        )
        .into_response();
    }
    if let Err(f) = validate_origin(&req) {
        return f.into_response();
    }
    req.extensions_mut().insert(scope);
    let mut response = next.run(req).await;
    secure(&mut response);
    response
}
pub(crate) fn validate_origin(req: &Request) -> Result<()> {
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        if req
            .headers()
            .get("sec-fetch-site")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| !matches!(v, "same-origin" | "none"))
        {
            return Err(Fault::new(
                403,
                "permission_error",
                "cross-origin mutation refused",
            ));
        }
        if let Some(origin) = req.headers().get("origin") {
            let valid = origin
                .to_str()
                .ok()
                .and_then(|s| url::Url::parse(s).ok())
                .zip(req.headers().get("host").and_then(|h| h.to_str().ok()))
                .is_some_and(|(url, host)| {
                    matches!(url.scheme(), "http" | "https")
                        && url.path() == "/"
                        && url.query().is_none()
                        && url.fragment().is_none()
                        && url.username().is_empty()
                        && url[url::Position::BeforeHost..url::Position::AfterPort] == *host
                });
            if !valid {
                return Err(Fault::new(
                    403,
                    "permission_error",
                    "cross-origin mutation refused",
                ));
            }
        }
    }
    Ok(())
}
pub(crate) fn secure(response: &mut Response) {
    for (k, v) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ),
    ] {
        response
            .headers_mut()
            .insert(k, HeaderValue::from_static(v));
    }
}
pub fn router(e: Arc<Engine>) -> Router {
    let api = Router::new()
        .route("/info", get(info))
        .route("/usage", get(usage))
        .route("/analytics", get(analytics))
        .route("/events", get(events))
        .route("/logs", get(logs))
        .route("/models", get(model_config).put(update_models))
        .route("/models/upstream", post(fetch_upstream_models))
        .route("/tokens", get(tokens).post(create_token))
        .route("/tokens/{id}", delete(delete_token).patch(update_token))
        .route("/accounts", get(accounts))
        .route("/accounts/import", post(import))
        .route("/accounts/proxy", patch(account_proxies))
        .route("/accounts/{id}", patch(account))
        .route("/accounts/{id}/resume", post(resume))
        .route("/sessions", get(sessions))
        .route("/sessions/{id}", delete(clear))
        .route("/settings", get(settings).put(update_settings))
        .route("/updates", get(standalone_updates))
        .route("/_runtime/export", post(runtime_export))
        .route("/_runtime/import", post(runtime_import))
        .route("/_runtime/stop", post(runtime_stop))
        .fallback(|| async { Fault::new(404, "not_found_error", "unknown control endpoint") })
        .layer(middleware::from_fn_with_state(e.clone(), auth));
    Router::new()
        .route(
            "/versions.js",
            get(|| async {
                let mut r = (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/javascript; charset=utf-8",
                    )],
                    include_str!("../ui/versions.js"),
                )
                    .into_response();
                secure(&mut r);
                r
            }),
        )
        .route(
            "/icon.svg",
            get(|| async {
                let mut response = (
                    [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
                    include_str!("../ui/icon.svg"),
                )
                    .into_response();
                secure(&mut response);
                response
            }),
        )
        .route(
            "/",
            get(|| async {
                let mut r = Html(include_str!("../ui/index.html")).into_response();
                secure(&mut r);
                r
            }),
        )
        .route(
            "/app.js",
            get(|| async {
                let mut r = (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/javascript; charset=utf-8",
                    )],
                    include_str!("../ui/app.js"),
                )
                    .into_response();
                secure(&mut r);
                r
            }),
        )
        .route(
            "/charts.js",
            get(|| async {
                let mut r = (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/javascript; charset=utf-8",
                    )],
                    include_str!("../ui/charts.js"),
                )
                    .into_response();
                secure(&mut r);
                r
            }),
        )
        .route(
            "/selects.js",
            get(|| async {
                let mut r = (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/javascript; charset=utf-8",
                    )],
                    include_str!("../ui/selects.js"),
                )
                    .into_response();
                secure(&mut r);
                r
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                let mut r = (
                    [(axum::http::header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../ui/style.css"),
                )
                    .into_response();
                secure(&mut r);
                r
            }),
        )
        .nest("/control/v1", api)
        .layer(DefaultBodyLimit::max(1 << 20))
        .with_state(e)
}
async fn standalone_updates() -> Json<Value> {
    Json(
        json!({"schema_version":1,"current_version":crate::VERSION,"enabled":false,"update_available":false,"rollback_versions":[],"check_error":"this process is not running behind the Linux AMD64 update supervisor"}),
    )
}
fn worker_capability(headers: &axum::http::HeaderMap) -> Result<()> {
    let expected = std::env::var("CLAUD2API_WORKER_TOKEN").unwrap_or_default();
    if !crate::updates::managed()
        || expected.len() != 64
        || headers
            .get("x-claud2api-worker")
            .and_then(|v| v.to_str().ok())
            .is_none_or(|v| !equal(v, &expected))
    {
        return Err(Fault::new(
            403,
            "permission_error",
            "internal worker capability required",
        ));
    }
    Ok(())
}
async fn runtime_export(
    State(e): State<Arc<Engine>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Value>> {
    worker_capability(&headers)?;
    e.store.flush_status().await?;
    let value = json!({"legacy":*e.legacy.lock().await});
    if serde_json::to_vec(&value)
        .map_err(|_| Fault::internal())?
        .len()
        > 1 << 20
    {
        return Err(Fault::new(
            409,
            "api_error",
            "runtime handoff exceeds safe size; service left running",
        ));
    }
    Ok(Json(value))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeHandoff {
    legacy: std::collections::HashMap<String, (String, String, String)>,
}
async fn runtime_import(
    State(e): State<Arc<Engine>>,
    headers: axum::http::HeaderMap,
    Json(value): Json<RuntimeHandoff>,
) -> Result<Json<Value>> {
    worker_capability(&headers)?;
    let mut current = e.legacy.lock().await;
    if !current.is_empty() {
        return Err(Fault::new(
            409,
            "api_error",
            "runtime bindings already exist",
        ));
    }
    *current = value.legacy;
    Ok(Json(json!({"schema_version":1,"imported":true})))
}
async fn runtime_stop(
    State(e): State<Arc<Engine>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Value>> {
    worker_capability(&headers)?;
    e.shutdown.cancel();
    Ok(Json(json!({"schema_version":1,"stopping":true})))
}
async fn info(State(e): State<Arc<Engine>>) -> Json<Value> {
    let state = e.store.read().await;
    let mut active = 0;
    for id in state.accounts.keys() {
        if e.pool.busy(id).await {
            active += 1;
        }
    }
    Json(
        json!({"schema_version":1,"at":now(),"version":crate::VERSION,"healthy":e.store.healthy(),"revision":state.revision,"capacity":state.accounts.values().filter(|a|a.enabled&&!a.paused).count(),"busy_accounts":active,"sessions":state.sessions.len(),"pending_cleanup":state.pending.len(),"estimates_only":true,"provider_remaining":null,"event_api":"/control/v1/events","browser_profiles":crate::browser::BrowserProfile::catalog(),"default_browser_profile":e.config.browser_profile.id(),"status_log_healthy":e.store.status_healthy(),"status_log_dropped":e.store.status_dropped.load(std::sync::atomic::Ordering::Relaxed)}),
    )
}
#[derive(Deserialize)]
struct UsageQuery {
    #[serde(default)]
    since: Option<i64>,
}
async fn usage(State(e): State<Arc<Engine>>, Query(q): Query<UsageQuery>) -> Result<Json<Value>> {
    Ok(Json(
        e.store
            .usage(
                q.since
                    .unwrap_or(now() - 18000)
                    .clamp(now() - 31 * 86400, now()),
            )
            .await?,
    ))
}
#[derive(Deserialize)]
struct AnalyticsQuery {
    #[serde(default = "default_usage_range")]
    range: String,
    #[serde(default = "default_granularity")]
    granularity: String,
}
fn default_usage_range() -> String {
    "24h".to_owned()
}
fn default_granularity() -> String {
    "hour".to_owned()
}
async fn analytics(
    State(e): State<Arc<Engine>>,
    Query(q): Query<AnalyticsQuery>,
) -> Result<Json<Value>> {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        e.store
            .analytics(&q.range, &q.granularity, &e.config.timezone, now()),
    )
    .await
    .map_err(|_| Fault::timeout())??;
    Ok(Json(result))
}
#[derive(Deserialize)]
struct EventQuery {
    #[serde(default)]
    after: i64,
    #[serde(default)]
    limit: Option<i64>,
}
async fn events(State(e): State<Arc<Engine>>, Query(q): Query<EventQuery>) -> Result<Json<Value>> {
    Ok(Json(e.store.events(q.after, q.limit.unwrap_or(100)).await?))
}
#[derive(Deserialize)]
struct LogQuery {
    #[serde(default = "all_logs")]
    level: String,
    #[serde(default)]
    account_id: String,
    before: Option<i64>,
    limit: Option<i64>,
}
fn all_logs() -> String {
    "all".to_owned()
}
async fn logs(State(e): State<Arc<Engine>>, Query(q): Query<LogQuery>) -> Result<Json<Value>> {
    Ok(Json(
        e.store
            .status_logs(&q.level, &q.account_id, q.before, q.limit.unwrap_or(100))
            .await?,
    ))
}
async fn accounts(State(e): State<Arc<Engine>>) -> Result<Json<Value>> {
    let state = e.store.read().await;
    let cycles = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        e.store
            .account_cycles(state.accounts.keys().cloned().collect(), now()),
    )
    .await
    .map_err(|_| Fault::timeout())??;
    let mut out = Vec::new();
    for a in state.accounts.values() {
        let busy = e.pool.busy(&a.id).await;
        out.push(json!({"id":a.id,"label":a.label,"enabled":a.enabled,"paused":a.paused,"cooldown_until":a.cooldown_until,"ready_at":available(a,&state.settings),"busy":busy,"sessions":state.sessions.values().filter(|s|s.account_id==a.id).count(),"browser_profile":a.transport.browser_profile.unwrap_or(e.config.browser_profile).id(),"transport_revision":a.transport_revision,"proxy":a.transport.public_proxy(),"usage_cycles":cycles.get(&a.id)}));
    }
    Ok(Json(
        json!({"schema_version":1,"revision":state.revision,"accounts":out}),
    ))
}
fn revision(state: &crate::store::Snapshot, expected: u64) -> Result<()> {
    if state.revision != expected {
        Err(Fault::new(
            409,
            "conflict_error",
            "configuration revision changed; refresh before retrying",
        ))
    } else {
        Ok(())
    }
}
fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|_| Fault::invalid("invalid control payload"))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Import {
    expected_revision: u64,
    accounts: Vec<ImportItem>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportItem {
    credential: String,
    #[serde(default)]
    label: String,
    browser_profile: Option<crate::browser::BrowserProfile>,
    proxy_url: Option<String>,
}
fn valid_label(s: &str) -> bool {
    s.chars().count() <= 80 && !s.chars().any(char::is_control)
}
async fn import(State(e): State<Arc<Engine>>, bytes: Bytes) -> Result<Json<Value>> {
    let input: Import = decode(&bytes)?;
    if input.accounts.is_empty() || input.accounts.len() > 512 {
        return Err(Fault::invalid("import between 1 and 512 accounts"));
    }
    let parsed = input
        .accounts
        .iter()
        .map(|i| {
            let (label, credential) = if !i.credential.contains("sessionKey=") {
                i.credential.split_once(':').map_or(
                    (i.label.as_str(), i.credential.as_str()),
                    |(name, key)| {
                        (
                            if i.label.is_empty() {
                                name.trim()
                            } else {
                                i.label.as_str()
                            },
                            key.trim(),
                        )
                    },
                )
            } else {
                (i.label.as_str(), i.credential.as_str())
            };
            if !valid_label(label)
                || (i.credential.contains(':')
                    && !i.credential.contains("sessionKey=")
                    && label.is_empty())
            {
                return Err(Fault::invalid("invalid account label"));
            }
            let c = Credential::parse(credential)?;
            if [
                &e.config.gateway_key,
                &e.config.admin_key,
                &e.config.bot_key,
            ]
            .iter()
            .any(|k| !k.is_empty() && (equal(k, &c.session_key) || equal(k, &c.cookie)))
            {
                return Err(Fault::invalid(
                    "account credential must differ from control keys",
                ));
            }
            let transport = crate::browser::TransportSettings {
                browser_profile: i.browser_profile,
                proxy_url: i.proxy_url.clone().filter(|v| !v.is_empty()),
            };
            transport.validate()?;
            Ok((c, label.to_owned(), transport))
        })
        .collect::<Result<Vec<_>>>()?;
    let result = e
        .store
        .mutate(
            "accounts_imported",
            json!({"submitted":parsed.len()}),
            |s| {
                revision(s, input.expected_revision)?;
                let mut added = 0;
                for (c, label, transport) in parsed {
                    let id = c.id();
                    if !s.accounts.contains_key(&id) {
                        let mut a = Account::new(c, s.accounts.len() + 1);
                        a.transport = transport;
                        if !label.is_empty() {
                            a.label = label;
                        }
                        s.accounts.insert(id, a);
                        added += 1;
                    }
                }
                if s.accounts.len() > 512 {
                    return Err(Fault::invalid("account capacity exceeds 512"));
                }
                s.revision += 1;
                Ok(json!({"schema_version":1,"revision":s.revision,"added":added}))
            },
        )
        .await?;
    Ok(Json(result))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountPatch {
    expected_revision: u64,
    enabled: Option<bool>,
    label: Option<String>,
    browser_profile: Option<crate::browser::BrowserProfile>,
    proxy_url: Option<String>,
}
async fn account(
    State(e): State<Arc<Engine>>,
    Path(id): Path<String>,
    bytes: Bytes,
) -> Result<Json<Value>> {
    let input: AccountPatch = decode(&bytes)?;
    if input.label.as_ref().is_some_and(|s| !valid_label(s)) {
        return Err(Fault::invalid("invalid account label"));
    }
    if input.browser_profile.is_some() || input.proxy_url.is_some() {
        let state = e.store.read().await;
        let mut transport = state
            .accounts
            .get(&id)
            .ok_or_else(|| Fault::new(404, "not_found_error", "account not found"))?
            .transport
            .clone();
        if let Some(profile) = input.browser_profile {
            transport.browser_profile = Some(profile);
        }
        if let Some(proxy) = input.proxy_url {
            transport.proxy_url = if proxy.is_empty() { None } else { Some(proxy) };
        }
        return Ok(Json(
            e.pool
                .configure(
                    &id,
                    input.expected_revision,
                    transport,
                    input.label,
                    input.enabled,
                )
                .await?,
        ));
    }
    let value = e
        .store
        .mutate(
            "account_updated",
            json!({"account_id":id,"enabled":input.enabled}),
            |s| {
                revision(s, input.expected_revision)?;
                let a = s
                    .accounts
                    .get_mut(&id)
                    .ok_or_else(|| Fault::new(404, "not_found_error", "account not found"))?;
                if let Some(enabled) = input.enabled {
                    a.enabled = enabled;
                }
                if let Some(label) = input.label {
                    a.label = label;
                }
                s.revision += 1;
                Ok(json!({"schema_version":1,"revision":s.revision}))
            },
        )
        .await?;
    Ok(Json(value))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    expected_revision: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProxyBatch {
    expected_revision: u64,
    account_ids: Vec<String>,
    proxy_url: String,
}
async fn account_proxies(State(e): State<Arc<Engine>>, bytes: Bytes) -> Result<Json<Value>> {
    let input: ProxyBatch = decode(&bytes)?;
    Ok(Json(
        e.pool
            .configure_proxies(input.account_ids, input.expected_revision, input.proxy_url)
            .await?,
    ))
}
async fn resume(
    State(e): State<Arc<Engine>>,
    Path(id): Path<String>,
    bytes: Bytes,
) -> Result<Json<Value>> {
    let input: Revision = decode(&bytes)?;
    let value = e
        .store
        .mutate("account_reviewed", json!({"account_id":id}), |s| {
            revision(s, input.expected_revision)?;
            let a = s
                .accounts
                .get_mut(&id)
                .ok_or_else(|| Fault::new(404, "not_found_error", "account not found"))?;
            a.paused = false;
            s.revision += 1;
            Ok(json!({"schema_version":1,"revision":s.revision}))
        })
        .await?;
    Ok(Json(value))
}
async fn sessions(State(e): State<Arc<Engine>>) -> Json<Value> {
    let s = e.store.read().await;
    let list:Vec<_>=s.sessions.values().map(|v|json!({"id":v.id,"account_id":v.account_id,"model":v.model,"active":v.active,"expires_at":v.expires_at,"turns":v.history.len(),"context_estimate":v.context_estimate,"checkpoint":v.checkpoint.is_some(),"uncertain":v.uncertain})).collect();
    Json(json!({"schema_version":1,"revision":s.revision,"sessions":list}))
}
async fn clear(
    State(e): State<Arc<Engine>>,
    Path(id): Path<String>,
    bytes: Bytes,
) -> Result<Json<Value>> {
    let input: Revision = decode(&bytes)?;
    let value = e
        .store
        .mutate("session_cleared", json!({"session_id":id}), |s| {
            revision(s, input.expected_revision)?;
            let session = s
                .sessions
                .get(&id)
                .ok_or_else(|| Fault::new(404, "not_found_error", "session not found"))?;
            if session.active {
                return Err(Fault::new(
                    409,
                    "conflict_error",
                    "active session cannot be cleared",
                ));
            }
            s.retire(&id);
            s.revision += 1;
            Ok(json!({"schema_version":1,"revision":s.revision,"cleanup_queued":true}))
        })
        .await?;
    Ok(Json(value))
}
async fn settings(State(e): State<Arc<Engine>>) -> Json<Value> {
    let s = e.store.read().await;
    Json(
        json!({"schema_version":1,"revision":s.revision,"settings":s.settings,"effective_for":"new requests; in-flight requests keep their captured limits"}),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsUpdate {
    expected_revision: u64,
    settings: Settings,
}
async fn update_settings(State(e): State<Arc<Engine>>, bytes: Bytes) -> Result<Json<Value>> {
    let input: SettingsUpdate = decode(&bytes)?;
    input.settings.validate()?;
    let value = e
        .store
        .mutate(
            "settings_updated",
            json!({"expected_revision":input.expected_revision}),
            |s| {
                revision(s, input.expected_revision)?;
                s.settings = input.settings;
                s.revision += 1;
                Ok(json!({"schema_version":1,"revision":s.revision}))
            },
        )
        .await?;
    Ok(Json(value))
}

async fn model_config(State(e): State<Arc<Engine>>) -> Json<Value> {
    let state = e.store.read().await;
    let defaults = state
        .upstream_catalog
        .as_ref()
        .map(|c| c.defaults())
        .unwrap_or_default();
    Json(
        json!({"schema_version":1,"revision":state.revision,"models":state.models,"upstream_catalog":state.upstream_catalog,"default_models":defaults}),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogRequest {
    #[serde(default)]
    account_id: Option<String>,
}
async fn fetch_upstream_models(State(e): State<Arc<Engine>>, bytes: Bytes) -> Result<Json<Value>> {
    let input: CatalogRequest = decode(&bytes)?;
    if let Some(id) = &input.account_id
        && !e.store.read().await.accounts.contains_key(id)
    {
        return Err(Fault::new(404, "not_found_error", "account not found"));
    }
    let mut scope =
        crate::guard::Scope::new(e.store.read().await.settings.request_timeout_seconds.min(8));
    let trace = crate::observability::RequestTrace::new(&e.store, "model_catalog_admin");
    trace.bind_token_name("管理端模型查询");
    scope.trace = Some(trace);
    let lease = e
        .pool
        .acquire(
            input.account_id.as_deref(),
            &std::collections::HashSet::new(),
            &scope,
            false,
        )
        .await?;
    let ids = lease.slot.client.models(&lease, &scope, true).await?;
    let catalog = crate::models::UpstreamCatalog {
        account_id: lease.account_id.clone(),
        fetched_at: now(),
        models: ids,
    };
    catalog.validate()?;
    let value=e.store.mutate("model_catalog_refreshed",json!({"account_id":catalog.account_id,"count":catalog.models.len()}),|s| {
        s.upstream_catalog=Some(catalog.clone());
        Ok(json!({"schema_version":1,"revision":s.revision,"upstream_catalog":catalog,"default_models":catalog.defaults()}))
    }).await?;
    Ok(Json(value))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelUpdate {
    expected_revision: u64,
    models: crate::models::ModelConfig,
}
async fn update_models(State(e): State<Arc<Engine>>, bytes: Bytes) -> Result<Json<Value>> {
    let input: ModelUpdate = decode(&bytes)?;
    input.models.validate()?;
    let value = e
        .store
        .mutate(
            "models_updated",
            json!({"enabled":input.models.enabled,"count":input.models.entries.len()}),
            |s| {
                revision(s, input.expected_revision)?;
                s.models = input.models;
                s.revision += 1;
                Ok(json!({"schema_version":1,"revision":s.revision}))
            },
        )
        .await?;
    Ok(Json(value))
}
async fn tokens(State(e): State<Arc<Engine>>) -> Json<Value> {
    let state = e.store.read().await;
    let mut tokens = Vec::new();
    for t in state.tokens.values() {
        tokens.push(json!({"id":t.id,"label":t.label,"max_concurrency":t.max_concurrency,"created_at":t.created_at,"expires_at":t.expires_at,"expired":t.expires_at.is_some_and(|v|v<=now()),"active_requests":e.tokens.busy(&t.id,t.max_concurrency).await}));
    }
    Json(
        json!({"schema_version":1,"revision":state.revision,"tokens":tokens,"environment_key_enabled":true}),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenCreate {
    expected_revision: u64,
    label: String,
    max_concurrency: usize,
    expires_at: Option<i64>,
}
async fn create_token(State(e): State<Arc<Engine>>, bytes: Bytes) -> Result<Json<Value>> {
    let input: TokenCreate = decode(&bytes)?;
    let secret = format!("c2a_tok_{}{}", crate::store::nonce(), crate::store::nonce());
    let token = crate::tokens::ApiToken {
        id: crate::store::uuid(),
        label: input.label,
        digest: crate::tokens::digest(&e.config.gateway_key, &secret),
        max_concurrency: input.max_concurrency,
        created_at: now(),
        expires_at: input.expires_at,
    };
    token.validate()?;
    if token.expires_at.is_some_and(|v| v <= now()) {
        return Err(Fault::invalid("token expiration must be in the future"));
    }
    let id = token.id.clone();
    let value=e.store.mutate("token_created",json!({"token_id":id,"label":token.label,"max_concurrency":token.max_concurrency,"expires_at":token.expires_at}),|s|{
        revision(s,input.expected_revision)?;if s.tokens.len()>=512 {return Err(Fault::invalid("token capacity reached"));}
        s.tokens.insert(id.clone(),token);s.revision+=1;
        Ok(json!({"schema_version":1,"revision":s.revision,"id":id,"token":secret}))
    }).await?;
    Ok(Json(value))
}
async fn update_token(
    State(e): State<Arc<Engine>>,
    Path(id): Path<String>,
    bytes: Bytes,
) -> Result<Json<Value>> {
    let input: TokenCreate = decode(&bytes)?;
    let value = e
        .store
        .mutate("token_updated", json!({"token_id":id}), |s| {
            revision(s, input.expected_revision)?;
            let token = s
                .tokens
                .get_mut(&id)
                .ok_or_else(|| Fault::new(404, "not_found_error", "token not found"))?;
            token.label = input.label;
            token.max_concurrency = input.max_concurrency;
            token.expires_at = input.expires_at;
            token.validate()?;
            if token.expires_at.is_some_and(|v| v <= now()) {
                return Err(Fault::invalid("token expiration must be in the future"));
            }
            s.revision += 1;
            Ok(json!({"schema_version":1,"revision":s.revision,"id":id}))
        })
        .await?;
    Ok(Json(value))
}
async fn delete_token(
    State(e): State<Arc<Engine>>,
    Path(id): Path<String>,
    bytes: Bytes,
) -> Result<Json<Value>> {
    let input: Revision = decode(&bytes)?;
    let value = e
        .store
        .mutate("token_deleted", json!({"token_id":id}), |s| {
            revision(s, input.expected_revision)?;
            if s.tokens.remove(&id).is_none() {
                return Err(Fault::new(404, "not_found_error", "token not found"));
            }
            s.revision += 1;
            Ok(json!({"schema_version":1,"revision":s.revision,"deleted":true}))
        })
        .await?;
    Ok(Json(value))
}
