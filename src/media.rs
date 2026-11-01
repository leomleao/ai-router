//! Bounded normalization and provider projections for user supplied media.
//! This module never fetches URLs or writes media to disk.
use crate::protocol::ApiError;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

pub(crate) const MAX_MEDIA_BYTES: usize = 768 * 1024;
pub(crate) const MAX_IMAGES: usize = 8;
pub(crate) const MAX_FILES: usize = 1;

#[derive(Clone, Copy)]
pub(crate) enum ContentApi {
    Chat,
    Responses,
}

/// Convert either public API's content parts to the canonical Responses-style
/// typed history. Chat text remains `input_text`; image URLs remain typed.
pub(crate) fn normalize_content(
    value: &Value,
    api: ContentApi,
    allow_media: bool,
) -> Result<Value, ApiError> {
    if let Some(text) = value.as_str() {
        return Ok(json!(text));
    }
    if value.is_null() {
        return Ok(Value::Null);
    }
    let blocks = value
        .as_array()
        .ok_or_else(|| ApiError::bad_request("Message content must be text or content blocks"))?;
    if blocks.len() > 128 {
        return Err(ApiError::bad_request("Too many content blocks"));
    }
    let mut output = Vec::with_capacity(blocks.len());
    let mut images = 0usize;
    for block in blocks {
        let object = block
            .as_object()
            .ok_or_else(|| ApiError::bad_request("Content blocks must be objects"))?;
        match object.get("type").and_then(Value::as_str) {
            Some("text") if matches!(api, ContentApi::Chat) => {
                fields(
                    object,
                    &["type", "text", "annotations", "logprobs", "cache_control"],
                )?;
                let text = object
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ApiError::bad_request("Content text must be a string"))?;
                bounded(text)?;
                output.push(json!({"type":"input_text","text":text}));
            }
            Some("input_text" | "output_text") if matches!(api, ContentApi::Responses) => {
                fields(
                    object,
                    &["type", "text", "annotations", "logprobs", "cache_control"],
                )?;
                let text = object
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ApiError::bad_request("Content text must be a string"))?;
                bounded(text)?;
                output.push(json!({"type":object["type"],"text":text}));
            }
            Some("image_url") if matches!(api, ContentApi::Chat) => {
                if !allow_media {
                    return Err(ApiError::unsupported("image content in this role"));
                }
                fields(object, &["type", "image_url"])?;
                let image = object
                    .get("image_url")
                    .and_then(Value::as_object)
                    .ok_or_else(|| ApiError::bad_request("image_url must be an object"))?;
                fields(image, &["url", "detail"])?;
                let url = image
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ApiError::bad_request("image_url.url must be a string"))?;
                let detail = detail(image.get("detail"))?;
                let url = normalize_image_url(url)?;
                images += 1;
                output.push(json!({"type":"input_image","image_url":url,"detail":detail}));
            }
            Some("input_image") if matches!(api, ContentApi::Responses) => {
                if !allow_media {
                    return Err(ApiError::unsupported("image content in this role"));
                }
                fields(object, &["type", "image_url", "detail"])?;
                let url = object
                    .get("image_url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ApiError::bad_request("input_image.image_url must be a string")
                    })?;
                let detail = detail(object.get("detail"))?;
                let url = normalize_image_url(url)?;
                images += 1;
                output.push(json!({"type":"input_image","image_url":url,"detail":detail}));
            }
            Some("input_file") if matches!(api, ContentApi::Responses) => {
                if !allow_media {
                    return Err(ApiError::unsupported("file content in this role"));
                }
                fields(object, &["type", "file_data", "filename", "file_url"])?;
                let has_data = object.contains_key("file_data");
                let has_url = object.contains_key("file_url");
                if has_data == has_url {
                    return Err(ApiError::bad_request(
                        "input_file requires exactly one of file_data or file_url",
                    ));
                }
                if has_data {
                    let data = object["file_data"]
                        .as_str()
                        .ok_or_else(|| ApiError::bad_request("file_data must be a string"))?;
                    let filename =
                        safe_filename(object.get("filename").and_then(Value::as_str).ok_or_else(
                            || ApiError::bad_request("filename is required with file_data"),
                        )?)?;
                    let canonical = normalize_file_data(data, &filename, false)?;
                    output.push(
                        json!({"type":"input_file","file_data":canonical,"filename":filename}),
                    );
                } else {
                    if object.contains_key("filename") {
                        return Err(ApiError::bad_request(
                            "filename is only valid with file_data",
                        ));
                    }
                    let url = object["file_url"]
                        .as_str()
                        .ok_or_else(|| ApiError::bad_request("file_url must be a string"))?;
                    output.push(json!({"type":"input_file","file_url":https_url(url)?}));
                }
            }
            Some("file") if matches!(api, ContentApi::Chat) => {
                if !allow_media {
                    return Err(ApiError::unsupported("file content in this role"));
                }
                fields(object, &["type", "file"])?;
                let file = object
                    .get("file")
                    .and_then(Value::as_object)
                    .ok_or_else(|| ApiError::bad_request("file must be an object"))?;
                fields(file, &["filename", "file_data"])?;
                let filename = safe_filename(
                    file.get("filename")
                        .and_then(Value::as_str)
                        .ok_or_else(|| ApiError::bad_request("filename is required"))?,
                )?;
                let data = file
                    .get("file_data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ApiError::bad_request("file_data must be a string"))?;
                let canonical = normalize_file_data(data, filename, true)?;
                output.push(json!({"type":"input_file","file_data":canonical,"filename":filename}));
            }
            Some(_) => return Err(ApiError::unsupported("content block type")),
            None => return Err(ApiError::bad_request("Content block type is required")),
        }
    }
    if images > MAX_IMAGES {
        return Err(ApiError::bad_request("Too many images (maximum 8)"));
    }
    Ok(Value::Array(output))
}

