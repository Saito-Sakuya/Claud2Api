use crate::{
    engine::Engine,
    error::{Fault, Result},
    guard::Scope,
    observability::RequestTrace,
    store::now,
};
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, AeadCore, OsRng, Payload},
};
use axum::{
    body::Body,
    extract::{Extension, Path, State},
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, sync::Arc};

const DOWNLOAD_PREFIX: &str = "/v1/native-files/";
const HANDLE_PREFIX: &str = "v1.";
const HANDLE_AAD: &[u8] = b"Claud2Api native file handle v1\0";
const OUTPUT_PREFIX: &str = "/mnt/user-data/outputs/";
const MAX_PATH_BYTES: usize = 1024;
const MAX_HANDLE_BYTES: usize = 4096;
const MAX_CLAIM_BYTES: usize = 2048;
const MAX_LINK_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;

#[derive(Debug)]
pub struct Decorated {
    pub block: Value,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileClaim {
    version: u8,
    token_id: Option<String>,
    account_id: String,
    conversation_id: String,
    path: String,
    expires_at: i64,
}

/// Adds authenticated gateway download links only to Claude output-file metadata.
/// All original block fields and content are retained.
pub fn decorate(
    mut block: Value,
    key: &str,
    token_id: &Option<String>,
    account: &str,
    target: &str,
    ttl: u64,
) -> Result<Decorated> {
    let mut expires_at = None;

    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
        return Ok(Decorated { block, expires_at });
    }

    if let Some(items) = block.get_mut("content").and_then(Value::as_array_mut) {
        for item in items {
            if item.get("type").and_then(Value::as_str) != Some("local_resource") {
                continue;
            }
            let Some(path) = item.get("file_path").and_then(Value::as_str) else {
                continue;
            };
            if !valid_output_path(path) {
                continue;
            }
            let (url, expiry) = mint_url(key, token_id, account, target, path, ttl)?;
            item.as_object_mut()
                .ok_or_else(Fault::internal)?
                .insert("claud2api_download_url".into(), Value::String(url));
            expires_at = Some(expires_at.map_or(expiry, |current: i64| current.max(expiry)));
        }
    }

    if let Some(structured) = block.get_mut("structured_content")
        && let Some(path) = structured.get("path").and_then(Value::as_str)
        && valid_output_path(path)
    {
        let (url, expiry) = mint_url(key, token_id, account, target, path, ttl)?;
        structured
            .as_object_mut()
            .ok_or_else(Fault::internal)?
            .insert("claud2api_download_url".into(), Value::String(url));
        expires_at = Some(expires_at.map_or(expiry, |current: i64| current.max(expiry)));
    }

    Ok(Decorated { block, expires_at })
}

fn mint_url(
    key: &str,
    token_id: &Option<String>,
    account_id: &str,
    conversation_id: &str,
    path: &str,
    ttl: u64,
) -> Result<(String, i64)> {
    if key.is_empty()
        || ttl == 0
        || ttl > MAX_LINK_TTL_SECONDS
        || !valid_output_path(path)
        || !valid_account_id(account_id)
        || uuid::Uuid::parse_str(conversation_id).is_err()
        || token_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 128 || id.chars().any(char::is_control))
    {
        return Err(Fault::internal());
    }

    let expiry = now().saturating_add(ttl as i64);
    let claim = FileClaim {
        version: 1,
        token_id: token_id.clone(),
        account_id: account_id.to_owned(),
        conversation_id: conversation_id.to_owned(),
        path: path.to_owned(),
        expires_at: expiry,
    };
    let plain = serde_json::to_vec(&claim).map_err(|_| Fault::internal())?;
    if plain.len() > MAX_CLAIM_BYTES {
        return Err(Fault::internal());
    }

    let cipher = file_cipher(key);
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: &plain,
                aad: HANDLE_AAD,
            },
        )
        .map_err(|_| Fault::internal())?;
    let mut payload = Vec::with_capacity(nonce.len() + ciphertext.len());
    payload.extend_from_slice(&nonce);
    payload.extend_from_slice(&ciphertext);
    let handle = format!("{HANDLE_PREFIX}{}", hex::encode(payload));
    if handle.len() > MAX_HANDLE_BYTES {
        return Err(Fault::internal());
    }
    Ok((format!("{DOWNLOAD_PREFIX}{handle}"), expiry))
}

