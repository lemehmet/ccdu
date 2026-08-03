//! Lossless serialisation for paths.
//!
//! Unix paths are bytes, not text, and `serde`'s own `PathBuf` support refuses anything that is
//! not UTF-8. For a tool that deletes files that is not an acceptable failure mode: a plan must be
//! able to name every file it can see, and a name must survive the round trip unchanged.
//!
//! Ordinary paths serialise as plain strings, so plan files stay readable. Anything that is not
//! valid UTF-8 becomes `{"hex": "..."}` instead.

use std::fmt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use serde::de::{Error, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserializer, Serializer};

pub fn serialize<S: Serializer>(path: &Path, s: S) -> Result<S::Ok, S::Error> {
    match path.to_str() {
        Some(text) => s.serialize_str(text),
        None => {
            let mut map = s.serialize_map(Some(1))?;
            map.serialize_entry("hex", &to_hex(path.as_os_str().as_bytes()))?;
            map.end()
        }
    }
}

pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
    d.deserialize_any(PathVisitor)
}

struct PathVisitor;

impl<'de> Visitor<'de> for PathVisitor {
    type Value = PathBuf;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a path string, or a map with a \"hex\" key for non-UTF-8 paths")
    }

    fn visit_str<E: Error>(self, v: &str) -> Result<PathBuf, E> {
        Ok(PathBuf::from(v))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<PathBuf, A::Error> {
        let mut bytes = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "hex" => {
                    let text: String = map.next_value()?;
                    bytes = Some(from_hex(&text).ok_or_else(|| {
                        A::Error::custom(format!("malformed hex path: {text:?}"))
                    })?);
                }
                other => return Err(A::Error::unknown_field(other, &["hex"])),
            }
        }
        let bytes = bytes.ok_or_else(|| A::Error::missing_field("hex"))?;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    out
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            Some((hi * 16 + lo) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
    struct Holder(#[serde(with = "super")] PathBuf);

    fn round_trip(path: PathBuf) -> (String, PathBuf) {
        let json = serde_json::to_string(&Holder(path)).unwrap();
        let back: Holder = serde_json::from_str(&json).unwrap();
        (json, back.0)
    }

    #[test]
    fn ordinary_paths_stay_readable() {
        let (json, back) = round_trip(PathBuf::from("/data/some file.txt"));
        assert_eq!(json, r#""/data/some file.txt""#);
        assert_eq!(back, PathBuf::from("/data/some file.txt"));
    }

    #[test]
    fn non_utf8_paths_survive_the_round_trip() {
        // A lone 0xFF byte: a perfectly legal filename that is not valid UTF-8.
        let raw = std::ffi::OsString::from_vec(b"/data/broken\xffname".to_vec());
        let path = PathBuf::from(raw);
        let (json, back) = round_trip(path.clone());

        assert!(json.contains("hex"), "{json}");
        assert_eq!(back, path, "a name we cannot print must still be a name we can delete");
        assert_eq!(back.as_os_str().as_bytes(), b"/data/broken\xffname");
    }

    #[test]
    fn malformed_hex_is_rejected_rather_than_guessed() {
        assert!(serde_json::from_str::<Holder>(r#"{"hex":"zz"}"#).is_err());
        assert!(serde_json::from_str::<Holder>(r#"{"hex":"abc"}"#).is_err());
        assert!(serde_json::from_str::<Holder>(r#"{"other":"2f"}"#).is_err());
    }

    #[test]
    fn hex_helpers_are_inverses() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        assert_eq!(from_hex(&to_hex(&bytes)).unwrap(), bytes);
    }
}
