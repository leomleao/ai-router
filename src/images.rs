//! OpenAI compatible image request normalization for the Antigravity image route.
use crate::{media, protocol::ApiError};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

pub const MAX_IMAGE_COUNT: usize = 4;
pub const MAX_GENERATED_IMAGE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_GENERATED_TOTAL_BYTES: usize = 12 * 1024 * 1024;
const MAX_EDIT_TOTAL_BYTES: usize = 1024 * 1024;

const ASPECT_RATIOS: &[&str] = &[
    "1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9",
];
const IMAGE_SIZES: &[&str] = &["1K", "2K", "4K"];

#[derive(Clone, Debug)]
pub struct ReferenceImage {
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

impl ReferenceImage {
    /// Construct from media bytes that have already passed the shared media validator.
    pub fn from_validated_bytes(mime_type: &str, bytes: Vec<u8>) -> Result<Self, ApiError> {
        if !matches!(
            mime_type,
            "image/png" | "image/jpeg" | "image/webp" | "image/gif"
        ) {
            return Err(ApiError::unsupported("image media type"));
        }
        if bytes.is_empty() || bytes.len() > media::MAX_MEDIA_BYTES {
            return Err(ApiError::new(
                413,
                "media_too_large",
                "Image input exceeds the media size limit",
            ));
        }
        media::validate_image_bytes(mime_type, &bytes)?;
        Ok(Self {
            mime_type: mime_type.to_owned(),
            bytes,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ImageRequest {
    pub model: String,
    pub prompt: String,
    pub count: usize,
    pub aspect_ratio: Option<String>,
    pub image_size: Option<String>,
    pub reference_images: Vec<ReferenceImage>,
}

impl ImageRequest {
    /// Normalize the supported OpenAI `/v1/images/generations` JSON fields.
    pub fn from_generation_json(value: &Value) -> Result<Self, ApiError> {
        let object = value
            .as_object()
            .ok_or_else(|| ApiError::bad_request("Image request must be a JSON object"))?;
        for key in object.keys() {
            if !matches!(
                key.as_str(),
                "model" | "prompt" | "n" | "size" | "response_format"
            ) {
                return Err(ApiError::unsupported(key));
            }
        }
        let model = required_string(object.get("model"), "model", 128)?;
        if !crate::config::valid_model_id(&model) {
            return Err(ApiError::bad_request("Invalid image model identifier").with_param("model"));
        }
        let prompt = required_string(object.get("prompt"), "prompt", 64 * 1024)?;
        if prompt.trim().is_empty() {
            return Err(ApiError::bad_request("prompt must not be empty").with_param("prompt"));
        }
        let count = match object.get("n") {
            None => 1,
            Some(value) => value
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| (1..=MAX_IMAGE_COUNT).contains(n))
                .ok_or_else(|| {
                    ApiError::bad_request("n must be between 1 and 4").with_param("n")
                })?,
        };
        if let Some(format) = object.get("response_format") {
            if format.as_str() != Some("b64_json") {
                return Err(
                    ApiError::bad_request("Only response_format=b64_json is supported")
                        .with_param("response_format"),
                );
            }
        }
        let (aspect_ratio, image_size) = match object.get("size") {
            None => (None, None),
            Some(value) => {
                let size = value.as_str().ok_or_else(|| {
                    ApiError::bad_request("size must be a reviewed image ratio or resolution")
                        .with_param("size")
                })?;
                map_size(size).ok_or_else(|| {
                    ApiError::bad_request("size is not a supported image ratio or resolution")
                        .with_param("size")
                })?
            }
        };
        Ok(Self {
            model,
            prompt,
            count,
            aspect_ratio,
            image_size,
            reference_images: Vec::new(),
        })
    }

    /// Build an OpenAI `/v1/images/edits` request from already validated reference bytes.
    pub fn for_edit(
        model: String,
        prompt: String,
        reference_images: Vec<ReferenceImage>,
    ) -> Result<Self, ApiError> {
        if !crate::config::valid_model_id(&model) {
            return Err(ApiError::bad_request("Invalid image model").with_param("model"));
        }
        if prompt.trim().is_empty()
            || prompt.len() > 64 * 1024
            || prompt
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
            return Err(ApiError::bad_request("Invalid image prompt").with_param("prompt"));
        }
        if reference_images.is_empty() || reference_images.len() > media::MAX_IMAGES {
            return Err(ApiError::bad_request(
                "Image edits require between 1 and 8 reference images",
            )
            .with_param("image"));
        }
        if reference_images
            .iter()
            .any(|image| image.bytes.is_empty() || image.bytes.len() > media::MAX_MEDIA_BYTES)
            || reference_images
                .iter()
                .map(|image| image.bytes.len())
                .sum::<usize>()
                > MAX_EDIT_TOTAL_BYTES
        {
            return Err(ApiError::new(
                413,
                "media_too_large",
                "Image input exceeds the media size limit",
            ));
        }
        Ok(Self {
            model,
            prompt,
            count: 1,
            aspect_ratio: None,
            image_size: None,
            reference_images,
        })
    }

    pub(crate) fn wire_body(&self, project: &str, model: &str) -> Value {
        let mut parts: Vec<Value> = self.reference_images.iter().map(|image| json!({"inlineData":{"mimeType":image.mime_type,"data":STANDARD.encode(&image.bytes)}})).collect();
        parts.push(json!({"text":self.prompt}));
        let mut generation_config =
            json!({"responseModalities":["IMAGE"],"candidateCount":self.count});
        if self.aspect_ratio.is_some() || self.image_size.is_some() {
            let mut config = serde_json::Map::new();
            if let Some(ratio) = &self.aspect_ratio {
                config.insert("aspectRatio".into(), json!(ratio));
            }
            if let Some(size) = &self.image_size {
                config.insert("imageSize".into(), json!(size));
            }
            generation_config["imageConfig"] = Value::Object(config);
        }
        json!({"project":project,"model":model,"request":{"contents":[{"role":"user","parts":parts}],"systemInstruction":{"parts":[{"text":"You are an AI image generator. Generate images based on user descriptions. Focus on creating high-quality, visually appealing images that match the user's request."}]},"generationConfig":generation_config},"requestId":format!("agent-{}-{}",chrono::Utc::now().timestamp_millis(),&uuid::Uuid::new_v4().simple().to_string()[..9]),"userAgent":"antigravity"})
    }
}

/// Resolution aliases map to the provider's supported ratio and size controls;
/// the provider does not guarantee that returned images have these exact pixels.
fn map_size(size: &str) -> Option<(Option<String>, Option<String>)> {
    match size {
        "auto" => Some((None, None)),
        "1024x1024" => Some((Some("1:1".to_owned()), Some("1K".to_owned()))),
        "1536x1024" => Some((Some("3:2".to_owned()), Some("1K".to_owned()))),
        "1024x1536" => Some((Some("2:3".to_owned()), Some("1K".to_owned()))),
        _ if ASPECT_RATIOS.contains(&size) => Some((Some(size.to_owned()), None)),
        _ if IMAGE_SIZES.contains(&size) => Some((None, Some(size.to_owned()))),
        _ => None,
    }
}

pub(crate) fn image_response(images: &[crate::antigravity_images::GeneratedImage]) -> Value {
    json!({"created":chrono::Utc::now().timestamp(),"data":images.iter().map(|image| json!({"b64_json":image.b64_json})).collect::<Vec<_>>()})
}

fn required_string(
    value: Option<&Value>,
    field: &'static str,
    max: usize,
) -> Result<String, ApiError> {
    let string = value
        .and_then(Value::as_str)
        .filter(|s| {
            !s.is_empty()
                && s.len() <= max
                && !s
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        })
        .ok_or_else(|| {
            ApiError::bad_request(format!(
                "{field} must be a non-empty string of at most {max} bytes"
            ))
            .with_param(field)
        })?;
    Ok(string.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generation_request_accepts_only_reviewed_fields_and_formats() {
        let request = ImageRequest::from_generation_json(&json!({"model":"fixture-image","prompt":"a synthetic\nlighthouse","n":2,"size":"16:9","response_format":"b64_json"})).unwrap();
        assert_eq!(request.prompt, "a synthetic\nlighthouse");
        assert_eq!(request.count, 2);
        assert_eq!(request.aspect_ratio.as_deref(), Some("16:9"));
        let body = request.wire_body("fixture-project", &request.model);
        assert_eq!(
            body["request"]["generationConfig"]["responseModalities"],
            json!(["IMAGE"])
        );
        assert!(body["request"].get("safetySettings").is_none());
        assert!(
            ImageRequest::from_generation_json(
                &json!({"model":"fixture","prompt":"x","response_format":"url"})
            )
            .is_err()
        );
        assert!(
            ImageRequest::from_generation_json(&json!({"model":"fixture/image","prompt":"x"}))
                .is_err()
        );
        for (size, ratio) in [
            ("1024x1024", "1:1"),
            ("1536x1024", "3:2"),
            ("1024x1536", "2:3"),
        ] {
            let request = ImageRequest::from_generation_json(
                &json!({"model":"fixture","prompt":"x","size":size}),
            )
            .unwrap();
            assert_eq!(request.aspect_ratio.as_deref(), Some(ratio));
            assert_eq!(request.image_size.as_deref(), Some("1K"));
        }
        let auto = ImageRequest::from_generation_json(
            &json!({"model":"fixture","prompt":"x","size":"auto"}),
        )
        .unwrap();
        assert!(auto.aspect_ratio.is_none() && auto.image_size.is_none());
        assert!(
            ImageRequest::from_generation_json(
                &json!({"model":"fixture","prompt":"x","size":"999x999"})
            )
            .is_err()
        );
        assert!(
            ImageRequest::from_generation_json(
                &json!({"model":"fixture","prompt":"x","extra":"ignored?"})
            )
            .is_err()
        );
    }
    #[test]
    fn edits_require_validated_bounded_reference_images() {
        let image = ReferenceImage::from_validated_bytes(
            "image/png",
            STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/T9sAAAAASUVORK5CYII=").unwrap(),
        )
        .unwrap();
        let request =
            ImageRequest::for_edit("fixture-image".into(), "edit this".into(), vec![image])
                .unwrap();
        let body = request.wire_body("fixture-project", &request.model);
        assert_eq!(
            body["request"]["contents"][0]["parts"][0]["inlineData"]["mimeType"],
            "image/png"
        );
        assert!(ReferenceImage::from_validated_bytes("text/plain", b"synthetic".to_vec()).is_err());
        assert!(
            ImageRequest::for_edit("fixture-image".into(), "edit this".into(), vec![]).is_err()
        );
        assert!(
            ReferenceImage::from_validated_bytes("image/png", vec![0; media::MAX_MEDIA_BYTES + 1])
                .is_err()
        );
    }
}
