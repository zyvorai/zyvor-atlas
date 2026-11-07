# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Job controller and node agent for data-ready scheduling.

`GryviaAdapter` is OUR interface, not Gryvia's API. `FakeGryvia` is the in-memory test double;
`gryvia_adapter.HttpGryvia` implements it against Gryvia's REST gateway (contract-tested against a
local fake only, never a live Gryvia).
"""
import contextlib
import time

from .core import DataError, require
from .integrations import CostCollector, DataReadyGate

TERMINAL = ("succeeded", "failed", "cancelled")


class GryviaAdapter:
    """Minimal contract the controller needs from a job scheduler. Implement per real system."""

    def apply_node_selector(self, job, node_selector):
        """Constrain the job's placement to `node_selector` (label -> value) before it starts."""
        raise NotImplementedError

    def job_state(self, job):
        """One of "pending", "running", "succeeded", "failed", "cancelled"."""
        raise NotImplementedError

    def report(self, job, message):
        """Surface a data-path problem (stage timeout, lost lease) on the job."""
        raise NotImplementedError


class FakeGryvia(GryviaAdapter):
    """In-memory adapter for tests: scripted states (last one repeats), records selectors/reports."""

    def __init__(self, states=None):
        self.states = {}
        self.script = {k: list(v) for k, v in (states or {}).items()}
        self.selectors, self.reports = {}, []

    def apply_node_selector(self, job, node_selector):
        self.selectors[job] = dict(node_selector)

    def job_state(self, job):
        queue = self.script.get(job)
        if queue:
            self.states[job] = queue.pop(0) if len(queue) > 1 else queue[0]
        return self.states.get(job, "pending")

    def report(self, job, message):
        self.reports.append((job, message))


class JobController:
    """submit -> schedule -> stage on node -> wait ready -> node selector -> lease renew -> release.

    `trigger(node, job)` asks the chosen node's agent to stage (in-process call, queue or RPC;
    the transport is the caller's). The reservation is released when the job ends or any step
    fails, so a failed admit does not pin cache capacity until the TTL.
    """

    def __init__(self, client, gryvia, trigger, lease_ttl=300, ready_timeout=600, ready_interval=5):
        self.gate = DataReadyGate(client)
        self.gryvia, self.trigger = gryvia, trigger
        self.lease_ttl, self.ready_timeout, self.ready_interval = lease_ttl, ready_timeout, ready_interval

    def admit(self, job, artifacts, labels=None, sleep=time.sleep, clock=time.monotonic):
        """Reserve, stage and gate one job; returns the ready selection already given to Gryvia."""
        decision = self.gate.reserve(job, artifacts, labels)
        try:
            self.trigger(decision["node"], job)
            ready = self.gate.wait_ready(job, self.ready_timeout, self.ready_interval, sleep, clock)
            self.gryvia.apply_node_selector(job, ready["node_selector"])
            return ready
        except BaseException as exc:
            self._release(job)
            self.gryvia.report(job, f"data not ready: {exc}")
            raise

    def supervise(self, job, interval=30, sleep=time.sleep, max_checks=None):
        """Renew the lease while the job is live; release when it ends. Returns the final state."""
        require(interval > 0, "interval must be positive")
        checks = 0
        try:
            while True:
                state = self.gryvia.job_state(job)
                if state in TERMINAL:
                    return state
                checks += 1
                require(max_checks is None or checks <= max_checks, "supervision check limit reached", 504)
                try:
                    self.gate.renew(job, self.lease_ttl)
                except DataError as exc:
                    self.gryvia.report(job, f"data lease lost: {exc}")
                    raise
                sleep(interval)
        finally:
            self._release(job)

    def run(self, job, artifacts, labels=None, interval=30, sleep=time.sleep, clock=time.monotonic):
        ready = self.admit(job, artifacts, labels, sleep, clock)
        return ready, self.supervise(job, interval, sleep)

    def _release(self, job):
        with contextlib.suppress(DataError):  # already expired/released is the desired end state
            self.gate.release(job)


class NodeAgent:
    """Per-node trusted worker (admin credential): periodic heartbeat plus job staging with metering."""

    def __init__(self, client, node, cache, capacity, bandwidth, labels=None, collector=None):
        self.client, self.node, self.cache = client, node, cache
        self.capacity, self.bandwidth, self.labels = capacity, bandwidth, dict(labels or {})
        self.collector = collector if collector is not None else CostCollector(client)
        self.meter_errors, self.errors = [], []

    def heartbeat(self):
        return self.client.call("nodes.heartbeat", node=self.node, capacity=self.capacity,
                                bandwidth=self.bandwidth, labels=self.labels)

    def stage_job(self, job):
        """Stage a reserved job's artifacts, then meter transferred and missed bytes (best effort).

        A metering failure (e.g. no rate configured) is recorded in `meter_errors` and never
        fails staging.
        """
        stats = {}
        status = self.client.stage(job, self.node, self.cache, stats)
        for meter, units in (("transfer", stats["network_bytes"]), ("cache_miss", stats["miss_bytes"])):
            if units:
                event = f"stage.{meter}.{job}.{self.node}.{units}"
                try:
                    (self.collector.transfer if meter == "transfer" else self.collector.cache_miss)(job, event, units)
                except DataError as exc:
                    self.meter_errors.append((meter, str(exc)))
        return {**status, "stage_stats": stats}

    def run(self, stop, interval=30, pending=lambda: ()):
        """Loop until `stop` (threading.Event) is set: heartbeat, then stage each job `pending()` lists.

        A failed heartbeat or stage is recorded in `errors` and retried next tick; while the
        service is unreachable the node goes stale and the scheduler skips it.
        """
        require(interval > 0, "interval must be positive")
        while not stop.is_set():
            try:
                self.heartbeat()
                for job in pending():
                    self.stage_job(job)
            except DataError as exc:
                self.errors.append(str(exc))
            stop.wait(interval)
