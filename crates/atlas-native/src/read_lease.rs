// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! A short leader lease for linearizable metadata reads.
//!
//! After the leader confirms a quorum (a successful ReadIndex, or its check-quorum round) it may
//! serve later local reads without another barrier, until the monotonic deadline or until a write
//! advances the term or the commit index. Followers do not use it: they cannot see a commit that
//! happened on the leader after the lease was taken. The sans-IO node stores one of these; the
//! clock is supplied by the caller so tests do not sleep.
//!
//! The TTL is one quarter of the minimum election timeout, so a lease ends before a new leader
//! can be elected if this process's monotonic clock is not wildly slow. That is a bound, not a
//! measurement of clock skew between hosts.

/// How long a lease lasts, in the same millisecond unit as the caller's clock.
///
/// `election_min_ticks` is the lower end of the Raft election timeout. `tick_ms` is the
/// metadata tick. The result is at least 1.
pub fn ttl_ms(election_min_ticks: u64, tick_ms: u64) -> u64 {
    let min_election = tick_ms.saturating_mul(election_min_ticks.max(1));
    (min_election / 4).max(1)
}

/// Permission to skip ReadIndex while the confirmed `(term, index)` is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadLease {
    term: u64,
    index: u64,
    deadline_ms: u64,
}

impl ReadLease {
    /// `deadline` is `now_ms + ttl_ms` (saturating). `ttl_ms` of 0 still lasts 1 ms so a grant
    /// is observable at `now_ms`.
    pub fn grant(term: u64, index: u64, now_ms: u64, ttl_ms: u64) -> Self {
        Self {
            term,
            index,
            deadline_ms: now_ms.saturating_add(ttl_ms.max(1)),
        }
    }

    pub fn term(self) -> u64 {
        self.term
    }

    pub fn index(self) -> u64 {
        self.index
    }

    pub fn deadline_ms(self) -> u64 {
        self.deadline_ms
    }

    /// The confirmed term and commit index still match, and `now_ms` is strictly before the
    /// deadline. A write that advances either value, or a clock at or past the deadline, misses.
    pub fn allows(self, now_ms: u64, term: u64, index: u64) -> bool {
        now_ms < self.deadline_ms && self.term == term && self.index == index
    }

    /// Leader-only form of [`Self::allows`]. A follower always misses.
    pub fn allows_leader(self, is_leader: bool, now_ms: u64, term: u64, index: u64) -> bool {
        is_leader && self.allows(now_ms, term, index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_is_a_quarter_of_the_minimum_election_timeout() {
        // Default metadata tick is 50 ms and the election minimum is 10 ticks (500 ms).
        assert_eq!(ttl_ms(10, 50), 125);
        assert_eq!(ttl_ms(0, 50), 12);
        assert_eq!(ttl_ms(10, 0), 1);
    }

    #[test]
    fn grant_allows_until_the_deadline() {
        let lease = ReadLease::grant(3, 10, 1_000, 100);
        assert_eq!(lease.term(), 3);
        assert_eq!(lease.index(), 10);
        assert_eq!(lease.deadline_ms(), 1_100);
        assert!(lease.allows(1_000, 3, 10));
        assert!(lease.allows(1_099, 3, 10));
        assert!(lease.allows_leader(true, 1_050, 3, 10));
    }

    #[test]
    fn a_lease_expires_at_the_deadline() {
        let lease = ReadLease::grant(3, 10, 1_000, 100);
        assert!(!lease.allows(1_100, 3, 10));
        assert!(!lease.allows(1_500, 3, 10));
    }

    #[test]
    fn a_write_that_advances_the_term_or_index_invalidates_the_lease() {
        let lease = ReadLease::grant(3, 10, 1_000, 100);
        assert!(!lease.allows(1_050, 4, 10), "new term");
        assert!(!lease.allows(1_050, 3, 11), "commit index moved");
        assert!(!lease.allows(1_050, 4, 11));
        assert!(lease.allows(1_050, 3, 10), "unchanged state still covered");
    }

    #[test]
    fn a_follower_never_skips_the_barrier() {
        let lease = ReadLease::grant(3, 10, 1_000, 100);
        assert!(!lease.allows_leader(false, 1_050, 3, 10));
    }
}
