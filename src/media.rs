//! In-memory storage for request media and serializable, payload-free references.
//!
//! This module deliberately does not turn media into prompt text. Callers retain
//! `Reference` values in conversation history and resolve them to `Upload`s only
//! at the outbound transport boundary.

use std::collections::{HashMap, HashSet};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{Fault, Result};

pub const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_FILES: usize = 20;
const MAX_BASE64_BYTES: usize = MAX_FILE_BYTES.div_ceil(3) * 4;
const MAX_FILENAME_BYTES: usize = 128;
const HASH_SUFFIX_BYTES: usize = 12;

/// Safe, serializable metadata referring to bytes held by a `Sources` value.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reference {
    pub sha256: String,
    pub filename: String,
    pub media_type: String,
    pub size: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// Bytes ready for an outbound upload. The byte payload is never formatted.
#[derive(Clone)]
pub struct Upload {
    pub reference: Reference,
    pub data: Bytes,
}

impl std::fmt::Debug for Upload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upload")
            .field("reference", &self.reference)
            .field("data_len", &self.data.len())
            .finish()
    }
}

/// Per-request in-memory media bytes, indexed by their SHA-256 digest.
#[derive(Clone, Default)]
pub struct Sources {
    files: HashMap<String, Bytes>,
}

impl std::fmt::Debug for Sources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sources")
            .field("files", &self.files.len())
            .finish()
    }
}

