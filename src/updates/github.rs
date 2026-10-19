use super::{DATA_FORMAT, PROTOCOL, RUNTIME_ABI, version::Version};
use crate::error::{Fault, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
use tokio::io::AsyncWriteExt;

pub const BINARY: &str = "claud2api-linux-amd64";
pub const MANIFEST: &str = "claud2api-update.json";
const MAX_BINARY: u64 = 128 << 20;

#[derive(Clone)]
pub struct Source {
    pub repository: String,
    pub api: String,
    pub fixture: bool,
    pub disabled: bool,
    pub client: wreq::Client,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Asset {
    pub id: u64,
    pub name: String,
    pub size: u64,
    pub digest: String,
    pub browser_download_url: String,
    pub state: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Release {
    pub id: u64,
    pub version: String,
    pub tag: String,
    pub published_at: String,
    pub prerelease: bool,
    pub binary: Asset,
    pub manifest: Asset,
}
#[derive(Deserialize)]
struct ApiRelease {
    id: u64,
    tag_name: String,
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    published_at: Option<String>,
    assets: Vec<serde_json::Value>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub protocol: u32,
    pub version: String,
    pub runtime_abi: String,
    pub data_format: String,
    pub binary: String,
    pub size: u64,
    pub sha256: String,
}
impl Manifest {
    pub fn validate(&self, release: &Release) -> Result<()> {
        if self.protocol != PROTOCOL
            || self.runtime_abi != RUNTIME_ABI
            || self.data_format != DATA_FORMAT
        {
            return Err(Fault::new(
                409,
                "api_error",
                "release requires a different runtime or data format; automatic update refused",
            ));
        }
        if self.version != release.version
            || self.binary != BINARY
            || self.size != release.binary.size
            || self.sha256 != release.binary.digest.trim_start_matches("sha256:")
        {
            return Err(Fault::new(
                422,
                "api_error",
                "release manifest and verified asset metadata disagree",
            ));
        }
        Ok(())
    }
}
pub enum Listing {
    Unchanged,
    Changed {
        releases: Vec<Release>,
        etag: Option<String>,
        truncated: bool,
    },
}
pub struct CheckFailure {
    pub message: &'static str,
    pub retry_at: i64,
}
impl Source {
    pub fn load() -> Result<Self> {
        let repository = std::env::var("CLAUD2API_UPDATE_REPOSITORY")
            .unwrap_or_else(|_| "Saito-Sakuya/Claud2Api".into());
        if !valid_repository(&repository) {
            return Err(Fault::invalid("invalid update repository; use owner/repo"));
        }
        let fixture_url = std::env::var("CLAUD2API_UPDATE_TEST_API").ok();
        let fixture = cfg!(debug_assertions) && fixture_url.is_some();
        if fixture_url.is_some() && !fixture {
            return Err(Fault::invalid(
                "test update sources are disabled in release builds",
            ));
        }
        let api = fixture_url
            .unwrap_or_else(|| "https://api.github.com".into())
            .trim_end_matches('/')
            .to_owned();
        let parsed = url::Url::parse(&api).map_err(|_| Fault::invalid("invalid update API"))?;
        if fixture
            && !(parsed.scheme() == "http"
                && parsed
                    .host_str()
                    .and_then(|v| v.parse::<std::net::IpAddr>().ok())
                    .is_some_and(|v| v.is_loopback())
                && parsed.path() == "/"
                && parsed.query().is_none()
                && parsed.fragment().is_none()
                && parsed.username().is_empty()
                && parsed.password().is_none())
        {
            return Err(Fault::invalid(
                "test update API must be numeric loopback HTTP",
            ));
        }
        let client = wreq::Client::builder()
            .no_proxy()
            .redirect(wreq::redirect::Policy::none())
            .no_gzip()
            .no_deflate()
            .no_brotli()
            .no_zstd()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| Fault::internal())?;
        Ok(Self {
            repository,
            api,
            fixture,
            disabled: std::env::var("CLAUD2API_UPDATE_DISABLED").is_ok_and(|v| v == "true"),
            client,
        })
    }
    fn request(&self, url: &str) -> wreq::RequestBuilder {
        self.client
            .get(url)
            .header(
                "user-agent",
                concat!("Claud2Api/", env!("CARGO_PKG_VERSION")),
            )
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .timeout(Duration::from_secs(8))
    }
    pub async fn list(&self, etag: Option<&str>) -> std::result::Result<Listing, CheckFailure> {
        let failure = || CheckFailure {
            message: "GitHub is temporarily unavailable",
            retry_at: crate::store::now() + 300,
        };
        let mut releases = vec![];
        let mut first_etag = None;
        for page in 1..=5 {
            let mut request = self.request(&format!(
                "{}/repos/{}/releases?per_page=100&page={page}",
                self.api, self.repository
            ));
            if page == 1
                && let Some(tag) = etag
            {
                request = request.header("if-none-match", tag);
            }
            let response = request.send().await.map_err(|_| failure())?;
            if page == 1 && response.status().as_u16() == 304 {
                return Ok(Listing::Unchanged);
            }
            if matches!(response.status().as_u16(), 403 | 429) {
                let now = crate::store::now();
                let seconds = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<i64>().ok())
                    .map(|v| now + v);
                let reset = response
                    .headers()
                    .get("x-ratelimit-reset")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<i64>().ok());
                return Err(CheckFailure {
                    message: "GitHub rate limit reached; retry deferred",
                    retry_at: seconds
                        .or(reset)
                        .unwrap_or(now + 3600)
                        .clamp(now + 300, now + 86400),
                });
            }
            if response.status().as_u16() == 404 {
                return Err(CheckFailure {
                    message: "repository has not published releases yet",
                    retry_at: crate::store::now() + 300,
                });
            }
            if !response.status().is_success() {
                return Err(failure());
            }
            if page == 1 {
                first_etag = response
                    .headers()
                    .get("etag")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
            }
            let bytes = bounded(response, 4 << 20).await.map_err(|_| failure())?;
            let entries: Vec<ApiRelease> = serde_json::from_slice(&bytes).map_err(|_| failure())?;
            let full = entries.len() == 100;
            for entry in entries {
                if let Some(release) = self.release(entry) {
                    releases.push(release);
                }
            }
            if !full {
                return Ok(Listing::Changed {
                    releases,
                    etag: first_etag,
                    truncated: false,
                });
            }
        }
        Ok(Listing::Changed {
            releases,
            etag: first_etag,
            truncated: true,
        })
    }
    pub async fn resolve(&self, tag: &str) -> Result<Release> {
        if Version::parse(tag).is_none() {
            return Err(Fault::invalid("invalid release version"));
        }
        let mut url = url::Url::parse(&format!(
            "{}/repos/{}/releases/tags/",
            self.api, self.repository
        ))
        .map_err(|_| Fault::internal())?;
        url.path_segments_mut()
            .map_err(|_| Fault::internal())?
            .pop_if_empty()
            .push(tag);
        let response = self
            .request(url.as_str())
            .send()
            .await
            .map_err(|_| Fault::upstream())?;
        if !response.status().is_success() {
            return Err(Fault::new(
                409,
                "api_error",
                "release is no longer available; check again",
            ));
        }
        let entry: ApiRelease = serde_json::from_slice(&bounded(response, 4 << 20).await?)
            .map_err(|_| Fault::upstream())?;
        self.release(entry)
            .filter(|r| r.tag == tag)
            .ok_or_else(|| Fault::new(409, "api_error", "release has no verified update assets"))
    }
    fn release(&self, entry: ApiRelease) -> Option<Release> {
        if entry.draft || entry.published_at.is_none() || Version::parse(&entry.tag_name).is_none()
        {
            return None;
        }
        let assets: Vec<_> = entry
            .assets
            .into_iter()
            .filter_map(|v| serde_json::from_value::<Asset>(v).ok())
            .collect();
        let binary = assets.iter().find(|v| v.name == BINARY)?.clone();
        let manifest = assets.iter().find(|v| v.name == MANIFEST)?.clone();
        if !valid_asset(&binary, MAX_BINARY)
            || !valid_asset(&manifest, 16384)
            || !self.asset_url(&binary.browser_download_url, &entry.tag_name, BINARY)
            || !self.asset_url(&manifest.browser_download_url, &entry.tag_name, MANIFEST)
        {
            return None;
        }
        Some(Release {
            id: entry.id,
            version: format!("v{}", entry.tag_name.trim_start_matches('v')),
            tag: entry.tag_name,
            published_at: entry.published_at?,
            prerelease: entry.prerelease,
            binary,
            manifest,
        })
    }
    pub fn asset_url(&self, raw: &str, tag: &str, name: &str) -> bool {
        let Ok(url) = url::Url::parse(raw) else {
            return false;
        };
        if self.fixture {
            return url.origin() == url::Url::parse(&self.api).unwrap().origin();
        }
        let expected = format!("/{}/releases/download/{}/{}", self.repository, tag, name);
        url.scheme() == "https"
            && url.host_str() == Some("github.com")
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == expected
    }
    fn redirect_url(&self, raw: &url::Url) -> bool {
        if self.fixture {
            return raw.origin() == url::Url::parse(&self.api).unwrap().origin();
        }
        raw.scheme() == "https"
            && raw.port().is_none()
            && raw.username().is_empty()
            && raw.password().is_none()
            && matches!(
                raw.host_str(),
                Some(
                    "github.com"
                        | "release-assets.githubusercontent.com"
                        | "objects.githubusercontent.com"
                )
            )
    }
    async fn response(&self, asset: &Asset) -> Result<wreq::Response> {
        let mut url =
            url::Url::parse(&asset.browser_download_url).map_err(|_| Fault::upstream())?;
        for _ in 0..6 {
            if !self.redirect_url(&url) {
                return Err(Fault::new(
                    422,
                    "api_error",
                    "release redirected outside trusted GitHub hosts",
                ));
            }
            let response = self
                .client
                .get(url.as_str())
                .header("user-agent", "Claud2Api updater")
                .timeout(Duration::from_secs(180))
                .send()
                .await
                .map_err(|_| Fault::upstream())?;
            if response.status().is_redirection() {
                url = response
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| url.join(v).ok())
                    .ok_or_else(Fault::upstream)?;
                continue;
            }
            if !response.status().is_success()
                || response.content_length().is_some_and(|v| v != asset.size)
            {
                return Err(Fault::new(
                    422,
                    "api_error",
                    "release asset is unavailable or has changed size",
                ));
            }
            return Ok(response);
        }
        Err(Fault::new(422, "api_error", "too many release redirects"))
    }
    pub async fn manifest(&self, release: &Release) -> Result<Manifest> {
        let bytes = bounded(self.response(&release.manifest).await?, 16384).await?;
        if bytes.len() as u64 != release.manifest.size
            || format!("sha256:{}", hex::encode(Sha256::digest(&bytes))) != release.manifest.digest
        {
            return Err(Fault::new(
                422,
                "api_error",
                "release manifest checksum failed",
            ));
        }
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|_| Fault::new(422, "api_error", "invalid release manifest"))?;
        manifest.validate(release)?;
        Ok(manifest)
    }
    pub async fn download(
        &self,
        release: &Release,
        path: &Path,
        progress: impl Fn(u64),
    ) -> Result<()> {
        let response = self.response(&release.binary).await?;
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path).map_err(|_| Fault::internal())?;
        let mut file = tokio::fs::File::from_std(file);
        let mut stream = response.bytes_stream();
        let mut hash = Sha256::new();
        let mut length = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| Fault::upstream())?;
            length = length
                .checked_add(chunk.len() as u64)
                .ok_or_else(Fault::upstream)?;
            if length > release.binary.size || length > MAX_BINARY {
                return Err(Fault::new(
                    422,
                    "api_error",
                    "release asset exceeds its declared size",
                ));
            }
            hash.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|_| Fault::internal())?;
            progress(length);
        }
        file.sync_all().await.map_err(|_| Fault::internal())?;
        if length != release.binary.size
            || format!("sha256:{}", hex::encode(hash.finalize())) != release.binary.digest
        {
            return Err(Fault::new(
                422,
                "api_error",
                "release binary checksum failed",
            ));
        }
        Ok(())
    }
}
pub async fn bounded(response: wreq::Response, limit: usize) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|v| v > limit as u64) {
        return Err(Fault::new(422, "api_error", "update response is too large"));
    }
    let mut data = vec![];
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| Fault::upstream())?;
        if data.len().saturating_add(chunk.len()) > limit {
            return Err(Fault::new(422, "api_error", "update response is too large"));
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}
fn valid_repository(value: &str) -> bool {
    let parts: Vec<_> = value.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 100
                && *p != "."
                && *p != ".."
                && p.bytes()
                    .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'-' | b'_' | b'.'))
        })
}
fn valid_asset(asset: &Asset, max: u64) -> bool {
    asset.state == "uploaded"
        && asset.size > 0
        && asset.size <= max
        && asset.digest.strip_prefix("sha256:").is_some_and(|v| {
            v.len() == 64
                && v.bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn conditional_listing_and_rate_limit_backoff_use_response_headers() {
        use axum::{
            Router,
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
            routing::get,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/repos/owner/repo/releases",
                    get(|headers: HeaderMap| async move {
                        match headers.get("if-none-match").and_then(|v| v.to_str().ok()) {
                            Some("\"fixture\"") => StatusCode::NOT_MODIFIED.into_response(),
                            Some("rate") => {
                                (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "900")])
                                    .into_response()
                            }
                            _ => ([("etag", "\"fixture\"")], "[]").into_response(),
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let mut source = source();
        source.api = format!("http://{address}");
        source.fixture = true;
        let result = source.list(None).await;
        assert!(
            matches!(result, Ok(Listing::Changed {etag: Some(tag), ..}) if tag == "\"fixture\"")
        );
        assert!(matches!(
            source.list(Some("\"fixture\"")).await,
            Ok(Listing::Unchanged)
        ));
        let before = crate::store::now();
        let Err(failure) = source.list(Some("rate")).await else {
            panic!("rate limit accepted")
        };
        assert!(failure.retry_at >= before + 900);
        server.abort();
    }
    fn source() -> Source {
        Source {
            repository: "owner/repo".into(),
            api: "https://api.github.com".into(),
            fixture: false,
            disabled: false,
            client: wreq::Client::builder().no_proxy().build().unwrap(),
        }
    }
    fn release() -> serde_json::Value {
        serde_json::json!({"id":1,"tag_name":"v1.2.3-alpha.1","draft":false,"prerelease":true,"published_at":"2026-10-18T00:00:00Z","assets":[
            {"id":1,"name":BINARY,"size":20,"digest":format!("sha256:{}","0".repeat(64)),"browser_download_url":format!("https://github.com/owner/repo/releases/download/v1.2.3-alpha.1/{BINARY}"),"state":"uploaded"},
            {"id":2,"name":MANIFEST,"size":20,"digest":format!("sha256:{}","1".repeat(64)),"browser_download_url":format!("https://github.com/owner/repo/releases/download/v1.2.3-alpha.1/{MANIFEST}"),"state":"uploaded"}]})
    }
    #[test]
    fn release_catalog_requires_published_complete_hashed_trusted_assets() {
        let source = source();
        let good = release();
        let parsed = source
            .release(serde_json::from_value(good.clone()).unwrap())
            .unwrap();
        assert!(parsed.prerelease);
        assert_eq!(parsed.version, "v1.2.3-alpha.1");
        for (field, value) in [
            ("draft", serde_json::json!(true)),
            ("published_at", serde_json::Value::Null),
            ("tag_name", serde_json::json!("../../1.2.3")),
        ] {
            let mut value0 = good.clone();
            value0[field] = value;
            assert!(
                source
                    .release(serde_json::from_value(value0).unwrap())
                    .is_none()
            );
        }
        for (field, value) in [
            ("digest", serde_json::json!("sha256:bad")),
            ("size", serde_json::json!(129 << 20)),
            ("state", serde_json::json!("new")),
            (
                "browser_download_url",
                serde_json::json!(
                    "https://github.com/other/repo/releases/download/v1.2.3-alpha.1/claud2api-linux-amd64"
                ),
            ),
        ] {
            let mut value0 = good.clone();
            value0["assets"][0][field] = value;
            assert!(
                source
                    .release(serde_json::from_value(value0).unwrap())
                    .is_none()
            );
        }
        assert!(!source.redirect_url(
            &url::Url::parse("http://release-assets.githubusercontent.com/a").unwrap()
        ));
        assert!(
            !source.redirect_url(&url::Url::parse("https://github.com.evil.invalid/a").unwrap())
        );
        assert!(
            !source.redirect_url(&url::Url::parse("https://user:secret@github.com/a").unwrap())
        );
        assert!(!source.redirect_url(&url::Url::parse("https://github.com:444/a").unwrap()));
    }
    #[test]
    fn manifest_rejects_mismatched_versions_digests_and_data_runtime_protocols() {
        let release = source()
            .release(serde_json::from_value(release()).unwrap())
            .unwrap();
        let mut manifest = Manifest {
            protocol: PROTOCOL,
            version: release.version.clone(),
            runtime_abi: RUNTIME_ABI.into(),
            data_format: DATA_FORMAT.into(),
            binary: BINARY.into(),
            size: 20,
            sha256: "0".repeat(64),
        };
        assert!(manifest.validate(&release).is_ok());
        manifest.data_format = "future-data-v2".into();
        assert_eq!(manifest.validate(&release).unwrap_err().status, 409);
        manifest.data_format = DATA_FORMAT.into();
        manifest.runtime_abi = "other-platform".into();
        assert_eq!(manifest.validate(&release).unwrap_err().status, 409);
        manifest.runtime_abi = RUNTIME_ABI.into();
        manifest.protocol = 2;
        assert_eq!(manifest.validate(&release).unwrap_err().status, 409);
        manifest.protocol = PROTOCOL;
        manifest.sha256 = "f".repeat(64);
        assert_eq!(manifest.validate(&release).unwrap_err().status, 422);
        manifest.sha256 = "0".repeat(64);
        manifest.version = "v9.0.0".into();
        assert_eq!(manifest.validate(&release).unwrap_err().status, 422);
        for value in ["owner/repo", "a-b/c_d.e"] {
            assert!(valid_repository(value));
        }
        for value in [
            "../repo",
            "owner/repo/other",
            "owner//repo",
            "owner/repo?x=1",
            "owner/..",
        ] {
            assert!(!valid_repository(value));
        }
    }
}
