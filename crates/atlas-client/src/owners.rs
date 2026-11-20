// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Owner ids Atlas records in `product_bindings`. This is a local registry, not a client of
//! those products: they are not called from here. See `docs/PRODUCTS.md`.
//!
//! Transiva's wire id is `hyper2kvm` and is not renamed. The others use their lowercase wire ids.

use atlas_api_types::Owner;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownProduct {
    /// Display name Transiva. Wire id `hyper2kvm`.
    Transiva,
    /// Display name Machina. Wire id `machina`.
    Machina,
    /// Display name Veyron. Wire id `veyron`.
    Veyron,
    /// Display name GuestKit. Wire id `guestkit`.
    GuestKit,
    /// Display name PacketWolf. Wire id `packetwolf`.
    PacketWolf,
    /// Display name Fabric. Wire id `fabric`.
    Fabric,
    /// Display name Netra. Wire id `netra`.
    Netra,
    /// Display name Kavira. Wire id `kavira`.
    Kavira,
}

impl KnownProduct {
    pub fn wire_id(self) -> &'static str {
        match self {
            Self::Transiva => "hyper2kvm",
            Self::Machina => "machina",
            Self::Veyron => "veyron",
            Self::GuestKit => "guestkit",
            Self::PacketWolf => "packetwolf",
            Self::Fabric => "fabric",
            Self::Netra => "netra",
            Self::Kavira => "kavira",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Transiva => "Transiva",
            Self::Machina => "Machina",
            Self::Veyron => "Veyron",
            Self::GuestKit => "GuestKit",
            Self::PacketWolf => "PacketWolf",
            Self::Fabric => "Fabric",
            Self::Netra => "Netra",
            Self::Kavira => "Kavira",
        }
    }

    /// Resource classes these products are expected to stamp. Not enforced by Atlas.
    pub fn resource_types(self) -> &'static [&'static str] {
        match self {
            Self::Transiva => &["vm", "datastore"],
            Self::Machina => &["vm", "fleet"],
            Self::Veyron => &["vm", "volume"],
            Self::GuestKit => &["vm", "volume"],
            Self::PacketWolf => &["vm", "volume"],
            Self::Fabric => &["vm", "volume", "pool"],
            Self::Netra => &["node", "flow"],
            Self::Kavira => &["incident", "repair"],
        }
    }
}

/// Wire ids only. Display names (`transiva`, `Transiva`) are not accepted.
pub fn lookup(wire_id: &str) -> Option<KnownProduct> {
    match wire_id {
        "hyper2kvm" => Some(KnownProduct::Transiva),
        "machina" => Some(KnownProduct::Machina),
        "veyron" => Some(KnownProduct::Veyron),
        "guestkit" => Some(KnownProduct::GuestKit),
        "packetwolf" => Some(KnownProduct::PacketWolf),
        "fabric" => Some(KnownProduct::Fabric),
        "netra" => Some(KnownProduct::Netra),
        "kavira" => Some(KnownProduct::Kavira),
        _ => None,
    }
}

pub fn known() -> &'static [KnownProduct] {
    &[
        KnownProduct::Transiva,
        KnownProduct::Machina,
        KnownProduct::Veyron,
        KnownProduct::GuestKit,
        KnownProduct::PacketWolf,
        KnownProduct::Fabric,
        KnownProduct::Netra,
        KnownProduct::Kavira,
    ]
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

/// Local health of a known product. `called_remote` is always false: this does not open a
/// connection to the product.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthPing {
    pub wire_id: &'static str,
    pub display_name: &'static str,
    pub status: &'static str,
    pub called_remote: bool,
    pub detail: &'static str,
}

pub fn health_ping(product: KnownProduct) -> HealthPing {
    HealthPing {
        wire_id: product.wire_id(),
        display_name: product.display_name(),
        status: "not_called",
        called_remote: false,
        detail: "Atlas records this owner locally and does not call the product",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transiva_stays_hyper2kvm_and_the_other_wire_ids_are_their_own() {
        assert_eq!(KnownProduct::Transiva.wire_id(), "hyper2kvm");
        assert_eq!(lookup("hyper2kvm"), Some(KnownProduct::Transiva));
        assert_eq!(lookup("transiva"), None, "display name is not the wire id");
        assert_eq!(lookup("machina"), Some(KnownProduct::Machina));
        assert_eq!(lookup("veyron"), Some(KnownProduct::Veyron));
        assert_eq!(lookup("guestkit"), Some(KnownProduct::GuestKit));
        assert_eq!(lookup("packetwolf"), Some(KnownProduct::PacketWolf));
        assert_eq!(lookup("fabric"), Some(KnownProduct::Fabric));
        assert_eq!(lookup("netra"), Some(KnownProduct::Netra));
        assert_eq!(lookup("kavira"), Some(KnownProduct::Kavira));
        assert_eq!(lookup("PacketWolf"), None);
        let o = owner(KnownProduct::Machina, "vm", "vm-1");
        assert_eq!(o.product, "machina");
        assert_eq!(o.resource_type, "vm");
        assert_eq!(o.role, "data_disk");
    }

    #[test]
    fn health_pings_are_local_stubs() {
        for product in known() {
            let ping = health_ping(*product);
            assert_eq!(ping.wire_id, product.wire_id());
            assert!(!ping.called_remote);
            assert_eq!(ping.status, "not_called");
        }
    }
}