fn open_handle(key: &str, handle: &str) -> Result<FileClaim> {
    if handle.len() > MAX_HANDLE_BYTES {
        return Err(not_found());
    }
    let encoded = handle.strip_prefix(HANDLE_PREFIX).ok_or_else(not_found)?;
    if encoded.len() < 2 * (12 + 16) || encoded.len() % 2 != 0 {
        return Err(not_found());
    }
    let payload = hex::decode(encoded).map_err(|_| not_found())?;
    if payload.len() < 12 + 16 {
        return Err(not_found());
    }
    let cipher = file_cipher(key);
    let plain = cipher
        .decrypt(
            Nonce::from_slice(&payload[..12]),
            Payload {
                msg: &payload[12..],
                aad: HANDLE_AAD,
            },
        )
        .map_err(|_| not_found())?;
    if plain.len() > MAX_CLAIM_BYTES {
        return Err(not_found());
    }
    let claim: FileClaim = serde_json::from_slice(&plain).map_err(|_| not_found())?;
    validate_claim(&claim)?;
    Ok(claim)
}

fn validate_claim(claim: &FileClaim) -> Result<()> {
    if claim.version != 1
        || !valid_account_id(&claim.account_id)
        || uuid::Uuid::parse_str(&claim.conversation_id).is_err()
        || !valid_output_path(&claim.path)
        || claim.expires_at <= now()
        || claim
            .token_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 128 || id.chars().any(char::is_control))
    {
        return Err(not_found());
    }
    Ok(())
}

fn file_cipher(key: &str) -> Aes256Gcm {
    let mut hash = Sha256::new();
    hash.update(HANDLE_AAD);
    hash.update(key.as_bytes());
    Aes256Gcm::new(&hash.finalize())
}

fn valid_account_id(account_id: &str) -> bool {
    !account_id.is_empty()
        && account_id.len() <= 128
        && account_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn valid_output_path(path: &str) -> bool {
    path.starts_with(OUTPUT_PREFIX)
        && path.len() <= MAX_PATH_BYTES
        && !path.contains(['\\', '#', '?'])
        && !path.chars().any(char::is_control)
        && !path.to_ascii_lowercase().contains("%2e")
        && path[OUTPUT_PREFIX.len()..]
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

fn owner_matches(expected: &Option<String>, actual: &Option<String>) -> bool {
    match (expected, actual) {
        (None, None) => true,
        (Some(expected), Some(actual)) => crate::web::equal(expected, actual),
        _ => false,
    }
}

fn not_found() -> Fault {
    Fault::new(404, "not_found_error", "file link is invalid or expired")
}

/// Serves an authenticated file from its originating Claude conversation.
/// The route must be mounted behind the public API-token authentication layer.
pub async fn download(
    State(engine): State<Arc<Engine>>,
    Extension(trace): Extension<Arc<RequestTrace>>,
    Path(handle): Path<String>,
) -> Result<Response> {
    let claim = open_handle(&engine.config.gateway_key, &handle)?;
    if !owner_matches(&claim.token_id, &trace.token_id()) {
        return Err(Fault::new(
            403,
            "authentication_error",
            "file link belongs to a different API token",
        ));
    }

    let settings = engine.store.read().await.settings;
    let mut scope = Scope::new(settings.request_timeout_seconds);
    scope.trace = Some(trace);
    let lease = engine
        .pool
        .acquire(Some(&claim.account_id), &HashSet::new(), &scope, false)
        .await?;
    if let Some(trace) = &scope.trace {
        trace.bind_account(&lease.account_id);
    }
    let bytes = scope
        .wait(async {
            lease
                .slot
                .client
                .download_file(&lease, &scope, &claim.conversation_id, &claim.path)
                .await
        })
        .await?;

    let filename = claim.path.rsplit('/').next().unwrap_or("download");
    let disposition = content_disposition(filename);
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        axum::http::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).map_err(|_| Fault::internal())?,
    );
    Ok(response)
}

