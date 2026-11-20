// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! SMB gateway configuration (binary `atlas-native-smb`): which filesystems to share under which
//! names, and the users who may connect, checked and rendered as a Samba configuration serving
//! each share's FUSE mount.

use std::{collections::HashSet, fmt, fmt::Write, path::Path};

use serde::Deserialize;

pub use crate::nfs::Access;

/// One SMB share of a filesystem.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Share {
    /// Share name clients connect to (`\\server\<name>`).
    pub name: String,
    /// Filesystem id.
    pub fs: String,
    #[serde(default)]
    pub access: Access,
    /// Users who may connect; every configured user if absent.
    #[serde(default)]
    pub users: Option<Vec<String>>,
    /// Users who may only read, even on a read-write share.
    #[serde(default)]
    pub read_only_users: Vec<String>,
    /// Client addresses or CIDR networks allowed to connect; any if absent.
    #[serde(default)]
    pub clients: Option<Vec<String>>,
    #[serde(default = "yes")]
    pub browseable: bool,
}

fn yes() -> bool {
    true
}

/// An SMB user: a Unix account in the gateway with this uid and gid, so files keep the same
/// owners through SMB, NFS and native mounts, and mode bits and ACLs apply to SMB access.
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct User {
    pub user: String,
    pub password: String,
    pub uid: u32,
    pub gid: u32,
}

impl fmt::Debug for User {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("User")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("uid", &self.uid)
            .field("gid", &self.gid)
            .finish()
    }
}

/// `server smb encrypt`.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Encrypt {
    /// Encrypt with clients that support it (SMB 3).
    #[default]
    Desired,
    /// Refuse clients that can't encrypt.
    Required,
    Off,
}

impl Encrypt {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "desired" => Ok(Self::Desired),
            "required" => Ok(Self::Required),
            "off" => Ok(Self::Off),
            _ => Err(format!(
                "encryption {s:?}: expected desired, required or off"
            )),
        }
    }
}

fn plain(s: &str, extra: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c) || extra.contains(c))
}

/// Share names Samba reserves.
const RESERVED_SHARES: [&str; 5] = ["global", "homes", "printers", "print$", "ipc$"];

fn valid_user_name(u: &str) -> bool {
    u.len() <= 32
        && u.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && u.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c))
}

/// Checks the users; errors name the user, never the password.
pub fn validate_users(users: &[User]) -> Result<(), String> {
    if users.is_empty() {
        return Err("no users".into());
    }
    let (mut names, mut uids) = (HashSet::new(), HashSet::new());
    for u in users {
        if !valid_user_name(&u.user) {
            return Err(format!(
                "invalid user name {:?}: lowercase letters, digits, `_.-`, at most 32",
                u.user
            ));
        }
        if u.uid == 0 || u.gid == 0 {
            return Err(format!("user {}: uid and gid must not be 0", u.user));
        }
        if u.password.is_empty() || u.password.len() > 256 {
            return Err(format!(
                "user {}: the password must be 1 to 256 bytes",
                u.user
            ));
        }
        // The password reaches Samba as a line on stdin.
        if u.password.contains(['\n', '\r', '\0']) {
            return Err(format!(
                "user {}: the password must not contain line breaks or NUL",
                u.user
            ));
        }
        if !names.insert(u.user.as_str()) {
            return Err(format!("user {} is listed twice", u.user));
        }
        if !uids.insert(u.uid) {
            return Err(format!("user {}: uid {} is used twice", u.user, u.uid));
        }
    }
    Ok(())
}

