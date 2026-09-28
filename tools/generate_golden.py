#!/usr/bin/env python3
"""Generate CPU-DIT-01 source manifest and independent golden vector v1."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path

MASK64 = (1 << 64) - 1
MATRIX_WIDTH = 4
EXPANSION_MULTIPLIER = 0x9E3779B97F4A7C15


def gf_multiply(left: int, right: int) -> int:
    result = 0
    for _ in range(8):
        if right & 1:
            result ^= left
        high_bit = left & 0x80
        left = (left << 1) & 0xFF
        if high_bit:
            left ^= 0x1D
        right >>= 1
    return result


def reference_matrix() -> list[list[int]]:
    matrix: list[list[int]] = []
    for row in range(MATRIX_WIDTH):
        base = row + 1
        value = 1
        matrix_row = []
        for _ in range(MATRIX_WIDTH):
            matrix_row.append(value)
            value = gf_multiply(value, base)
        matrix.append(matrix_row)
    return matrix


def splitmix64(value: int) -> int:
    value = ((value ^ (value >> 30)) * 0xBF58476D1CE4E5B9) & MASK64
    value = ((value ^ (value >> 27)) * 0x94D049BB133111EB) & MASK64
    return value ^ (value >> 31)


def expanded_byte(source: bytes, seed: int, global_offset: int) -> int:
    word_index = global_offset // 8
    lane = (global_offset % 8) * 8
    counter = (seed + word_index * EXPANSION_MULTIPLIER) & MASK64
    mixed = splitmix64(counter)
    return source[global_offset % len(source)] ^ ((mixed >> lane) & 0xFF)


def groups(target_bytes: int, block_sizes: list[int]):
    global_offset = 0
    group_id = 0
    while global_offset < target_bytes:
        requested = block_sizes[group_id % len(block_sizes)]
        remaining = target_bytes - global_offset
        block_size = (
            (remaining + MATRIX_WIDTH - 1) // MATRIX_WIDTH
            if requested * MATRIX_WIDTH > remaining
            else requested
        )
        valid_bytes = min(remaining, block_size * MATRIX_WIDTH)
        yield group_id, global_offset, block_size, valid_bytes
        global_offset += valid_bytes
        group_id += 1


def generate_group(
    source: bytes, seed: int, global_offset: int, block_size: int, valid_bytes: int
) -> list[bytearray]:
    blocks = [bytearray(block_size) for _ in range(MATRIX_WIDTH)]
    remaining = valid_bytes
    for shard, block in enumerate(blocks):
        valid_in_block = min(remaining, block_size)
        shard_base = shard * block_size
        for offset in range(valid_in_block):
            block[offset] = expanded_byte(source, seed, global_offset + shard_base + offset)
        remaining -= valid_in_block
    return blocks


def matrix_apply(matrix: list[list[int]], blocks: list[bytearray]) -> list[bytearray]:
    block_size = len(blocks[0])
    output = [bytearray(block_size) for _ in range(MATRIX_WIDTH)]
    for row in range(MATRIX_WIDTH):
        for column in range(MATRIX_WIDTH):
            coefficient = matrix[row][column]
            for offset in range(block_size):
                output[row][offset] ^= gf_multiply(coefficient, blocks[column][offset])
    return output


def final_digest(
    test_id: str,
    source_sha256: str,
    seed: int,
    target_bytes: int,
    block_sizes: list[int],
    recovered_digest: bytes,
    encoded_digest: bytes,
) -> str:
    hasher = hashlib.sha256()
    hasher.update(b"CPU-DIT-01-FINAL-v1\0")
    hasher.update(test_id.encode("utf-8"))
    hasher.update(b"\0")
    hasher.update(source_sha256.lower().encode("ascii"))
    hasher.update(b"\0")
    hasher.update(seed.to_bytes(8, "little"))
    hasher.update(target_bytes.to_bytes(8, "little"))
    hasher.update(recovered_digest)
    hasher.update(encoded_digest)
    for block_size in block_sizes:
        hasher.update(block_size.to_bytes(8, "little"))
    return hasher.hexdigest()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", type=Path, default=Path("vectors/source-v1.dat"))
    parser.add_argument(
        "--manifest", type=Path, default=Path("vectors/source-v1.manifest.json")
    )
    parser.add_argument("--golden", type=Path, default=Path("vectors/golden-v1.json"))
    args = parser.parse_args()

    source = args.source.read_bytes()
    if not source:
        raise SystemExit("source must not be empty")

    test_id = "CPU-DIT-01-REF-v1"
    descriptor_version = 1
    seed = 0x0123456789ABCDEF
    target_bytes = 1_048_579
    block_sizes = [65_536, 131_071, 4_093]
    source_sha256 = hashlib.sha256(source).hexdigest()
    generated_hasher = hashlib.sha256()
    encoded_hasher = hashlib.sha256()
    matrix = reference_matrix()

    for _, global_offset, block_size, valid_bytes in groups(target_bytes, block_sizes):
        data = generate_group(source, seed, global_offset, block_size, valid_bytes)
        remaining = valid_bytes
        for block in data:
            valid = min(remaining, block_size)
            generated_hasher.update(block[:valid])
            remaining -= valid
        for block in matrix_apply(matrix, data):
            encoded_hasher.update(block)

    generated_digest = generated_hasher.digest()
    encoded_digest = encoded_hasher.digest()
    final_sha256 = final_digest(
        test_id,
        source_sha256,
        seed,
        target_bytes,
        block_sizes,
        generated_digest,
        encoded_digest,
    )

    manifest = {
        "schema_version": 1,
        "source_id": "cpu-dit-source-v1",
        "size_bytes": len(source),
        "sha256": source_sha256,
    }
    golden = {
        "schema_version": 1,
        "test_id": test_id,
        "descriptor_version": descriptor_version,
        "source_sha256": source_sha256,
        "seed_hex": f"0x{seed:016x}",
        "target_bytes": target_bytes,
        "block_sizes": block_sizes,
        "expected_generated_sha256": generated_digest.hex(),
        "expected_encoded_sha256": encoded_digest.hex(),
        "expected_final_sha256": final_sha256,
    }

    args.manifest.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    args.golden.write_text(json.dumps(golden, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