pub(crate) fn image_part_to_agy(part: &Value) -> Result<Value, ApiError> {
    if part["type"] != "input_image" {
        return Err(ApiError::unsupported("image content"));
    }
    let url = part["image_url"]
        .as_str()
        .ok_or_else(|| ApiError::bad_request("Invalid normalized image"))?;
    if url.starts_with("data:") {
        let (mime, bytes) =
            decode_data_url(url, &["image/png", "image/jpeg", "image/webp", "image/gif"])?;
        validate_image_bytes(mime, &bytes)?;
        Ok(json!({"inlineData":{"mimeType":mime,"data":STANDARD.encode(bytes)}}))
    } else {
        Err(ApiError::unsupported("HTTPS image URLs for Antigravity"))
    }
}

pub(crate) fn media_counts(value: &Value) -> (usize, usize) {
    value.as_array().map_or((0, 0), |parts| {
        parts.iter().fold((0, 0), |(images, files), part| {
            match part["type"].as_str() {
                Some("input_image") => (images + 1, files),
                Some("input_file") => (images, files + 1),
                _ => (images, files),
            }
        })
    })
}

/// Produce the documented Responses content block array when media is present;
/// text-only input remains a compact string for backward compatibility.
pub(crate) fn openai_content(value: &Value) -> Result<Value, ApiError> {
    if value.is_null() {
        return Ok(Value::String(String::new()));
    }
    let Some(parts) = value.as_array() else {
        return Ok(value.clone());
    };
    let has_media = parts
        .iter()
        .any(|part| matches!(part["type"].as_str(), Some("input_image" | "input_file")));
    if !has_media {
        let mut text = String::new();
        for part in parts {
            let part = part["text"]
                .as_str()
                .ok_or_else(|| ApiError::unsupported("non-text input"))?;
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(part);
        }
        return Ok(Value::String(text));
    }
    let mut output = Vec::with_capacity(parts.len());
    for part in parts {
        match part["type"].as_str() {
            Some("input_text" | "output_text") => {
                output.push(json!({"type":"input_text","text":part["text"]}))
            }
            Some("input_image") => output.push(
                json!({"type":"input_image","image_url":part["image_url"],"detail":part["detail"]}),
            ),
            Some("input_file") => {
                let mut file = part.clone();
                if let Some(obj) = file.as_object_mut() {
                    obj.insert("type".into(), json!("input_file"));
                }
                output.push(file);
            }
            _ => return Err(ApiError::unsupported("normalized media content")),
        }
    }
    Ok(Value::Array(output))
}

