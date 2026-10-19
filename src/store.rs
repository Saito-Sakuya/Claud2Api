use crate::{
    config::{Config, Credential, Settings},
    error::{Fault, Result},
    protocol,
};
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, AeadCore, OsRng, Payload},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::sync::{Mutex, mpsc, oneshot};

pub fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
pub fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}
pub fn nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    pub paused: bool,
    pub cooldown_until: i64,
    #[serde(default)]
    pub quota_until: i64,
    pub last_start: i64,
    pub starts: Vec<i64>,
    pub credential: Credential,
    #[serde(default)]
    pub transport: crate::browser::TransportSettings,
    #[serde(default)]
    pub transport_revision: u64,
}
impl Account {
    pub fn new(c: Credential, index: usize) -> Self {
        Self {
            id: c.id(),
            label: format!("账号 {index}"),
            enabled: true,
            paused: false,
            cooldown_until: 0,
            quota_until: 0,
            last_start: 0,
            starts: vec![],
            credential: c,
            transport: crate::browser::TransportSettings::default(),
            transport_revision: 0,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub summary: String,
    pub covered: usize,
    pub prefix_hash: String,
    pub context_hash: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub token_id: Option<String>,
    pub id: String,
    pub key: String,
    pub account_id: String,
    pub conversation_id: String,
    pub parent_uuid: String,
    pub nonce: String,
    pub context_hash: String,
    pub history: Vec<String>,
    pub expires_at: i64,
    pub uncertain: bool,
    pub cache_tokens: usize,
    pub cache_until: i64,
    pub checkpoint: Option<Checkpoint>,
    pub context_estimate: usize,
    pub compaction_retry_after: i64,
    pub model: String,
    #[serde(skip)]
    pub active: bool,
    #[serde(skip)]
    pub active_ticket: String,
}
impl Session {
    pub fn new(key: String, context: String, account_id: String, ttl: u64, model: String) -> Self {
        Self {
            token_id: None,
            id: uuid(),
            key,
            account_id,
            conversation_id: String::new(),
            parent_uuid: String::new(),
            nonce: nonce(),
            context_hash: context,
            history: vec![],
            expires_at: now() + ttl as i64,
            uncertain: false,
            cache_tokens: 0,
            cache_until: 0,
            checkpoint: None,
            context_estimate: 0,
            compaction_retry_after: 0,
            model,
            active: false,
            active_ticket: String::new(),
        }
    }
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Cleanup {
    pub account_id: String,
    pub conversation_id: String,
    pub next_attempt: i64,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub models: crate::models::ModelConfig,
    #[serde(default)]
    pub upstream_catalog: Option<crate::models::UpstreamCatalog>,
    #[serde(default)]
    pub tokens: BTreeMap<String, crate::tokens::ApiToken>,
    pub schema_version: u32,
    pub revision: u64,
    pub settings: Settings,
    pub accounts: BTreeMap<String, Account>,
    pub sessions: BTreeMap<String, Session>,
    pub pending: Vec<Cleanup>,
    pub imported_v1: bool,
}

// Keep these settings outside the encrypted snapshot until older alpha4 builds
// no longer need to open it. Their Settings type rejects unknown JSON fields.
const fn default_native_tool_enabled() -> bool {
    true
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeToolSettings {
    native_web_search: bool,
    native_artifacts: bool,
    native_repl: bool,
    native_widgets: bool,
    #[serde(default = "default_native_tool_enabled")]
    native_project_knowledge: bool,
}
impl From<&Settings> for NativeToolSettings {
    fn from(settings: &Settings) -> Self {
        Self {
            native_web_search: settings.native_web_search,
            native_artifacts: settings.native_artifacts,
            native_repl: settings.native_repl,
            native_widgets: settings.native_widgets,
            native_project_knowledge: settings.native_project_knowledge,
        }
    }
}
impl NativeToolSettings {
    fn apply(self, settings: &mut Settings) {
        settings.native_web_search = self.native_web_search;
        settings.native_artifacts = self.native_artifacts;
        settings.native_repl = self.native_repl;
        settings.native_widgets = self.native_widgets;
        settings.native_project_knowledge = self.native_project_knowledge;
    }
}

fn storage_snapshot(snapshot: &Snapshot) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(snapshot).map_err(|_| Fault::internal())?;
    let settings = value
        .get_mut("settings")
        .and_then(Value::as_object_mut)
        .ok_or_else(Fault::internal)?;
    for field in [
        "native_web_search",
        "native_artifacts",
        "native_repl",
        "native_widgets",
        "native_project_knowledge",
    ] {
        settings.remove(field);
    }
    serde_json::to_vec(&value).map_err(|_| Fault::internal())
}

fn parse_native_tool_settings(payload: &str) -> Result<NativeToolSettings> {
    if payload.is_empty() || payload.len() > 1024 {
        return Err(Fault::internal());
    }
    serde_json::from_str(payload).map_err(|_| Fault::internal())
}

impl Snapshot {
    pub fn queue(&mut self, account: &str, conversation: &str) {
        if !conversation.is_empty()
            && !self
                .pending
                .iter()
                .any(|j| j.account_id == account && j.conversation_id == conversation)
        {
            self.pending.push(Cleanup {
                account_id: account.to_owned(),
                conversation_id: conversation.to_owned(),
                next_attempt: 0,
            });
        }
    }
    pub fn retire(&mut self, id: &str) {
        if let Some(s) = self.sessions.remove(id) {
            self.queue(&s.account_id, &s.conversation_id);
        }
    }
    pub fn expire(&mut self) {
        let expired: Vec<_> = self
            .sessions
            .values()
            .filter(|s| !s.active && s.expires_at <= now())
            .map(|s| s.id.clone())
            .collect();
        for id in expired {
            self.retire(&id);
        }
    }
}
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct Usage {
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub accounting_version: u8,
    #[serde(default)]
    pub token_id: Option<String>,
    #[serde(default)]
    pub token_name: Option<String>,
    #[serde(default)]
    pub simulated_cache: Option<bool>,
    #[serde(default)]
    pub charge: Option<crate::usage::Charge>,
    pub input_estimate: usize,
    pub output_estimate: usize,
    pub physical_estimate: usize,
    pub cache_read_simulated: usize,
    pub cache_creation_simulated: usize,
    pub internal: bool,
    pub status: u16,
    pub account_id: String,
    pub model: String,
}
pub struct Store {
    pub db: SqlitePool,
    pub state: Mutex<Snapshot>,
    cipher: Aes256Gcm,
    fault: Arc<AtomicBool>,
    status_fault: Arc<AtomicBool>,
    status_gap: Arc<AtomicBool>,
    status_tx: mpsc::Sender<StatusMessage>,
    status_worker: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub status_dropped: AtomicU64,
    _lock: std::sync::Mutex<fslock::LockFile>,
}
enum StatusMessage {
    Record(Box<crate::observability::StatusRecord>),
    Flush(oneshot::Sender<()>),
}
impl Drop for Store {
    fn drop(&mut self) {
        if let Ok(mut worker) = self.status_worker.lock()
            && let Some(worker) = worker.take()
        {
            worker.abort();
        }
    }
}
fn vault_cipher(key: &str, domain: &[u8]) -> Aes256Gcm {
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update(key.as_bytes());
    Aes256Gcm::new(&hash.finalize())
}
impl Store {
    pub async fn open(config: &Config) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&config.data_dir).map_err(|_| Fault::internal())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&config.data_dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| Fault::internal())?;
        }
        let lock_path = config.data_dir.join("instance.lock");
        let mut lock = fslock::LockFile::open(&lock_path).map_err(|_| Fault::internal())?;
        if !lock.try_lock().map_err(|_| Fault::internal())? {
            return Err(Fault::new(
                503,
                "api_error",
                "data directory already belongs to another running instance",
            ));
        }
        let db_path = config.data_dir.join("control.sqlite3");
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&db_path)
            .map_err(|_| Fault::internal())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [&db_path, &lock_path] {
                std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600))
                    .map_err(|_| Fault::internal())?;
            }
        }
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));
        let db = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .map_err(|_| Fault::internal())?;
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&db)
            .await
            .map_err(|_| Fault::internal())?;
        if !(0..=1).contains(&version) {
            return Err(Fault::new(
                503,
                "api_error",
                "unsupported database schema version",
            ));
        }
        for sql in [
            "CREATE TABLE IF NOT EXISTS vault (id INTEGER PRIMARY KEY CHECK(id=1), payload BLOB NOT NULL)",
            "CREATE TABLE IF NOT EXISTS native_tool_settings (id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS events (seq INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, kind TEXT NOT NULL, data TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS usage (seq INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, data TEXT NOT NULL)",
            "CREATE INDEX IF NOT EXISTS usage_at ON usage(at)",
            "CREATE TABLE IF NOT EXISTS protocol_logs (seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL, at INTEGER NOT NULL, account_id TEXT NOT NULL, level TEXT NOT NULL, finished_at INTEGER, data TEXT NOT NULL)",
            "CREATE INDEX IF NOT EXISTS protocol_logs_account ON protocol_logs(account_id,seq)",
            "CREATE INDEX IF NOT EXISTS protocol_logs_level ON protocol_logs(level,seq)",
            "PRAGMA user_version=1",
        ] {
            sqlx::query(sql)
                .execute(&db)
                .await
                .map_err(|_| Fault::internal())?;
        }
        let cipher = vault_cipher(&config.gateway_key, b"Claud2Api durable vault v1\0");
        let row = sqlx::query("SELECT payload FROM vault WHERE id=1")
            .fetch_optional(&db)
            .await
            .map_err(|_| Fault::internal())?;
        let mut state = if let Some(row) = row {
            let bytes: Vec<u8> = row.get(0);
            if bytes.len() < 28 || bytes.len() > 16 << 20 {
                return Err(Fault::internal());
            }
            let plain = cipher
                .decrypt(
                    Nonce::from_slice(&bytes[..12]),
                    Payload {
                        msg: &bytes[12..],
                        aad: b"Claud2Api/snapshot/1",
                    },
                )
                .or_else(|_| {
                    // Authenticated compatibility read; startup commits the current format.
                    vault_cipher(&config.gateway_key, b"ClaudAPI durable vault v1\0").decrypt(
                        Nonce::from_slice(&bytes[..12]),
                        Payload {
                            msg: &bytes[12..],
                            aad: b"ClaudAPI/snapshot/1",
                        },
                    )
                })
                .map_err(|_| {
                    Fault::new(
                        503,
                        "api_error",
                        "cannot decrypt durable state; check gateway key and backup",
                    )
                })?;
            let s: Snapshot = serde_json::from_slice(&plain).map_err(|_| Fault::internal())?;
            if s.schema_version != 1 {
                return Err(Fault::internal());
            }
            s
        } else {
            Snapshot {
                models: crate::models::ModelConfig::default(),
                upstream_catalog: None,
                tokens: BTreeMap::new(),
                schema_version: 1,
                revision: 1,
                settings: config.settings.clone(),
                accounts: BTreeMap::new(),
                sessions: BTreeMap::new(),
                pending: vec![],
                imported_v1: false,
            }
        };
        if let Some(payload) =
            sqlx::query_scalar::<_, String>("SELECT payload FROM native_tool_settings WHERE id=1")
                .fetch_optional(&db)
                .await
                .map_err(|_| Fault::internal())?
        {
            parse_native_tool_settings(&payload)?.apply(&mut state.settings);
        }
        state.settings.validate()?;
        state.models.validate()?;
        if let Some(catalog) = &state.upstream_catalog {
            catalog.validate()?;
        }
        if state.tokens.len() > 512 {
            return Err(Fault::invalid("token capacity reached"));
        }
        for (id, token) in &state.tokens {
            token.validate()?;
            if id != &token.id {
                return Err(Fault::internal());
            }
        }
        for account in state.accounts.values() {
            account.transport.validate()?;
        }
        for c in &config.accounts {
            let id = c.id();
            let index = state.accounts.len() + 1;
            state
                .accounts
                .entry(id)
                .or_insert_with(|| Account::new(c.clone(), index));
        }
        if state.accounts.values().any(|a| {
            [&config.gateway_key, &config.admin_key, &config.bot_key]
                .iter()
                .any(|k| {
                    !k.is_empty() && (**k == a.credential.session_key || **k == a.credential.cookie)
                })
        }) {
            return Err(Fault::invalid(
                "stored credentials must differ from gateway and control keys",
            ));
        }
        if state.accounts.len() > 512 || state.sessions.len() > 512 || state.pending.len() > 65536 {
            return Err(Fault::internal());
        }
        if !state.imported_v1 {
            if let Some(path) = &config.import_state
                && path.exists()
            {
                let meta = std::fs::metadata(path).map_err(|_| Fault::internal())?;
                if !meta.is_file() || meta.len() > 8 << 20 {
                    return Err(Fault::invalid("invalid Go state file"));
                }
                let bytes = std::fs::read(path).map_err(|_| Fault::internal())?;
                import_v1(&mut state, &bytes, &config.gateway_key)?;
            }
            state.imported_v1 = true;
        }
        let uncertain: Vec<_> = state
            .sessions
            .values()
            .filter(|s| s.uncertain)
            .map(|s| s.id.clone())
            .collect();
        for id in uncertain {
            state.retire(&id);
        }
        state.expire();
        close_incomplete_logs(
            &mut *db.acquire().await.map_err(|_| Fault::internal())?,
            "interrupted",
            499,
        )
        .await
        .map_err(|_| Fault::internal())?;
        let fault = Arc::new(AtomicBool::new(false));
        let status_fault = Arc::new(AtomicBool::new(false));
        let status_gap = Arc::new(AtomicBool::new(false));
        let (status_tx, status_rx) = mpsc::channel(512);
        let status_worker = tokio::spawn(status_writer(
            db.clone(),
            status_rx,
            status_fault.clone(),
            status_gap.clone(),
        ));
        let store = Arc::new(Self {
            db,
            state: Mutex::new(state),
            cipher,
            fault,
            status_fault,
            status_gap,
            status_tx,
            status_worker: std::sync::Mutex::new(Some(status_worker)),
            status_dropped: AtomicU64::new(0),
            _lock: std::sync::Mutex::new(lock),
        });
        store
            .mutate(
                "startup",
                json!({"version":env!("CARGO_PKG_VERSION")}),
                |_| Ok(()),
            )
            .await?;
        Ok(store)
    }
    pub fn healthy(&self) -> bool {
        !self.fault.load(Ordering::Acquire)
    }
    pub fn status_healthy(&self) -> bool {
        !self.status_fault.load(Ordering::Acquire)
    }
    pub fn enqueue_status(&self, record: crate::observability::StatusRecord) {
        if self
            .status_tx
            .try_send(StatusMessage::Record(Box::new(record)))
            .is_err()
        {
            self.status_dropped.fetch_add(1, Ordering::Relaxed);
            self.status_gap.store(true, Ordering::Release);
        }
    }
    pub async fn flush_status(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            self.status_tx
                .send(StatusMessage::Flush(tx))
                .await
                .map_err(|_| Fault::internal())?;
            rx.await.map_err(|_| Fault::internal())
        })
        .await
        .map_err(|_| Fault::timeout())?
    }
    pub async fn status_logs(
        &self,
        level: &str,
        account: &str,
        before: Option<i64>,
        limit: i64,
    ) -> Result<Value> {
        if !matches!(level, "all" | "error" | "normal")
            || (!account.is_empty() && (account.len() != 64 || hex::decode(account).is_err()))
            || before.is_some_and(|v| v < 1)
        {
            return Err(Fault::invalid("invalid status-log filter"));
        }
        let limit = limit.clamp(1, 200);
        let rows=sqlx::query("SELECT seq,data FROM protocol_logs WHERE (?='all' OR level=?) AND (?='' OR account_id=?) AND (? IS NULL OR seq<?) ORDER BY seq DESC LIMIT ?")
            .bind(level).bind(level).bind(account).bind(account).bind(before).bind(before).bind(limit).fetch_all(&self.db).await.map_err(|_|Fault::internal())?;
        let mut records = Vec::new();
        let mut cursor = None;
        for row in &rows {
            let seq: i64 = row.get("seq");
            cursor = Some(seq);
            let mut record: Value = serde_json::from_str(&row.get::<String, _>("data"))
                .map_err(|_| Fault::internal())?;
            if record.get("model").is_none() {
                record["model"] = Value::Null;
            }
            if record.get("token_name").is_none() {
                record["token_name"] = Value::Null;
            }
            if record.get("usage").is_none() {
                record["usage"] = Value::Null;
            }
            // Old status rows predate native-call auditing; keep them unknown
            // instead of presenting them as observed zero-call requests.
            if record.get("native_tools").is_none() {
                record["native_tools"] = Value::Null;
            }
            if record.get("native_tools_complete").is_none() {
                record["native_tools_complete"] = Value::Null;
            }
            record["seq"] = json!(seq);
            records.push(record);
        }
        Ok(
            json!({"schema_version":1,"records":records,"next_before":cursor,"has_more":rows.len()==limit as usize,"retention_limit":10000,"retention_days":7,"dropped":self.status_dropped.load(Ordering::Relaxed),"healthy":self.status_healthy()}),
        )
    }
    pub async fn read(&self) -> Snapshot {
        self.state.lock().await.clone()
    }
    pub async fn mutate<T>(
        &self,
        kind: &str,
        event: Value,
        change: impl FnOnce(&mut Snapshot) -> Result<T>,
    ) -> Result<T> {
        if !self.healthy() {
            return Err(Fault::internal());
        }
        let mut guard = self.state.lock().await;
        let mut next = guard.clone();
        let output = change(&mut next)?;
        if next.sessions.len() > 512 || next.pending.len() > 65536 {
            return Err(Fault::busy("session state capacity reached", 15));
        }
        let full_state = serde_json::to_vec(&next).map_err(|_| Fault::internal())?;
        if full_state.len() > 16 << 20 {
            return Err(Fault::internal());
        }
        // Active tickets are intentionally runtime-only. Empty sweeps and runtime
        // ownership changes must not encrypt/fsync an unchanged durable snapshot.
        // Named events still commit even when their state happens to be unchanged.
        if kind.is_empty()
            && full_state == serde_json::to_vec(&*guard).map_err(|_| Fault::internal())?
        {
            *guard = next;
            return Ok(output);
        }
        let plain = storage_snapshot(&next)?;
        if plain.len() > 16 << 20 {
            return Err(Fault::internal());
        }
        let native_settings = serde_json::to_string(&NativeToolSettings::from(&next.settings))
            .map_err(|_| Fault::internal())?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let encrypted = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: &plain,
                    aad: b"Claud2Api/snapshot/1",
                },
            )
            .map_err(|_| Fault::internal())?;
        let payload = [nonce.as_slice(), encrypted.as_slice()].concat();
        let saved = async {
            let mut tx = self.db.begin().await?;
            sqlx::query("INSERT INTO vault (id,payload) VALUES (1,?) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload")
                .bind(payload).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO native_tool_settings (id,payload) VALUES (1,?) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload")
                .bind(native_settings).execute(&mut *tx).await?;
            if !kind.is_empty() {
                sqlx::query("INSERT INTO events(at,kind,data) VALUES(?,?,?)")
                    .bind(now()).bind(kind).bind(event.to_string()).execute(&mut *tx).await?;
            }
            tx.commit().await
        }.await;
        if saved.is_err() {
            self.fault.store(true, Ordering::Release);
            return Err(Fault::internal());
        }
        *guard = next;
        Ok(output)
    }
    pub async fn record(&self, usage: &Usage) -> Result<()> {
        if sqlx::query("INSERT INTO usage(at,data) VALUES(?,?)")
            .bind(now())
            .bind(serde_json::to_string(usage).map_err(|_| Fault::internal())?)
            .execute(&self.db)
            .await
            .is_err()
        {
            self.fault.store(true, Ordering::Release);
            return Err(Fault::internal());
        }
        Ok(())
    }
    pub async fn record_scoped(
        &self,
        mut usage: Usage,
        scope: &crate::guard::Scope,
        cache: bool,
    ) -> Result<()> {
        usage.accounting_version = 1;
        usage.simulated_cache = Some(cache);
        if let Some(trace) = &scope.trace {
            usage.started_at = trace.generation_started(&usage.account_id);
            usage.token_id = trace.token_id();
            usage.token_name = trace.token_name();
        }
        usage.charge = crate::usage::quote(&usage);
        self.record(&usage).await?;
        if usage.status == 200
            && !usage.internal
            && let Some(trace) = &scope.trace
        {
            trace.complete_usage(
                usage.input_estimate,
                usage.output_estimate,
                cache,
                usage.cache_read_simulated,
            );
        }
        Ok(())
    }
    pub async fn events(&self, after: i64, limit: i64) -> Result<Value> {
        let rows =
            sqlx::query("SELECT seq,at,kind,data FROM events WHERE seq>? ORDER BY seq LIMIT ?")
                .bind(after.max(0))
                .bind(limit.clamp(1, 200))
                .fetch_all(&self.db)
                .await
                .map_err(|_| Fault::internal())?;
        let mut cursor = after.max(0);
        let events:Vec<_>=rows.iter().map(|r|{let seq:i64=r.get(0);cursor=seq;json!({"seq":seq,"at":r.get::<i64,_>(1),"kind":r.get::<String,_>(2),"data":serde_json::from_str::<Value>(&r.get::<String,_>(3)).unwrap_or(Value::Null)})}).collect();
        Ok(json!({"schema_version":1,"cursor":cursor,"events":events}))
    }
    pub async fn usage(&self, since: i64) -> Result<Value> {
        let rows =
            sqlx::query("SELECT at,data FROM usage WHERE at>=? ORDER BY seq DESC LIMIT 10000")
                .bind(since)
                .fetch_all(&self.db)
                .await
                .map_err(|_| Fault::internal())?;
        let mut by = BTreeMap::<String, Value>::new();
        let mut total = 0usize;
        let mut input = 0usize;
        let mut output = 0usize;
        let mut physical = 0usize;
        for r in &rows {
            let u: Usage =
                serde_json::from_str(&r.get::<String, _>(1)).map_err(|_| Fault::internal())?;
            total += 1;
            input += u.input_estimate;
            output += u.output_estimate;
            physical += u.physical_estimate;
            let e=by.entry(u.account_id.clone()).or_insert_with(||json!({"requests":0,"input_estimate":0,"output_estimate":0,"physical_estimate":0}));
            for (k, v) in [
                ("requests", 1),
                ("input_estimate", u.input_estimate),
                ("output_estimate", u.output_estimate),
                ("physical_estimate", u.physical_estimate),
            ] {
                e[k] = json!(e[k].as_u64().unwrap_or(0) + v as u64);
            }
        }
        Ok(
            json!({"schema_version":1,"estimated":true,"provider_remaining":null,"since":since,"window_seconds":now()-since,"requests":total,"input_estimate":input,"output_estimate":output,"physical_estimate":physical,"by_account":by,"truncated":rows.len()==10000}),
        )
    }
}
async fn status_writer(
    db: SqlitePool,
    mut rx: mpsc::Receiver<StatusMessage>,
    fault: Arc<AtomicBool>,
    gap: Arc<AtomicBool>,
) {
    while let Some(first) = rx.recv().await {
        let mut messages = vec![first];
        while messages.len() < 64 {
            match rx.try_recv() {
                Ok(next) => messages.push(next),
                Err(_) => break,
            }
        }
        let mut records = Vec::new();
        let mut barriers = Vec::new();
        for message in messages {
            match message {
                StatusMessage::Record(record) => records.push(record),
                StatusMessage::Flush(tx) => barriers.push(tx),
            }
        }
        let result=async {
            let incomplete = gap.swap(false, Ordering::AcqRel);
            if !records.is_empty() || incomplete {
                let mut tx=db.begin().await?;
                for record in records {
                    let data=serde_json::to_string(&record).expect("bounded status metadata");
                    sqlx::query("INSERT INTO protocol_logs(id,at,account_id,level,finished_at,data) VALUES(?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET account_id=excluded.account_id,level=excluded.level,finished_at=excluded.finished_at,data=excluded.data WHERE protocol_logs.finished_at IS NULL OR excluded.finished_at IS NOT NULL")
                        .bind(record.id).bind(record.at).bind(record.account_id).bind(record.level).bind(record.finished_at).bind(data).execute(&mut *tx).await?;
                }
                if incomplete { close_incomplete_logs(&mut tx, "unknown", 0).await?; }
                sqlx::query("DELETE FROM protocol_logs WHERE at<? OR seq < (SELECT seq FROM protocol_logs ORDER BY seq DESC LIMIT 1 OFFSET 9999)").bind(now()-7*86400).execute(&mut *tx).await?;
                tx.commit().await?;
            }
            Ok::<_,sqlx::Error>(())
        }.await;
        if result.is_err() {
            fault.store(true, Ordering::Release);
            eprintln!("status_log_failed code=storage");
            break;
        }
        for barrier in barriers {
            let _ = barrier.send(());
        }
    }
}
async fn close_incomplete_logs(
    db: &mut sqlx::SqliteConnection,
    phase: &str,
    status: u16,
) -> std::result::Result<(), sqlx::Error> {
    let rows = sqlx::query("SELECT seq,data FROM protocol_logs WHERE finished_at IS NULL")
        .fetch_all(&mut *db)
        .await?;
    for row in rows {
        let mut data: Value = serde_json::from_str(&row.get::<String, _>("data"))
            .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
        data["phase"] = json!(phase);
        data["level"] = json!("error");
        data["status"] = json!(status);
        data["finished_at"] = json!(now());
        sqlx::query("UPDATE protocol_logs SET level='error',finished_at=?,data=? WHERE seq=?")
            .bind(now())
            .bind(data.to_string())
            .bind(row.get::<i64, _>("seq"))
            .execute(&mut *db)
            .await?;
    }
    Ok(())
}
fn timestamp(value: Option<&Value>) -> Result<i64> {
    match value.and_then(Value::as_str) {
        None | Some("") | Some("0001-01-01T00:00:00Z") => Ok(0),
        Some(s) => time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
            .map(|v| v.unix_timestamp())
            .map_err(|_| Fault::invalid("invalid Go state timestamp")),
    }
}
fn hexlen(value: &str, len: usize) -> bool {
    value.len() == len && hex::decode(value).is_ok()
}
fn id(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok()
}
pub fn import_v1(state: &mut Snapshot, bytes: &[u8], key: &str) -> Result<()> {
    let disk: Value =
        serde_json::from_slice(bytes).map_err(|_| Fault::invalid("invalid Go state JSON"))?;
    if disk["version"] != 1 || bytes.len() > 8 << 20 {
        return Err(Fault::invalid("invalid Go state version or size"));
    }
    let sessions = disk["sessions"]
        .as_array()
        .ok_or_else(|| Fault::invalid("invalid Go state sessions"))?;
    if sessions.len() > 512 {
        return Err(Fault::invalid("too many Go sessions"));
    }
    let mut keys = std::collections::HashSet::new();
    for v in sessions {
        let s = |k: &str| v[k].as_str().unwrap_or("").to_owned();
        let n = |k: &str| v[k].as_u64().unwrap_or(0) as usize;
        let history: Vec<String> = match v.get("history").filter(|v| !v.is_null()) {
            None => vec![],
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|_| Fault::invalid("invalid Go history"))?,
        };
        let checkpoint = if let Some(cp) = v.get("checkpoint").filter(|v| !v.is_null()) {
            let cp: Checkpoint = serde_json::from_value(cp.clone())
                .map_err(|_| Fault::invalid("invalid Go checkpoint"))?;
            if cp.covered == 0
                || cp.covered > history.len()
                || cp.context_hash != s("context_hash")
                || cp.prefix_hash
                    != protocol::digest(key, "compaction-prefix", &history[..cp.covered])
            {
                return Err(Fault::invalid("invalid Go checkpoint fingerprint"));
            }
            crate::engine::normalize_summary(&cp.summary)?;
            Some(cp)
        } else {
            None
        };
        let session = Session {
            token_id: None,
            id: s("id"),
            key: s("key"),
            account_id: s("account_id"),
            conversation_id: s("conversation_id"),
            parent_uuid: s("parent_uuid"),
            nonce: s("nonce"),
            context_hash: s("context_hash"),
            history,
            expires_at: timestamp(v.get("expires_at"))?,
            uncertain: v["uncertain"].as_bool().unwrap_or(false),
            cache_tokens: n("simulated_cache_tokens"),
            cache_until: timestamp(v.get("simulated_cache_until"))?,
            checkpoint,
            context_estimate: n("context_estimate"),
            compaction_retry_after: timestamp(v.get("compaction_retry_after"))?,
            model: String::new(),
            active: false,
            active_ticket: String::new(),
        };
        if !id(&session.id)
            || !hexlen(&session.account_id, 64)
            || !hexlen(&session.nonce, 32)
            || !hexlen(&session.context_hash, 64)
            || session.history.len() > 2001
            || session.history.iter().any(|h| !hexlen(h, 64))
            || session.expires_at == 0
            || state.sessions.contains_key(&session.id)
            || (!session.key.is_empty()
                && (!hexlen(&session.key, 64) || !keys.insert(session.key.clone())))
            || (!session.conversation_id.is_empty() && !id(&session.conversation_id))
            || (!session.parent_uuid.is_empty() && !id(&session.parent_uuid))
            || session.cache_tokens > 32 << 20
            || session.context_estimate > 32 << 20
            || (session.conversation_id.is_empty()
                && (!session.parent_uuid.is_empty() || !session.history.is_empty()))
            || (!session.conversation_id.is_empty()
                && !session.uncertain
                && (session.parent_uuid.is_empty() || session.history.len() < 2))
        {
            return Err(Fault::invalid("invalid Go session binding"));
        }
        state.sessions.insert(session.id.clone(), session);
    }
    if let Some(pending) = disk.get("pending").and_then(Value::as_array) {
        if pending.len() > 65536 {
            return Err(Fault::invalid("too many cleanup records"));
        }
        for job in pending {
            let account = job["account_id"].as_str().unwrap_or("");
            let conversation = job["conversation_id"].as_str().unwrap_or("");
            if !hexlen(account, 64) || !id(conversation) {
                return Err(Fault::invalid("invalid Go cleanup record"));
            }
            state.queue(account, conversation);
        }
    }
    if let Some(limits) = disk.get("account_rate_limits").and_then(Value::as_array) {
        let mut seen = std::collections::HashSet::new();
        if limits.len() > 65536 {
            return Err(Fault::invalid("too many account limit records"));
        }
        for l in limits {
            let account = l["account_id"].as_str().unwrap_or("");
            let until = timestamp(l.get("until"))?;
            if !hexlen(account, 64) || until == 0 || !seen.insert(account) {
                return Err(Fault::invalid("invalid Go account limit"));
            }
            if let Some(a) = state.accounts.get_mut(account) {
                a.cooldown_until = a.cooldown_until.max(until);
                a.quota_until = a.quota_until.max(until);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BrowserProfile;
    use std::net::SocketAddr;
    use tempfile::TempDir;

    fn config(data_dir: &TempDir) -> Config {
        Config {
            browser_profile: BrowserProfile::Chrome146,
            public_bind: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
            control_bind: "127.0.0.1:18082".parse().unwrap(),
            gateway_key: "gateway-store-test-key-at-least-32-bytes".to_owned(),
            admin_key: "admin-store-test-key-at-least-32-bytes".to_owned(),
            bot_key: String::new(),
            data_dir: data_dir.path().to_owned(),
            import_state: None,
            base_url: "http://127.0.0.1:8080".to_owned(),
            locale: "en-US".to_owned(),
            timezone: "UTC".to_owned(),
            default_model: "test".to_owned(),
            effort: "medium".to_owned(),
            legacy_thinking: "off".to_owned(),
            accounts: vec![],
            settings: Settings::default(),
        }
    }

    #[tokio::test]
    async fn old_status_logs_keep_native_activity_and_completeness_unknown() {
        let data_dir = tempfile::tempdir().unwrap();
        let store = Store::open(&config(&data_dir)).await.unwrap();
        sqlx::query("INSERT INTO protocol_logs(id,at,account_id,level,finished_at,data) VALUES(?,?,?,?,?,?)")
            .bind("legacy-status").bind(now()).bind("").bind("normal").bind(now())
            .bind(r#"{"direction":"downstream","status":200}"#)
            .execute(&store.db).await.unwrap();
        let logs = store.status_logs("all", "", None, 10).await.unwrap();
        let record = &logs["records"][0];
        assert_eq!(record.get("native_tools"), Some(&Value::Null));
        assert_eq!(record.get("native_tools_complete"), Some(&Value::Null));
    }

    #[tokio::test]
    async fn native_tool_settings_live_outside_legacy_snapshot_and_survive_reopen() {
        let data_dir = tempfile::tempdir().unwrap();
        let config = config(&data_dir);
        let store = Store::open(&config).await.unwrap();
        store
            .mutate("settings_updated", json!({}), |snapshot| {
                snapshot.settings.native_web_search = false;
                snapshot.settings.native_artifacts = true;
                snapshot.settings.native_repl = false;
                snapshot.settings.native_widgets = true;
                snapshot.settings.native_project_knowledge = false;
                Ok(())
            })
            .await
            .unwrap();

        let encrypted: Vec<u8> = sqlx::query_scalar("SELECT payload FROM vault WHERE id=1")
            .fetch_one(&store.db)
            .await
            .unwrap();
        let plain = store
            .cipher
            .decrypt(
                Nonce::from_slice(&encrypted[..12]),
                Payload {
                    msg: &encrypted[12..],
                    aad: b"Claud2Api/snapshot/1",
                },
            )
            .unwrap();
        let persisted: Value = serde_json::from_slice(&plain).unwrap();
        let legacy_settings = persisted["settings"].clone();
        assert!(legacy_settings.get("session_ttl_seconds").is_some());
        for field in [
            "native_web_search",
            "native_artifacts",
            "native_repl",
            "native_widgets",
            "native_project_knowledge",
        ] {
            assert!(
                legacy_settings.get(field).is_none(),
                "{field} must not be in the vault"
            );
        }
        // The published alpha4 Settings parser denies unknown fields.
        let _: Settings = serde_json::from_value(legacy_settings).unwrap();

        let native_payload: String =
            sqlx::query_scalar("SELECT payload FROM native_tool_settings WHERE id=1")
                .fetch_one(&store.db)
                .await
                .unwrap();
        let native_settings = parse_native_tool_settings(&native_payload).unwrap();
        assert!(!native_settings.native_web_search);
        assert!(native_settings.native_artifacts);
        assert!(!native_settings.native_repl);
        assert!(native_settings.native_widgets);
        assert!(!native_settings.native_project_knowledge);
        let four_flag_record: NativeToolSettings = serde_json::from_value(json!({
            "native_web_search": false,
            "native_artifacts": true,
            "native_repl": false,
            "native_widgets": true
        }))
        .unwrap();
        assert!(four_flag_record.native_project_knowledge);
        drop(store);

        let reopened = Store::open(&config).await.unwrap();
        let settings = reopened.read().await.settings;
        assert!(!settings.native_web_search);
        assert!(settings.native_artifacts);
        assert!(!settings.native_repl);
        assert!(settings.native_widgets);
        assert!(!settings.native_project_knowledge);
    }

    #[tokio::test]
    async fn malformed_native_tool_settings_fail_closed_on_open() {
        let data_dir = tempfile::tempdir().unwrap();
        let config = config(&data_dir);
        let store = Store::open(&config).await.unwrap();
        drop(store);

        let db_path = data_dir.path().join("control.sqlite3");
        let db = SqlitePoolOptions::new()
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&db_path)
                    .create_if_missing(false),
            )
            .await
            .unwrap();
        sqlx::query("UPDATE native_tool_settings SET payload=? WHERE id=1")
            .bind(
                r#"{"native_web_search":"false","native_artifacts":true,"native_repl":false,"native_widgets":true}"#,
            )
            .execute(&db)
            .await
            .unwrap();
        db.close().await;

        assert!(Store::open(&config).await.is_err());
    }
}
