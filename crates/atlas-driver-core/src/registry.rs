// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! A small registry mapping backend ids to live driver instances. Interior-mutable so drivers can be
//! registered at startup *and* at runtime (e.g. `POST /backends`) through a shared `Arc`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::StorageDriver;

/// Holds the set of active backend drivers, keyed by backend id.
#[derive(Clone, Default)]
pub struct DriverRegistry {
    drivers: Arc<RwLock<HashMap<String, Arc<dyn StorageDriver>>>>,
}

impl DriverRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a driver for its backend id. Takes `&self` so it works after the
    /// registry is shared behind an `Arc` (runtime backend registration).
    pub fn register(&self, driver: Arc<dyn StorageDriver>) {
        if let Ok(mut g) = self.drivers.write() {
            g.insert(driver.backend_id().to_string(), driver);
        }
    }

    pub fn get(&self, backend_id: &str) -> Option<Arc<dyn StorageDriver>> {
        self.drivers.read().ok()?.get(backend_id).cloned()
    }

    /// The first registered driver, if any — convenient for the single-backend MVP.
    pub fn any(&self) -> Option<Arc<dyn StorageDriver>> {
        self.drivers.read().ok()?.values().next().cloned()
    }

    pub fn backend_ids(&self) -> Vec<String> {
        self.drivers
            .read()
            .map(|g| g.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.drivers.read().map(|g| g.is_empty()).unwrap_or(true)
    }
}
