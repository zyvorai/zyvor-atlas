// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Next-step RDMA send path. Not used by the verbs stream.
//!
//! [`crate::rdma::RdmaStream`] (feature `rdma`, Linux) still posts one send and waits, copying
//! into a staging slot. This module is the credit window and the registered-memory hook that
//! stream would call. [`runtime_enabled`] is always false: `ATLAS_NATIVE_RDMA_PIPELINE=1` is
//! only recorded by [`pipeline_requested`] and does not change I/O.
//!
//! No RDMA device is required to build or test this module. The ignored verbs tests elsewhere
//! need a device; SoftRoCE is enough and hardware is not (`rdma link add rxe0 type rxe netdev lo`,
//! GID index 1 unless `ATLAS_RDMA_GID_INDEX` is set). They are not CI. A pass on `rxe0` checks
//! that queue pairs connect and bytes match. It is not an RDMA NIC result and not a throughput
//! or latency number.

/// Sends that may be posted before one must complete. The live stream does not use this yet.
pub const PIPELINE_DEPTH: usize = 8;

/// `ATLAS_NATIVE_RDMA_PIPELINE=1`. The verbs runtime ignores it until the stream is wired.
pub fn pipeline_requested() -> bool {
    requested_from(std::env::var("ATLAS_NATIVE_RDMA_PIPELINE").ok().as_deref())
}

pub fn requested_from(value: Option<&str>) -> bool {
    value == Some("1")
}

