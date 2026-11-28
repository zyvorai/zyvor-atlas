// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Whether to ask for a restart after a storage failure. A file that failed a sync stays failed
//! until it is reopened, so a restart is the way back; on a disk that is bad for good, asking
//! every time would be a loop that keeps taking away everything else the process serves. Each
//! component (a data device, the metadata replica) therefore gets a bounded number of restarts
//! per window, counted in a file that outlives the process. Past that it stays failed and the
//! process stays up.

use std::{
    collections::BTreeMap,
    fs, io,
    path::PathBuf,
    sync::{Mutex, PoisonError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::durable;

pub const DEFAULT_MAX_RESTARTS: u32 = 3;
pub const DEFAULT_RESTART_WINDOW: Duration = Duration::from_secs(3600);
/// How long until a restart that could not be counted is tried again (health probes ask often,
/// and the record may sit on the disk that is failing).
const RECOUNT_SECS: u64 = 10;

/// When a failed data device asks for a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceRestart {
    /// As soon as any device refuses writes.
    Any,
    /// Only once no device of the node accepts writes: at once for a node with one device,
    /// while a node with healthy ones goes on serving them. The default.
    Last,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    /// Restarts each component may ask for within `window`; 0 asks for none.
    pub max_restarts: u32,
    pub window: Duration,
    pub devices: DeviceRestart,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_restarts: DEFAULT_MAX_RESTARTS,
            window: DEFAULT_RESTART_WINDOW,
            devices: DeviceRestart::Last,
        }
    }
}

impl RestartPolicy {
    /// The defaults, overridden by `ATLAS_NATIVE_FAILURE_RESTARTS`,
    /// `ATLAS_NATIVE_FAILURE_WINDOW_SECS` and `ATLAS_NATIVE_DEVICE_FAILURE_RESTART`
    /// (`any`, `last` or `never`).
    pub fn from_env() -> Result<Self, String> {
        Self::parse(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
    }

    fn parse(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        fn number<T: std::str::FromStr>(name: &str, value: &str) -> Result<T, String> {
            value
                .parse()
                .map_err(|_| format!("{name} must be a whole number, not {value:?}"))
        }
        let mut p = Self::default();
        let name = "ATLAS_NATIVE_FAILURE_RESTARTS";
        if let Some(v) = var(name) {
            p.max_restarts = number(name, &v)?;
        }
        let name = "ATLAS_NATIVE_FAILURE_WINDOW_SECS";
        if let Some(v) = var(name) {
            p.window = Duration::from_secs(number(name, &v)?);
        }
        let name = "ATLAS_NATIVE_DEVICE_FAILURE_RESTART";
        if let Some(v) = var(name) {
            p.devices = match v.as_str() {
                "any" => DeviceRestart::Any,
                "last" => DeviceRestart::Last,
                "never" => DeviceRestart::Never,
                _ => return Err(format!("{name} must be any, last or never, not {v:?}")),
            };
        }
        Ok(p)
    }
}

/// The restarts asked for, by component: Unix seconds.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    restarts: BTreeMap<String, Vec<u64>>,
}

#[derive(Debug)]
struct Inner {
    policy: RestartPolicy,
    /// Whether each failed component asks for a restart, decided once per process.
    decided: BTreeMap<String, bool>,
    /// Components whose restart could not be counted, and when that was last tried.
    uncounted: BTreeMap<String, u64>,
}

