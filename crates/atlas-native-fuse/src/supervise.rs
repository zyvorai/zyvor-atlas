// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Process supervision shared by the gateways (`atlas-native-nfs`, `atlas-native-smb`): child
//! processes stopped in order, mount detection, and a SIGTERM/SIGINT flag.

use std::{
    os::unix::fs::MetadataExt,
    path::Path,
    process::{Child, Command},
    sync::atomic::{AtomicBool, Ordering},
    thread::sleep,
    time::{Duration, Instant},
};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Sets [`stopping`] on SIGTERM and SIGINT.
pub fn handle_signals() {
    for sig in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: the handler only stores to an atomic.
        unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
    }
}

pub fn stopping() -> bool {
    STOP.load(Ordering::SeqCst)
}

pub struct Proc {
    pub name: String,
    child: Child,
}

impl Proc {
    pub fn spawn(name: impl Into<String>, cmd: &mut Command) -> Result<Proc, String> {
        let name = name.into();
        let child = cmd.spawn().map_err(|e| format!("start {name}: {e}"))?;
        Ok(Proc { name, child })
    }

    /// Why the process is gone, if it is.
    pub fn exited(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(format!("{} exited ({status})", self.name)),
            Ok(None) => None,
            Err(e) => Some(format!("{}: {e}", self.name)),
        }
    }

    /// SIGTERM, then SIGKILL after `grace`.
    pub fn stop(&mut self, grace: Duration) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        if let Ok(pid) = i32::try_from(self.child.id()) {
            // SAFETY: signalling our own child by pid.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        if !wait_for(&mut self.child, grace) {
            tracing::warn!(process = %self.name, "did not stop in time; killing it");
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn wait_for(child: &mut Child, limit: Duration) -> bool {
    let until = Instant::now() + limit;
    while Instant::now() < until {
        if child.try_wait().ok().flatten().is_some() {
            return true;
        }
        sleep(Duration::from_millis(100));
    }
    false
}

/// Whether `dir` is a different filesystem from its parent, i.e. the mount is up.
pub fn mounted(dir: &Path) -> bool {
    match (dir.metadata(), dir.parent().map(Path::metadata)) {
        (Ok(d), Some(Ok(p))) => d.dev() != p.dev(),
        _ => false,
    }
}

/// Unmounts a FUSE mount if it is up, which sends its buffered writes.
pub fn unmount(dir: &Path) {
    if mounted(dir) {
        match Command::new("fusermount3").arg("-u").arg(dir).status() {
            Ok(s) if s.success() => {}
            other => tracing::warn!(dir = %dir.display(), result = ?other, "unmount failed"),
        }
    }
}
