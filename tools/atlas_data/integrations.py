# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Explicit orchestration contracts; these adapters do not assume a Gryvia API schema."""
import time

from .core import DataError, require


class DataReadyGate:
    """Use from Gryvia/Kubernetes submission hooks before starting expensive compute.

    The trusted node worker must separately stage each reservation. This gate
    returns a node selection only while readiness is verified and the lease is live.
    """
    def __init__(self, client):
        self.client = client

    def reserve(self, job, artifacts, labels=None):
        return self.client.call("jobs.reserve", job=job, artifacts=artifacts, labels=labels or {})

    def ready(self, job):
        status = self.client.call("jobs.status", job=job)
        require(status["data_ready"], "data not ready; stage artifacts before launching compute", 409)
        return {"node_selector": {"kubernetes.io/hostname": status["node"]},
                "artifact_ids": status["artifacts"], "lease_expires_at": status["expires_at"]}

    def wait_ready(self, job, timeout=600, interval=5, sleep=time.sleep, clock=time.monotonic):
        """Block until `ready()` succeeds, for a Kubernetes init container or a submission hook.

        Only "data not ready" (HTTP 409) is retried; every other error, such as a refused
        credential or an unknown job, surfaces at once. Raises a 504 `DataError` after `timeout`
        seconds, so the pod fails visibly instead of starting compute without its data.
        """
        require(timeout > 0 and interval > 0, "timeout and interval must be positive")
        deadline = clock() + timeout
        while True:
            try:
                return self.ready(job)
            except DataError as exc:
                if exc.status != 409:
                    raise
                if clock() + interval > deadline:
                    raise DataError(f"timed out after {timeout}s waiting for job {job} data", 504) from exc
            sleep(interval)

    def renew(self, job, ttl=300):
        return self.client.call("jobs.renew", job=job, ttl=ttl)

    def release(self, job):
        return self.client.call("jobs.release", job=job)


class CostCollector:
    """Trusted byte counters -> idempotent estimate ledger. Rates are configured by an operator."""
    def __init__(self, client):
        self.client = client

    def transfer(self, job, event_id, bytes_transferred):
        return self.client.call("costs.meter", event=event_id, job=job,
                                meter="transfer_bytes", units=bytes_transferred)

    def checkpoint(self, job, event_id, bytes_written):
        return self.client.call("costs.meter", event=event_id, job=job,
                                meter="checkpoint_bytes", units=bytes_written)
