//! 内容摘要：Hotpot 一切跨机复用都以内容哈希为身份。

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// 内容寻址摘要。当前算法为 BLAKE3；序列化形态为 64 位十六进制字符串。
///
/// 注意：Hotpot 自有存储使用 BLAKE3；协议兼容层（sccache / Turborepo /
/// REAPI）的摘要由各自协议定义，本类型不与它们混用。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentDigest(#[serde(with = "hex_bytes")] [u8; 32]);

impl ContentDigest {
    /// 计算一段字节的摘要。
    pub fn of_bytes(data: &[u8]) -> Self {
        ContentDigest(blake3::hash(data).into())
    }

    /// 返回流式摘要器，用于大文件增量喂入。
    pub fn hasher() -> Hasher {
        Hasher(blake3::Hasher::new())
    }

    /// 计算文件内容的摘要。
    pub fn of_file(path: impl AsRef<Path>) -> std::io::Result<Self> {
        use std::io::Read;

        let mut file = std::fs::File::open(path)?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(ContentDigest(*hasher.finalize().as_bytes()))
    }

    /// 以 64 位十六进制字符串返回。
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// 解析十六进制字符串。
    pub fn from_hex(s: &str) -> Result<Self, hex::FromHexError> {
        let mut out = [0u8; 32];
        hex::decode_to_slice(s, &mut out)?;
        Ok(ContentDigest(out))
    }

    /// 原始字节。
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl fmt::Debug for ContentDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentDigest({})", self.to_hex())
    }
}

/// 流式内容摘要器。
pub struct Hasher(blake3::Hasher);

impl Hasher {
    /// 喂入数据。
    pub fn update(&mut self, data: &[u8]) -> &mut Self {
        self.0.update(data);
        self
    }

    /// 结束并返回摘要。
    pub fn finalize(&self) -> ContentDigest {
        ContentDigest(*self.0.finalize().as_bytes())
    }
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let text = String::deserialize(d)?;
        let mut out = [0u8; 32];
        hex::decode_to_slice(&text, &mut out).map_err(serde::de::Error::custom)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let d = ContentDigest::of_bytes(b"hotpot");
        let s = d.to_hex();
        assert_eq!(ContentDigest::from_hex(&s).unwrap(), d);
        serde_json::from_str::<ContentDigest>(&format!("\"{s}\"")).unwrap();
    }

    #[test]
    fn streaming_matches_one_shot() {
        let mut h = ContentDigest::hasher();
        h.update(b"hot").update(b"pot");
        assert_eq!(h.finalize(), ContentDigest::of_bytes(b"hotpot"));
    }
}
