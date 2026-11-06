# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""vLLM-style KV connector shape over `KVAdapter`; no vLLM dependency and not tested against vLLM.

The serving engine hands over opaque serialized bytes per token block. This class maps each full
block to its chained prefix digest, splits a block larger than the 4 MiB service limit into
parts, and loads only the longest unbroken cached prefix. It performs no tensor conversion.
"""
import struct

from .client import prefix_digests
from .core import CHUNK, DataError, require, sha

SINGLE, MULTI = b"\x00", b"\x01"
PART = CHUNK - 1  # one tag byte of overhead per stored value


class KVConnector:
    def __init__(self, adapter, block_size, ttl=3600, max_block_bytes=64 * 1024**2):
        require(isinstance(block_size, int) and 0 < block_size <= 4096, "block_size must be 1..4096")
        self.adapter, self.block_size, self.ttl, self.max_block_bytes = adapter, block_size, ttl, max_block_bytes
        self.stats = {"saved_blocks": 0, "hit_blocks": 0, "miss_blocks": 0}

    def digests(self, token_ids):
        return prefix_digests(list(token_ids), self.block_size)

    def save(self, token_ids, blocks):
        """Store one serialized block per full token block. Returns the digests stored.

        Everything is validated before the first write, so an oversize block stores nothing.
        """
        digests = self.digests(token_ids)
        require(len(blocks) == len(digests), "one serialized block per full token block")
        for block in blocks:
            require(isinstance(block, (bytes, bytearray)) and 0 < len(block) <= self.max_block_bytes,
                    "block must be 1 byte to max_block_bytes", 413)
        for digest, block in zip(digests, blocks):
            block = bytes(block)
            if len(block) <= PART:
                self.adapter.put(digest, SINGLE + block, self.ttl)
            else:
                parts = [block[i:i + PART] for i in range(0, len(block), PART)]
                for index, part in enumerate(parts):
                    self.adapter.put(self._part(digest, index), SINGLE + part, self.ttl)
                self.adapter.put(digest, MULTI + struct.pack(">I", len(parts)), self.ttl)
            self.stats["saved_blocks"] += 1
        return digests

    def load(self, token_ids):
        """Longest unbroken cached prefix: (matched_tokens, blocks). The engine prefills the rest."""
        blocks = []
        for digest in self.digests(token_ids):
            block = self._read(digest)
            if block is None:
                self.stats["miss_blocks"] += 1
                break
            blocks.append(block)
            self.stats["hit_blocks"] += 1
        return len(blocks) * self.block_size, blocks

    def _part(self, digest, index):
        return sha(f"{digest}:{index}".encode())

    def _read(self, digest):
        head = self.adapter.get(digest)
        if head is None:
            return None
        if head[:1] == SINGLE:
            return head[1:]
        if head[:1] != MULTI or len(head) != 5:
            raise DataError("corrupt KV block header", 422)
        out = []
        for index in range(struct.unpack(">I", head[1:])[0]):
            part = self.adapter.get(self._part(digest, index))
            if part is None or part[:1] != SINGLE:
                return None  # a lost part is a miss, not an error
            out.append(part[1:])
        return b"".join(out)