impl Sources {
    /// Register an Anthropic image block with a base64 source.
    pub fn image(&mut self, block: &Value) -> Result<Reference> {
        if block.get("type").and_then(Value::as_str) != Some("image") {
            return Err(Fault::invalid("expected an image content block"));
        }
        let source = block
            .get("source")
            .and_then(Value::as_object)
            .ok_or_else(|| Fault::invalid("image source is missing or invalid"))?;
        if source.get("type").and_then(Value::as_str) != Some("base64") {
            return Err(Fault::invalid(
                "image source must contain embedded base64 data",
            ));
        }
        let media_type = source
            .get("media_type")
            .and_then(Value::as_str)
            .ok_or_else(|| Fault::invalid("image media type is missing or invalid"))?;
        if !is_image_type(media_type) {
            return Err(Fault::invalid("unsupported image media type"));
        }
        let encoded = source
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| Fault::invalid("image data is missing or invalid"))?;
        let data = decode_base64(encoded)?;
        let title = optional_title(block)?;
        let context = optional_context(block)?;
        self.register(data, media_type, title, context)
    }

    /// Register a document block or a plain-text content block.
    ///
    /// Document base64 sources accept PDF and UTF-8 text. `text` and `data`
    /// blocks contain plain UTF-8 text directly, without base64 encoding.
    pub fn document(&mut self, block: &Value) -> Result<Reference> {
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| Fault::invalid("document type is missing or invalid"))?;

        if block_type == "document"
            && block
                .get("citations")
                .and_then(|v| v.get("enabled"))
                .and_then(Value::as_bool)
                == Some(true)
        {
            return Err(Fault::invalid(
                "document citations are not supported by the upstream API",
            ));
        }

        let (data, media_type, title) = match block_type {
            "document" => {
                let source = block
                    .get("source")
                    .and_then(Value::as_object)
                    .ok_or_else(|| Fault::invalid("document source is missing or invalid"))?;
                let source_type = source.get("type").and_then(Value::as_str).unwrap_or("");
                let media_type = match source_type {
                    "base64" => source
                        .get("media_type")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            Fault::invalid("document media type is missing or invalid")
                        })?,
                    "text" | "data" => "text/plain",
                    _ => {
                        return Err(Fault::invalid("document source must contain embedded data"));
                    }
                };
                if media_type != "application/pdf" && media_type != "text/plain" {
                    return Err(Fault::invalid("unsupported document media type"));
                }
                let value = source
                    .get("data")
                    .or_else(|| source.get("text"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| Fault::invalid("document data is missing or invalid"))?;
                let data = if source_type == "base64" {
                    decode_base64(value)?
                } else {
                    text_bytes(value)?
                };
                (data, media_type, optional_title(block)?)
            }
            "text" => {
                let value = block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Fault::invalid("text data is missing or invalid"))?;
                (text_bytes(value)?, "text/plain", optional_title(block)?)
            }
            "data" => {
                let value = block
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Fault::invalid("text data is missing or invalid"))?;
                (text_bytes(value)?, "text/plain", optional_title(block)?)
            }
            _ => return Err(Fault::invalid("unsupported document content block")),
        };

        self.register(data, media_type, title, optional_context(block)?)
    }

    /// Register a base64 `data:` URL. Remote URLs are never fetched.
    pub fn data_url(&mut self, url: &str, filename: Option<&str>) -> Result<Reference> {
        if url.len() > MAX_BASE64_BYTES.saturating_add(96) {
            return Err(Fault::invalid("media data exceeds the per-file size limit"));
        }
        let Some(payload) = url.strip_prefix("data:") else {
            if url
                .get(..7)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
                || url
                    .get(..8)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
            {
                return Err(Fault::invalid(
                    "remote media URLs are not supported; provide embedded data",
                ));
            }
            return Err(Fault::invalid(
                "only embedded base64 data URLs are supported",
            ));
        };
        let (metadata, encoded) = payload
            .split_once(',')
            .ok_or_else(|| Fault::invalid("invalid embedded data URL"))?;
        let mut parts = metadata.split(';');
        let media_type = parts.next().unwrap_or_default();
        if parts.next() != Some("base64") || parts.next().is_some() {
            return Err(Fault::invalid("data URL must use base64 encoding"));
        }
        if !is_supported_type(media_type) {
            return Err(Fault::invalid("unsupported media type"));
        }
        let data = decode_base64(encoded)?;
        self.register(data, media_type, filename, None)
    }

    /// Resolve references to bytes, validating every reference and deduplicating
    /// by digest, filename and media type in first-reference order. The total
    /// bound applies to bytes actually uploaded, including differently named
    /// references to the same payload.
    pub fn uploads(&self, references: &[Reference]) -> Result<Vec<Upload>> {
        let mut seen = HashSet::new();
        let mut uploads = Vec::new();
        let mut total = 0usize;
        for reference in references {
            let data = self
                .files
                .get(&reference.sha256)
                .ok_or_else(|| Fault::invalid("media reference is unavailable"))?;
            validate_reference(reference, data)?;
            let key = (
                reference.sha256.clone(),
                reference.filename.clone(),
                reference.media_type.clone(),
            );
            if !seen.insert(key) {
                continue;
            }
            if uploads.len() >= MAX_FILES {
                return Err(Fault::invalid("too many distinct media references"));
            }
            total = total.saturating_add(data.len());
            if total > MAX_TOTAL_BYTES {
                return Err(Fault::invalid("request media exceeds the total size limit"));
            }
            uploads.push(Upload {
                reference: reference.clone(),
                data: data.clone(),
            });
        }
        Ok(uploads)
    }

    fn register(
        &mut self,
        data: Bytes,
        media_type: &str,
        title: Option<&str>,
        context: Option<String>,
    ) -> Result<Reference> {
        if data.is_empty() || data.len() > MAX_FILE_BYTES {
            return Err(Fault::invalid("media data exceeds the per-file size limit"));
        }
        validate_media(media_type, &data)?;
        let sha256 = digest(&data);
        if let Some(existing) = self.files.get(&sha256) {
            if existing.as_ref() != data.as_ref() {
                return Err(Fault::invalid("media digest conflict"));
            }
        } else {
            if self.files.len() >= MAX_FILES {
                return Err(Fault::invalid("request contains too many media files"));
            }
            let total = self
                .files
                .values()
                .fold(0usize, |sum, bytes| sum.saturating_add(bytes.len()));
            if total.saturating_add(data.len()) > MAX_TOTAL_BYTES {
                return Err(Fault::invalid("request media exceeds the total size limit"));
            }
            self.files.insert(sha256.clone(), data.clone());
        }

        Ok(Reference {
            filename: safe_filename(title, &sha256, media_type),
            sha256,
            media_type: media_type.to_owned(),
            size: data.len(),
            context,
        })
    }
}

fn optional_title(block: &Value) -> Result<Option<&str>> {
    let value = block.get("filename").or_else(|| block.get("title"));
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(Fault::invalid("media title is invalid")),
    }
}