fn content_disposition(filename: &str) -> String {
    let fallback: String = filename
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        .take(100)
        .collect();
    let fallback = if fallback.is_empty() {
        "download"
    } else {
        &fallback
    };
    let mut encoded = String::new();
    for byte in filename.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            )
        {
            encoded.push(*byte as char);
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const KEY: &str = "01234567890123456789012345678901";
    const ACCOUNT: &str = "a1b2c3d4";
    const CONVERSATION: &str = "d439d501-b8cc-426c-801f-48d154c419e6";

    fn minted_handle(token_id: Option<String>, path: &str) -> String {
        mint_url(KEY, &token_id, ACCOUNT, CONVERSATION, path, 60)
            .unwrap()
            .0
            .strip_prefix(DOWNLOAD_PREFIX)
            .unwrap()
            .to_owned()
    }

    #[test]
    fn decorates_only_output_resource_and_artifact_path() {
        let token_id = Some("token-a".to_owned());
        let block = json!({
            "type":"tool_result",
            "tool_use_id":"toolu-1",
            "content":[
                {"type":"local_resource","file_path":"/mnt/user-data/outputs/report.csv","name":"report","mime_type":"text/csv"},
                {"type":"text","text":"kept"},
                {"type":"local_resource","file_path":"/tmp/private.txt"}
            ],
            "structured_content":{"artifact_id":"artifact-1","path":"/mnt/user-data/outputs/page.html","title":"Report","url":"https://example.test"}
        });
        let decorated = decorate(block, KEY, &token_id, ACCOUNT, CONVERSATION, 60).unwrap();
        assert!(decorated.expires_at.is_some());
        let content = decorated.block["content"].as_array().unwrap();
        assert!(
            content[0]["claud2api_download_url"]
                .as_str()
                .unwrap()
                .starts_with(DOWNLOAD_PREFIX)
        );
        assert_eq!(content[1], json!({"type":"text","text":"kept"}));
        assert!(content[2].get("claud2api_download_url").is_none());
        assert!(
            decorated.block["structured_content"]["claud2api_download_url"]
                .as_str()
                .unwrap()
                .starts_with(DOWNLOAD_PREFIX)
        );
        assert_eq!(decorated.block["tool_use_id"], "toolu-1");
        assert_eq!(
            decorated.block["structured_content"]["artifact_id"],
            "artifact-1"
        );
    }

    #[test]
    fn encrypted_handle_roundtrips_and_binds_owner() {
        let owner = Some("token-a".to_owned());
        let handle = minted_handle(owner.clone(), "/mnt/user-data/outputs/a.csv");
        let claim = open_handle(KEY, &handle).unwrap();
        assert_eq!(claim.token_id, owner);
        assert!(owner_matches(&claim.token_id, &Some("token-a".to_owned())));
        assert!(!owner_matches(&claim.token_id, &Some("token-b".to_owned())));
        assert!(!owner_matches(&claim.token_id, &None));
    }

    #[test]
    fn unauthenticated_gateway_owner_is_distinct_from_managed_token() {
        let handle = minted_handle(None, "/mnt/user-data/outputs/a.csv");
        let claim = open_handle(KEY, &handle).unwrap();
        assert!(owner_matches(&claim.token_id, &None));
        assert!(!owner_matches(&claim.token_id, &Some("token-a".to_owned())));
    }

    #[test]
    fn altered_or_wrong_key_handle_is_rejected() {
        let mut handle = minted_handle(Some("token-a".to_owned()), "/mnt/user-data/outputs/a.csv");
        let last = handle.pop().unwrap();
        handle.push(if last == '0' { '1' } else { '0' });
        assert!(open_handle(KEY, &handle).is_err());
        assert!(
            open_handle(
                "different gateway key",
                &minted_handle(None, "/mnt/user-data/outputs/a.csv")
            )
            .is_err()
        );
    }

    #[test]
    fn expired_claims_and_unsafe_paths_are_rejected() {
        let claim = FileClaim {
            version: 1,
            token_id: None,
            account_id: ACCOUNT.to_owned(),
            conversation_id: CONVERSATION.to_owned(),
            path: "/mnt/user-data/outputs/a.csv".to_owned(),
            expires_at: now() - 1,
        };
        assert!(validate_claim(&claim).is_err());
        assert!(
            open_handle(KEY, &minted_handle(None, "/mnt/user-data/outputs/a.csv"))
                .unwrap()
                .expires_at
                > now()
        );
        assert!(!valid_output_path("/mnt/user-data/outputs/../secret"));
        assert!(!valid_output_path("/mnt/user-data/outputs/%2E%2E/secret"));
        assert!(!valid_output_path(
            "/mnt/user-data/outputs/file.csv?download=1"
        ));
        assert!(!valid_output_path(
            "/mnt/user-data/outputs/file.csv#fragment"
        ));
        assert!(!valid_output_path("/tmp/file.csv"));
    }

    #[test]
    fn ignores_non_result_blocks_even_if_they_contain_output_paths() {
        let block = json!({
            "type":"tool_use",
            "input":{"file_path":"/mnt/user-data/outputs/a.csv"},
            "content":[{"type":"local_resource","file_path":"/mnt/user-data/outputs/a.csv"}]
        });
        let decorated = decorate(block.clone(), KEY, &None, ACCOUNT, CONVERSATION, 60).unwrap();
        assert_eq!(decorated.block, block);
        assert!(decorated.expires_at.is_none());
    }

    #[test]
    fn content_disposition_uses_ascii_fallback_and_utf8_filename() {
        let value = content_disposition("结果 文件.csv");
        assert!(value.starts_with("attachment; filename=\".csv\"; filename*=UTF-8''"));
        assert!(value.contains("%E7%BB%93%E6%9E%9C%20%E6%96%87%E4%BB%B6.csv"));
    }
}
