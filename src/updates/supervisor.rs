use super::{
    disk::{self, Installed, Registry, Transaction},
    github::{self, Listing, Release, Source},
    policy::Policy,
    version::Version,
};
use crate::{
    config::Config,
    error::{Fault, Result},
    store::now,
    web::equal,
};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    process::Child,
    sync::{Mutex, RwLock, Semaphore},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct Target {
    public: SocketAddr,
    control: SocketAddr,
    token: String,
}
#[derive(Default)]
struct Catalog {
    releases: Vec<Release>,
    etag: Option<String>,
    revision: u64,
    checked_at: i64,
    next_check: i64,
    error: Option<String>,
    truncated: bool,
}
#[derive(Clone, Default, Serialize)]
struct Job {
    id: Option<String>,
    phase: &'static str,
    target: Option<String>,
    error: Option<String>,
    recovered: bool,
    automatic: bool,
    total: u64,
}
struct Manager {
    config: Config,
    root: PathBuf,
    source: Source,
    registry: Mutex<Registry>,
    policy: Mutex<Policy>,
    catalog: Mutex<Catalog>,
    job: Mutex<Job>,
    target: Arc<RwLock<Option<Target>>>,
    worker: Mutex<Option<Child>>,
    checking: AtomicBool,
    busy: AtomicBool,
    downloaded: AtomicU64,
    ingress: Arc<Semaphore>,
    stop: CancellationToken,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Apply {
    expected_version: String,
    target_version: String,
    catalog_revision: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyChange {
    expected_revision: u64,
    auto_patch: bool,
}
fn patch_choice<'a>(releases: &'a [Release], current: &Version) -> Option<&'a Release> {
    releases.iter().find(|r| {
        (!r.prerelease || current.prerelease())
            && Version::parse(&r.version).is_some_and(|target| target.is_patch_of(current))
    })
}
fn choices<'a>(
    releases: &'a [Release],
    current: &Version,
) -> (Option<&'a Release>, Vec<&'a Release>) {
    let latest = releases.iter().find(|r| {
        Version::parse(&r.version).is_some_and(|v| {
            v > *current && (current.prerelease() || (!v.prerelease() && !r.prerelease))
        })
    });
    let previous = releases
        .iter()
        .filter(|r| Version::parse(&r.version).is_some_and(|v| v < *current))
        .take(2)
        .collect();
    (latest, previous)
}

