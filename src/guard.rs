use crate::{
    config::Config,
    error::{Fault, Result},
    store::{Account, Store, now},
    transport::WebClient,
};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Scope {
    pub cancel: CancellationToken,
    pub deadline: tokio::time::Instant,
    pub trace: Option<Arc<crate::observability::RequestTrace>>,
}
impl Scope {
    pub fn new(seconds: u64) -> Self {
        Self {
            cancel: CancellationToken::new(),
            deadline: tokio::time::Instant::now() + Duration::from_secs(seconds),
            trace: None,
        }
    }
    pub async fn wait<T>(&self, future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        tokio::select! {biased;_=self.cancel.cancelled()=>Err(Fault::cancelled()),_=tokio::time::sleep_until(self.deadline)=>Err(Fault::timeout()),v=future=>v}
    }
    pub async fn sleep(&self, seconds: u64) -> Result<()> {
        self.wait(async {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            Ok(())
        })
        .await
    }
}
pub struct Slot {
    pub client: WebClient,
    pub gate: Arc<Semaphore>,
    pub revision: u64,
}
struct AccountChange {
    id: String,
    transport: crate::browser::TransportSettings,
    label: Option<String>,
    enabled: Option<bool>,
}
pub struct Pool {
    pub store: Arc<Store>,
    config: Config,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
}
pub struct Lease {
    pub account_id: String,
    pub slot: Arc<Slot>,
    pub pool: Arc<Pool>,
    permit: Option<OwnedSemaphorePermit>,
    generations: std::sync::atomic::AtomicUsize,
}
pub fn available(account: &Account, s: &crate::config::Settings) -> i64 {
    let mut until = account
        .cooldown_until
        .max(account.last_start + s.account_min_interval_seconds as i64);
    let recent: Vec<_> = account.starts.iter().filter(|&&t| t > now() - 60).collect();
    if recent.len() >= s.account_requests_per_minute {
        until = until.max(*recent[recent.len() - s.account_requests_per_minute] + 60);
    }
    until
}
impl Pool {
    pub fn new(config: Config, store: Arc<Store>) -> Arc<Self> {
        Arc::new(Self {
            config,
            store,
            slots: Mutex::new(HashMap::new()),
        })
    }
    pub async fn busy(&self, id: &str) -> bool {
        self.slots
            .lock()
            .await
            .get(id)
            .is_some_and(|s| s.gate.available_permits() == 0)
    }
    async fn slot(&self, a: &Account) -> Result<Arc<Slot>> {
        let mut slots = self.slots.lock().await;
        if let Some(s) = slots.get(&a.id) {
            if s.revision >= a.transport_revision {
                return Ok(s.clone());
            }
            // A cancelled configuration request may have committed before its
            // cache update. Repair under the original physical account gate.
            let Ok(_permit) = s.gate.clone().try_acquire_owned() else {
                return Ok(s.clone());
            };
            let slot = Arc::new(Slot {
                client: WebClient::new(&self.config, &a.credential, &a.transport)?,
                gate: s.gate.clone(),
                revision: a.transport_revision,
            });
            slots.insert(a.id.clone(), slot.clone());
            return Ok(slot);
        }
        let slot = Arc::new(Slot {
            client: WebClient::new(&self.config, &a.credential, &a.transport)?,
            gate: Arc::new(Semaphore::new(1)),
            revision: a.transport_revision,
        });
        slots.insert(a.id.clone(), slot.clone());
        Ok(slot)
    }
    pub async fn acquire(
        self: &Arc<Self>,
        preferred: Option<&str>,
        excluded: &HashSet<String>,
        scope: &Scope,
        wait: bool,
    ) -> Result<Lease> {
        'admission: loop {
            if !self.store.healthy() {
                return Err(Fault::internal());
            }
            let snapshot = self.store.read().await;
            let mut candidates: Vec<_> = snapshot
                .accounts
                .values()
                .filter(|a| {
                    a.enabled
                        && !a.paused
                        && !excluded.contains(&a.id)
                        && preferred.is_none_or(|id| a.id == id)
                })
                .collect();
            candidates.sort_by_key(|a| {
                (
                    available(a, &snapshot.settings) > now(),
                    snapshot
                        .sessions
                        .values()
                        .filter(|s| s.account_id == a.id)
                        .count(),
                    a.id.clone(),
                )
            });
            let mut eligible = false;
            let mut retry = 60u64;
            for a in candidates {
                eligible = true;
                let until = available(a, &snapshot.settings);
                if until > now() {
                    retry = retry.min((until - now()) as u64);
                    continue;
                }
                let slot = self.slot(a).await?;
                if let Ok(permit) = slot.gate.clone().try_acquire_owned() {
                    let latest = self.store.read().await;
                    if latest
                        .accounts
                        .get(&a.id)
                        .is_none_or(|account| account.transport_revision != slot.revision)
                    {
                        drop(permit);
                        continue 'admission;
                    }
                    let lease = Lease {
                        account_id: a.id.clone(),
                        slot,
                        pool: self.clone(),
                        permit: Some(permit),
                        generations: std::sync::atomic::AtomicUsize::new(0),
                    };
                    lease.check().await?;
                    lease.admit().await?;
                    if let Some(trace) = &scope.trace {
                        trace.bind_account(&lease.account_id);
                    }
                    return Ok(lease);
                }
                retry = 1;
            }
            if !wait || !eligible {
                if !eligible
                    && snapshot
                        .accounts
                        .values()
                        .any(|a| a.enabled && a.paused && !excluded.contains(&a.id))
                {
                    return Err(Fault::new(
                        503,
                        "api_error",
                        "upstream account paused; operator review required",
                    ));
                }
                return Err(Fault::busy(
                    if eligible {
                        "account pool is busy or cooling down"
                    } else {
                        "no eligible account is available"
                    },
                    retry,
                ));
            }
            scope.sleep(1).await?;
        }
    }
    pub async fn configure(
        self: &Arc<Self>,
        id: &str,
        expected_revision: u64,
        transport: crate::browser::TransportSettings,
        label: Option<String>,
        enabled: Option<bool>,
    ) -> Result<serde_json::Value> {
        self.configure_all(
            vec![AccountChange {
                id: id.to_owned(),
                transport,
                label,
                enabled,
            }],
            expected_revision,
        )
        .await
    }
    pub async fn configure_proxies(
        self: &Arc<Self>,
        mut ids: Vec<String>,
        expected_revision: u64,
        proxy: String,
    ) -> Result<serde_json::Value> {
        ids.sort();
        if ids.is_empty() || ids.len() > 512 || ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(Fault::invalid("select between1 and512 distinct accounts"));
        }
        let state = self.store.read().await;
        let changes = ids
            .into_iter()
            .map(|id| {
                let a = state
                    .accounts
                    .get(&id)
                    .ok_or_else(|| Fault::new(404, "not_found_error", "account not found"))?;
                let mut transport = a.transport.clone();
                transport.proxy_url = if proxy.is_empty() {
                    None
                } else {
                    Some(proxy.clone())
                };
                Ok(AccountChange {
                    id,
                    transport,
                    label: None,
                    enabled: None,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.configure_all(changes, expected_revision).await
    }
    async fn configure_all(
        self: &Arc<Self>,
        mut changes: Vec<AccountChange>,
        expected_revision: u64,
    ) -> Result<serde_json::Value> {
        changes.sort_by(|a, b| a.id.cmp(&b.id));
        let state = self.store.read().await;
        if state.revision != expected_revision {
            return Err(Fault::new(
                409,
                "conflict_error",
                "configuration revision changed; refresh before retrying",
            ));
        }
        let mut prepared = Vec::new();
        let mut permits = Vec::new();
        for change in changes {
            change.transport.validate()?;
            let account = state
                .accounts
                .get(&change.id)
                .ok_or_else(|| Fault::new(404, "not_found_error", "account not found"))?;
            let slot = self.slot(account).await?;
            permits.push(slot.gate.clone().try_acquire_owned().map_err(|_| {
                Fault::new(
                    409,
                    "conflict_error",
                    "selected account is active; change transport after it becomes idle",
                )
            })?);
            let client = WebClient::new(&self.config, &account.credential, &change.transport)?;
            prepared.push((change, slot, client));
        }
        // Hold every original account gate through the single durable commit.
        // On cancellation, slot() repairs each committed transport under that gate.
        let result = self
            .store
            .mutate(
                "account_transport_updated",
                json!({"account_ids":prepared.iter().map(|(c,_,_)|&c.id).collect::<Vec<_>>()}),
                |s| {
                    if s.revision != expected_revision {
                        return Err(Fault::new(
                            409,
                            "conflict_error",
                            "configuration revision changed; refresh before retrying",
                        ));
                    }
                    let mut versions = Vec::new();
                    for (change, _, _) in &prepared {
                        let a = s.accounts.get_mut(&change.id).ok_or_else(Fault::internal)?;
                        a.transport = change.transport.clone();
                        a.transport_revision = a
                            .transport_revision
                            .checked_add(1)
                            .ok_or_else(Fault::internal)?;
                        if let Some(label) = &change.label {
                            a.label = label.clone();
                        }
                        if let Some(enabled) = change.enabled {
                            a.enabled = enabled;
                        }
                        versions.push(a.transport_revision);
                    }
                    s.revision += 1;
                    Ok((
                        versions,
                        json!({"schema_version":1,"revision":s.revision,"updated":prepared.len()}),
                    ))
                },
            )
            .await?;
        let mut slots = self.slots.lock().await;
        for ((change, slot, client), revision) in prepared.into_iter().zip(result.0) {
            slots.insert(
                change.id,
                Arc::new(Slot {
                    client,
                    gate: slot.gate.clone(),
                    revision,
                }),
            );
        }
        drop(permits);
        Ok(result.1)
    }
    pub async fn observe(&self, id: &str, status: u16, retry: Option<&str>) -> Result<()> {
        if !matches!(status, 401 | 403 | 429 | 529) {
            return Ok(());
        }
        self.store
            .mutate(
                "account_guard",
                json!({"account_id":id,"status":status}),
                |s| {
                    let policy = s.settings.clone();
                    let a = s.accounts.get_mut(id).ok_or_else(Fault::internal)?;
                    match status {
                        401 | 403 => a.paused = true,
                        429 => {
                            let delay = retry
                                .and_then(|v| {
                                    v.trim().parse::<u64>().ok().or_else(|| {
                                        httpdate::parse_http_date(v).ok().map(|d| {
                                            d.duration_since(std::time::SystemTime::now())
                                                .unwrap_or_default()
                                                .as_secs()
                                        })
                                    })
                                })
                                .unwrap_or(policy.rate_limit_fallback_seconds)
                                .max(policy.rate_limit_min_seconds);
                            a.cooldown_until =
                                a.cooldown_until.max(now() + delay.min(604800) as i64);
                            a.quota_until = a.quota_until.max(a.cooldown_until);
                        }
                        529 => {
                            a.cooldown_until =
                                a.cooldown_until.max(now() + policy.overload_seconds as i64)
                        }
                        _ => {}
                    }
                    Ok(())
                },
            )
            .await
    }
}
impl Lease {
    async fn admit(&self) -> Result<()> {
        self.pool
            .store
            .mutate("", json!({}), |state| {
                let settings = state.settings.clone();
                let current = state
                    .accounts
                    .get(&self.account_id)
                    .ok_or_else(Fault::internal)?;
                if !current.enabled || current.paused {
                    return Err(Fault::busy("account is unavailable for new requests", 1));
                }
                let ready = available(current, &settings);
                if ready > now() {
                    return Err(Fault::busy(
                        "account request rate limit reached",
                        (ready - now()) as u64,
                    ));
                }
                let current = state
                    .accounts
                    .get_mut(&self.account_id)
                    .ok_or_else(Fault::internal)?;
                current.last_start = now();
                current.starts.retain(|&t| t > now() - 60);
                current.starts.push(now());
                Ok(())
            })
            .await
    }
    pub fn release(&mut self) {
        self.permit.take();
    }
    pub async fn check(&self) -> Result<()> {
        if !self.pool.store.healthy() {
            return Err(Fault::internal());
        }
        let s = self.pool.store.read().await;
        let a = s
            .accounts
            .get(&self.account_id)
            .ok_or_else(Fault::internal)?;
        if a.paused {
            return Err(Fault::new(
                403,
                "permission_error",
                "account requires operator review",
            ));
        }
        if a.cooldown_until > now() {
            return Err(Fault::busy(
                "account is cooling down",
                (a.cooldown_until - now()) as u64,
            ));
        }
        Ok(())
    }
    pub async fn start(&self, scope: &Scope) -> Result<()> {
        self.check().await?;
        if self
            .generations
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            == 0
        {
            return Ok(());
        }
        loop {
            self.check().await?;
            let s = self.pool.store.read().await;
            let a = s
                .accounts
                .get(&self.account_id)
                .ok_or_else(Fault::internal)?;
            let delay = available(a, &s.settings) - now();
            if delay > 0 {
                scope.sleep(delay as u64).await?;
                continue;
            }
            return self
                .pool
                .store
                .mutate("", json!({}), |s| {
                    let a = s
                        .accounts
                        .get_mut(&self.account_id)
                        .ok_or_else(Fault::internal)?;
                    let at = now();
                    a.last_start = at;
                    a.starts.retain(|&t| t > at - 60);
                    a.starts.push(at);
                    Ok(())
                })
                .await;
        }
    }
    pub async fn failure(&self, status: u16, retry: Option<&str>) -> Result<Fault> {
        self.pool.observe(&self.account_id, status, retry).await?;
        let mut f = match status {
            400 => Fault::new(
                400,
                "invalid_request_error",
                "upstream rejected the request",
            ),
            401 | 403 => Fault::new(
                502,
                "api_error",
                "upstream account authentication or access failed",
            ),
            429 => Fault::busy("upstream account rate limited", 60),
            529 | 503 => Fault::new(529, "overloaded_error", "upstream is overloaded"),
            404 => Fault::new(404, "not_found_error", "upstream resource not found"),
            _ => Fault::upstream(),
        };
        f.quota = status == 429;
        if status == 429 {
            let state = self.pool.store.read().await;
            if let Some(a) = state.accounts.get(&self.account_id) {
                f.retry_after = Some((a.cooldown_until - now()).max(1) as u64);
            }
        }
        Ok(f)
    }
}
