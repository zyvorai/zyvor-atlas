// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! POSIX ACLs in the Linux xattr encoding (`system.posix_acl_access` / `_default`): a 4-byte
//! version (2) and 8-byte entries `(tag: u16, perm: u16, id: u32)`, little-endian. The kernel
//! enforces them on a FUSE mount that negotiates `FUSE_POSIX_ACL`; the filesystem keeps the mode
//! and the access ACL in sync and applies default-ACL inheritance, which is what this module does.

use crate::metadata::MetaError;

pub const ACCESS: &str = "system.posix_acl_access";
pub const DEFAULT: &str = "system.posix_acl_default";

const VERSION: u32 = 2;
const USER_OBJ: u16 = 0x01;
const USER: u16 = 0x02;
const GROUP_OBJ: u16 = 0x04;
const GROUP: u16 = 0x08;
const MASK: u16 = 0x10;
const OTHER: u16 = 0x20;
const UNDEFINED_ID: u32 = u32::MAX;
/// More entries than any real ACL needs (ext4 fits ~500 in a block); bounds parsing work.
const MAX_ENTRIES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    tag: u16,
    perm: u16,
    id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acl(Vec<Entry>);

fn invalid(why: &str) -> MetaError {
    MetaError::Invalid(format!("invalid POSIX ACL: {why}"))
}

