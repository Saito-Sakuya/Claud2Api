pub use crate::browser::BrowserProfile;
use crate::error::{Fault, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
};

const fn enabled_by_default() -> bool {
    true
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub session_ttl_seconds: u64,
    pub request_timeout_seconds: u64,
    pub max_request_bytes: usize,
    pub simulated_cache: bool,
    pub compaction_enabled: bool,
    pub context_budget: usize,
    pub compaction_trigger_percent: usize,
    pub keep_recent_rounds: usize,
    pub account_min_interval_seconds: u64,
    pub account_requests_per_minute: usize,
    pub rate_limit_min_seconds: u64,
    pub rate_limit_fallback_seconds: u64,
    pub overload_seconds: u64,
    #[serde(default = "enabled_by_default")]
    pub native_web_search: bool,
    #[serde(default = "enabled_by_default")]
    pub native_artifacts: bool,
    #[serde(default = "enabled_by_default")]
    pub native_repl: bool,
    #[serde(default = "enabled_by_default")]
    pub native_widgets: bool,
    #[serde(default = "enabled_by_default")]
    pub native_project_knowledge: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            session_ttl_seconds: 3600,
            request_timeout_seconds: 300,
            max_request_bytes: 4 << 20,
            simulated_cache: true,
            compaction_enabled: true,
            context_budget: 128000,
            compaction_trigger_percent: 75,
            keep_recent_rounds: 6,
            account_min_interval_seconds: 5,
            account_requests_per_minute: 6,
            rate_limit_min_seconds: 60,
            rate_limit_fallback_seconds: 18000,
            overload_seconds: 15,
            native_web_search: true,
            native_artifacts: true,
            native_repl: true,
            native_widgets: true,
            native_project_knowledge: true,
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        let checks = [
            (self.session_ttl_seconds, 60, 604800),
            (self.request_timeout_seconds, 1, 900),
            (self.max_request_bytes as u64, 1024, 32 << 20),
            (self.context_budget as u64, 4096, 1_000_000),
            (self.compaction_trigger_percent as u64, 10, 90),
            (self.keep_recent_rounds as u64, 1, 32),
            (self.account_min_interval_seconds, 1, 300),
            (self.account_requests_per_minute as u64, 1, 60),
            (self.rate_limit_min_seconds, 60, 86400),
            (self.rate_limit_fallback_seconds, 60, 604800),
            (self.overload_seconds, 1, 900),
        ];
        if checks.iter().any(|(v, min, max)| v < min || v > max) {
            return Err(Fault::invalid(
                "configuration contains an out-of-range value",
            ));
        }
        Ok(())
    }
    fn environment() -> Result<Self> {
        let mut s = Self::default();
        macro_rules! number {
            ($field:ident, $name:literal) => {
                if let Ok(v) = env::var($name) {
                    s.$field = v
                        .parse()
                        .map_err(|_| Fault::invalid(concat!("invalid ", $name)))?;
                }
            };
        }
        number!(session_ttl_seconds, "STICKY_SESSION_TTL_SECONDS");
        number!(request_timeout_seconds, "REQUEST_TIMEOUT_SECONDS");
        number!(max_request_bytes, "MAX_REQUEST_BYTES");
        number!(context_budget, "CONTEXT_BUDGET_TOKENS");
        number!(
            compaction_trigger_percent,
            "CONTEXT_COMPACTION_TRIGGER_PERCENT"
        );
        number!(keep_recent_rounds, "CONTEXT_KEEP_RECENT_ROUNDS");
        number!(account_min_interval_seconds, "ACCOUNT_MIN_INTERVAL_SECONDS");
        number!(account_requests_per_minute, "ACCOUNT_REQUESTS_PER_MINUTE");
        number!(
            rate_limit_min_seconds,
            "ACCOUNT_RATE_LIMIT_COOLDOWN_SECONDS"
        );
        number!(
            rate_limit_fallback_seconds,
            "ACCOUNT_RATE_LIMIT_FALLBACK_SECONDS"
        );
        number!(overload_seconds, "ACCOUNT_OVERLOAD_COOLDOWN_SECONDS");
        number!(simulated_cache, "SIMULATED_CACHE_ENABLED");
        number!(compaction_enabled, "CONTEXT_COMPACTION_ENABLED");
        number!(native_web_search, "NATIVE_WEB_SEARCH_ENABLED");
        number!(native_artifacts, "NATIVE_ARTIFACTS_ENABLED");
        number!(native_repl, "NATIVE_REPL_ENABLED");
        number!(native_widgets, "NATIVE_WIDGETS_ENABLED");
        number!(native_project_knowledge, "NATIVE_PROJECT_KNOWLEDGE_ENABLED");
        s.validate()?;
        Ok(s)
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Credential {
    pub session_key: String,
    pub cookie: String,
}
impl Credential {
    pub fn parse(line: &str) -> Result<Self> {
        let line = line.trim();
        if line.is_empty() || line.len() > 65536 || line.contains(['\r', '\n']) {
            return Err(Fault::invalid("credential must be a nonempty single line"));
        }
        let cookie = if line.contains("sessionKey=") {
            line.to_owned()
        } else {
            String::new()
        };
        let session_key = if cookie.is_empty() {
            line.to_owned()
        } else {
            cookie_value(&cookie, "sessionKey").unwrap_or_default()
        };
        if session_key.is_empty() || !line.bytes().all(|b| (32..127).contains(&b)) {
            return Err(Fault::invalid("invalid account credential"));
        }
        Ok(Self {
            session_key,
            cookie,
        })
    }
    pub fn id(&self) -> String {
        hex::encode(Sha256::digest(self.session_key.trim().as_bytes()))
    }
}
pub fn cookie_value(cookie: &str, key: &str) -> Option<String> {
    cookie.split(';').find_map(|v| {
        v.trim()
            .split_once('=')
            .filter(|(k, _)| *k == key)
            .map(|(_, v)| v.to_owned())
    })
}
#[derive(Clone)]
pub struct Config {
    pub browser_profile: BrowserProfile,
    pub public_bind: SocketAddr,
    pub control_bind: SocketAddr,
    pub gateway_key: String,
    pub admin_key: String,
    pub bot_key: String,
    pub data_dir: PathBuf,
    pub import_state: Option<PathBuf>,
    pub base_url: String,
    pub locale: String,
    pub timezone: String,
    pub default_model: String,
    pub effort: String,
    pub legacy_thinking: String,
    pub accounts: Vec<Credential>,
    pub settings: Settings,
}
fn var(name: &str, fallback: &str) -> String {
    env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}
fn file_text(path: &Path, max: u64) -> Result<String> {
    let meta =
        std::fs::metadata(path).map_err(|_| Fault::invalid("cannot read configured file"))?;
    if !meta.is_file() || meta.len() > max {
        return Err(Fault::invalid("configured file is invalid or too large"));
    }
    std::fs::read_to_string(path).map_err(|_| Fault::invalid("configured file is not UTF-8"))
}
fn secret(name: &str) -> Result<String> {
    let value = match env::var(format!("{name}_FILE")) {
        Ok(path) => file_text(Path::new(&path), 1 << 20)?,
        Err(_) => var(name, ""),
    };
    let v = value.trim_start_matches('\u{feff}').trim().to_owned();
    if v.contains(['\r', '\n']) {
        return Err(Fault::invalid("key must be a single line"));
    }
    Ok(v)
}
impl Config {
    pub fn load() -> Result<Self> {
        let gateway_key = secret("GATEWAY_API_KEY")?;
        let admin_key = secret("ADMIN_API_KEY")?;
        let bot_key = secret("BOT_READ_KEY")?;
        if gateway_key.len() < 32
            || admin_key.len() < 32
            || gateway_key == admin_key
            || (!bot_key.is_empty()
                && (bot_key.len() < 32 || bot_key == gateway_key || bot_key == admin_key))
        {
            return Err(Fault::invalid(
                "configure distinct gateway and admin keys (and optional bot key), each at least 32 bytes",
            ));
        }
        let public_bind = SocketAddr::new(
            var("BIND_HOST", "127.0.0.1")
                .parse::<IpAddr>()
                .map_err(|_| Fault::invalid("BIND_HOST must be an IP address"))?,
            var("PORT", "8080")
                .parse()
                .map_err(|_| Fault::invalid("invalid PORT"))?,
        );
        let control_bind = var("CONTROL_BIND", "127.0.0.1:18082")
            .parse::<SocketAddr>()
            .map_err(|_| Fault::invalid("invalid CONTROL_BIND"))?;
        if !control_bind.ip().is_loopback()
            || (!crate::updates::managed() && (public_bind.port() == 0 || control_bind.port() == 0))
        {
            return Err(Fault::invalid(
                "control listener must bind a loopback address with a nonzero port",
            ));
        }
        let parsed = url::Url::parse(&var("CLAUDE_BASE_URL", "https://claude.ai"))
            .map_err(|_| Fault::invalid("invalid upstream URL"))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(Fault::invalid("invalid upstream URL"));
        }
        let mut accounts = Vec::new();
        if let Ok(path) = env::var("CLAUDE_SESSION_FILE") {
            accounts.push(Credential::parse(
                file_text(Path::new(&path), 1 << 20)?
                    .trim_start_matches('\u{feff}')
                    .trim(),
            )?);
        } else {
            let path = var("CLAUDE_ACCOUNTS_FILE", "accounts.txt");
            if env::var("CLAUDE_ACCOUNTS_FILE").is_ok() && !Path::new(&path).is_file() {
                return Err(Fault::invalid("cannot read configured accounts file"));
            }
            if Path::new(&path).exists() {
                for line in file_text(Path::new(&path), 1 << 20)?.lines() {
                    let l = line.trim_start_matches('\u{feff}').trim();
                    if !l.is_empty() && !l.starts_with('#') {
                        accounts.push(Credential::parse(l)?);
                    }
                }
            }
            let cookie = var("CLAUDE_COOKIE", "");
            let session = var("CLAUDE_SESSION_KEY", "");
            let c = if cookie.is_empty() {
                session
            } else if cookie.contains("sessionKey=") {
                cookie
            } else if !session.is_empty() {
                format!("sessionKey={session}; {cookie}")
            } else {
                return Err(Fault::invalid(
                    "CLAUDE_COOKIE requires a sessionKey or CLAUDE_SESSION_KEY",
                ));
            };
            if !c.is_empty() {
                accounts.push(Credential::parse(&c)?);
            }
        }
        let mut ids = std::collections::HashSet::new();
        accounts.retain(|a| ids.insert(a.id()));
        if accounts.len() > 512
            || accounts.iter().any(|a| {
                [&gateway_key, &admin_key, &bot_key]
                    .iter()
                    .any(|k| !k.is_empty() && (**k == a.session_key || **k == a.cookie))
            })
        {
            return Err(Fault::invalid("invalid account pool or key separation"));
        }
        let data_dir = PathBuf::from(var("CLAUD2API_DATA_DIR", "/var/lib/claud2api"));
        if !data_dir.is_absolute() {
            return Err(Fault::invalid("CLAUD2API_DATA_DIR must be absolute"));
        }
        let locale = var("CLAUDE_LOCALE", "zh-CN");
        if locale.is_empty()
            || locale.len() > 64
            || !locale
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(Fault::invalid("invalid CLAUDE_LOCALE"));
        }
        let import_state = env::var("STICKY_SESSION_STATE_FILE")
            .ok()
            .map(PathBuf::from);
        if import_state
            .as_ref()
            .is_some_and(|p| !p.is_absolute() || !p.is_file())
        {
            return Err(Fault::invalid(
                "Go state import requires an existing absolute file path",
            ));
        }
        let timezone = var("CLAUDE_TIMEZONE", "Asia/Singapore");
        if timezone.parse::<chrono_tz::Tz>().is_err() {
            return Err(Fault::invalid("invalid CLAUDE_TIMEZONE"));
        }
        Ok(Self {
            browser_profile: BrowserProfile::parse(&var("BROWSER_PROFILE", "chrome_146_windows"))?,
            public_bind,
            control_bind,
            gateway_key,
            admin_key,
            bot_key,
            data_dir,
            import_state,
            base_url: parsed.to_string().trim_end_matches('/').to_owned(),
            locale,
            timezone,
            default_model: var("DEFAULT_MODEL", "claude-sonnet-5"),
            legacy_thinking: if matches!(var("CLAUDE_THINKING", "").as_str(), "none" | "disabled")
                || serde_json::from_str::<serde_json::Value>(&var("CLAUDE_THINKING", ""))
                    .ok()
                    .is_some_and(|v| v["type"] == "disabled")
            {
                "off".to_owned()
            } else {
                "auto".to_owned()
            },
            effort: {
                let value = var("CLAUDE_EFFORT", &var("CLAUDE_CODE_EFFORT_LEVEL", "medium"))
                    .trim()
                    .to_lowercase();
                if matches!(value.as_str(), "low" | "medium" | "high" | "xhigh" | "max") {
                    value
                } else {
                    "medium".to_owned()
                }
            },
            accounts,
            settings: Settings::environment()?,
        })
    }
}
