"""Real SDK registration/run handoff and refusal of invented storage authority."""
from __future__ import annotations

from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import moruna


def test_engine_ports_cannot_mint_storage_authority_from_python() -> None:
    with pytest.raises(TypeError):
        moruna.EnginePorts()


def test_cost_session_refuses_empty_configuration(tmp_path: Path) -> None:
    with pytest.raises(ValueError, match="nonempty"):
        moruna.CostSession("")
    with pytest.raises(ValueError, match="nonempty"):
        moruna.CostSession(str(tmp_path), model="")
    invalid = moruna.CostSession(str(tmp_path / "unknown-model"), model="does-not-exist")
    with pytest.raises(moruna.ConfigError, match="registry"):
        invalid.refresh()


def test_registration_warms_the_actual_kernel_and_hands_the_same_session_to_run(tmp_path: Path) -> None:
    module = tmp_path / "registered_cleaner.py"
    module.write_text("import moruna\nclean = moruna.std.fill_null(values={'id': 0})\n")
    cost = moruna.CostSession(str(tmp_path / "observations"))
    registration = moruna.prepare_registration(str(module), cost=cost, budget=1.0)
    assert registration.cost is cost
    assert registration.report["exit"] == 0
    [checked] = registration.report["functions"]
    assert checked["verdict"] == "agreed"
    assert checked["warm_up"]["observations"] > 0
    assert checked["warm_up"]["consumed"]["wall_seconds"] > 0
    assert checked["warm_up"]["consumed"]["store_bytes"] > 0
    source = tmp_path / "input.parquet"
    output = tmp_path / "output"
    output.mkdir()
    pq.write_table(pa.table({"id": pa.array([19, None, 41], type=pa.int64())}), source)
    report = moruna.run(moruna.ParquetSource(str(source)), moruna.std.fill_null(values={"id": 0}),
                        moruna.ParquetSink(str(output)), cost=registration.cost)
    assert report.exit["status"] == "completed"
    assert pq.read_table(output).column("id").to_pylist() == [19, 0, 41]


def test_registration_never_publishes_an_uncheckable_target_or_disables_warmup(tmp_path: Path) -> None:
    module = tmp_path / "uncheckable.py"
    module.write_text("import moruna\n@moruna.function\ndef unknown(batch):\n    return batch\n")
    cost = moruna.CostSession(str(tmp_path / "observations"))
    with pytest.raises(moruna.RegistrationRefused) as refused:
        moruna.prepare_registration(str(module), cost=cost, budget=0.1)
    assert refused.value.report["exit"] != 0
    assert refused.value.report["functions"][0]["verdict"] != "agreed"
    with pytest.raises(TypeError, match="warm_up"):
        moruna.prepare_registration(str(module), cost=cost, warm_up=False)