impl Manager {
    async fn open(
        config: Config,
        stop: CancellationToken,
    ) -> Result<(Arc<Self>, fslock::LockFile)> {
        disk::protected_dir(&config.data_dir)?;
        let root = config.data_dir.join("updates");
        for path in [
            &root,
            &root.join("versions"),
            &root.join("private"),
            &root.join("backups"),
        ] {
            disk::protected_dir(path)?;
            disk::sync_parent(path)?;
        }
        let mut lock =
            fslock::LockFile::open(&root.join("supervisor.lock")).map_err(|_| Fault::internal())?;
        if !lock.try_lock().map_err(|_| Fault::internal())? {
            return Err(Fault::new(
                503,
                "api_error",
                "another update supervisor owns this state directory",
            ));
        }
        let source = Source::load()?;
        let registry = Registry::load(&root)?;
        let mut policy = Policy::load(&root)?;
        policy.completed(&root, &registry.active.version)?;
        let pending = root.join("transaction.json");
        if pending.exists() {
            let transaction: Transaction = disk::read_json(&pending, 16384)?;
            transaction.old.validate()?;
            transaction.target.validate()?;
            let backup = disk::backup_path(&root, &transaction.backup)?;
            if registry.active.sha256 != transaction.target.sha256 && backup.exists() {
                disk::restore(&config.data_dir, &backup).await?;
            }
            std::fs::remove_file(pending).map_err(|_| Fault::internal())?;
        }
        let manager = Arc::new(Self {
            config,
            root,
            source,
            registry: Mutex::new(registry),
            policy: Mutex::new(policy),
            catalog: Mutex::new(Catalog::default()),
            job: Mutex::new(Job {
                phase: "idle",
                ..Default::default()
            }),
            target: Arc::new(RwLock::new(None)),
            worker: Mutex::new(None),
            checking: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            downloaded: AtomicU64::new(0),
            ingress: Arc::new(Semaphore::new(256)),
            stop,
        });
        let active = manager.registry.lock().await.active.clone();
        let (target, child) = manager.start_worker(&active).await?;
        *manager.target.write().await = Some(target);
        *manager.worker.lock().await = Some(child);
        Ok((manager, lock))
    }
    async fn start_worker(&self, installed: &Installed) -> Result<(Target, Child)> {
        let executable = installed.verify(&self.root)?;
        disk::preflight(&executable, &installed.version).await?;
        // An orphan from an abruptly terminated supervisor closes its stdin and
        // drains before releasing this same lock. Never race a second writer.
        drop(disk::database_lock(&self.config.data_dir).await?);
        let token = format!("{}{}", crate::store::nonce(), crate::store::nonce());
        let ready = self
            .root
            .join("private")
            .join(format!("{}.json", uuid::Uuid::new_v4()));
        let mut child = tokio::process::Command::new(executable)
            .arg("--worker")
            .env("BIND_HOST", "127.0.0.1")
            .env("PORT", "0")
            .env("CONTROL_BIND", "127.0.0.1:0")
            .env("CLAUD2API_DATA_DIR", &self.config.data_dir)
            .env("CLAUD2API_WORKER_TOKEN", &token)
            .env("CLAUD2API_WORKER_READY", &ready)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| Fault::new(503, "api_error", "cannot start release worker"))?;
        let attempt = async {
            for _ in 0..300 {
                if child.try_wait().map_err(|_| Fault::internal())?.is_some() {
                    return Err(Fault::new(
                        503,
                        "api_error",
                        "release worker exited during startup",
                    ));
                }
                if ready.exists() {
                    let value: Value = disk::read_json(&ready, 8192)?;
                    let public: SocketAddr = value["public"]
                        .as_str()
                        .ok_or_else(Fault::internal)?
                        .parse()
                        .map_err(|_| Fault::internal())?;
                    let control: SocketAddr = value["control"]
                        .as_str()
                        .ok_or_else(Fault::internal)?
                        .parse()
                        .map_err(|_| Fault::internal())?;
                    if value["token"] != token
                        || value["build"]["version"] != installed.version
                        || !public.ip().is_loopback()
                        || !control.ip().is_loopback()
                        || public.port() == 0
                        || control.port() == 0
                        || public == control
                    {
                        return Err(Fault::internal());
                    }
                    let target = Target {
                        public,
                        control,
                        token: token.clone(),
                    };
                    if let Ok(info) = self.internal(&target, "/info", None).await
                        && info["healthy"] == true
                        && info["version"] == installed.version
                    {
                        return Ok(target);
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(Fault::new(
                503,
                "api_error",
                "release health check timed out",
            ))
        }
        .await;
        let _ = std::fs::remove_file(&ready);
        match attempt {
            Ok(target) => Ok((target, child)),
            Err(f) => {
                child.stdin.take();
                let _ = child.kill().await;
                let _ = child.wait().await;
                Err(f)
            }
        }
    }
    async fn internal(&self, target: &Target, path: &str, body: Option<Value>) -> Result<Value> {
        let request = self
            .source
            .client
            .request(
                if body.is_some() {
                    wreq::Method::POST
                } else {
                    wreq::Method::GET
                },
                format!("http://{}/control/v1{path}", target.control),
            )
            .header("authorization", format!("Bearer {}", self.config.admin_key))
            .header("x-claud2api-worker", &target.token)
            .timeout(Duration::from_secs(5));
        let response = if let Some(body) = body {
            request.json(&body)
        } else {
            request
        }
        .send()
        .await
        .map_err(|_| Fault::upstream())?;
        if !response.status().is_success() {
            return Err(Fault::new(
                503,
                "api_error",
                "worker handoff request failed",
            ));
        }
        serde_json::from_slice(&github::bounded(response, 1 << 20).await?)
            .map_err(|_| Fault::upstream())
    }
    async fn stop_worker(&self, target: &Target) -> Result<()> {
        let mut child = self
            .worker
            .lock()
            .await
            .take()
            .ok_or_else(Fault::internal)?;
        let _ = self
            .internal(target, "/_runtime/stop", Some(json!({})))
            .await;
        child.stdin.take();
        if !matches!(
            tokio::time::timeout(Duration::from_secs(20), child.wait()).await,
            Ok(Ok(_))
        ) {
            let _ = child.kill().await;
            child.wait().await.map_err(|_| Fault::internal())?;
        }
        Ok(())
    }
    async fn check(self: &Arc<Self>, manual: bool) {
        if self.source.disabled || self.checking.swap(true, Ordering::AcqRel) {
            return;
        }
        let etag = {
            let c = self.catalog.lock().await;
            if now() < c.next_check && (!manual || c.error.is_some() || now() < c.checked_at + 30) {
                self.checking.store(false, Ordering::Release);
                return;
            }
            c.etag.clone()
        };
        let result = self.source.list(etag.as_deref()).await;
        let mut catalog = self.catalog.lock().await;
        catalog.checked_at = now();
        catalog.next_check = now() + 300;
        match result {
            Ok(Listing::Unchanged) => catalog.error = None,
            Ok(Listing::Changed {
                mut releases,
                etag,
                truncated,
            }) => {
                releases
                    .sort_by(|a, b| Version::parse(&b.version).cmp(&Version::parse(&a.version)));
                releases.dedup_by(|a, b| Version::parse(&a.version) == Version::parse(&b.version));
                catalog.releases = releases;
                catalog.etag = etag;
                catalog.truncated = truncated;
                catalog.revision = catalog.revision.saturating_add(1);
                catalog.error = None;
            }
            Err(f) => {
                catalog.error = Some(f.message.into());
                catalog.next_check = f.retry_at;
            }
        }
        self.checking.store(false, Ordering::Release);
        let checked = catalog.error.is_none();
        drop(catalog);
        if checked {
            self.maybe_auto_patch().await;
        }
    }
    async fn maybe_auto_patch(self: &Arc<Self>) {
        if self.source.disabled || self.stop.is_cancelled() || self.busy.load(Ordering::Acquire) {
            return;
        }
        let registry = self.registry.lock().await;
        let Some(current) = Version::parse(&registry.active.version) else {
            return;
        };
        let catalog = self.catalog.lock().await;
        let policy = self.policy.lock().await;
        if !policy.enabled || catalog.error.is_some() || catalog.checked_at == 0 {
            return;
        }
        let candidate = catalog.releases.iter().find(|r| {
            (!r.prerelease || current.prerelease())
                && Version::parse(&r.version).is_some_and(|target| target.is_patch_of(&current))
                && !policy.blocked_versions.contains(&r.version)
        });
        let Some(release) = candidate else { return };
        let input = Apply {
            expected_version: registry.active.version.clone(),
            target_version: release.version.clone(),
            catalog_revision: catalog.revision,
        };
        drop(policy);
        drop(catalog);
        drop(registry);
        // accept rechecks the catalog, policy and current version under their locks.
        let _ = self.accept_kind(input, true).await;
    }
    async fn change_policy(self: &Arc<Self>, input: PolicyChange) -> Result<()> {
        if self.source.disabled {
            return Err(Fault::new(409, "api_error", "managed updates are disabled"));
        }
        let mut policy = self.policy.lock().await;
        if self.busy.load(Ordering::Acquire) {
            return Err(Fault::new(
                409,
                "api_error",
                "a version change is already running",
            ));
        }
        if policy.revision != input.expected_revision {
            return Err(Fault::new(
                409,
                "api_error",
                "update policy changed; check again",
            ));
        }
        policy.set_enabled(&self.root, input.auto_patch)?;
        drop(policy);
        if input.auto_patch {
            self.maybe_auto_patch().await;
        }
        Ok(())
    }
    async fn status(&self) -> Value {
        let registry = self.registry.lock().await;
        let current = Version::parse(&registry.active.version).expect("validated registry");
        let catalog = self.catalog.lock().await;
        let (latest, previous) = choices(&catalog.releases, &current);
        let patch = patch_choice(&catalog.releases, &current);
        let policy = self.policy.lock().await;
        let previous: Vec<_> = previous
            .into_iter()
            .map(|r| json!({"version":r.version,"published_at":r.published_at}))
            .collect();
        let job = self.job.lock().await.clone();
        json!({"schema_version":1,"repository":self.source.repository,"current_version":registry.active.version,"supervisor_version":crate::VERSION,"protocol":super::PROTOCOL,"enabled":!self.source.disabled,"auto_patch_supported":true,"auto_patch":{"enabled":policy.enabled,"revision":policy.revision,"blocked_versions":policy.blocked_versions},"patch_update":patch.map(|r|json!({"version":r.version,"published_at":r.published_at})),"checking":self.checking.load(Ordering::Acquire),"checked_at":catalog.checked_at,"next_check_at":catalog.next_check,"check_interval_seconds":300,"catalog_revision":catalog.revision,"check_error":catalog.error,"history_truncated":catalog.truncated,"update_available":latest.is_some(),"latest":latest.map(|r|json!({"version":r.version,"published_at":r.published_at})),"rollback_versions":previous,"busy":self.busy.load(Ordering::Acquire),"job":job,"downloaded":self.downloaded.load(Ordering::Relaxed)})
    }
    async fn accept(self: &Arc<Self>, input: Apply) -> Result<Value> {
        self.accept_kind(input, false).await
    }
    async fn accept_kind(self: &Arc<Self>, input: Apply, automatic: bool) -> Result<Value> {
        if self.source.disabled {
            return Err(Fault::new(409, "api_error", "managed updates are disabled"));
        }
        let registry = self.registry.lock().await;
        if registry.active.version != input.expected_version {
            return Err(Fault::new(
                409,
                "api_error",
                "running version changed; reopen version manager",
            ));
        }
        let catalog = self.catalog.lock().await;
        if catalog.revision != input.catalog_revision {
            return Err(Fault::new(
                409,
                "api_error",
                "release catalog changed; check again",
            ));
        }
        let current = Version::parse(&registry.active.version).ok_or_else(Fault::internal)?;
        let target = Version::parse(&input.target_version)
            .ok_or_else(|| Fault::invalid("invalid release version"))?;
        let (latest, previous) = choices(&catalog.releases, &current);
        let patch = patch_choice(&catalog.releases, &current);
        let eligible = latest.is_some_and(|r| r.version == input.target_version)
            || patch.is_some_and(|r| r.version == input.target_version)
            || previous.iter().any(|r| r.version == input.target_version);
        let release = catalog
            .releases
            .iter()
            .find(|r| r.version == input.target_version)
            .cloned()
            .ok_or_else(|| Fault::new(409, "api_error", "target release is not in the catalog"))?;
        let mut policy = self.policy.lock().await;
        let automatic_eligible = policy.enabled
            && target.is_patch_of(&current)
            && (!release.prerelease || current.prerelease())
            && !policy.blocked_versions.contains(&input.target_version)
            && catalog.error.is_none();
        if target == current || (automatic && !automatic_eligible) || (!automatic && !eligible) {
            return Err(Fault::new(
                409,
                "api_error",
                "target is not an eligible patch, latest update or preceding release",
            ));
        }
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(Fault::new(
                409,
                "api_error",
                "a version change is already running",
            ));
        }
        if let Err(f) = policy.attempted(&self.root, &release.version, target < current, automatic)
        {
            self.busy.store(false, Ordering::Release);
            return Err(f);
        }
        drop(policy);
        drop(catalog);
        drop(registry);
        let id = uuid::Uuid::new_v4().to_string();
        *self.job.lock().await = Job {
            id: Some(id.clone()),
            phase: "queued",
            target: Some(release.version.clone()),
            automatic,
            ..Default::default()
        };
        self.downloaded.store(0, Ordering::Relaxed);
        let manager = self.clone();
        tokio::spawn(async move {
            let version = release.version.clone();
            let result = manager.apply(release).await;
            if result.is_ok() {
                let _ = manager
                    .policy
                    .lock()
                    .await
                    .completed(&manager.root, &version);
            }
            let mut job = manager.job.lock().await;
            match result {
                Ok(()) => job.phase = "completed",
                Err(f) => {
                    job.phase = "failed";
                    job.error = Some(f.message);
                }
            }
            manager.busy.store(false, Ordering::Release);
        });
        Ok(json!({"schema_version":1,"job_id":id,"accepted":true}))
    }
    async fn phase(&self, phase: &'static str) {
        self.job.lock().await.phase = phase;
    }
    async fn staged(&self, release: &Release) -> Result<Installed> {
        self.phase("downloading").await;
        let release = self.source.resolve(&release.tag).await?;
        let manifest = self.source.manifest(&release).await?;
        self.job.lock().await.total = manifest.size;
        let installed = Installed {
            version: release.version.clone(),
            sha256: manifest.sha256,
            size: manifest.size,
            tag: Some(release.tag.clone()),
        };
        let path = installed.path(&self.root)?;
        disk::protected_dir(path.parent().ok_or_else(Fault::internal)?)?;
        disk::sync_parent(path.parent().ok_or_else(Fault::internal)?)?;
        if !path.exists() {
            let temp = path.with_extension(format!("download-{}", uuid::Uuid::new_v4()));
            if let Err(f) = self
                .source
                .download(&release, &temp, |n| {
                    self.downloaded.store(n, Ordering::Relaxed)
                })
                .await
            {
                let _ = std::fs::remove_file(temp);
                return Err(f);
            }
            disk::executable_permissions(&temp)?;
            std::fs::rename(temp, &path).map_err(|_| Fault::internal())?;
            disk::sync_parent(&path)?;
        }
        installed.verify(&self.root)?;
        self.phase("verifying").await;
        disk::preflight(&path, &installed.version).await?;
        Ok(installed)
    }
    async fn apply(&self, release: Release) -> Result<()> {
        let candidate = self.staged(&release).await?;
        self.phase("waiting").await;
        let started = Instant::now();
        // try_write leaves ordinary traffic running while busy responses drain.
        let mut gate = loop {
            if self.stop.is_cancelled() {
                return Err(Fault::cancelled());
            }
            if let Ok(gate) = self.target.clone().try_write_owned() {
                break gate;
            }
            if started.elapsed() > Duration::from_secs(1800) {
                return Err(Fault::new(
                    409,
                    "api_error",
                    "no idle switch window; existing traffic was left running",
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let old_target = gate.clone().ok_or_else(Fault::internal)?;
        let old = self.registry.lock().await.clone();
        let handoff = self
            .internal(&old_target, "/_runtime/export", Some(json!({})))
            .await?;
        let backup_name = uuid::Uuid::new_v4().to_string();
        let transaction = Transaction {
            old: old.active.clone(),
            target: candidate.clone(),
            backup: backup_name.clone(),
        };
        disk::atomic_json(&self.root.join("transaction.json"), &transaction)?;
        self.phase("switching").await;
        let backup = disk::backup_path(&self.root, &backup_name)?;
        let result = async {
            *gate = None;
            self.stop_worker(&old_target).await?;
            self.phase("backing_up").await;
            disk::backup(&self.config.data_dir, &backup).await?;
            self.phase("starting").await;
            let (target, child) = self.start_worker(&candidate).await?;
            *self.worker.lock().await = Some(child);
            if let Err(f) = self
                .internal(&target, "/_runtime/import", Some(handoff.clone()))
                .await
            {
                let _ = self.stop_worker(&target).await;
                return Err(f);
            }
            let updated = old.activate(candidate);
            if let Err(f) = updated.save(&self.root) {
                let _ = self.stop_worker(&target).await;
                return Err(f);
            }
            *self.registry.lock().await = updated;
            *gate = Some(target);
            Ok(())
        }
        .await;
        if let Err(original) = result {
            self.phase("recovering").await;
            if backup.exists() {
                disk::restore(&self.config.data_dir, &backup).await?;
            }
            let (target, child) = self.start_worker(&old.active).await?;
            *self.worker.lock().await = Some(child);
            if let Err(f) = self
                .internal(&target, "/_runtime/import", Some(handoff))
                .await
            {
                let _ = self.stop_worker(&target).await;
                return Err(f);
            }
            *gate = Some(target);
            self.job.lock().await.recovered = true;
            let _ = std::fs::remove_file(self.root.join("transaction.json"));
            return Err(original);
        }
        // A committed registry is authoritative even if cleanup is deferred.
        let _ = std::fs::remove_file(self.root.join("transaction.json"));
        self.prune().await;
        Ok(())
    }
    async fn prune(&self) {
        let registry = self.registry.lock().await;
        let keep: std::collections::HashSet<_> = std::iter::once(&registry.active)
            .chain(registry.history.iter())
            .map(|v| v.sha256.clone())
            .collect();
        if let Ok(entries) = std::fs::read_dir(self.root.join("versions")) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.len() == 64
                    && name.bytes().all(|v| v.is_ascii_hexdigit())
                    && !keep.contains(&name)
                    && entry.file_type().is_ok_and(|v| v.is_dir())
                {
                    let _ = std::fs::remove_file(entry.path().join("claud2api"));
                    let _ = std::fs::remove_dir(entry.path());
                }
            }
        }
        if let Ok(entries) = std::fs::read_dir(self.root.join("backups")) {
            let mut files: Vec<_> = entries
                .flatten()
                .filter(|e| {
                    e.path()
                        .file_stem()
                        .and_then(|v| v.to_str())
                        .is_some_and(|v| uuid::Uuid::parse_str(v).is_ok())
                        && e.file_type().is_ok_and(|v| v.is_file())
                })
                .collect();
            files.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
            let count = files.len().saturating_sub(2);
            for entry in files.into_iter().take(count) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    async fn monitor(self: &Arc<Self>) {
        if self.busy.load(Ordering::Acquire) {
            return;
        }
        let dead = self
            .worker
            .lock()
            .await
            .as_mut()
            .is_none_or(|child| child.try_wait().ok().flatten().is_some());
        if !dead || self.busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut gate = self.target.write().await;
        *gate = None;
        self.worker.lock().await.take();
        let active = self.registry.lock().await.active.clone();
        let pending = self.root.join("transaction.json");
        if pending.exists() {
            let recovery = async {
                let transaction: Transaction = disk::read_json(&pending, 16384)?;
                let backup = disk::backup_path(&self.root, &transaction.backup)?;
                if active.sha256 != transaction.target.sha256 && backup.exists() {
                    disk::restore(&self.config.data_dir, &backup).await?;
                }
                Ok::<_, Fault>(())
            }
            .await;
            if recovery.is_err() {
                self.busy.store(false, Ordering::Release);
                return;
            }
        }
        if let Ok((target, child)) = self.start_worker(&active).await {
            *gate = Some(target);
            *self.worker.lock().await = Some(child);
            let _ = std::fs::remove_file(pending);
        }
        self.busy.store(false, Ordering::Release);
    }
}

fn authorize(manager: &Manager, request: &Request) -> Result<()> {
    if request.headers().get_all("authorization").iter().count() != 1
        || request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_none_or(|v| !equal(v, &manager.config.admin_key))
    {
        return Err(Fault::new(
            401,
            "authentication_error",
            "administrator key required",
        ));
    }
    crate::control::validate_origin(request)
}
async fn status(State(manager): State<Arc<Manager>>, request: Request) -> Response {
    if let Err(f) = authorize(&manager, &request) {
        return f.into_response();
    }
    if manager.catalog.lock().await.checked_at == 0 {
        let m = manager.clone();
        tokio::spawn(async move {
            m.check(false).await;
        });
    }
    let mut response = Json(manager.status().await).into_response();
    crate::control::secure(&mut response);
    response
}
async fn check(State(manager): State<Arc<Manager>>, request: Request) -> Response {
    if let Err(f) = authorize(&manager, &request) {
        return f.into_response();
    }
    let m = manager.clone();
    tokio::spawn(async move {
        m.check(true).await;
    });
    let mut response = (
        StatusCode::ACCEPTED,
        Json(json!({"schema_version":1,"accepted":true})),
    )
        .into_response();
    crate::control::secure(&mut response);
    response
}
async fn apply(State(manager): State<Arc<Manager>>, request: Request) -> Response {
    if let Err(f) = authorize(&manager, &request) {
        return f.into_response();
    }
    let bytes = match axum::body::to_bytes(request.into_body(), 4096).await {
        Ok(b) => b,
        Err(_) => return Fault::invalid("invalid version change request").into_response(),
    };
    let input = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return Fault::invalid("invalid version change request").into_response(),
    };
    let mut response = match manager.accept(input).await {
        Ok(v) => (StatusCode::ACCEPTED, Json(v)).into_response(),
        Err(f) => f.into_response(),
    };
    crate::control::secure(&mut response);
    response
}
async fn policy(State(manager): State<Arc<Manager>>, request: Request) -> Response {
    if let Err(f) = authorize(&manager, &request) {
        return f.into_response();
    }
    let bytes = match axum::body::to_bytes(request.into_body(), 4096).await {
        Ok(b) => b,
        Err(_) => return Fault::invalid("invalid update policy request").into_response(),
    };
    let input = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return Fault::invalid("invalid update policy request").into_response(),
    };
    let mut response = match manager.change_policy(input).await {
        Ok(()) => Json(manager.status().await).into_response(),
        Err(f) => f.into_response(),
    };
    crate::control::secure(&mut response);
    response
}
fn strip_hop(headers: &mut HeaderMap) {
    let extra: Vec<_> = headers
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(',')
        .map(|v| v.trim().to_owned())
        .collect();
    for name in extra {
        headers.remove(name);
    }
    for name in [
        "connection",
        "transfer-encoding",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "upgrade",
        "x-claud2api-worker",
    ] {
        headers.remove(name);
    }
}
async fn proxy(manager: Arc<Manager>, request: Request, private: bool) -> Result<Response> {
    if private && request.uri().path().starts_with("/control/v1/_runtime") {
        return Err(Fault::new(
            403,
            "permission_error",
            "internal worker operation",
        ));
    }
    let permit = manager
        .ingress
        .clone()
        .try_acquire_owned()
        .map_err(|_| Fault::busy("service switch queue is full", 1))?;
    let gate = tokio::time::timeout(Duration::from_secs(60), manager.target.clone().read_owned())
        .await
        .map_err(|_| Fault::new(503, "api_error", "version switch is still in progress"))?;
    let target = gate
        .as_ref()
        .ok_or_else(|| Fault::new(503, "api_error", "worker is recovering"))?;
    let address = if private {
        target.control
    } else {
        target.public
    };
    let path = request.uri().path_and_query().map_or("/", |v| v.as_str());
    let url = format!("http://{address}{path}");
    let (mut parts, body) = request.into_parts();
    strip_hop(&mut parts.headers);
    let response = manager
        .source
        .client
        .request(parts.method, url)
        .headers(parts.headers)
        .body(wreq::Body::wrap_stream(body.into_data_stream()))
        .timeout(Duration::from_secs(1000))
        .send()
        .await
        .map_err(|_| Fault::new(502, "api_error", "worker connection failed"))?;
    let status = response.status();
    let mut headers = response.headers().clone();
    strip_hop(&mut headers);
    let stream = async_stream::stream! {
        let _gate=gate;let _permit=permit;let mut stream=response.bytes_stream();
        while let Some(chunk)=stream.next().await {yield chunk;}
    };
    let mut result = Body::from_stream(stream).into_response();
    *result.status_mut() = status;
    *result.headers_mut() = headers;
    Ok(result)
}
async fn public_proxy(State(manager): State<Arc<Manager>>, request: Request) -> Response {
    proxy(manager, request, false)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}
async fn private_proxy(State(manager): State<Arc<Manager>>, request: Request) -> Response {
    proxy(manager, request, true)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

pub async fn run(config: Config, stop: CancellationToken) -> Result<()> {
    let public = tokio::net::TcpListener::bind(config.public_bind)
        .await
        .map_err(|_| Fault::new(503, "api_error", "cannot bind public listener"))?;
    let private = tokio::net::TcpListener::bind(config.control_bind)
        .await
        .map_err(|_| Fault::new(503, "api_error", "cannot bind control listener"))?;
    let (manager, _lock) = Manager::open(config, stop.clone()).await?;
    let private_router = Router::new()
        .route("/control/v1/updates", get(status))
        .route("/control/v1/updates/check", post(check))
        .route("/control/v1/updates/apply", post(apply))
        .route("/control/v1/updates/policy", post(policy))
        .fallback(private_proxy)
        .layer(DefaultBodyLimit::max(4096))
        .layer(axum::middleware::from_fn(
            |request: Request, next: axum::middleware::Next| async move {
                let mut response = next.run(request).await;
                crate::control::secure(&mut response);
                response
            },
        ))
        .with_state(manager.clone());
    let public_router = Router::new()
        .fallback(public_proxy)
        .with_state(manager.clone());
    let ph = axum_server::Handle::new();
    let ch = axum_server::Handle::new();
    let p = super::bounded_server(public)?.handle(ph.clone());
    let c = super::bounded_server(private)?.handle(ch.clone());
    let mut ps = tokio::spawn(async move { p.serve(public_router.into_make_service()).await });
    let mut cs = tokio::spawn(async move { c.serve(private_router.into_make_service()).await });
    let checker = manager.clone();
    let checks_stop = stop.clone();
    let background = tokio::spawn(async move {
        let mut check_timer = tokio::time::interval(Duration::from_secs(300));
        let mut monitor_timer = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {_=checks_stop.cancelled()=>break,_=check_timer.tick()=>{let m=checker.clone();tokio::spawn(async move{m.check(false).await;});},_=monitor_timer.tick()=>checker.monitor().await}
        }
    });
    eprintln!("Claud2Api supervisor {} ready", crate::VERSION);
    tokio::select! {_=stop.cancelled()=>{},_=&mut ps=>{},_=&mut cs=>{}}
    stop.cancel();
    ph.graceful_shutdown(Some(Duration::from_secs(5)));
    ch.graceful_shutdown(Some(Duration::from_secs(5)));
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = tokio::join!(&mut ps, &mut cs);
    })
    .await;
    ps.abort();
    cs.abort();
    background.abort();
    if let Ok(gate) = tokio::time::timeout(Duration::from_secs(2), manager.target.read()).await {
        if let Some(target) = gate.clone() {
            let _ = manager.stop_worker(&target).await;
        }
    } else if let Some(child) = manager.worker.lock().await.as_mut() {
        // A switch in progress retains its durable recovery journal. Closing
        // stdin also terminates a worker if the supervisor itself disappears.
        child.stdin.take();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offers_only_latest_channel_update_and_two_preceding_releases() {
        let asset = github::Asset {
            id: 1,
            name: "unused".into(),
            size: 1,
            digest: String::new(),
            browser_download_url: String::new(),
            state: "uploaded".into(),
        };
        let mut releases: Vec<_> = [
            ("2.0.0", true),
            ("1.5.0-beta.1", true),
            ("1.4.0", false),
            ("1.3.0", false),
            ("1.2.0", false),
            ("1.1.0", false),
        ]
        .into_iter()
        .map(|(v, pre)| Release {
            id: 1,
            version: format!("v{v}"),
            tag: format!("v{v}"),
            published_at: "2026-10-18".into(),
            prerelease: pre,
            binary: asset.clone(),
            manifest: asset.clone(),
        })
        .collect();
        releases.sort_by(|a, b| Version::parse(&b.version).cmp(&Version::parse(&a.version)));
        let (latest, previous) = choices(&releases, &Version::parse("1.3.0").unwrap());
        assert_eq!(latest.unwrap().version, "v1.4.0");
        assert_eq!(
            previous
                .iter()
                .map(|r| r.version.as_str())
                .collect::<Vec<_>>(),
            ["v1.2.0", "v1.1.0"]
        );
        assert_eq!(
            choices(&releases, &Version::parse("1.3.0-alpha.1").unwrap())
                .0
                .unwrap()
                .version,
            "v2.0.0"
        );
        assert!(choices(&[], &Version::parse("1.0.0").unwrap()).0.is_none());
    }
}
