// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Mount points from `/proc/self/mountinfo`.

use std::{io, path::Path};

/// The mount points listed in mountinfo `text`, octal escapes (`\040` for a space) decoded.
pub fn mount_points(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(unescape)
        .collect()
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let octal = b
            .get(i + 1..i + 4)
            .filter(|d| d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match octal {
            Some(d) if b[i] == b'\\' => {
                let v = d.iter().fold(0u32, |v, c| v * 8 + u32::from(c - b'0'));
                out.push(v as u8);
                i += 4;
            }
            _ => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether `path` is a mount point of this mount namespace.
pub fn is_mount_point(path: &Path) -> io::Result<bool> {
    let text = std::fs::read_to_string("/proc/self/mountinfo")?;
    let want = path.to_string_lossy();
    let want = match want.trim_end_matches('/') {
        "" => "/",
        w => w,
    };
    Ok(mount_points(&text).iter().any(|m| m == want))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mount_points_and_escapes() {
        let text = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
36 22 0:45 / /var/lib/kubelet/pods/p/volumes/kubernetes.io~csi/pvc-1/mount rw,nosuid shared:9 - fuse.atlas atlas-native:pvc-1 rw
37 22 0:46 / /mnt/with\\040space\\134x rw - tmpfs tmpfs rw
";
        assert_eq!(
            mount_points(text),
            vec![
                "/",
                "/var/lib/kubelet/pods/p/volumes/kubernetes.io~csi/pvc-1/mount",
                "/mnt/with space\\x",
            ]
        );
    }

    #[test]
    fn a_backslash_without_three_octal_digits_is_kept() {
        assert_eq!(unescape("a\\9b\\04"), "a\\9b\\04");
    }
}
