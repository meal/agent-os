use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(String);

impl TaskId {
    pub fn new() -> TaskId {
        TaskId(uuid::Uuid::now_v7().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for TaskId {
    fn default() -> Self {
        TaskId::new()
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid digest: {0}")]
pub struct DigestError(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest([u8; 32]);

impl Digest {
    pub fn of(bytes: &[u8]) -> Digest {
        Digest(*blake3::hash(bytes).as_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn from_hex(s: &str) -> Result<Digest, DigestError> {
        let h = blake3::Hash::from_hex(s).map_err(|e| DigestError(e.to_string()))?;
        Ok(Digest(*h.as_bytes()))
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&blake3::Hash::from_bytes(self.0).to_hex())
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Digest, D::Error> {
        let s = String::deserialize(d)?;
        Digest::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_is_unique_uuid_v7() {
        let (a, b) = (TaskId::new(), TaskId::new());
        assert_ne!(a, b);
        let u = uuid::Uuid::parse_str(a.as_str()).unwrap();
        assert_eq!(u.get_version_num(), 7);
        assert_eq!(a.to_string(), a.as_str());
    }

    #[test]
    fn digest_of_is_deterministic_and_input_sensitive() {
        assert_eq!(Digest::of(b"a"), Digest::of(b"a"));
        assert_ne!(Digest::of(b"a"), Digest::of(b"b"));
        assert_eq!(Digest::of(b"a").to_string().len(), 64);
    }

    #[test]
    fn digest_hex_round_trip() {
        let d = Digest::of(b"hello");
        let hex = d.to_string();
        assert_eq!(hex, hex.to_lowercase());
        assert_eq!(Digest::from_hex(&hex).unwrap(), d);
        assert!(Digest::from_hex("zz").is_err());
        assert!(Digest::from_hex(&hex[..62]).is_err());
    }

    #[test]
    fn digest_serde_is_hex_string() {
        let d = Digest::of(b"hello");
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, format!("\"{d}\""));
        assert_eq!(serde_json::from_str::<Digest>(&json).unwrap(), d);
        assert!(serde_json::from_str::<Digest>("\"nothex\"").is_err());
    }
}
