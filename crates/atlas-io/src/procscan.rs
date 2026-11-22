// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Finds the tgids of named processes under a procfs root, for the native PID map.
//!
//! Pids are only meaningful to the kernel in the host pid namespace, so a containerised agent
//! needs `hostPID: true` for the native view to see anything.

use std::path::Path;

/// The kernel truncates `comm` to 15 bytes (TASK_COMM_LEN - 1).
const COMM_LEN: usize = 15;

fn comm_of(name: &str) -> &str {
    let mut end = name.len().min(COMM_LEN);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// Sorted tgids under `proc_root` whose `comm` matches one of `names` (compared after the
/// kernel's 15-byte truncation). Unreadable entries are skipped: processes come and go.
pub fn find_pids(proc_root: &Path, names: &[String]) -> Vec<u32> {
    let wanted: Vec<&str> = names
        .iter()
        .map(|n| comm_of(n.trim()))
        .filter(|n| !n.is_empty())
        .collect();
    if wanted.is_empty() {
        return Vec::new();
    }
    let Ok(dir) = std::fs::read_dir(proc_root) else {
        return Vec::new();
    };
    let mut pids: Vec<u32> = dir
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            std::fs::read_to_string(proc_root.join(pid.to_string()).join("comm"))
                .map(|c| wanted.contains(&c.trim_end_matches('\n')))
                .unwrap_or(false)
        })
        .collect();
    pids.sort_unstable();
    pids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_truncated_comm() {
        let root = std::env::temp_dir().join(format!("atlas-procscan-{}", uuid::Uuid::new_v4()));
        for (pid, comm) in [
            ("10", "atlas-native-no\n"),
            ("11", "atlas-native-s3\n"),
            ("12", "bash\n"),
            ("self", "atlas-native-no\n"),
        ] {
            std::fs::create_dir_all(root.join(pid)).unwrap();
            std::fs::write(root.join(pid).join("comm"), comm).unwrap();
        }
        std::fs::create_dir_all(root.join("13")).unwrap();
        let names = vec![
            "atlas-native-node".to_string(),
            "atlas-native-s3".to_string(),
        ];
        assert_eq!(find_pids(&root, &names), vec![10, 11]);
        assert!(find_pids(&root, &[]).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
