// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Owner ids Atlas records in `product_bindings`. This is a local registry, not a client of
//! Transiva or Machina: those products are not called from here. See `docs/PRODUCTS.md`.
//!
//! Transiva's wire id is `hyper2kvm` and is not renamed. Machina's wire id is `machina`.

use atlas_api_types::Owner;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownProduct {
    /// Display name Transiva. Wire id `hyper2kvm`.
    Transiva,
    /// Display name Machina. Wire id `machina`.
    Machina,
}

impl KnownProduct {
    pub fn wire_id(self) -> &'static str {
        match self {
            Self::Transiva => "hyper2kvm",
            Self::Machina => "machina",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Transiva => "Transiva",
            Self::Machina => "Machina",
        }
    }

    /// Resource classes these products are expected to stamp. Not enforced by Atlas.
    pub fn resource_types(self) -> &'static [&'static str] {
        match self {
            Self::Transiva => &["vm", "datastore"],
            Self::Machina => &["vm", "fleet"],
        }
    }
}

/// `hyper2kvm` and `machina` only. The display name `transiva` is not a wire id.
pub fn lookup(wire_id: &str) -> Option<KnownProduct> {
    match wire_id {
        "hyper2kvm" => Some(KnownProduct::Transiva),
        "machina" => Some(KnownProduct::Machina),
        _ => None,
    }
}

pub fn known() -> &'static [KnownProduct] {
    &[KnownProduct::Transiva, KnownProduct::Machina]
}

/// An [`Owner`] for `CreateVolume`. Role defaults to `data_disk`, matching `atlas-api-types`.
pub fn owner(product: KnownProduct, resource_type: &str, resource_id: &str) -> Owner {
    Owner {
        product: product.wire_id().to_string(),
        resource_type: resource_type.to_string(),
        resource_id: resource_id.to_string(),
        role: "data_disk".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transiva_stays_hyper2kvm_and_machina_is_its_own_id() {
        assert_eq!(KnownProduct::Transiva.wire_id(), "hyper2kvm");
        assert_eq!(lookup("hyper2kvm"), Some(KnownProduct::Transiva));
        assert_eq!(lookup("transiva"), None, "display name is not the wire id");
        assert_eq!(lookup("machina"), Some(KnownProduct::Machina));
        let o = owner(KnownProduct::Machina, "vm", "vm-1");
        assert_eq!(o.product, "machina");
        assert_eq!(o.resource_type, "vm");
        assert_eq!(o.role, "data_disk");
    }
}
