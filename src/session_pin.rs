//! In-memory conversation pins. Memory only: a restart or eviction loses every pin and the
//! router then refuses a stateful continuation rather than guessing where it began.
//!
//! Identity is never derived from conversation content: only an explicit client-supplied
//! session identifier can create or address a pin.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub(crate) const MAX_PINS: usize = 4096;
pub(crate) const MAX_PINS_PER_CREDENTIAL: usize = 512;
pub(crate) const PIN_TTL: Duration = Duration::from_secs(4 * 3600);

pub(crate) type PinKey = [u8; 32];

struct Pin {
    target: String,
    owner: [u8; 8],
    last_used: Instant,
}

/// Bounded total size, bounded per credential, sliding TTL. Never persisted or shared.
pub(crate) struct PinStore {
    pins: HashMap<PinKey, Pin>,
    max: usize,
    per_owner: usize,
    ttl: Duration,
}

impl Default for PinStore {
    fn default() -> Self {
        Self::new(MAX_PINS, MAX_PINS_PER_CREDENTIAL, PIN_TTL)
    }
}

pub(crate) fn owner_id(credential: &str) -> [u8; 8] {
    let digest = Sha256::digest(credential.as_bytes());
    digest[..8].try_into().expect("8 bytes")
}

impl PinStore {
    pub(crate) fn new(max: usize, per_owner: usize, ttl: Duration) -> Self {
        Self {
            pins: HashMap::new(),
            max: max.max(1),
            per_owner: per_owner.max(1),
            ttl,
        }
    }
    /// Returns the pinned target and refreshes its idle clock; expired pins are removed.
    pub(crate) fn get(&mut self, key: &PinKey, now: Instant) -> Option<String> {
        let ttl = self.ttl;
        match self.pins.get_mut(key) {
            Some(pin) if now.saturating_duration_since(pin.last_used) <= ttl => {
                pin.last_used = now;
                Some(pin.target.clone())
            }
            Some(_) => {
                self.pins.remove(key);
                None
            }
            None => None,
        }
    }
    fn evict_oldest(&mut self, owner: Option<[u8; 8]>) {
        if let Some(oldest) = self
            .pins
            .iter()
            .filter(|(_, pin)| owner.is_none_or(|o| pin.owner == o))
            .min_by_key(|(_, pin)| pin.last_used)
            .map(|(key, _)| *key)
        {
            self.pins.remove(&oldest);
        }
    }
    /// First placement wins: an existing live pin is never overwritten. Returns whether a
    /// pin was created. A credential at its own cap evicts only its own oldest pin; at the
    /// global cap the oldest pin of the credential holding the most pins is evicted.
    /// This bounds one credential's pressure; it does not reserve capacity for others.
    pub(crate) fn put_if_absent(
        &mut self,
        key: PinKey,
        owner: [u8; 8],
        target: &str,
        now: Instant,
    ) -> bool {
        if self.get(&key, now).is_some() {
            return false;
        }
        let ttl = self.ttl;
        if self.pins.len() >= self.max {
            self.pins
                .retain(|_, pin| now.saturating_duration_since(pin.last_used) <= ttl);
        }
        if self.pins.values().filter(|pin| pin.owner == owner).count() >= self.per_owner {
            self.evict_oldest(Some(owner));
        }
        if self.pins.len() >= self.max {
            let mut counts: HashMap<[u8; 8], usize> = HashMap::new();
            for pin in self.pins.values() {
                *counts.entry(pin.owner).or_default() += 1;
            }
            let heaviest = counts.into_iter().max_by_key(|(_, n)| *n).map(|(o, _)| o);
            self.evict_oldest(heaviest);
        }
        self.pins.insert(
            key,
            Pin {
                target: target.to_owned(),
                owner,
                last_used: now,
            },
        );
        true
    }
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.pins.len()
    }
}

/// The pin is scoped to the authenticated credential and the requested policy alias, so a
/// session identifier supplied by a client can never address another credential's pin.
pub(crate) fn pin_key(credential: &str, requested: &str, session: &str) -> PinKey {
    let mut digest = Sha256::new();
    for part in [credential, requested, session] {
        digest.update((part.len() as u64).to_le_bytes());
        digest.update(part.as_bytes());
    }
    digest.finalize().into()
}

