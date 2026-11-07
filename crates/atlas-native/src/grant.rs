// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Write grants: the credential that lets a client write to a data node.
//!
//! Reading a data node needs only the cluster's client certificate (or nothing, without TLS).
//! A node that holds a [`GrantKey`] additionally refuses every write that does not carry a
//! [`Grant`]: an HMAC-SHA256 tag, made with a secret shared only by the metadata leaders and the
//! data nodes, over exactly what the write may do (node, metadata group, device, fence, offset
//! or append, length, expiry). A client holding only read access cannot make one. The leader
//! hands a client one grant per replica it leased, valid for seconds, for one range, one length
//! and one fence; the client can write those bytes and nothing else.
//!
//! Expiry is wall-clock milliseconds, so leaders and data nodes need roughly synchronized clocks
//! (the grant lifetime is tens of seconds).

use ring::hmac;
use serde::{Deserialize, Serialize};

use crate::engine::{now_ms, NativeError};

/// Shortest accepted secret, in bytes.
pub const MIN_KEY_BYTES: usize = 16;
/// Lifetime of the grants an engine mints for its own writes.
const ENGINE_GRANT_MS: u64 = 60_000;

/// The shared secret behind grants and append receipts.
#[derive(Clone)]
pub struct GrantKey(hmac::Key);

impl std::fmt::Debug for GrantKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GrantKey(..)")
    }
}

/// Permission for one write to one data node; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// Unique per grant (the lease and replica it was minted for); receipts are bound to it.
    pub id: String,
    /// The data node it is for; empty for a grant an engine mints for its own write.
    #[serde(default)]
    pub node: String,
    pub fence: u64,
    pub group: u32,
    pub device: usize,
    /// The offset it may write; `None` allows an append.
    pub offset: Option<u64>,
    pub len: u64,
    /// Wall-clock expiry (ms since the Unix epoch).
    pub exp_ms: u64,
    /// Hex HMAC-SHA256 tag over every other field.
    pub mac: String,
}

pub(crate) fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

impl Grant {
    fn message(&self) -> String {
        let offset = self
            .offset
            .map_or_else(|| "append".to_string(), |o| o.to_string());
        format!(
            "atlas-native-grant-v1|{}|{}|{}|{}|{}|{}|{}|{}",
            self.id, self.node, self.fence, self.group, self.device, offset, self.len, self.exp_ms
        )
    }
}

impl GrantKey {
    pub fn new(secret: &[u8]) -> Result<Self, NativeError> {
        if secret.len() < MIN_KEY_BYTES {
            return Err(NativeError::Invalid(format!(
                "the write-grant key must be at least {MIN_KEY_BYTES} bytes"
            )));
        }
        Ok(Self(hmac::Key::new(hmac::HMAC_SHA256, secret)))
    }

    /// Mints a grant for one write.
    #[allow(clippy::too_many_arguments)]
    pub fn mint(
        &self,
        id: impl Into<String>,
        node: impl Into<String>,
        fence: u64,
        group: u32,
        device: usize,
        offset: Option<u64>,
        len: u64,
        exp_ms: u64,
    ) -> Grant {
        let mut g = Grant {
            id: id.into(),
            node: node.into(),
            fence,
            group,
            device,
            offset,
            len,
            exp_ms,
            mac: String::new(),
        };
        g.mac = to_hex(hmac::sign(&self.0, g.message().as_bytes()).as_ref());
        g
    }

    /// A short-lived grant for a write an engine makes itself (it holds the key anyway).
    pub(crate) fn mint_own(
        &self,
        fence: u64,
        group: u32,
        device: usize,
        offset: Option<u64>,
        len: u64,
    ) -> Grant {
        self.mint(
            "engine",
            "",
            fence,
            group,
            device,
            offset,
            len,
            now_ms().saturating_add(ENGINE_GRANT_MS),
        )
    }