fn optional_context(block: &Value) -> Result<Option<String>> {
    match block.get("context") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(Fault::invalid("document context is invalid")),
    }
}

fn decode_base64(encoded: &str) -> Result<Bytes> {
    if encoded.len() > MAX_BASE64_BYTES {
        return Err(Fault::invalid("media data exceeds the per-file size limit"));
    }
    let estimated = encoded
        .len()
        .saturating_div(4)
        .saturating_mul(3)
        .saturating_sub(
            encoded
                .as_bytes()
                .iter()
                .rev()
                .take_while(|&&b| b == b'=')
                .count()
                .min(2),
        );
    if estimated > MAX_FILE_BYTES {
        return Err(Fault::invalid("media data exceeds the per-file size limit"));
    }
    let decoded = STANDARD
        .decode(encoded)
        .map_err(|_| Fault::invalid("media data is not valid base64"))?;
    if decoded.len() > MAX_FILE_BYTES {
        return Err(Fault::invalid("media data exceeds the per-file size limit"));
    }
    Ok(Bytes::from(decoded))
}

fn text_bytes(text: &str) -> Result<Bytes> {
    if text.len() > MAX_FILE_BYTES {
        return Err(Fault::invalid("media data exceeds the per-file size limit"));
    }
    if text.is_empty() || text.as_bytes().contains(&0) {
        return Err(Fault::invalid(
            "text media must contain non-empty UTF-8 text",
        ));
    }
    Ok(Bytes::copy_from_slice(text.as_bytes()))
}

