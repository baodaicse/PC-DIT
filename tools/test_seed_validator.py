#!/usr/bin/env python3
"""Cross-language known-answer tests for seed evidence retained in schema v9."""

from validate_session import derive_round_seed, parse_seed_hex, seed_commitment


SESSION_SEED = 0x7BC0239B31D734ED
TEST_ID = "CPU-DIT-01-SYSTEM-v1"
SOURCE_SHA256 = "b9cf106acdb9e550d6311cea214595a78a86dd8bcc612cf9ea5795cefc38c499"


def main() -> int:
    assert parse_seed_hex("0x7bc0239b31d734ed") == SESSION_SEED
    assert parse_seed_hex("7bc0239b31d734ed") is None
    assert derive_round_seed(SESSION_SEED, TEST_ID, 0) == 0x9EA085116157A55F
    assert derive_round_seed(SESSION_SEED, TEST_ID, 1) == 0x84D7FA32BD6E9DDD
    assert derive_round_seed(SESSION_SEED, TEST_ID, 0) != derive_round_seed(
        SESSION_SEED, TEST_ID, 1
    )
    assert (
        seed_commitment(TEST_ID, SOURCE_SHA256, SESSION_SEED)
        == "945162f0546c33201905435a963423f295e2a70028f5f0feb06437685637ea2a"
    )
    print("Schema-v9 seed validator known-answer tests: PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