/// Checks the shares against the users so every value can go into the configuration as is.
pub fn validate_shares(shares: &[Share], users: &[User]) -> Result<(), String> {
    if shares.is_empty() {
        return Err("no shares".into());
    }
    let known: HashSet<&str> = users.iter().map(|u| u.user.as_str()).collect();
    let mut names = HashSet::new();
    for s in shares {
        let lower = s.name.to_ascii_lowercase();
        if !plain(&s.name, "")
            || s.name.len() > 80
            || s.name.starts_with('.')
            || RESERVED_SHARES.contains(&lower.as_str())
        {
            return Err(format!("invalid share name {:?}", s.name));
        }
        if !plain(&s.fs, "") || s.fs.len() > 128 || s.fs.starts_with('.') {
            return Err(format!(
                "share {}: invalid filesystem id {:?}",
                s.name, s.fs
            ));
        }
        if !names.insert(lower) {
            return Err(format!("share {} is listed twice", s.name));
        }
        if s.users.as_ref().is_some_and(Vec::is_empty) {
            return Err(format!("share {} lists no users", s.name));
        }
        for u in s.users.iter().flatten().chain(&s.read_only_users) {
            if !known.contains(u.as_str()) {
                return Err(format!("share {}: unknown user {u:?}", s.name));
            }
        }
        if let Some(c) = &s.clients {
            if c.is_empty() {
                return Err(format!("share {} lists no clients", s.name));
            }
            if let Some(bad) = c.iter().find(|c| !plain(c, ":/")) {
                return Err(format!("share {}: invalid client {bad:?}", s.name));
            }
        }
    }
    Ok(())
}