/// TODO(runtime): `RdmaStream::write` should post through [`SendPipeline`] and
/// [`RecordingZeroCopy`] when this becomes true. It is false even if the env var is set.
pub fn runtime_enabled() -> bool {
    false
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineError {
    /// `post` of an empty payload.
    ZeroLength,
    /// `depth` sends are already in flight.
    Full,
    /// `complete` of an id that is not in flight.
    UnknownId(u64),
}

/// In-flight send credits. Work-request ids start at 1 and increase. Completions may arrive
/// out of order; each id is retired on its own.
#[derive(Debug, Clone)]
pub struct SendPipeline {
    depth: usize,
    next_id: u64,
    inflight: Vec<(u64, usize)>,
}

impl SendPipeline {
    /// `depth` of 0 is raised to 1 so a window can always hold one send.
    pub fn new(depth: usize) -> Self {
        Self {
            depth: depth.max(1),
            next_id: 1,
            inflight: Vec::new(),
        }
    }

    /// The default window ([`PIPELINE_DEPTH`]).
    pub fn with_default_depth() -> Self {
        Self::new(PIPELINE_DEPTH)
    }

    pub fn depth(&self) -> usize {
        self.depth
    }

    pub fn in_flight(&self) -> usize {
        self.inflight.len()
    }

    pub fn can_post(&self) -> bool {
        self.inflight.len() < self.depth
    }

    /// Reserves a credit and returns the work-request id. The caller would then post the
    /// verbs send; this method does not.
    pub fn post(&mut self, len: usize) -> Result<u64, PipelineError> {
        if len == 0 {
            return Err(PipelineError::ZeroLength);
        }
        if !self.can_post() {
            return Err(PipelineError::Full);
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.inflight.push((id, len));
        Ok(id)
    }

    /// Retires `wr_id` and returns the length that was posted.
    pub fn complete(&mut self, wr_id: u64) -> Result<usize, PipelineError> {
        let Some(pos) = self.inflight.iter().position(|(id, _)| *id == wr_id) else {
            return Err(PipelineError::UnknownId(wr_id));
        };
        Ok(self.inflight.remove(pos).1)
    }
}

/// A memory region the zero-copy path would hand to `ibv_post_send` instead of copying into
/// the stream's staging slot. `addr` is an address the caller registered; this module never
/// dereferences it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisteredRegion {
    pub addr: u64,
    pub length: usize,
    pub lkey: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZeroCopyRequest {
    pub region: RegisteredRegion,
    pub offset: usize,
    pub len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZeroCopyStatus {
    /// The verbs post is not wired. The stream copies into its staging buffer.
    PendingRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZeroCopyError {
    ZeroLength,
    /// `offset + len` does not fit in the region.
    OutOfRegion,
}

/// Records zero-copy attempts. [`Self::submit`] always returns [`ZeroCopyStatus::PendingRuntime`]
/// while [`runtime_enabled`] is false, including when a region is valid.
#[derive(Debug, Default, Clone)]
pub struct RecordingZeroCopy {
    pub requests: Vec<ZeroCopyRequest>,
}

impl RecordingZeroCopy {
    pub fn submit(
        &mut self,
        region: RegisteredRegion,
        offset: usize,
        len: usize,
    ) -> Result<ZeroCopyStatus, ZeroCopyError> {
        if len == 0 {
            return Err(ZeroCopyError::ZeroLength);
        }
        if offset.saturating_add(len) > region.length {
            return Err(ZeroCopyError::OutOfRegion);
        }
        self.requests.push(ZeroCopyRequest {
            region,
            offset,
            len,
        });
        // TODO(runtime): when `runtime_enabled()` is true, post from `region.addr + offset`
        // with `region.lkey` and return the work-request id. Until then the caller copies.
        let _ = runtime_enabled();
        Ok(ZeroCopyStatus::PendingRuntime)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_flag_is_only_a_request() {
        assert!(requested_from(Some("1")));
        assert!(!requested_from(Some("0")));
        assert!(!requested_from(Some("true")));
        assert!(!requested_from(None));
        assert!(!runtime_enabled());
    }

    #[test]
    fn the_window_blocks_when_full_and_frees_on_completion() {
        let mut p = SendPipeline::new(2);
        assert_eq!(p.depth(), 2);
        let a = p.post(4).unwrap();
        let b = p.post(8).unwrap();
        assert!(!p.can_post());
        assert!(matches!(p.post(1), Err(PipelineError::Full)));
        assert!(matches!(p.post(0), Err(PipelineError::ZeroLength)));
        assert_eq!(p.complete(a).unwrap(), 4);
        assert!(p.can_post());
        let c = p.post(2).unwrap();
        assert_ne!(c, b);
        assert_eq!(p.in_flight(), 2);
        // Out-of-order completion of the later id.
        assert_eq!(p.complete(c).unwrap(), 2);
        assert!(matches!(p.complete(c), Err(PipelineError::UnknownId(_))));
        assert_eq!(p.complete(b).unwrap(), 8);
        assert_eq!(p.in_flight(), 0);
    }

    #[test]
    fn a_zero_depth_still_holds_one_send() {
        let mut p = SendPipeline::new(0);
        assert_eq!(p.depth(), 1);
        assert!(p.post(3).is_ok());
        assert!(matches!(p.post(3), Err(PipelineError::Full)));
    }

    #[test]
    fn the_default_window_matches_the_constant() {
        assert_eq!(SendPipeline::with_default_depth().depth(), PIPELINE_DEPTH);
    }

    #[test]
    fn zero_copy_records_the_region_and_does_not_touch_it() {
        let mut hook = RecordingZeroCopy::default();
        let region = RegisteredRegion {
            addr: 0x1000,
            length: 64,
            lkey: 7,
        };
        let status = hook.submit(region, 8, 16).unwrap();
        assert_eq!(status, ZeroCopyStatus::PendingRuntime);
        assert_eq!(
            hook.requests,
            vec![ZeroCopyRequest {
                region,
                offset: 8,
                len: 16,
            }]
        );
        assert!(matches!(
            hook.submit(region, 60, 8),
            Err(ZeroCopyError::OutOfRegion)
        ));
        assert!(matches!(
            hook.submit(region, 0, 0),
            Err(ZeroCopyError::ZeroLength)
        ));
        assert_eq!(hook.requests.len(), 1);
        assert!(!runtime_enabled());
    }
}
