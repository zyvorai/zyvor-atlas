# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Explicit orchestration contracts; these adapters do not assume a Gryvia API schema."""
from .core import require


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
