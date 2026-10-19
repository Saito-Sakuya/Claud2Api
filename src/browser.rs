use crate::error::{Fault, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

macro_rules! profiles {
    ($(($variant:ident, $id:literal, $family:literal, $version:literal)),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        pub enum BrowserProfile { $(#[serde(rename=$id)] $variant),+ }
        impl BrowserProfile {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub fn id(self) -> &'static str { match self { $(Self::$variant => $id),+ } }
            pub fn family(self) -> &'static str { match self { $(Self::$variant => $family),+ } }
            pub fn version(self) -> u16 { match self { $(Self::$variant => $version),+ } }
            pub fn native(self) -> wreq_util::Profile { match self { $(Self::$variant => wreq_util::Profile::$variant),+ } }
        }
    }
}
profiles! {
    (Chrome146, "chrome_146_windows", "Chrome", 146),
    (Chrome147, "chrome_147_windows", "Chrome", 147),
    (Chrome148, "chrome_148_windows", "Chrome", 148),
    (Chrome149, "chrome_149_windows", "Chrome", 149),
    (Chrome150, "chrome_150_windows", "Chrome", 150),
    (Chrome151, "chrome_151_windows", "Chrome", 151),
    (Chrome152, "chrome_152_windows", "Chrome", 152),
    (Firefox144, "firefox_144_windows", "Firefox", 144),
    (Firefox145, "firefox_145_windows", "Firefox", 145),
    (Firefox146, "firefox_146_windows", "Firefox", 146),
    (Firefox147, "firefox_147_windows", "Firefox", 147),
    (Firefox148, "firefox_148_windows", "Firefox", 148),
    (Firefox149, "firefox_149_windows", "Firefox", 149),
    (Firefox150, "firefox_150_windows", "Firefox", 150),
    (Firefox151, "firefox_151_windows", "Firefox", 151),
}
impl BrowserProfile {
    pub fn parse(value: &str) -> Result<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|p| p.id() == value)
            .ok_or_else(|| Fault::invalid("unsupported browser profile"))
    }
    pub fn catalog() -> Value {
        json!(Self::ALL.iter().map(|p| json!({"id":p.id(),"label":format!("{} {} / Windows",p.family(),p.version()),"family":p.family(),"version":p.version(),"platform":"Windows"})).collect::<Vec<_>>())
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TransportSettings {
    pub browser_profile: Option<BrowserProfile>,
    pub proxy_url: Option<String>,
}
impl TransportSettings {
    pub fn validate(&self) -> Result<()> {
        if let Some(raw) = &self.proxy_url {
            if raw.len() > 4096 || raw.chars().any(char::is_control) {
                return Err(Fault::invalid("invalid account proxy URL"));
            }
            let url =
                url::Url::parse(raw).map_err(|_| Fault::invalid("invalid account proxy URL"))?;
            if !matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
                || url.host_str().is_none()
                || !matches!(url.path(), "" | "/")
                || url.query().is_some()
                || url.fragment().is_some()
                || (matches!(url.scheme(), "socks5" | "socks5h") && url.port().is_none())
            {
                return Err(Fault::invalid(
                    "proxy must be an HTTP(S) or SOCKS5 URL with a valid host and port",
                ));
            }
            wreq::Proxy::all(raw.as_str())
                .map_err(|_| Fault::invalid("invalid account proxy URL"))?;
        }
        Ok(())
    }
    pub fn public_proxy(&self) -> Value {
        self.proxy_url.as_ref().and_then(|raw|url::Url::parse(raw).ok())
            .map(|url|json!({"configured":true,"scheme":url.scheme(),"host":url.host_str(),"port":url.port_or_known_default(),"authenticated":!url.username().is_empty()||url.password().is_some()}))
            .unwrap_or_else(||json!({"configured":false}))
    }
}
