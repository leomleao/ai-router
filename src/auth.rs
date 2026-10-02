use crate::config::{KeyConfig, encode_hex};
use rand::{RngCore, rngs::OsRng};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

/// 512 bits of operating-system entropy encoded as unpadded base64url.
/// The returned config contains only the digest; callers must show the key once.
pub fn generate_key(id: &str, scopes: Vec<String>) -> (String, KeyConfig) {
    let mut random = [0u8; 64];
    OsRng.fill_bytes(&mut random);
    let key = base64url(&random);
    let config = KeyConfig {
        id: id.to_owned(),
        sha256: encode_hex(&Sha256::digest(key.as_bytes())),
        scopes,
    };
    (key, config)
}

/// Compare every configured digest, without returning early on a match.
pub fn authenticate(keys: &[KeyConfig], bearer: &str) -> Option<KeyConfig> {
    if bearer.is_empty() || bearer.len() > 256 {
        return None;
    }
    let presented = Sha256::digest(bearer.as_bytes());
    let mut matched = None;
    for key in keys {
        let Some(expected) = decode_digest(&key.sha256) else {
            continue;
        };
        if bool::from(presented.as_slice().ct_eq(&expected)) {
            matched = Some(key.clone());
        }
    }
    matched
}

fn decode_digest(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut result = [0u8; 32];
    for (index, pair) in text.as_bytes().chunks_exact(2).enumerate() {
        let digit = |b: u8| -> Option<u8> {
            match b {
                b'0'..=b'9' => Some(b - b'0'),
                b'a'..=b'f' => Some(b - b'a' + 10),
                b'A'..=b'F' => Some(b - b'A' + 10),
                _ => None,
            }
        };
        result[index] = digit(pair[0])? * 16 + digit(pair[1])?;
    }
    Some(result)
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::with_capacity((bytes.len() * 4).div_ceil(3));
    for part in bytes.chunks(3) {
        let bits = (part[0] as u32) << 16
            | (part.get(1).copied().unwrap_or(0) as u32) << 8
            | part.get(2).copied().unwrap_or(0) as u32;
        output.push(ALPHABET[((bits >> 18) & 63) as usize] as char);
        output.push(ALPHABET[((bits >> 12) & 63) as usize] as char);
        if part.len() > 1 {
            output.push(ALPHABET[((bits >> 6) & 63) as usize] as char);
        }
        if part.len() > 2 {
            output.push(ALPHABET[(bits & 63) as usize] as char);
        }
    }
    output
}

#[derive(Debug)]
struct Window {
    started: Instant,
    count: u32,
}

/// Bounded fixed-minute admission gate. Full tables deny new identities instead
/// of evicting active ones, which would let attackers reset their limits.
#[derive(Debug)]
pub struct RateLimiter {
    limit: u32,
    max_entries: usize,
    windows: Mutex<HashMap<String, Window>>,
}

impl RateLimiter {
    pub fn new(limit: u32, max_entries: usize) -> Self {
        Self {
            limit,
            max_entries,
            windows: Mutex::new(HashMap::new()),
        }
    }
    pub fn check(&self, key: &str) -> bool {
        self.check_at(key, Instant::now())
    }
    fn check_at(&self, key: &str, now: Instant) -> bool {
        if key.is_empty() || key.len() > 256 || self.limit == 0 || self.max_entries == 0 {
            return false;
        }
        let Ok(mut windows) = self.windows.lock() else {
            return false;
        };
        let period = Duration::from_secs(60);
        if !windows.contains_key(key) && windows.len() >= self.max_entries {
            windows.retain(|_, window| now.saturating_duration_since(window.started) < period);
            if windows.len() >= self.max_entries {
                return false;
            }
        }
        let window = windows.entry(key.to_owned()).or_insert(Window {
            started: now,
            count: 0,
        });
        if now.saturating_duration_since(window.started) >= period {
            window.started = now;
            window.count = 0;
        }
        if window.count >= self.limit {
            return false;
        }
        window.count += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_keys_are_unique_512_bit_url_tokens_with_digest_only_records() {
        let (first, config) = generate_key("n8n", vec!["model".to_owned()]);
        let (second, _) = generate_key("n8n", vec!["model".to_owned()]);
        assert_eq!(first.len(), 86);
        assert_ne!(first, second);
        assert!(
            first
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert!(!serde_json::to_string(&config).unwrap().contains(&first));
        assert_eq!(authenticate(&[config.clone()], &first), Some(config));
        assert!(authenticate(&[], &first).is_none());
    }
    #[test]
    fn rejects_invalid_digests_and_wrong_keys() {
        let (_, mut config) = generate_key("test", vec!["model".to_owned()]);
        assert!(authenticate(&[config.clone()], "incorrect").is_none());
        config.sha256 = "invalid".to_owned();
        assert!(authenticate(&[config], "incorrect").is_none());
        assert!(authenticate(&[], &"a".repeat(257)).is_none());
    }
    #[test]
    fn rate_limit_remains_bounded_and_expires_without_eviction_bypass() {
        let limiter = RateLimiter::new(2, 2);
        let now = Instant::now();
        assert!(limiter.check_at("one", now));
        assert!(limiter.check_at("one", now));
        assert!(!limiter.check_at("one", now));
        assert!(limiter.check_at("two", now));
        assert!(!limiter.check_at("three", now));
        assert!(!limiter.check_at("one", now));
        assert!(limiter.check_at("three", now + Duration::from_secs(60)));
        assert!(limiter.windows.lock().unwrap().len() <= 2);
    }
    #[test]
    fn concurrency_cannot_overshoot_the_limit() {
        let limiter = std::sync::Arc::new(RateLimiter::new(5, 1));
        let joins: Vec<_> = (0..50)
            .map(|_| {
                let limiter = limiter.clone();
                std::thread::spawn(move || limiter.check("same"))
            })
            .collect();
        assert_eq!(
            joins
                .into_iter()
                .map(|j| j.join().unwrap())
                .filter(|allowed| *allowed)
                .count(),
            5
        );
    }
}