/// Convert normalized conversation content to reviewed CCA parts. HTTPS images
/// are rejected because this wire contract has no verified remote-image shape.
pub(crate) fn agy_parts(value: &Value) -> Result<Vec<Value>, ApiError> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    if let Some(text) = value.as_str() {
        return Ok(if text.is_empty() {
            Vec::new()
        } else {
            vec![json!({"text":text})]
        });
    }
    let parts = value
        .as_array()
        .ok_or_else(|| ApiError::bad_request("Invalid normalized content"))?;
    let mut output = Vec::with_capacity(parts.len());
    for part in parts {
        match part["type"].as_str() {
            Some("input_text" | "output_text") => output.push(json!({"text":part["text"]})),
            Some("input_image") => output.push(image_part_to_agy(part)?),
            Some("input_file") => {
                return Err(ApiError::unsupported("OpenAI file input for Antigravity"));
            }
            _ => return Err(ApiError::unsupported("content shape for Antigravity")),
        }
    }
    Ok(output)
}

fn normalize_image_url(url: &str) -> Result<String, ApiError> {
    if url.starts_with("data:") {
        let (mime, bytes) =
            decode_data_url(url, &["image/png", "image/jpeg", "image/webp", "image/gif"])?;
        validate_image_bytes(mime, &bytes)?;
        Ok(url.to_owned())
    } else {
        https_url(url)
    }
}

fn normalize_file_data(
    data: &str,
    filename: &str,
    chat_pdf_only: bool,
) -> Result<String, ApiError> {
    let allowed = [
        "application/pdf",
        "text/plain",
        "text/csv",
        "application/json",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ];
    let (mime, bytes) = decode_data_url(
        data,
        if chat_pdf_only {
            &allowed[..1]
        } else {
            &allowed
        },
    )?;
    let ext = filename
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase());
    let valid = match mime {
        "application/pdf" => ext.as_deref() == Some("pdf") && valid_pdf(&bytes),
        "text/plain" => ext.as_deref() == Some("txt") && std::str::from_utf8(&bytes).is_ok(),
        "text/csv" => ext.as_deref() == Some("csv") && std::str::from_utf8(&bytes).is_ok(),
        "application/json" => {
            ext.as_deref() == Some("json")
                && std::str::from_utf8(&bytes)
                    .is_ok_and(|s| serde_json::from_str::<Value>(s).is_ok())
        }
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            ext.as_deref() == Some("docx") && valid_zip(&bytes)
        }
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => {
            ext.as_deref() == Some("pptx") && valid_zip(&bytes)
        }
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => {
            ext.as_deref() == Some("xlsx") && valid_zip(&bytes)
        }
        _ => false,
    };
    if !valid {
        return Err(ApiError::bad_request(
            "file_data does not match its MIME type and filename",
        ));
    }
    Ok(data.to_owned())
}

fn valid_pdf(b: &[u8]) -> bool {
    b.len() >= 12
        && b.starts_with(b"%PDF-1.")
        && b[7].is_ascii_digit()
        && b.windows(5).any(|w| w == b"%%EOF")
}

fn valid_zip(b: &[u8]) -> bool {
    b.len() >= 22 && b.starts_with(b"PK\x03\x04") && b.windows(4).any(|w| w == b"PK\x05\x06")
}

fn decode_data_url<'a>(url: &'a str, allowed: &[&str]) -> Result<(&'a str, Vec<u8>), ApiError> {
    let (header, encoded) = url
        .split_once(',')
        .ok_or_else(|| ApiError::bad_request("Invalid data URL"))?;
    let mime = header
        .strip_prefix("data:")
        .and_then(|s| s.strip_suffix(";base64"))
        .ok_or_else(|| ApiError::bad_request("Media data URL must use base64"))?;
    if !allowed.contains(&mime) {
        return Err(ApiError::unsupported("media type"));
    }
    if encoded.len() > MAX_MEDIA_BYTES * 4 / 3 + 8 {
        return Err(too_large());
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| ApiError::bad_request("Invalid base64 media data"))?;
    if STANDARD.encode(&bytes) != encoded {
        return Err(ApiError::bad_request(
            "Media data URL must use canonical base64",
        ));
    }
    if bytes.is_empty() {
        return Err(ApiError::bad_request("Media data is empty"));
    }
    if bytes.len() > MAX_MEDIA_BYTES {
        return Err(too_large());
    }
    Ok((mime, bytes))
}

pub(crate) fn validate_image_bytes(mime: &str, b: &[u8]) -> Result<(), ApiError> {
    if b.len() > MAX_MEDIA_BYTES {
        return Err(too_large());
    }
    validate_image_container(mime, b)
}

