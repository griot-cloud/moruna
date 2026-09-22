"""`amoru.inspect_host()` (d.2, j): what the runtime would discover here, without starting it."""

from __future__ import annotations

import amoru


def test_inspect_host_reports_limits_and_profile() -> None:
    host = amoru.inspect_host()
    assert set(host) >= {"limits", "host_profile", "notes"}

    limits = host["limits"]
    assert limits["memory_ceiling"] > 0
    assert limits["cpu_quota"] > 0
    assert limits["page_bytes"] > 0
    assert limits["source"] in {"Cgroup", "Os", "Explicit"}
    assert isinstance(limits["devices"], list)

    profile = host["host_profile"]
    for field in ("huge_pages", "memlock", "io_uring", "direct_io_staging", "gds", "rdma"):
        assert profile[field] in {"present", "absent", "unknown"} or profile[field].startswith(
            "probed: "
        )
    assert isinstance(host["notes"], list)
