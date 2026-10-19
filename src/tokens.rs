use crate::{
    error::{Fault, Result},
    store::{Store, now},
    web::equal,
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{
    collections::HashMap,
    sync::{
        Arc, Weak,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::Mutex;

#[derive(Clone, Serialize, Deserialize)]
pub struct ApiToken {
    pub id: String,
    pub label: String,
    pub digest: String,
    pub max_concurrency: usize,
    pub created_at: i64,
    pub expires_at: Option<i64>,
}
impl ApiToken {
    pub fn validate(&self) -> Result<()> {
        if uuid::Uuid::parse_str(&self.id).is_err()
            || self.label.trim().is_empty()
            || self.label.chars().count() > 80
            || self.label.chars().any(char::is_control)
            || !(1..=512).contains(&self.max_concurrency)
            || self.digest.len() != 64
            || hex::decode(&self.digest).is_err()
            || self.created_at < 0
            || self
                .expires_at
                .is_some_and(|v| !(1..=253402300799).contains(&v))
        {
            return Err(Fault::invalid(
                "invalid token name, concurrency or expiration",
            ));
        }
        Ok(())
    }
}
pub fn digest(root: &str, secret: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(root.as_bytes()).expect("HMAC accepts any key length");
    mac.update(b"Claud2Api downstream token v1\0");
    mac.update(secret.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}
pub struct Admission {
    pub id: Option<String>,
    pub label: String,
    active: Option<Arc<AtomicUsize>>,
}
impl Drop for Admission {
    fn drop(&mut self) {
        if let Some(active) = &self.active {
            active.fetch_sub(1, Ordering::AcqRel);
        }
    }
}
pub struct Tokens {
    store: Arc<Store>,
    root: String,
    gates: Mutex<HashMap<String, Weak<AtomicUsize>>>,
}
fn denied() -> Fault {
    Fault::new(401, "authentication_error", "invalid or expired API token")
}
impl Tokens {
    pub fn new(store: Arc<Store>, root: String) -> Arc<Self> {
        Arc::new(Self {
            store,
            root,
            gates: Mutex::new(HashMap::new()),
        })
    }
    pub async fn admit(&self, key: &str) -> Result<Admission> {
        if equal(key, &self.root) {
            return Ok(Admission {
                id: None,
                label: "环境网关密钥".to_owned(),
                active: None,
            });
        }
        if !key.starts_with("c2a_tok_") || key.len() != 72 {
            return Err(denied());
        }
        let hash = digest(&self.root, key);
        // Keep admission and metadata updates under the same state lock. A live
        // limit change never replaces the counter held by existing requests.
        let state = self.store.state.lock().await;
        let token = state
            .tokens
            .values()
            .find(|t| equal(&t.digest, &hash) && t.expires_at.is_none_or(|v| v > now()))
            .cloned()
            .ok_or_else(denied)?;
        let gate = {
            let mut gates = self.gates.lock().await;
            gates.retain(|_, v| v.strong_count() > 0);
            if let Some(gate) = gates.get(&token.id).and_then(Weak::upgrade) {
                gate
            } else {
                let gate = Arc::new(AtomicUsize::new(0));
                gates.insert(token.id.clone(), Arc::downgrade(&gate));
                gate
            }
        };
        if gate.load(Ordering::Acquire) >= token.max_concurrency {
            return Err(Fault::busy("API token concurrency limit reached", 1));
        }
        gate.fetch_add(1, Ordering::AcqRel);
        Ok(Admission {
            id: Some(token.id),
            label: token.label,
            active: Some(gate),
        })
    }
    pub async fn identify(&self, key: &str) -> Option<(Option<String>, String)> {
        if equal(key, &self.root) {
            return Some((None, "环境网关密钥".to_owned()));
        }
        if !key.starts_with("c2a_tok_") || key.len() != 72 {
            return None;
        }
        let hash = digest(&self.root, key);
        self.store
            .state
            .lock()
            .await
            .tokens
            .values()
            .find(|t| equal(&t.digest, &hash))
            .map(|t| (Some(t.id.clone()), t.label.clone()))
    }
    pub async fn busy(&self, id: &str, _limit: usize) -> usize {
        self.gates
            .lock()
            .await
            .get(id)
            .and_then(Weak::upgrade)
            .map_or(0, |gate| gate.load(Ordering::Acquire))
    }
}