impl Acl {
    /// Parses and validates the xattr encoding: one owner, owning group and other entry, a mask
    /// whenever there are named entries, entries in tag then id order without duplicates.
    pub fn parse(value: &[u8]) -> Result<Self, MetaError> {
        if value.len() < 4 || !(value.len() - 4).is_multiple_of(8) {
            return Err(invalid("bad length"));
        }
        if u32::from_le_bytes(value[..4].try_into().unwrap()) != VERSION {
            return Err(invalid("unsupported version"));
        }
        let n = (value.len() - 4) / 8;
        if n > MAX_ENTRIES {
            return Err(invalid("too many entries"));
        }
        let entries: Vec<Entry> = value[4..]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| Entry {
                tag: u16::from_le_bytes([c[0], c[1]]),
                perm: u16::from_le_bytes([c[2], c[3]]),
                id: u32::from_le_bytes([c[4], c[5], c[6], c[7]]),
            })
            .collect();
        let mut prev: Option<(u16, u32)> = None;
        let (mut user_obj, mut group_obj, mut other, mut mask, mut named) = (0, 0, 0, 0, 0);
        for e in &entries {
            if e.perm & !7 != 0 {
                return Err(invalid("permission bits other than rwx"));
            }
            match e.tag {
                USER_OBJ => user_obj += 1,
                GROUP_OBJ => group_obj += 1,
                OTHER => other += 1,
                MASK => mask += 1,
                USER | GROUP => {
                    if e.id == UNDEFINED_ID {
                        return Err(invalid("named entry without an id"));
                    }
                    named += 1;
                }
                _ => return Err(invalid("unknown tag")),
            }
            let key = (
                e.tag,
                if matches!(e.tag, USER | GROUP) {
                    e.id
                } else {
                    0
                },
            );
            if prev.is_some_and(|p| p >= key) {
                return Err(invalid("entries out of order or duplicated"));
            }
            prev = Some(key);
        }
        if (user_obj, group_obj, other) != (1, 1, 1) || mask > 1 || (named > 0 && mask == 0) {
            return Err(invalid(
                "needs exactly one owner, group and other entry, and a mask with named entries",
            ));
        }
        Ok(Self(entries))
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + 8 * self.0.len());
        out.extend_from_slice(&VERSION.to_le_bytes());
        for e in &self.0 {
            out.extend_from_slice(&e.tag.to_le_bytes());
            out.extend_from_slice(&e.perm.to_le_bytes());
            out.extend_from_slice(&e.id.to_le_bytes());
        }
        out
    }

    fn perm(&self, tag: u16) -> Option<u32> {
        self.0
            .iter()
            .find(|e| e.tag == tag)
            .map(|e| u32::from(e.perm))
    }

    fn set_perm(&mut self, tag: u16, perm: u32) {
        if let Some(e) = self.0.iter_mut().find(|e| e.tag == tag) {
            e.perm = (perm & 7) as u16;
        }
    }

    /// The tag whose permissions the mode's group bits mirror: the mask if there is one.
    fn group_tag(&self) -> u16 {
        if self.perm(MASK).is_some() {
            MASK
        } else {
            GROUP_OBJ
        }
    }

    /// Whether the ACL says no more than the mode bits (owner, group, other only).
    pub fn is_minimal(&self) -> bool {
        self.0.len() == 3
    }

    /// `mode` with its permission bits taken from the ACL (setuid/setgid/sticky kept).
    pub fn apply_to_mode(&self, mode: u32) -> u32 {
        let p = |tag| self.perm(tag).unwrap_or(0);
        (mode & !0o777) | (p(USER_OBJ) << 6) | (p(self.group_tag()) << 3) | p(OTHER)
    }

    /// `chmod`: the owner, group (mask) and other entries take the new mode's bits.
    pub fn chmod(&mut self, mode: u32) {
        self.set_perm(USER_OBJ, mode >> 6);
        self.set_perm(self.group_tag(), mode >> 3);
        self.set_perm(OTHER, mode);
    }

    /// Creation under a directory with this default ACL: the new node's access ACL is the
    /// default with the requested mode's bits ANDed into owner, group (mask) and other, and the
    /// mode follows the result (the umask is not applied). As `posix_acl_create_masq`.
    pub fn create_masq(&self, mode: u32) -> (Self, u32) {
        let mut acl = self.clone();
        let g = acl.group_tag();
        for (tag, shift) in [(USER_OBJ, 6), (g, 3), (OTHER, 0)] {
            let have = acl.perm(tag).unwrap_or(0);
            acl.set_perm(tag, have & (mode >> shift));
        }
        let mode = acl.apply_to_mode(mode);
        (acl, mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acl(entries: &[(u16, u16, u32)]) -> Vec<u8> {
        Acl(entries
            .iter()
            .map(|&(tag, perm, id)| Entry { tag, perm, id })
            .collect())
        .encode()
    }

    const U: u32 = UNDEFINED_ID;

    #[test]
    fn parses_and_validates_the_xattr_encoding() {
        let full = acl(&[
            (USER_OBJ, 7, U),
            (USER, 6, 1000),
            (GROUP_OBJ, 5, U),
            (GROUP, 4, 50),
            (MASK, 6, U),
            (OTHER, 0, U),
        ]);
        let a = Acl::parse(&full).unwrap();
        assert_eq!(a.encode(), full);
        assert!(!a.is_minimal());
        // Group bits mirror the mask, not the owning group.
        assert_eq!(a.apply_to_mode(0o4000), 0o4760);

        let minimal = Acl::parse(&acl(&[(USER_OBJ, 6, U), (GROUP_OBJ, 4, U), (OTHER, 4, U)]));
        assert!(minimal.unwrap().is_minimal());

        for bad in [
            Vec::new(),
            vec![2, 0, 0, 0, 1],
            acl(&[(USER_OBJ, 7, U), (OTHER, 0, U)]),
            acl(&[
                (USER_OBJ, 7, U),
                (USER, 6, 1),
                (GROUP_OBJ, 5, U),
                (OTHER, 0, U),
            ]),
            acl(&[(USER_OBJ, 7, U), (GROUP_OBJ, 5, U), (OTHER, 8, U)]),
            acl(&[(GROUP_OBJ, 5, U), (USER_OBJ, 7, U), (OTHER, 0, U)]),
            acl(&[
                (USER_OBJ, 7, U),
                (USER, 6, 2),
                (USER, 6, 1),
                (GROUP_OBJ, 5, U),
                (MASK, 7, U),
                (OTHER, 0, U),
            ]),
            acl(&[
                (USER_OBJ, 7, U),
                (USER, 6, U),
                (GROUP_OBJ, 5, U),
                (MASK, 7, U),
                (OTHER, 0, U),
            ]),
        ] {
            assert!(Acl::parse(&bad).is_err(), "{bad:?}");
        }
        let mut v1 = acl(&[(USER_OBJ, 7, U), (GROUP_OBJ, 5, U), (OTHER, 0, U)]);
        v1[0] = 1;
        assert!(Acl::parse(&v1).is_err());
    }

    #[test]
    fn chmod_and_inheritance_follow_posix() {
        let mut a = Acl::parse(&acl(&[
            (USER_OBJ, 7, U),
            (USER, 7, 1000),
            (GROUP_OBJ, 7, U),
            (MASK, 7, U),
            (OTHER, 5, U),
        ]))
        .unwrap();
        a.chmod(0o640);
        // The mask takes the group bits; the owning group and named entries are untouched.
        assert_eq!(a.perm(MASK), Some(4));
        assert_eq!(a.perm(GROUP_OBJ), Some(7));
        assert_eq!(a.apply_to_mode(0), 0o640);

        let default = Acl::parse(&acl(&[
            (USER_OBJ, 7, U),
            (USER, 7, 1000),
            (GROUP_OBJ, 5, U),
            (MASK, 7, U),
            (OTHER, 5, U),
        ]))
        .unwrap();
        // open(O_CREAT, 0666) under it: no umask, execute bits dropped by the requested mode.
        let (child, mode) = default.create_masq(0o100666);
        assert_eq!(mode, 0o100664);
        assert_eq!((child.perm(USER_OBJ), child.perm(MASK)), (Some(6), Some(6)));
        assert_eq!(child.perm(GROUP_OBJ), Some(5));
        // Without a mask the owning group takes the group bits.
        let plain = Acl::parse(&acl(&[(USER_OBJ, 7, U), (GROUP_OBJ, 7, U), (OTHER, 0, U)]));
        assert_eq!(plain.unwrap().create_masq(0o40755).1, 0o40750);
    }
}