    /// Whether `g` was minted with this key, has not expired, and describes exactly this write.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn authorize(
        &self,
        g: &Grant,
        node: &str,
        fence: u64,
        group: u32,
        device: usize,
        offset: Option<u64>,
        len: u64,
    ) -> Result<(), String> {
        let tag = from_hex(&g.mac).ok_or("malformed write grant")?;
        hmac::verify(&self.0, g.message().as_bytes(), &tag)
            .map_err(|_| "invalid write grant".to_string())?;
        if g.exp_ms < now_ms() {
            return Err("write grant expired".into());
        }
        if !g.node.is_empty() && g.node != node {
            return Err("write grant is for another data node".into());
        }
        if (g.fence, g.group, g.device, g.offset, g.len) != (fence, group, device, offset, len) {
            return Err("write does not match its grant".into());
        }
        Ok(())
    }

    /// Proof from the node that a granted append landed at `offset`.
    pub(crate) fn receipt(&self, grant_id: &str, offset: u64) -> String {
        to_hex(
            hmac::sign(
                &self.0,
                format!("atlas-native-receipt-v1|{grant_id}|{offset}").as_bytes(),
            )
            .as_ref(),
        )
    }

    /// Whether `receipt` is the node's proof for grant `grant_id` appending at `offset`.
    pub(crate) fn check_receipt(&self, grant_id: &str, offset: u64, receipt: &str) -> bool {
        from_hex(receipt).is_some_and(|tag| {
            hmac::verify(
                &self.0,
                format!("atlas-native-receipt-v1|{grant_id}|{offset}").as_bytes(),
                &tag,
            )
            .is_ok()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> GrantKey {
        GrantKey::new(b"0123456789abcdef0123").unwrap()
    }

    #[test]
    fn a_grant_authorizes_exactly_its_write() {
        let k = key();
        let g = k.mint("g1", "n1", 7, 0, 1, Some(4096), 8192, now_ms() + 10_000);
        k.authorize(&g, "n1", 7, 0, 1, Some(4096), 8192).unwrap();
        for (node, fence, group, device, offset, len) in [
            ("n2", 7, 0, 1, Some(4096), 8192),
            ("n1", 8, 0, 1, Some(4096), 8192),
            ("n1", 7, 1, 1, Some(4096), 8192),
            ("n1", 7, 0, 0, Some(4096), 8192),
            ("n1", 7, 0, 1, Some(0), 8192),
            ("n1", 7, 0, 1, None, 8192),
            ("n1", 7, 0, 1, Some(4096), 4096),
        ] {
            assert!(
                k.authorize(&g, node, fence, group, device, offset, len)
                    .is_err(),
                "{node} {fence} {group} {device} {offset:?} {len}"
            );
        }
    }

    #[test]
    fn forged_expired_and_foreign_grants_are_refused() {
        let k = key();
        let mut g = k.mint("g1", "n1", 1, 0, 0, None, 10, now_ms() + 10_000);
        g.len = 11;
        assert!(
            k.authorize(&g, "n1", 1, 0, 0, None, 11).is_err(),
            "edited field"
        );
        let old = k.mint("g1", "n1", 1, 0, 0, None, 10, now_ms().saturating_sub(1));
        let err = k.authorize(&old, "n1", 1, 0, 0, None, 10).unwrap_err();
        assert!(err.contains("expired"), "{err}");
        let other = GrantKey::new(b"another-secret-of-16+").unwrap();
        let g = other.mint("g1", "n1", 1, 0, 0, None, 10, now_ms() + 10_000);
        assert!(
            k.authorize(&g, "n1", 1, 0, 0, None, 10).is_err(),
            "other key"
        );
        assert!(GrantKey::new(b"short").is_err());
    }

    #[test]
    fn receipts_bind_the_grant_and_offset() {
        let k = key();
        let r = k.receipt("g1", 4096);
        assert!(k.check_receipt("g1", 4096, &r));
        assert!(!k.check_receipt("g2", 4096, &r));
        assert!(!k.check_receipt("g1", 0, &r));
        assert!(!k.check_receipt("g1", 4096, "zz"));
    }
}