#[derive(Debug)]
pub(crate) struct Restarts {
    path: PathBuf,
    inner: Mutex<Inner>,
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Restarts {
    /// Restarts are counted in `path`.
    pub fn new(path: PathBuf, policy: RestartPolicy) -> Self {
        Self {
            path,
            inner: Mutex::new(Inner {
                policy,
                decided: BTreeMap::new(),
                uncounted: BTreeMap::new(),
            }),
        }
    }

    pub fn policy(&self) -> RestartPolicy {
        self.lock().policy
    }

    /// Applies to failures not decided yet.
    pub fn set_policy(&self, policy: RestartPolicy) {
        self.lock().policy = policy;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `component`, which has failed, asks for a restart. The restart is counted on
    /// disk before it is asked for: while it cannot be, the answer is no (a restart that is not
    /// counted could repeat without end) and counting is tried again later.
    pub fn wanted(&self, component: &str) -> bool {
        self.wanted_at(component, unix_secs())
    }

    fn wanted_at(&self, component: &str, now: u64) -> bool {
        let mut inner = self.lock();
        if let Some(&wanted) = inner.decided.get(component) {
            return wanted;
        }
        let tried = inner.uncounted.get(component);
        if tried.is_some_and(|&t| now < t.saturating_add(RECOUNT_SECS)) {
            return false;
        }
        let policy = inner.policy;
        let counted = self.read(policy.window, now).and_then(|mut record| {
            let times = record.restarts.entry(component.to_string()).or_default();
            if times.len() >= policy.max_restarts as usize {
                return Ok(false);
            }
            times.push(now);
            let bytes = serde_json::to_vec(&record)?;
            durable::write_atomic(&self.path, &bytes)?;
            Ok(true)
        });
        match counted {
            Ok(wanted) => {
                if !wanted {
                    eprintln!(
                        "atlas-native: {component} failed and gets no restart ({} allowed per \
                         {} s); it stays failed until the process is restarted by hand",
                        policy.max_restarts,
                        policy.window.as_secs()
                    );
                }
                inner.decided.insert(component.to_string(), wanted);
                wanted
            }
            Err(e) => {
                if inner.uncounted.insert(component.to_string(), now).is_none() {
                    eprintln!(
                        "atlas-native: {component} failed; no restart yet, it cannot be counted \
                         in {}: {e}",
                        self.path.display()
                    );
                }
                false
            }
        }
    }

    /// The record without the restarts older than `window`. Times ahead of the clock are kept.
    fn read(&self, window: Duration, now: u64) -> io::Result<Record> {
        let mut record: Record = match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Record::default(),
            Err(e) => return Err(e),
        };
        for times in record.restarts.values_mut() {
            times.retain(|&t| now.saturating_sub(t) < window.as_secs());
        }
        Ok(record)
    }

    /// Restarts asked for within the window, by component.
    pub fn counted(&self) -> BTreeMap<String, usize> {
        let window = self.policy().window;
        self.read(window, unix_secs())
            .map(|r| {
                r.restarts
                    .into_iter()
                    .filter(|(_, t)| !t.is_empty())
                    .map(|(c, t)| (c, t.len()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(max_restarts: u32) -> RestartPolicy {
        RestartPolicy {
            max_restarts,
            window: Duration::from_secs(100),
            ..RestartPolicy::default()
        }
    }

    #[test]
    fn restarts_are_bounded_across_processes_until_the_window_passes() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("restarts.json");
        // Each `Restarts` stands for one process.
        let process = || Restarts::new(path.clone(), policy(2));
        for now in [1000, 1001] {
            let p = process();
            assert!(p.wanted_at("device0", now));
            // Asking again in the same process counts nothing more.
            assert!(p.wanted_at("device0", now + 50));
        }
        let p = process();
        assert!(!p.wanted_at("device0", 1002), "a third restart");
        assert!(
            p.wanted_at("metadata", 1002),
            "another component has its own"
        );
        // Decided: it stays so for this process, even once the window has passed.
        assert!(!p.wanted_at("device0", 5000));
        // The first has left the window.
        assert!(process().wanted_at("device0", 1100));
        assert!(!process().wanted_at("device0", 1100));
    }

    #[test]
    fn a_restart_that_cannot_be_counted_is_not_asked_for() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("missing").join("restarts.json");
        let r = Restarts::new(path.clone(), policy(3));
        assert!(!r.wanted_at("device0", 1000));
        fs::create_dir(path.parent().unwrap()).unwrap();
        // Not tried again at once, then counted.
        assert!(!r.wanted_at("device0", 1001));
        assert!(r.wanted_at("device0", 1000 + RECOUNT_SECS));
    }

    #[test]
    fn an_unreadable_record_allows_no_restart() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("restarts.json");
        fs::write(&path, b"not json").unwrap();
        assert!(!Restarts::new(path, policy(3)).wanted_at("device0", 1000));
    }

    #[test]
    fn no_restarts_are_asked_for_when_none_are_allowed() {
        let td = tempfile::tempdir().unwrap();
        let r = Restarts::new(td.path().join("restarts.json"), policy(0));
        assert!(!r.wanted_at("device0", 1000));
    }

    #[test]
    fn the_policy_comes_from_the_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            RestartPolicy::parse(move |n| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == n)
                    .map(|(_, v)| v.to_string())
            })
        };
        assert_eq!(env(&[]).unwrap(), RestartPolicy::default());
        let p = env(&[
            ("ATLAS_NATIVE_FAILURE_RESTARTS", "5"),
            ("ATLAS_NATIVE_FAILURE_WINDOW_SECS", "60"),
            ("ATLAS_NATIVE_DEVICE_FAILURE_RESTART", "any"),
        ])
        .unwrap();
        assert_eq!(
            (p.max_restarts, p.window, p.devices),
            (5, Duration::from_secs(60), DeviceRestart::Any)
        );
        assert!(env(&[("ATLAS_NATIVE_FAILURE_RESTARTS", "-1")]).is_err());
        assert!(env(&[("ATLAS_NATIVE_DEVICE_FAILURE_RESTART", "all")]).is_err());
    }
}