/// Short non-reversible correlation id for audit records (never the key itself).
pub(crate) fn short_id(key: &PinKey) -> String {
    key[..6].iter().map(|b| format!("{b:02x}")).collect()
}

/// An explicit client-supplied conversation identity: `prompt_cache_key`, else
/// `client_metadata.session_id` / `thread_id` (the Codex CLI sends both). Content is never
/// used, so two conversations can only share a pin if the client says they are the same.
pub(crate) fn session_identity(request: &Value) -> Option<String> {
    let text = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 512)
            .map(str::to_owned)
    };
    if let Some(id) = text(request.get("prompt_cache_key")) {
        return Some(format!("pck:{id}"));
    }
    let meta = request.get("client_metadata")?;
    ["session_id", "thread_id"]
        .iter()
        .find_map(|field| text(meta.get(*field)))
        .map(|id| format!("cm:{id}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const O1: [u8; 8] = [1; 8];
    const O2: [u8; 8] = [2; 8];

    #[test]
    fn pins_expire_evict_never_overwrite_and_never_cross_credentials() {
        let now = Instant::now();
        let mut store = PinStore::new(2, 2, Duration::from_secs(10));
        let (a, b, c) = (
            pin_key("k1", "smart", "s1"),
            pin_key("k1", "smart", "s2"),
            pin_key("k1", "smart", "s3"),
        );
        assert_ne!(a, pin_key("k2", "smart", "s1"));
        assert_ne!(a, pin_key("k1", "other", "s1"));
        assert!(store.put_if_absent(a, O1, "local", now));
        // First placement wins: a later re-decision cannot overwrite it.
        assert!(!store.put_if_absent(a, O1, "cloud", now + Duration::from_secs(1)));
        assert_eq!(
            store.get(&a, now + Duration::from_secs(1)).as_deref(),
            Some("local")
        );
        assert!(store.put_if_absent(b, O1, "cloud", now + Duration::from_secs(2)));
        // Per-credential cap: its own least recently used pin (a) is evicted.
        assert!(store.put_if_absent(c, O1, "local", now + Duration::from_secs(3)));
        assert_eq!(store.len(), 2);
        assert!(store.get(&a, now + Duration::from_secs(3)).is_none());
        // TTL is sliding and strict.
        assert!(store.get(&c, now + Duration::from_secs(12)).is_some());
        assert!(store.get(&c, now + Duration::from_secs(30)).is_none());
        // An expired pin may be re-created.
        assert!(store.put_if_absent(c, O1, "cloud", now + Duration::from_secs(31)));
    }

    #[test]
    fn one_credential_cannot_evict_another_credentials_pins() {
        let now = Instant::now();
        let mut store = PinStore::new(4, 4, Duration::from_secs(600));
        let victim = pin_key("victim", "p", "s");
        assert!(store.put_if_absent(victim, O2, "local", now));
        for i in 0..20 {
            let key = pin_key("noisy", "p", &format!("s{i}"));
            store.put_if_absent(key, O1, "local", now + Duration::from_secs(i + 1));
        }
        assert!(store.len() <= 4);
        assert_eq!(
            store.get(&victim, now + Duration::from_secs(30)).as_deref(),
            Some("local")
        );
    }

    #[test]
    fn identity_is_explicit_only_and_never_derived_from_content() {
        let id = |v: Value| session_identity(&v);
        assert_eq!(
            id(json!({"prompt_cache_key":"abc"})).as_deref(),
            Some("pck:abc")
        );
        assert_eq!(
            id(json!({"client_metadata":{"session_id":"s"}})).as_deref(),
            Some("cm:s")
        );
        assert_eq!(
            id(json!({"client_metadata":{"thread_id":"t"}})).as_deref(),
            Some("cm:t")
        );
        for none in [
            json!({"input":[{"type":"message","role":"user","content":"hello"}]}),
            json!({"messages":[{"role":"user","content":"hello"}]}),
            json!({"prompt_cache_key":"x".repeat(600)}),
            json!({"prompt_cache_key":""}),
            json!({"client_metadata":{"session_id":7}}),
        ] {
            assert_eq!(id(none.clone()), None, "{none}");
        }
    }
}