/// Validate a bounded image container without applying the smaller public-input byte cap.
/// Generated provider output has its own, larger byte limits and shares these structural checks.
pub(crate) fn validate_image_container(mime: &str, b: &[u8]) -> Result<(), ApiError> {
    let valid = match mime {
        "image/png" => valid_png(b),
        "image/jpeg" => valid_jpeg(b),
        "image/webp" => valid_webp(b),
        "image/gif" => gif_frame_count(b) == Some(1),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "Image bytes do not match a supported, non-animated image format",
        ))
    }
}

fn sane_dimensions(w: u32, h: u32) -> bool {
    w > 0 && h > 0 && w <= 16_384 && h <= 16_384 && (w as u64) * (h as u64) <= 100_000_000
}

fn valid_png(b: &[u8]) -> bool {
    if b.len() < 8 || &b[..8] != b"\x89PNG\r\n\x1a\n" {
        return false;
    }
    let mut i = 8usize;
    let mut ihdr = false;
    let mut image_data = false;
    while i.checked_add(12).is_some_and(|end| end <= b.len()) {
        let len = u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
        let Some(end) = i.checked_add(12).and_then(|n| n.checked_add(len)) else {
            return false;
        };
        if end > b.len() {
            return false;
        }
        let kind = &b[i + 4..i + 8];
        let data = &b[i + 8..i + 8 + len];
        if !ihdr {
            if kind != b"IHDR" || len != 13 {
                return false;
            }
            let w = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            let h = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
            if !sane_dimensions(w, h) {
                return false;
            }
            ihdr = true;
        }
        if kind == b"acTL" {
            return false;
        }
        if kind == b"IDAT" {
            image_data = len > 0;
        }
        if kind == b"IEND" {
            return len == 0 && image_data && end == b.len();
        }
        i = end;
    }
    false
}

fn valid_jpeg(b: &[u8]) -> bool {
    if b.len() < 4 || b[..2] != [0xff, 0xd8] || b[b.len() - 2..] != [0xff, 0xd9] {
        return false;
    }
    let mut i = 2usize;
    let mut dims = false;
    while i < b.len() - 2 {
        if b[i] != 0xff {
            return false;
        }
        while i < b.len() && b[i] == 0xff {
            i += 1;
        }
        let Some(&marker) = b.get(i) else {
            return false;
        };
        i += 1;
        if marker == 0xd9 {
            return false;
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        if i.checked_add(2).is_none_or(|end| end > b.len()) {
            return false;
        }
        let len = u16::from_be_bytes([b[i], b[i + 1]]) as usize;
        let Some(segment_end) = i.checked_add(len) else {
            return false;
        };
        if len < 2 || segment_end > b.len() - 2 {
            return false;
        }
        if marker == 0xda {
            // Check the SOS segment and require a nonempty scan before the final EOI.
            return dims && len >= 6 && segment_end < b.len() - 2;
        }
        if matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        ) {
            if len < 7 {
                return false;
            }
            let h = u16::from_be_bytes([b[i + 3], b[i + 4]]) as u32;
            let w = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
            if !sane_dimensions(w, h) {
                return false;
            }
            dims = true;
        }
        i += len;
    }
    false
}

fn valid_webp(b: &[u8]) -> bool {
    if b.len() < 20
        || &b[..4] != b"RIFF"
        || &b[8..12] != b"WEBP"
        || u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize != b.len() - 8
    {
        return false;
    }
    let mut i = 12usize;
    let mut image_data = false;
    let mut dimensions = false;
    while i.checked_add(8).is_some_and(|end| end <= b.len()) {
        let size = u32::from_le_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]]) as usize;
        let Some(start) = i.checked_add(8) else {
            return false;
        };
        let Some(end) = start.checked_add(size) else {
            return false;
        };
        if end > b.len() {
            return false;
        }
        match &b[i..i + 4] {
            b"VP8X" => {
                if size != 10 || b[start] & 0x02 != 0 {
                    return false;
                }
                let w = 1 + u32::from_le_bytes([b[start + 4], b[start + 5], b[start + 6], 0])
                    & 0x00ff_ffff;
                let h = 1 + u32::from_le_bytes([b[start + 7], b[start + 8], b[start + 9], 0])
                    & 0x00ff_ffff;
                if !sane_dimensions(w, h) {
                    return false;
                }
                dimensions = true;
            }
            b"ANIM" | b"ANMF" => return false,
            b"VP8 " => {
                if size < 10 || b[start + 3..start + 6] != [0x9d, 0x01, 0x2a] {
                    return false;
                }
                let w = u16::from_le_bytes([b[start + 6], b[start + 7]]) as u32 & 0x3fff;
                let h = u16::from_le_bytes([b[start + 8], b[start + 9]]) as u32 & 0x3fff;
                if !sane_dimensions(w, h) {
                    return false;
                }
                dimensions = true;
                image_data = true;
            }
            b"VP8L" => {
                if size < 5 || b[start] != 0x2f {
                    return false;
                }
                let w = 1 + (b[start + 1] as u32 | ((b[start + 2] as u32 & 0x3f) << 8));
                let h = 1
                    + ((b[start + 2] as u32 >> 6)
                        | ((b[start + 3] as u32) << 2)
                        | ((b[start + 4] as u32 & 0x0f) << 10));
                if !sane_dimensions(w, h) {
                    return false;
                }
                dimensions = true;
                image_data = true;
            }
            _ => {}
        }
        let Some(next) = end.checked_add(size & 1) else {
            return false;
        };
        if next > b.len() {
            return false;
        }
        i = next;
    }
    i == b.len() && image_data && dimensions
}