fn validate_media(media_type: &str, data: &[u8]) -> Result<()> {
    let valid = match media_type {
        "image/png" => {
            if data.len() < 24 {
                false
            } else {
                let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
                let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
                data.starts_with(b"\x89PNG\r\n\x1a\n")
                    && data.get(12..16) == Some(b"IHDR")
                    && width != 0
                    && height != 0
            }
        }
        "image/jpeg" => data.len() >= 4 && data.starts_with(b"\xff\xd8\xff"),
        "image/gif" => {
            data.len() >= 13
                && (data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a"))
                && (data[6] != 0 || data[7] != 0)
                && (data[8] != 0 || data[9] != 0)
        }
        "image/webp" => {
            data.len() >= 12
                && data.starts_with(b"RIFF")
                && data.get(8..12) == Some(b"WEBP")
                && u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize + 8
                    <= data.len()
        }
        "application/pdf" => {
            data.len() >= 8
                && data.starts_with(b"%PDF-")
                && data[5].is_ascii_digit()
                && data[6] == b'.'
                && data[7].is_ascii_digit()
        }
        "text/plain" => {
            std::str::from_utf8(data).is_ok()
                && !data.contains(&0)
                && !looks_like_supported_binary(data)
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(Fault::invalid(
            "media type does not match a supported file signature or encoding",
        ))
    }
}

fn looks_like_supported_binary(data: &[u8]) -> bool {
    data.starts_with(b"\x89PNG\r\n\x1a\n")
        || data.starts_with(b"\xff\xd8\xff")
        || data.starts_with(b"GIF87a")
        || data.starts_with(b"GIF89a")
        || (data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP"))
        || data.starts_with(b"%PDF-")
}

fn is_image_type(media_type: &str) -> bool {
    matches!(
        media_type,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

fn is_supported_type(media_type: &str) -> bool {
    is_image_type(media_type) || matches!(media_type, "application/pdf" | "text/plain")
}

fn extension(media_type: &str) -> Option<&'static str> {
    match media_type {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "application/pdf" => Some("pdf"),
        "text/plain" => Some("txt"),
        _ => None,
    }
}

fn safe_filename(title: Option<&str>, hash: &str, media_type: &str) -> String {
    let ext = extension(media_type).unwrap_or("bin");
    let fallback = || format!("{}.{}", &hash[..hash.len().min(12)], ext);
    let Some(title) = title else {
        return fallback();
    };
    let basename = title.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut filtered = String::new();
    for character in basename.chars() {
        if character.is_control() || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
            if !filtered.ends_with('_') {
                filtered.push('_');
            }
        } else {
            filtered.push(character);
        }
    }
    let mut stem = filtered
        .trim_matches(|character| matches!(character, ' ' | '.'))
        .to_owned();
    let expected_suffix = format!(".{ext}");
    if stem.to_ascii_lowercase().ends_with(&expected_suffix) {
        stem.truncate(stem.len() - expected_suffix.len());
    }
    if let Some((prefix, suffix)) = stem.rsplit_once('.')
        && is_executable_extension(suffix)
    {
        stem = prefix.to_owned();
    }
    stem = truncate_utf8(&stem, filename_stem_limit(ext))
        .trim_matches(|character| matches!(character, ' ' | '.'))
        .to_owned();
    if stem.is_empty() || is_reserved_filename(&stem) {
        return fallback();
    }
    let hash_suffix = &hash[..hash.len().min(HASH_SUFFIX_BYTES)];
    format!("{stem}-{hash_suffix}.{ext}")
}

fn filename_stem_limit(extension: &str) -> usize {
    MAX_FILENAME_BYTES.saturating_sub(HASH_SUFFIX_BYTES + 1 + extension.len() + 1)
}

fn truncate_utf8(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn is_executable_extension(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "exe"
            | "dll"
            | "com"
            | "bat"
            | "cmd"
            | "ps1"
            | "psm1"
            | "sh"
            | "bash"
            | "zsh"
            | "app"
            | "msi"
            | "scr"
            | "jar"
            | "js"
            | "vbs"
            | "wsf"
    )
}

fn is_reserved_filename(stem: &str) -> bool {
    let upper = stem.trim_end_matches([' ', '.']).to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && matches!(upper.as_bytes()[3], b'1'..=b'9'))
}

fn validate_reference(reference: &Reference, data: &[u8]) -> Result<()> {
    if reference.size != data.len()
        || reference.size > MAX_FILE_BYTES
        || digest(data) != reference.sha256
        || !is_supported_type(&reference.media_type)
    {
        return Err(Fault::invalid(
            "media reference metadata does not match its data",
        ));
    }
    validate_media(&reference.media_type, data)?;
    if !safe_reference_filename(&reference.filename, &reference.media_type) {
        return Err(Fault::invalid("media reference filename is invalid"));
    }
    Ok(())
}

fn safe_reference_filename(filename: &str, media_type: &str) -> bool {
    let Some(ext) = extension(media_type) else {
        return false;
    };
    if filename.is_empty()
        || filename.len() > MAX_FILENAME_BYTES
        || filename
            .chars()
            .any(|character| matches!(character, '/' | '\\'))
        || filename.chars().any(char::is_control)
        || filename
            .chars()
            .any(|character| matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        || filename.trim_matches(|character| matches!(character, ' ' | '.')) != filename
    {
        return false;
    }
    let Some((stem, suffix)) = filename.rsplit_once('.') else {
        return false;
    };
    suffix == ext
        && !stem.is_empty()
        && !is_reserved_filename(stem)
        && !stem
            .rsplit_once('.')
            .is_some_and(|(_, previous_ext)| is_executable_extension(previous_ext))
}

fn digest(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn encode(data: &[u8]) -> String {
        STANDARD.encode(data)
    }

    fn png(marker: &[u8]) -> Vec<u8> {
        let mut data = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0".to_vec();
        data.extend_from_slice(marker);
        data
    }

    #[test]
    fn stores_image_pdf_and_text_as_payload_free_references() {
        let mut sources = Sources::default();
        let image_bytes = png(b"image-payload-marker");
        let image = sources
            .image(&json!({
                "type":"image",
                "source":{"type":"base64","media_type":"image/png","data":encode(&image_bytes)}
            }))
            .unwrap();
        let pdf_bytes = b"%PDF-1.7\nfile payload";
        let pdf = sources
            .document(&json!({
                "type":"document",
                "title":"..\\reports\\brief.exe",
                "context":"attachment context",
                "source":{"type":"base64","media_type":"application/pdf","data":encode(pdf_bytes)}
            }))
            .unwrap();
        let text = sources
            .document(&json!({"type":"text","text":"plain UTF-8 text"}))
            .unwrap();

        assert_eq!(image.media_type, "image/png");
        assert!(image.filename.ends_with(".png"));
        assert_eq!(
            pdf.filename,
            format!("brief-{}.pdf", &pdf.sha256[..HASH_SUFFIX_BYTES])
        );
        assert_eq!(pdf.context.as_deref(), Some("attachment context"));
        assert_eq!(text.media_type, "text/plain");
        assert_eq!(text.size, "plain UTF-8 text".len());

        let uploads = sources
            .uploads(&[image.clone(), pdf.clone(), text.clone()])
            .unwrap();
        assert_eq!(uploads.len(), 3);
        assert_eq!(uploads[0].data.as_ref(), image_bytes.as_slice());
        assert_eq!(uploads[1].data.as_ref(), pdf_bytes);

        let reference_json = serde_json::to_string(&image).unwrap();
        let upload_debug = format!("{:?}", uploads[0]);
        let sources_debug = format!("{sources:?}");
        assert!(!reference_json.contains("image-payload-marker"));
        assert!(!upload_debug.contains("image-payload-marker"));
        assert!(!sources_debug.contains("image-payload-marker"));
        assert!(sources_debug.contains("files: 3"));
    }

    #[test]
    fn accepts_all_supported_image_signatures_and_data_text_blocks() {
        let mut sources = Sources::default();
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[1, 0, 1, 0, 0, 0, 0]);
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&4u32.to_le_bytes());
        webp.extend_from_slice(b"WEBP");
        for (media_type, bytes, suffix) in [
            ("image/png", png(b""), "png"),
            ("image/jpeg", b"\xff\xd8\xff\xd9".to_vec(), "jpg"),
            ("image/gif", gif, "gif"),
            ("image/webp", webp, "webp"),
        ] {
            let reference = sources
                .image(&json!({
                    "type":"image",
                    "source":{"type":"base64","media_type":media_type,"data":encode(&bytes)}
                }))
                .unwrap();
            assert!(reference.filename.ends_with(&format!(".{suffix}")));
        }
        let data = sources
            .document(&json!({"type":"data","data":"text carried by a data block"}))
            .unwrap();
        assert_eq!(data.media_type, "text/plain");
    }

    #[test]
    fn rejects_mime_mismatch_bad_base64_missing_source_and_citations() {
        let mut sources = Sources::default();
        let mismatch = sources.image(&json!({
            "type":"image",
            "source":{"type":"base64","media_type":"image/png","data":encode(b"%PDF-1.7\nnot an image")}
        }));
        assert_eq!(mismatch.unwrap_err().status, 400);
        let mut zero_width_png = png(b"");
        zero_width_png[16..20].fill(0);
        assert!(validate_media("image/png", &zero_width_png).is_err());
        let mut zero_height_png = png(b"");
        zero_height_png[20..24].fill(0);
        assert!(validate_media("image/png", &zero_height_png).is_err());

        let bad_base64 = sources.image(&json!({
            "type":"image",
            "source":{"type":"base64","media_type":"image/png","data":"%%%!"}
        }));
        assert!(bad_base64.unwrap_err().message.contains("base64"));

        assert!(sources.image(&json!({"type":"image"})).is_err());
        let mislabeled_pdf = sources.document(&json!({
            "type":"document",
            "source":{"type":"base64","media_type":"text/plain","data":encode(b"%PDF-1.7\nnot plain text")}
        }));
        assert!(mislabeled_pdf.is_err());
        assert!(sources.document(&json!({
            "type":"document",
            "citations":{"enabled":true},
            "source":{"type":"base64","media_type":"application/pdf","data":encode(b"%PDF-1.7\n")}
        })).unwrap_err().message.contains("citations"));
    }

    #[test]
    fn rejects_remote_urls_without_echoing_input() {
        let mut sources = Sources::default();
        let result = sources.data_url("https://example.invalid/private.png?token=secret", None);
        let error = result.unwrap_err();
        assert_eq!(error.status, 400);
        assert!(!error.message.contains("example.invalid"));
        assert!(!error.message.contains("secret"));
    }

    #[test]
    fn rejects_oversized_inputs_and_request_totals() {
        let mut sources = Sources::default();
        let oversized_encoded = "A".repeat(MAX_BASE64_BYTES + 4);
        let result = sources.image(&json!({
            "type":"image",
            "source":{"type":"base64","media_type":"image/png","data":oversized_encoded}
        }));
        assert!(result.unwrap_err().message.contains("size limit"));

        let oversized_text = "x".repeat(MAX_FILE_BYTES + 1);
        let result = sources.document(&json!({"type":"text","text":oversized_text}));
        assert!(result.unwrap_err().message.contains("size limit"));

        let ten_megabytes = "a".repeat(MAX_FILE_BYTES);
        sources
            .document(&json!({"type":"text","text":ten_megabytes}))
            .unwrap();
        let second = "b".repeat(MAX_FILE_BYTES);
        sources
            .document(&json!({"type":"text","text":second}))
            .unwrap();
        let result = sources.document(&json!({"type":"text","text":"c"}));
        assert!(result.unwrap_err().message.contains("total size limit"));
    }

    #[test]
    fn limits_distinct_file_count_and_deduplicates_by_first_reference() {
        let image_bytes = png(b"same image");
        let encoded = encode(&image_bytes);
        let mut sources = Sources::default();
        let first = sources
            .data_url(
                &format!("data:image/png;base64,{encoded}"),
                Some("first.png"),
            )
            .unwrap();
        let duplicate = sources
            .data_url(
                &format!("data:image/png;base64,{encoded}"),
                Some("second.png"),
            )
            .unwrap();
        assert_ne!(first.filename, duplicate.filename);
        let uploads = sources.uploads(&[first.clone(), duplicate]).unwrap();
        assert_eq!(uploads.len(), 2);
        assert_eq!(
            uploads[0].reference.filename,
            format!("first-{}.png", &first.sha256[..HASH_SUFFIX_BYTES])
        );

        let mut many = Sources::default();
        for index in 0..MAX_FILES {
            many.document(&json!({"type":"text","text":format!("file-{index}")}))
                .unwrap();
        }
        let error = many
            .document(&json!({"type":"text","text":"twenty-first"}))
            .unwrap_err();
        assert!(error.message.contains("too many"));
    }

    #[test]
    fn filenames_disambiguate_content_and_uploads_cap_distinct_references() {
        let mut sources = Sources::default();
        let first = sources
            .document(&json!({"type":"text","text":"same title first bytes","title":"notes.txt"}))
            .unwrap();
        let second = sources
            .document(&json!({"type":"text","text":"same title second bytes","title":"notes.txt"}))
            .unwrap();
        assert_ne!(first.sha256, second.sha256);
        assert_ne!(first.filename, second.filename);
        assert!(first.filename.ends_with(".txt"));
        assert!(second.filename.ends_with(".txt"));
        assert!(first.filename.len() <= MAX_FILENAME_BYTES);
        let long_title = sources
            .document(&json!({
                "type":"text",
                "text":"long title filename",
                "title":format!("{}.txt", "文件名".repeat(100))
            }))
            .unwrap();
        assert!(long_title.filename.len() <= MAX_FILENAME_BYTES);

        let mut refs = Vec::new();
        for index in 0..MAX_FILES {
            refs.push(
                sources
                    .document(&json!({
                        "type":"text",
                        "text":"same bytes under different names",
                        "title":format!("attachment-{index}.txt"),
                        "context":format!("context-{index}")
                    }))
                    .unwrap(),
            );
        }
        assert_eq!(sources.uploads(&refs).unwrap().len(), MAX_FILES);
        let too_many = sources
            .document(&json!({
                "type":"text",
                "text":"same bytes under different names",
                "title":"attachment-overflow.txt"
            }))
            .unwrap();
        refs.push(too_many);
        assert!(sources.uploads(&refs).is_err());

        let same_content_same_name = sources
            .document(&json!({
                "type":"text",
                "text":"same bytes under different names",
                "title":"attachment-0.txt",
                "context":"different context"
            }))
            .unwrap();
        assert_eq!(
            sources
                .uploads(&[refs[0].clone(), same_content_same_name])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn validates_references_before_resolving_uploads() {
        let mut sources = Sources::default();
        let reference = sources
            .document(&json!({"type":"text","text":"retained"}))
            .unwrap();
        let mut forged = reference.clone();
        forged.size += 1;
        assert!(sources.uploads(&[forged]).is_err());

        let unknown = Reference {
            sha256: "0".repeat(64),
            filename: "unknown.txt".into(),
            media_type: "text/plain".into(),
            size: 1,
            context: None,
        };
        assert!(sources.uploads(&[unknown]).is_err());
    }
}
