use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    env,
    net::{IpAddr, SocketAddr},
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeyConfig {
    pub id: String,
    pub sha256: String,
    pub scopes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: String,
    pub state_dir: PathBuf,
    pub antigravity_enabled: bool,
    pub openai_enabled: bool,
    pub openai_state_dir: PathBuf,
    pub openai_max_concurrent: usize,
    pub openai_max_queue: usize,
    pub telemetry_dir: PathBuf,
    pub monitor_socket: PathBuf,
    pub keys: Vec<KeyConfig>,
    pub trusted_proxies: Vec<IpAddr>,
    pub max_body_bytes: usize,
    pub max_output_bytes: usize,
    pub max_concurrent: usize,
    pub max_queue: usize,
    pub request_timeout_secs: u64,
    pub preauth_per_minute: u32,
    pub global_preauth_per_minute: u32,
    pub per_key_per_minute: u32,
    pub retention_days: u64,
    pub max_events: usize,
    pub model_fallback: bool,
    /// Hand noncompliant calls to non-strict tools back to the client
    /// (AI_ROUTER_TOOL_CALL_RELAY, default true) instead of failing closed.
    pub tool_call_relay: bool,
    pub fallback_openai_model: String,
    pub fallback_gemini_model: String,
}

impl Config {
    /// Read explicit process environment only. This intentionally never loads files.
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| match env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(env::VarError::NotPresent) => Ok(None),
            Err(_) => Err(format!("{name} must be valid UTF-8")),
        })
    }

    fn from_lookup(
        mut lookup: impl FnMut(&str) -> Result<Option<String>, String>,
    ) -> Result<Self, String> {
        let mut value = |name: &str, fallback: &str| -> Result<String, String> {
            Ok(lookup(name)?.unwrap_or_else(|| fallback.to_owned()))
        };
        let bind = value("AI_ROUTER_BIND", "0.0.0.0:8080")?;
        let state_dir = value("AI_ROUTER_STATE_DIR", "/var/lib/ai-router/antigravity-auth")?.into();
        let antigravity_enabled = match value("AI_ROUTER_ANTIGRAVITY_ENABLED", "true")?.as_str() {
            "true" => true,
            "false" => false,
            _ => return Err("AI_ROUTER_ANTIGRAVITY_ENABLED must be true or false".to_owned()),
        };
        let openai_enabled = match value("AI_ROUTER_OPENAI_ENABLED", "false")?.as_str() {
            "true" => true,
            "false" => false,
            _ => return Err("AI_ROUTER_OPENAI_ENABLED must be true or false".to_owned()),
        };
        let openai_state_dir = value(
            "AI_ROUTER_OPENAI_STATE_DIR",
            "/var/lib/ai-router/openai-auth",
        )?
        .into();
        let model_fallback = match value("AI_ROUTER_MODEL_FALLBACK", "true")?.as_str() {
            "true" => true,
            "false" => false,
            _ => return Err("AI_ROUTER_MODEL_FALLBACK must be true or false".to_owned()),
        };
        let tool_call_relay = match value("AI_ROUTER_TOOL_CALL_RELAY", "true")?.as_str() {
            "true" => true,
            "false" => false,
            _ => return Err("AI_ROUTER_TOOL_CALL_RELAY must be true or false".to_owned()),
        };
        let fallback_openai_model = value("AI_ROUTER_FALLBACK_OPENAI_MODEL", "gpt-6-luna")?;
        let fallback_gemini_model =
            value("AI_ROUTER_FALLBACK_GEMINI_MODEL", "gemini-3.8-flash-medium")?;
        let telemetry_dir =
            value("AI_ROUTER_TELEMETRY_DIR", "/var/lib/ai-router/telemetry")?.into();
        let monitor_socket =
            value("AI_ROUTER_MONITOR_SOCKET", "/run/ai-router/monitor.sock")?.into();
        let keys: Vec<KeyConfig> =
            serde_json::from_str(&value("AI_ROUTER_KEYS", "[]")?).map_err(|_| {
                "AI_ROUTER_KEYS must be a JSON array of id, sha256, and scopes records".to_owned()
            })?;
        let proxy_text = value("AI_ROUTER_TRUSTED_PROXIES", "")?;
        let trusted_proxies = if proxy_text.trim().is_empty() {
            Vec::new()
        } else {
            proxy_text
                .split(',')
                .map(|ip| {
                    ip.trim().parse::<IpAddr>().map_err(|_| {
                        "AI_ROUTER_TRUSTED_PROXIES must contain comma-separated IP addresses"
                            .to_owned()
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        macro_rules! number {
            ($name:literal, $default:literal) => {
                value($name, $default)?
                    .parse()
                    .map_err(|_| format!("{} must be an unsigned integer", $name))?
            };
        }
        let config = Self {
            bind,
            state_dir,
            antigravity_enabled,
            openai_enabled,
            openai_state_dir,
            openai_max_concurrent: number!("AI_ROUTER_OPENAI_MAX_CONCURRENT", "2"),
            openai_max_queue: number!("AI_ROUTER_OPENAI_MAX_QUEUE", "8"),
            telemetry_dir,
            monitor_socket,
            keys,
            trusted_proxies,
            max_body_bytes: number!("AI_ROUTER_MAX_BODY_BYTES", "1048576"),
            max_output_bytes: number!("AI_ROUTER_MAX_OUTPUT_BYTES", "8388608"),
            max_concurrent: number!("AI_ROUTER_MAX_CONCURRENT", "2"),
            max_queue: number!("AI_ROUTER_MAX_QUEUE", "8"),
            request_timeout_secs: number!("AI_ROUTER_REQUEST_TIMEOUT_SECS", "120"),
            preauth_per_minute: number!("AI_ROUTER_PREAUTH_PER_MINUTE", "60"),
            global_preauth_per_minute: number!("AI_ROUTER_GLOBAL_PREAUTH_PER_MINUTE", "600"),
            per_key_per_minute: number!("AI_ROUTER_PER_KEY_PER_MINUTE", "30"),
            retention_days: number!("AI_ROUTER_RETENTION_DAYS", "30"),
            max_events: number!("AI_ROUTER_MAX_EVENTS", "50000"),
            model_fallback,
            tool_call_relay,
            fallback_openai_model,
            fallback_gemini_model,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        self.bind
            .parse::<SocketAddr>()
            .map_err(|_| "AI_ROUTER_BIND must be an IP address and port".to_owned())?;
        for (name, path) in [
            ("STATE_DIR", &self.state_dir),
            ("OPENAI_STATE_DIR", &self.openai_state_dir),
            ("TELEMETRY_DIR", &self.telemetry_dir),
            ("MONITOR_SOCKET", &self.monitor_socket),
        ] {
            validate_path(name, path)?;
        }
        let storage = [&self.state_dir, &self.openai_state_dir, &self.telemetry_dir];
        for (i, path) in storage.iter().enumerate() {
            for other in storage.iter().skip(i + 1) {
                if path.starts_with(other) || other.starts_with(path) {
                    return Err("Antigravity authentication, OpenAI authentication, and telemetry paths must be separate directories".to_owned());
                }
            }
            if self.monitor_socket.starts_with(path) {
                return Err("Monitor socket must be outside Antigravity authentication, OpenAI authentication, and telemetry directories".to_owned());
            }
        }
        if self.keys.is_empty() || self.keys.len() > 1000 {
            return Err(
                "Configure between 1 and 1000 client digest records in AI_ROUTER_KEYS".to_owned(),
            );
        }
        let mut ids = HashSet::new();
        let mut digests = HashSet::new();
        for key in &self.keys {
            validate_key(key)?;
            if !ids.insert(&key.id) || !digests.insert(key.sha256.to_ascii_lowercase()) {
                return Err("Client key IDs and digests must be unique".to_owned());
            }
        }
        if self.trusted_proxies.len() > 64
            || self.trusted_proxies.iter().collect::<HashSet<_>>().len()
                != self.trusted_proxies.len()
        {
            return Err("Configure no more than 64 unique trusted proxy IPs".to_owned());
        }
        bounds(
            "MAX_BODY_BYTES",
            self.max_body_bytes as u64,
            1,
            16 * 1024 * 1024,
        )?;
        bounds(
            "MAX_OUTPUT_BYTES",
            self.max_output_bytes as u64,
            1024,
            64 * 1024 * 1024,
        )?;
        bounds("MAX_CONCURRENT", self.max_concurrent as u64, 1, 64)?;
        bounds("MAX_QUEUE", self.max_queue as u64, 0, 1024)?;
        bounds(
            "OPENAI_MAX_CONCURRENT",
            self.openai_max_concurrent as u64,
            1,
            64,
        )?;
        bounds("OPENAI_MAX_QUEUE", self.openai_max_queue as u64, 0, 1024)?;
        bounds("REQUEST_TIMEOUT_SECS", self.request_timeout_secs, 1, 3600)?;
        bounds(
            "PREAUTH_PER_MINUTE",
            self.preauth_per_minute as u64,
            1,
            1_000_000,
        )?;
        bounds(
            "GLOBAL_PREAUTH_PER_MINUTE",
            self.global_preauth_per_minute as u64,
            1,
            1_000_000,
        )?;
        bounds(
            "PER_KEY_PER_MINUTE",
            self.per_key_per_minute as u64,
            1,
            1_000_000,
        )?;
        bounds("RETENTION_DAYS", self.retention_days, 1, 365)?;
        bounds("MAX_EVENTS", self.max_events as u64, 1, 50_000)?;
        for (name, model) in [
            ("FALLBACK_OPENAI_MODEL", &self.fallback_openai_model),
            ("FALLBACK_GEMINI_MODEL", &self.fallback_gemini_model),
        ] {
            if !valid_model_id(model) {
                return Err(format!(
                    "AI_ROUTER_{name} must contain 1–128 ASCII letters, digits, dots, colons, underscores, or hyphens"
                ));
            }
        }
        Ok(())
    }

    pub fn for_test(root: &Path) -> Self {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(b"test-key");
        Self {
            bind: "127.0.0.1:0".to_owned(),
            state_dir: root.join("antigravity-auth"),
            antigravity_enabled: true,
            openai_enabled: false,
            openai_state_dir: root.join("openai-auth"),
            openai_max_concurrent: 2,
            openai_max_queue: 8,
            telemetry_dir: root.join("telemetry"),
            monitor_socket: root.join("monitor.sock"),
            keys: vec![KeyConfig {
                id: "test".to_owned(),
                sha256: encode_hex(&digest),
                scopes: vec!["model".to_owned()],
            }],
            trusted_proxies: Vec::new(),
            max_body_bytes: 1048576,
            max_output_bytes: 8388608,
            max_concurrent: 2,
            max_queue: 8,
            request_timeout_secs: 120,
            preauth_per_minute: 60,
            global_preauth_per_minute: 600,
            per_key_per_minute: 30,
            retention_days: 30,
            max_events: 50000,
            model_fallback: true,
            tool_call_relay: true,
            fallback_openai_model: "gpt-6-luna".to_owned(),
            fallback_gemini_model: "gemini-3.8-flash-medium".to_owned(),
        }
    }
}

pub fn validate_key(key: &KeyConfig) -> Result<(), String> {
    if !valid_identifier(&key.id) {
        return Err(
            "Client IDs must contain 1–64 ASCII letters, digits, underscores, or hyphens"
                .to_owned(),
        );
    }
    if key.sha256.len() != 64 || !key.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Client sha256 must contain exactly 64 hexadecimal characters".to_owned());
    }
    if key.scopes.is_empty()
        || key.scopes.len() > 1
        || key.scopes.iter().any(|s| s != "model")
        || key.scopes.iter().collect::<HashSet<_>>().len() != key.scopes.len()
    {
        return Err("Client scopes must contain only the model entry".to_owned());
    }
    Ok(())
}

pub fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Fallback targets are echoed in headers and capabilities, so they share the
/// catalogue's plain-identifier shape rather than accepting arbitrary text.
pub fn valid_model_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b':' | b'_' | b'-'))
}

pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|b| {
            [
                DIGITS[(b >> 4) as usize] as char,
                DIGITS[(b & 15) as usize] as char,
            ]
        })
        .collect()
}