fn gif_frame_count(b: &[u8]) -> Option<usize> {
    if b.len() < 14 || !(b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        return None;
    }
    let w = u16::from_le_bytes([b[6], b[7]]) as u32;
    let h = u16::from_le_bytes([b[8], b[9]]) as u32;
    if !sane_dimensions(w, h) {
        return None;
    }
    let mut i = 13usize;
    if b[10] & 0x80 != 0 {
        i += 3usize.checked_mul(1usize << ((b[10] & 7) + 1))?;
    }
    let mut frames = 0;
    while i < b.len() {
        match b[i] {
            0x3b => return (i + 1 == b.len()).then_some(frames),
            0x21 => {
                i += 2;
                i = skip_subblocks(b, i)?;
            }
            0x2c => {
                if i + 10 > b.len() {
                    return None;
                }
                let left = u16::from_le_bytes([b[i + 1], b[i + 2]]) as u32;
                let top = u16::from_le_bytes([b[i + 3], b[i + 4]]) as u32;
                let frame_width = u16::from_le_bytes([b[i + 5], b[i + 6]]) as u32;
                let frame_height = u16::from_le_bytes([b[i + 7], b[i + 8]]) as u32;
                if !sane_dimensions(frame_width, frame_height)
                    || left + frame_width > w
                    || top + frame_height > h
                {
                    return None;
                }
                let packed = b[i + 9];
                i += 10;
                if packed & 0x80 != 0 {
                    i += 3usize.checked_mul(1usize << ((packed & 7) + 1))?;
                }
                if i >= b.len() {
                    return None;
                }
                i += 1;
                i = skip_subblocks(b, i)?;
                frames += 1;
                if frames > 1 {
                    return Some(frames);
                }
            }
            _ => return None,
        }
    }
    None
}
fn skip_subblocks(b: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let n = *b.get(i)? as usize;
        i += 1;
        if n == 0 {
            return Some(i);
        }
        i = i.checked_add(n)?;
        if i > b.len() {
            return None;
        }
    }
}
fn https_url(value: &str) -> Result<String, ApiError> {
    let parsed =
        url::Url::parse(value).map_err(|_| ApiError::bad_request("Media URL must be HTTPS"))?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ApiError::bad_request(
            "Media URL must be an absolute HTTPS URL without credentials or fragment",
        ));
    }
    Ok(parsed.to_string())
}
fn safe_filename(name: &str) -> Result<&str, ApiError> {
    if name.is_empty()
        || name.len() > 255
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
    {
        return Err(ApiError::bad_request(
            "filename must be a safe basename up to 255 bytes",
        ));
    }
    Ok(name)
}
fn detail(value: Option<&Value>) -> Result<&'static str, ApiError> {
    match value.and_then(Value::as_str).unwrap_or("auto") {
        "auto" => Ok("auto"),
        "low" => Ok("low"),
        "high" => Ok("high"),
        _ => Err(ApiError::bad_request(
            "image detail must be auto, low, or high",
        )),
    }
}
fn fields(object: &serde_json::Map<String, Value>, allowed: &[&str]) -> Result<(), ApiError> {
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(ApiError::unsupported("content block field"));
    }
    Ok(())
}
fn bounded(text: &str) -> Result<(), ApiError> {
    if text.len() > 1024 * 1024 {
        Err(too_large())
    } else {
        Ok(())
    }
}
fn too_large() -> ApiError {
    ApiError::new(
        413,
        "input_too_large",
        "Media exceeds the 1 MiB input limit",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = (data.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(data);
        v.extend_from_slice(&[0; 4]);
        v
    }
    fn png(animated: bool) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = vec![0; 13];
        ihdr[..4].copy_from_slice(&1u32.to_be_bytes());
        ihdr[4..8].copy_from_slice(&1u32.to_be_bytes());
        b.extend(chunk(b"IHDR", &ihdr));
        if animated {
            b.extend(chunk(b"acTL", &[0; 8]));
        }
        b.extend(chunk(b"IDAT", &[1]));
        b.extend(chunk(b"IEND", &[]));
        b
    }
    #[test]
    fn normalizes_typed_images_and_rejects_bad_inputs() {
        let png = STANDARD.encode(png(false));
        let parts = normalize_content(&json!([{"type":"text","text":"look"},{"type":"image_url","image_url":{"url":format!("data:image/png;base64,{png}"),"detail":"high"}}]), ContentApi::Chat, true).unwrap();
        assert_eq!(parts[1]["type"], "input_image");
        assert_eq!(parts[1]["detail"], "high");
        assert!(
            normalize_content(
                &json!([{"type":"image_url","image_url":{"url":"data:image/png;base64,%%%"}}]),
                ContentApi::Chat,
                true
            )
            .is_err()
        );
        assert!(
            normalize_content(
                &json!([{"type":"image_url","image_url":{"url":"http://example.test/a.png"}}]),
                ContentApi::Chat,
                true
            )
            .is_err()
        );
        assert!(
            normalize_content(
                &json!([{"type":"image_url","image_url":{"url":"/tmp/a.png"}}]),
                ContentApi::Chat,
                true
            )
            .is_err()
        );
        assert!(normalize_content(
            &json!([{"type":"image_url","image_url":{"url":"https://img.example/a.png","detail":"ultra"}}]),
            ContentApi::Chat, true
        ).is_err());
        assert!(normalize_content(
            &json!([{"type":"image_url","image_url":{"url":"https://img.example/a.png","unexpected":true}}]),
            ContentApi::Chat, true
        ).is_err());
        let repeated = Value::Array(
            (0..=MAX_IMAGES)
                .map(|_| {
                    json!({
                        "type":"input_image","image_url":"https://img.example/a.png"
                    })
                })
                .collect(),
        );
        assert!(normalize_content(&repeated, ContentApi::Responses, true).is_err());
    }
    #[test]
    fn normalizes_openai_pdf_and_rejects_ids_paths_and_non_pdf() {
        let data = format!(
            "data:application/pdf;base64,{}",
            STANDARD.encode(b"%PDF-1.7\nsynthetic\n%%EOF")
        );
        let file = normalize_content(
            &json!([{"type":"input_file","file_data":data,"filename":"paper.pdf"}]),
            ContentApi::Responses,
            true,
        )
        .unwrap();
        assert_eq!(file[0]["filename"], "paper.pdf");
        for bad in [
            json!([{"type":"input_file","file_id":"file_abc"}]),
            json!([{"type":"input_file","file_url":"/tmp/a.pdf"}]),
            json!([{"type":"input_file","file_url":"https://example.test/a.pdf","filename":"a.pdf"}]),
        ] {
            assert!(normalize_content(&bad, ContentApi::Responses, true).is_err());
        }
    }

    #[test]
    fn validates_bounded_image_containers_and_animation_flags() {
        assert!(validate_image_bytes("image/png", &png(false)).is_ok());
        let mut animated_png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = vec![0; 13];
        ihdr[..4].copy_from_slice(&1u32.to_be_bytes());
        ihdr[4..8].copy_from_slice(&1u32.to_be_bytes());
        animated_png.extend(chunk(b"IHDR", &ihdr));
        animated_png.extend(chunk(b"tEXt", b"acTL is ordinary text"));
        animated_png.extend(chunk(b"IDAT", &[1]));
        animated_png.extend(chunk(b"IEND", &[]));
        assert!(validate_image_bytes("image/png", &animated_png).is_ok());
        assert!(validate_image_bytes("image/png", &png(true)).is_err());
        assert!(validate_image_bytes("image/png", b"\x89PNG\r\n\x1a\nIHDR").is_err());
        assert!(validate_image_bytes("image/jpeg", &[0xff, 0xd8, 0xff, 0xd9]).is_err());
        // Container validation is independent from the stricter public input cap.
        let mut large_png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = vec![0; 13];
        ihdr[..4].copy_from_slice(&1u32.to_be_bytes());
        ihdr[4..8].copy_from_slice(&1u32.to_be_bytes());
        large_png.extend(chunk(b"IHDR", &ihdr));
        large_png.extend(chunk(b"IDAT", &vec![0; MAX_MEDIA_BYTES]));
        large_png.extend(chunk(b"IEND", &[]));
        assert!(large_png.len() > MAX_MEDIA_BYTES);
        assert!(validate_image_container("image/png", &large_png).is_ok());
        assert!(validate_image_bytes("image/png", &large_png).is_err());
        assert!(validate_image_bytes("image/webp", b"RIFF\x0c\x00\x00\x00WEBPVP8X\x02").is_err());
        let gif =
            b"GIF89a\x01\x00\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x00;";
        assert!(validate_image_bytes("image/gif", gif).is_ok());
        let two_frames = b"GIF89a\x01\x00\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x00;";
        assert!(validate_image_bytes("image/gif", two_frames).is_err());
        assert!(validate_image_bytes("image/jpeg", &png(false)).is_err());
    }

    #[test]
    fn accepts_chat_pdf_and_responses_text_documents_with_safe_names() {
        let pdf = format!(
            "data:application/pdf;base64,{}",
            STANDARD.encode(b"%PDF-1.7\nsynthetic\n%%EOF")
        );
        let chat = normalize_content(
            &json!([{"type":"file","file":{"filename":"a.pdf","file_data":pdf}}]),
            ContentApi::Chat,
            true,
        )
        .unwrap();
        assert_eq!(chat[0]["type"], "input_file");
        let text_data = format!("data:text/plain;base64,{}", STANDARD.encode(b"hello"));
        assert!(
            normalize_content(
                &json!([{"type":"file","file":{"filename":"a.txt","file_data":text_data}}]),
                ContentApi::Chat,
                true
            )
            .is_err()
        );
        assert!(
            normalize_content(
                &json!([{"type":"input_file","file_url":"https://files.example.test/a.pdf"}]),
                ContentApi::Responses,
                true
            )
            .is_ok()
        );
        for (mime, name, bytes) in [
            ("text/plain", "notes.txt", &b"hello"[..]),
            ("text/csv", "rows.csv", &b"a,b\n1,2"[..]),
            ("application/json", "data.json", &b"{\"ok\":true}"[..]),
        ] {
            let data = format!("data:{mime};base64,{}", STANDARD.encode(bytes));
            assert!(
                normalize_content(
                    &json!([{"type":"input_file","filename":name,"file_data":data}]),
                    ContentApi::Responses,
                    true
                )
                .is_ok()
            );
        }
        let mut zip = b"PK\x03\x04".to_vec();
        zip.extend([0u8; 20]);
        zip.extend(b"PK\x05\x06");
        zip.extend([0u8; 18]);
        let docx = format!(
            "data:application/vnd.openxmlformats-officedocument.wordprocessingml.document;base64,{}",
            STANDARD.encode(zip)
        );
        assert!(
            normalize_content(
                &json!([{"type":"input_file","filename":"doc.docx","file_data":docx}]),
                ContentApi::Responses,
                true
            )
            .is_ok()
        );
        assert!(
            normalize_content(
                &json!([{"type":"input_file","filename":"../a.pdf","file_data":pdf}]),
                ContentApi::Responses,
                true
            )
            .is_err()
        );
        let mismatch = format!("data:image/png;base64,{}", STANDARD.encode(png(false)));
        assert!(
            normalize_content(
                &json!([{"type":"input_file","filename":"a.pdf","file_data":mismatch}]),
                ContentApi::Responses,
                true
            )
            .is_err()
        );
        for bad in [
            "http://example.test/a.pdf",
            "https://user@example.test/a.pdf",
            "/tmp/a.pdf",
            "file_123",
        ] {
            assert!(https_url(bad).is_err());
        }
    }

    #[test]
    fn text_only_content_keeps_newline_joining() {
        let input = json!([{"type":"text","text":"a"},{"type":"text","text":"b"}]);
        let canonical = normalize_content(&input, ContentApi::Chat, true).unwrap();
        assert_eq!(openai_content(&canonical).unwrap(), "a\nb");
    }
}
