//! Allocation-free container preflight before either endpoint materializes typed data.
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use std::fmt;

pub const MAX_PATH_BYTES: usize = 16 * 1024;
pub const MAX_IDENTIFIER_BYTES: usize = 1024;
pub const MAX_ERROR_BYTES: usize = 16 * 1024;
// A full 16 MiB textual diff fits; small-element arrays cannot amplify into
// millions of Rust String/Vec headers. The whole document also has a node cap.
pub const MAX_COLLECTION_ITEMS: usize = 262_144;
const MAX_NODES: usize = 2 * 1024 * 1024;

pub fn decode_json<T: de::DeserializeOwned>(bytes: &[u8]) -> serde_json::Result<T> {
    if bytes.len() > crate::MAX_FRAME_BYTES {
        return Err(<serde_json::Error as de::Error>::custom(
            "Git JSON exceeds frame limit",
        ));
    }
    validate_json(bytes)?;
    serde_json::from_slice(bytes)
}

pub(crate) fn validate_json(bytes: &[u8]) -> serde_json::Result<()> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let mut nodes = 0;
    Check {
        nodes: &mut nodes,
        limit: crate::MAX_FRAME_BYTES,
    }
    .deserialize(&mut deserializer)?;
    deserializer.end()
}

struct Check<'a> {
    nodes: &'a mut usize,
    limit: usize,
}
impl<'de> DeserializeSeed<'de> for Check<'_> {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        *self.nodes += 1;
        if *self.nodes > MAX_NODES {
            return Err(de::Error::custom("Git JSON node limit exceeded"));
        }
        deserializer.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Check<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded Git JSON")
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: de::Error>(self, value: u64) -> Result<(), E> {
        if self.limit == MAX_COLLECTION_ITEMS && value > MAX_COLLECTION_ITEMS as u64 {
            return Err(E::custom("Git requested collection limit exceeded"));
        }
        Ok(())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: de::Error>(self, text: &str) -> Result<(), E> {
        if text.len() > self.limit {
            return Err(E::custom("Git JSON field length exceeded"));
        }
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let mut count = 0;
        while seq
            .next_element_seed(Check {
                nodes: self.nodes,
                limit: self.limit,
            })?
            .is_some()
        {
            count += 1;
            if count > MAX_COLLECTION_ITEMS {
                return Err(de::Error::custom("Git JSON collection limit exceeded"));
            }
        }
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut count = 0;
        while let Some(key) = map.next_key_seed(Key)? {
            count += 1;
            if count > MAX_COLLECTION_ITEMS {
                return Err(de::Error::custom("Git JSON object limit exceeded"));
            }
            // Error envelopes contain a message; commit messages are bounded by
            // the frame instead. Carry only the error-envelope constraint down.
            let limit = if self.limit == MAX_ERROR_BYTES && key == crate::MAX_FRAME_BYTES {
                MAX_ERROR_BYTES
            } else {
                key
            };
            map.next_value_seed(Check {
                nodes: self.nodes,
                limit,
            })?;
        }
        Ok(())
    }
}
struct Key;
impl<'de> DeserializeSeed<'de> for Key {
    type Value = usize;
    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<usize, D::Error> {
        struct KeyVisitor;
        impl Visitor<'_> for KeyVisitor {
            type Value = usize;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("bounded field name")
            }
            fn visit_str<E: de::Error>(self, key: &str) -> Result<usize, E> {
                if key.len() > MAX_IDENTIFIER_BYTES {
                    return Err(E::custom("Git JSON field name too long"));
                }
                Ok(match key {
                    "limit" | "recentLimit" => MAX_COLLECTION_ITEMS,
                    "message" | "outcomeUnknown" | "service" | "worktreeInUse"
                    | "worktreeLeftover" => crate::MAX_FRAME_BYTES,
                    "Ok" => crate::MAX_FRAME_BYTES,
                    "Err" => MAX_ERROR_BYTES,
                    "repo" | "path" | "paths" | "file" | "filePath" | "oldPath" | "fileRel"
                    | "absFile" | "worktreePath" => MAX_PATH_BYTES,
                    "code" | "hash" | "hashes" | "expectedHash" | "expectedHead" | "stashHash"
                    | "branch" | "remoteBranch" | "remote" | "name" | "old" | "onto" | "target"
                    | "start" | "base" | "loaded" | "frontier" => MAX_IDENTIFIER_BYTES,
                    "busy" | "detail" => MAX_ERROR_BYTES,
                    _ => crate::MAX_FRAME_BYTES,
                })
            }
        }
        deserializer.deserialize_str(KeyVisitor)
    }
}