fn bounds(name: &str, value: u64, min: u64, max: u64) -> Result<(), String> {
    if value < min || value > max {
        Err(format!("AI_ROUTER_{name} must be between {min} and {max}"))
    } else {
        Ok(())
    }
}

fn validate_path(name: &str, path: &Path) -> Result<(), String> {
    if !path.is_absolute()
        || path == Path::new("/")
        || path.components().any(|c| matches!(c, Component::ParentDir))
    {
        return Err(format!(
            "AI_ROUTER_{name} must be an absolute, non-root path without parent traversal"
        ));
    }
    for component in path.components() {
        if let Component::Normal(component) = component {
            let Some(name) = component.to_str() else {
                return Err("Storage paths must be UTF-8".to_owned());
            };
            if name == "secrets"
                || name == "credentials"
                || name.starts_with(".env")
                || name.ends_with(".pem")
                || name.ends_with(".key")
            {
                return Err(
                    "Configured paths must not use protected secret file names or directories"
                        .to_owned(),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config_with(name: &str, value: &str) -> Result<Config, String> {
        let key_json =
            serde_json::to_string(&Config::for_test(Path::new("/tmp/router-test")).keys).unwrap();
        Config::from_lookup(|n| {
            Ok(if n == name {
                Some(value.to_owned())
            } else if n == "AI_ROUTER_KEYS" {
                Some(key_json.clone())
            } else {
                None
            })
        })
    }
    #[test]
    fn defaults_have_bounded_resources_and_require_digests() {
        assert!(Config::from_lookup(|_| Ok(None)).is_err());
        let config = config_with("AI_ROUTER_ANTIGRAVITY_ENABLED", "true").unwrap();
        assert_eq!(config.max_queue, 8);
        assert!(!config.openai_enabled);
        assert_eq!(
            config.state_dir,
            Path::new("/var/lib/ai-router/antigravity-auth")
        );
        assert_eq!(
            config.openai_state_dir,
            Path::new("/var/lib/ai-router/openai-auth")
        );
        assert_eq!(config.openai_max_concurrent, 2);
        assert_eq!(config.openai_max_queue, 8);
        assert!(config.antigravity_enabled);
        assert!(
            !config_with("AI_ROUTER_ANTIGRAVITY_ENABLED", "false")
                .unwrap()
                .antigravity_enabled
        );
        assert_eq!(config.keys[0].id, "test");
        assert!(config.model_fallback);
        assert_eq!(config.fallback_openai_model, "gpt-6-luna");
        assert_eq!(config.fallback_gemini_model, "gemini-3.8-flash-medium");
        assert!(
            !config_with("AI_ROUTER_MODEL_FALLBACK", "false")
                .unwrap()
                .model_fallback
        );
        assert!(config.tool_call_relay);
        assert!(
            !config_with("AI_ROUTER_TOOL_CALL_RELAY", "false")
                .unwrap()
                .tool_call_relay
        );
    }
    #[test]
    fn rejects_invalid_paths_limits_and_public_key_metadata() {
        for (name, value) in [
            ("AI_ROUTER_STATE_DIR", "/tmp/../auth"),
            ("AI_ROUTER_STATE_DIR", "/tmp/credentials/auth"),
            ("AI_ROUTER_OPENAI_ENABLED", "yes"),
            ("AI_ROUTER_OPENAI_STATE_DIR", "/tmp/auth/../openai"),
            (
                "AI_ROUTER_OPENAI_STATE_DIR",
                "/var/lib/ai-router/antigravity-auth/openai",
            ),
            ("AI_ROUTER_OPENAI_MAX_CONCURRENT", "0"),
            ("AI_ROUTER_OPENAI_MAX_QUEUE", "99999999"),
            ("AI_ROUTER_MAX_CONCURRENT", "0"),
            ("AI_ROUTER_MAX_QUEUE", "99999999"),
            ("AI_ROUTER_ANTIGRAVITY_ENABLED", "yes"),
            ("AI_ROUTER_TRUSTED_PROXIES", "10.0.0.0/8"),
            ("AI_ROUTER_MODEL_FALLBACK", "yes"),
            ("AI_ROUTER_TOOL_CALL_RELAY", "yes"),
            ("AI_ROUTER_FALLBACK_OPENAI_MODEL", ""),
            ("AI_ROUTER_FALLBACK_OPENAI_MODEL", "gpt 6"),
            ("AI_ROUTER_FALLBACK_GEMINI_MODEL", "gemini/../x"),
            ("AI_ROUTER_FALLBACK_GEMINI_MODEL", "g".repeat(129).as_str()),
        ] {
            assert!(config_with(name, value).is_err(), "{name}");
        }
        let mut config = Config::for_test(Path::new("/tmp/router-test"));
        config.keys.push(config.keys[0].clone());
        assert!(config.validate().is_err());
        config.keys.pop();
        config.keys[0].scopes = vec!["admin".to_owned()];
        assert!(config.validate().is_err());
        config.keys[0].scopes = vec!["native".to_owned()];
        assert!(config.validate().is_err());
    }
    #[test]
    fn rejects_overlapping_storage_and_duplicate_proxies() {
        let mut config = Config::for_test(Path::new("/tmp/router-test"));
        config.openai_state_dir = config.state_dir.join("openai");
        assert!(config.validate().is_err());
        config.openai_state_dir = PathBuf::from("/tmp/router-test/openai-auth");
        config.trusted_proxies = vec!["127.0.0.1".parse().unwrap(); 2];
        assert!(config.validate().is_err());
    }
}
