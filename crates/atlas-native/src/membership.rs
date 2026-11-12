// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Voter configuration for Raft quorum decisions. `Joint` is the intermediate configuration of a
//! joint-consensus membership change: every decision needs a majority of the old *and* the new
//! voter set. See `RaftNode::change_membership` for the change protocol.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::raft::NodeId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Membership {
    Stable {
        voters: BTreeSet<NodeId>,
    },
    Joint {
        old: BTreeSet<NodeId>,
        new: BTreeSet<NodeId>,
    },
}

impl Membership {
    pub fn stable(voters: impl IntoIterator<Item = NodeId>) -> Self {
        Self::Stable {
            voters: voters.into_iter().collect(),
        }
    }

    /// Every node whose vote or acknowledgement counts toward some quorum.
    pub fn voters(&self) -> BTreeSet<NodeId> {
        match self {
            Self::Stable { voters } => voters.clone(),
            Self::Joint { old, new } => old.union(new).cloned().collect(),
        }
    }

    /// The configuration being moved to (`Stable`'s voters, or `Joint`'s `new`).
    pub fn target(&self) -> &BTreeSet<NodeId> {
        match self {
            Self::Stable { voters } => voters,
            Self::Joint { new, .. } => new,
        }
    }

    pub fn has_quorum(&self, acks: &BTreeSet<NodeId>) -> bool {
        match self {
            Self::Stable { voters } => majority(voters, acks),
            Self::Joint { old, new } => majority(old, acks) && majority(new, acks),
        }
    }

    /// Joint-consensus steps that add `id` to a stable voter set: `C_old,new`, then `C_new`.
    /// `None` when `id` is already a voter (no log entries).
    pub fn add_voter_steps(voters: &BTreeSet<NodeId>, id: &NodeId) -> Option<[Membership; 2]> {
        if voters.contains(id) {
            return None;
        }
        let mut new = voters.clone();
        new.insert(id.clone());
        Some(joint_then_stable(voters, new))
    }

    /// Joint-consensus steps that remove `id`. `Ok(None)` when `id` is not a voter.
    /// Refuses to leave the group with no voters.
    pub fn remove_voter_steps(
        voters: &BTreeSet<NodeId>,
        id: &NodeId,
    ) -> Result<Option<[Membership; 2]>, &'static str> {
        if !voters.contains(id) {
            return Ok(None);
        }
        let mut new = voters.clone();
        new.remove(id);
        if new.is_empty() {
            return Err("cannot remove the last voter");
        }
        Ok(Some(joint_then_stable(voters, new)))
    }
}

fn joint_then_stable(old: &BTreeSet<NodeId>, new: BTreeSet<NodeId>) -> [Membership; 2] {
    [
        Membership::Joint {
            old: old.clone(),
            new: new.clone(),
        },
        Membership::stable(new),
    ]
}

fn majority(voters: &BTreeSet<NodeId>, acks: &BTreeSet<NodeId>) -> bool {
    !voters.is_empty() && voters.intersection(acks).count() > voters.len() / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(v: &[&str]) -> BTreeSet<NodeId> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn stable_majority() {
        let m = Membership::Stable {
            voters: set(&["a", "b", "c"]),
        };
        assert!(m.has_quorum(&set(&["a", "b"])));
        assert!(!m.has_quorum(&set(&["a"])));
        assert!(
            !m.has_quorum(&set(&["a", "x", "y"])),
            "non-voters never count"
        );
    }

    #[test]
    fn even_sized_set_needs_strict_majority() {
        let m = Membership::Stable {
            voters: set(&["a", "b", "c", "d"]),
        };
        assert!(!m.has_quorum(&set(&["a", "b"])));
        assert!(m.has_quorum(&set(&["a", "b", "c"])));
    }

    #[test]
    fn joint_requires_both_majorities() {
        let m = Membership::Joint {
            old: set(&["a", "b", "c"]),
            new: set(&["b", "c", "d"]),
        };
        assert!(m.has_quorum(&set(&["b", "c"])));
        assert!(!m.has_quorum(&set(&["a", "b"])));
        assert!(!m.has_quorum(&set(&["c", "d"])));
        assert_eq!(m.voters(), set(&["a", "b", "c", "d"]));
    }

    #[test]
    fn add_voter_steps_are_joint_then_stable() {
        let old = set(&["a", "b", "c"]);
        let id = "d".to_string();
        let [joint, stable] = Membership::add_voter_steps(&old, &id).unwrap();
        assert!(
            !joint.has_quorum(&set(&["a", "b"])),
            "a majority of only the old set must not commit the joint entry"
        );
        assert!(
            joint.has_quorum(&set(&["a", "b", "d"])),
            "majorities of both sets commit the joint entry"
        );
        assert_eq!(joint.target(), stable.target());
        assert!(matches!(stable, Membership::Stable { .. }));
        assert!(Membership::add_voter_steps(&old, &"a".into()).is_none());
    }

    #[test]
    fn remove_voter_steps_keep_both_majorities_and_refuse_the_last() {
        let old = set(&["a", "b", "c"]);
        let [joint, stable] = Membership::remove_voter_steps(&old, &"c".into())
            .unwrap()
            .unwrap();
        assert!(!joint.has_quorum(&set(&["a"])));
        assert!(joint.has_quorum(&set(&["a", "b"])));
        assert_eq!(stable.target(), &set(&["a", "b"]));
        assert!(Membership::remove_voter_steps(&old, &"z".into())
            .unwrap()
            .is_none());
        assert_eq!(
            Membership::remove_voter_steps(&set(&["a"]), &"a".into()),
            Err("cannot remove the last voter")
        );
    }

    #[test]
    fn empty_configuration_never_has_quorum() {
        assert!(!Membership::Stable {
            voters: BTreeSet::new()
        }
        .has_quorum(&set(&["a"])));
    }
}
