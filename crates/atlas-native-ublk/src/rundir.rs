// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `libublk` records each device in a file under `/run/ublksrvd`. It creates that directory
//! writable by everyone, without the sticky bit, and writes the file following symlinks, so any
//! local user could have a server running as root overwrite a file of their choosing. The server
//! therefore makes the directory its own before `libublk` writes there.

use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::Path,
};

/// Makes `dir` a directory of this process's user that nobody else can write, holding no
/// symlinks. Creates it if it is missing.
pub fn secure(dir: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o755).create(dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let meta = fs::symlink_metadata(dir)?;
    // SAFETY: geteuid has no arguments and cannot fail.
    let user = unsafe { libc::geteuid() };
    if !meta.is_dir() || meta.uid() != user {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("not a directory owned by user {user}"),
        ));
    }
    if meta.mode() & 0o022 != 0 {
        tracing::warn!(
            "{} was writable by other users; removing that permission",
            dir.display()
        );
        fs::set_permissions(dir, fs::Permissions::from_mode(meta.mode() & 0o7755))?;
    }
    // Only now: nobody else can add one any more.
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_symlink() {
            tracing::warn!("removing the symlink {}", entry.path().display());
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().mode() & 0o7777
    }

    #[test]
    fn a_missing_directory_is_created_for_its_owner_to_write() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join("run");
        secure(&dir).unwrap();
        assert_eq!(mode(&dir) & 0o022, 0);
        secure(&dir).unwrap();
    }

    #[test]
    fn a_shared_directory_is_closed_and_its_symlinks_removed() {
        let td = tempfile::tempdir().unwrap();
        let (dir, victim) = (td.path().join("run"), td.path().join("victim"));
        fs::write(&victim, "keep").unwrap();
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        symlink(&victim, dir.join("0000.json")).unwrap();
        fs::write(dir.join("0001.json"), "{}").unwrap();

        secure(&dir).unwrap();

        assert_eq!(mode(&dir), 0o755);
        assert!(fs::symlink_metadata(dir.join("0000.json")).is_err());
        assert_eq!(fs::read_to_string(dir.join("0001.json")).unwrap(), "{}");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
    }

    #[test]
    fn something_else_in_its_place_is_refused() {
        let td = tempfile::tempdir().unwrap();
        let (real, link, file) = (
            td.path().join("real"),
            td.path().join("link"),
            td.path().join("file"),
        );
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o777)).unwrap();
        symlink(&real, &link).unwrap();
        fs::write(&file, "").unwrap();
        assert!(secure(&link).is_err());
        assert!(secure(&file).is_err());
        assert_eq!(mode(&real), 0o777, "the target is left alone");
    }
}