/// The Samba configuration sharing each share's mount at `<root>/<name>`. Call the validators
/// first.
pub fn render(shares: &[Share], root: &Path, encrypt: Encrypt) -> String {
    let encrypt = match encrypt {
        Encrypt::Desired => "desired",
        Encrypt::Required => "required",
        Encrypt::Off => "off",
    };
    let mut c = String::from("# Generated by atlas-native-smb.\n[global]\n");
    for line in [
        "server role = standalone server",
        // The default, the pod's host name, is usually longer than NetBIOS allows.
        "netbios name = ATLAS-NATIVE",
        "security = user",
        "passdb backend = tdbsam",
        "map to guest = never",
        "restrict anonymous = 2",
        "server min protocol = SMB2_10",
        "smb ports = 445",
        "disable netbios = yes",
        "load printers = no",
        "printing = bsd",
        "printcap name = /dev/null",
        "disable spoolss = yes",
        "log level = 1",
        // NFS, S3 and native mounts change files behind Samba's back, and a FUSE mount can't
        // break an oplock: without oplocks and leases SMB clients never cache stale data.
        "oplocks = no",
        "level2 oplocks = no",
        "smb2 leases = no",
        "kernel oplocks = no",
        // Byte-range locks become fcntl locks on the mount, which conflict with NFS and native
        // mounts' locks.
        "posix locking = yes",
        // The gateway's own files (S3 temporaries and uploads).
        "veto files = /.atlas_*/",
    ] {
        let _ = writeln!(c, "    {line}");
    }
    let _ = writeln!(c, "    server smb encrypt = {encrypt}");
    for s in shares {
        let _ = writeln!(c, "[{}]", s.name);
        let _ = writeln!(c, "    path = {}", root.join(&s.name).display());
        let _ = writeln!(
            c,
            "    read only = {}",
            if s.access == Access::Ro { "yes" } else { "no" }
        );
        let _ = writeln!(
            c,
            "    browseable = {}",
            if s.browseable { "yes" } else { "no" }
        );
        if let Some(users) = &s.users {
            let _ = writeln!(c, "    valid users = {}", users.join(" "));
        }
        if !s.read_only_users.is_empty() {
            let _ = writeln!(c, "    read list = {}", s.read_only_users.join(" "));
        }
        if let Some(clients) = &s.clients {
            let _ = writeln!(c, "    hosts allow = {}", clients.join(" "));
            let _ = writeln!(c, "    hosts deny = ALL");
        }
        let _ = writeln!(c, "    create mask = 0664");
        let _ = writeln!(c, "    directory mask = 0775");
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(name: &str, uid: u32) -> User {
        User {
            user: name.into(),
            password: "pass word!".into(),
            uid,
            gid: 2000,
        }
    }

    fn share(name: &str, fs: &str) -> Share {
        Share {
            name: name.into(),
            fs: fs.into(),
            access: Access::Rw,
            users: None,
            read_only_users: vec![],
            clients: None,
            browseable: true,
        }
    }

    #[test]
    fn renders_one_section_per_share() {
        let users = [user("alice", 2001), user("bob", 2002)];
        let mut ro = share("Archive", "team");
        ro.access = Access::Ro;
        ro.users = Some(vec!["alice".into()]);
        ro.clients = Some(vec!["10.0.0.0/8".into(), "192.168.1.5".into()]);
        ro.browseable = false;
        let mut rw = share("team", "team");
        rw.read_only_users = vec!["bob".into()];
        let shares = [rw, ro];
        validate_users(&users).unwrap();
        validate_shares(&shares, &users).unwrap();
        let c = render(&shares, Path::new("/export"), Encrypt::Required);
        assert!(c.contains("server smb encrypt = required"));
        assert!(c.contains("smb2 leases = no") && c.contains("posix locking = yes"));
        assert!(c.contains(
            "[team]\n    path = /export/team\n    read only = no\n    browseable = yes\n    read list = bob\n"
        ));
        assert!(c.contains(
            "[Archive]\n    path = /export/Archive\n    read only = yes\n    browseable = no\n    valid users = alice\n    hosts allow = 10.0.0.0/8 192.168.1.5\n    hosts deny = ALL\n"
        ));
    }

    #[test]
    fn refuses_what_could_break_out_of_the_configuration() {
        let users = [user("alice", 2001)];
        for bad in [
            share("a]\n[evil", "fs"),
            share("global", "fs"),
            share("IPC$", "fs"),
            share(".hidden", "fs"),
            share("a", "fs\n    path = /"),
            share("a", ".."),
            Share {
                users: Some(vec!["mallory".into()]),
                ..share("a", "fs")
            },
            Share {
                users: Some(vec![]),
                ..share("a", "fs")
            },
            Share {
                read_only_users: vec!["alice bob".into()],
                ..share("a", "fs")
            },
            Share {
                clients: Some(vec!["10.0.0.0/8\n    guest ok = yes".into()]),
                ..share("a", "fs")
            },
        ] {
            assert!(
                validate_shares(std::slice::from_ref(&bad), &users).is_err(),
                "{bad:?}"
            );
        }
        assert!(validate_shares(&[], &users).is_err());
        assert!(validate_shares(&[share("a", "x"), share("A", "y")], &users).is_err());
    }

    #[test]
    fn checks_users_without_revealing_passwords() {
        let secret = "hunter2-very-secret";
        for bad in [
            User {
                user: "Alice".into(),
                ..user("a", 2001)
            },
            User {
                user: "root".into(),
                uid: 0,
                ..user("a", 2001)
            },
            User {
                gid: 0,
                ..user("a", 2001)
            },
            User {
                password: String::new(),
                ..user("a", 2001)
            },
            User {
                password: format!("{secret}\n{secret}"),
                ..user("a", 2001)
            },
        ] {
            let e = validate_users(std::slice::from_ref(&bad)).unwrap_err();
            assert!(!e.contains(secret), "{e}");
        }
        assert!(validate_users(&[]).is_err());
        assert!(validate_users(&[user("a", 2001), user("a", 2002)]).is_err());
        assert!(validate_users(&[user("a", 2001), user("b", 2001)]).is_err());
        assert!(!format!("{:?}", user("a", 2001)).contains("pass word"));
    }

    #[test]
    fn parses_the_chart_format() {
        let s: Vec<Share> = serde_json::from_str(
            r#"[{"name":"team","fs":"pvc-1"},{"name":"ro","fs":"x","access":"ro","users":["a"],"clients":["10.0.0.0/8"],"browseable":false}]"#,
        )
        .unwrap();
        assert!(s[0].browseable && s[0].users.is_none());
        assert_eq!(s[1].access, Access::Ro);
        assert!(serde_json::from_str::<Vec<Share>>(r#"[{"name":"a","fs":"b","x":1}]"#).is_err());
        let u: Vec<User> =
            serde_json::from_str(r#"[{"user":"a","password":"p","uid":2001,"gid":2000}]"#).unwrap();
        assert_eq!(u[0].uid, 2001);
        assert_eq!(Encrypt::parse("off"), Ok(Encrypt::Off));
        assert!(Encrypt::parse("maybe").is_err());
    }
}
