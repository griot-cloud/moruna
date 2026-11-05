"""What a task Moruna builds is made of (0.6.3): its import closure, found statically, and its
build key, which docstrings, comments and formatting do not change and code does. No engine,
no network."""

from __future__ import annotations

import textwrap
from pathlib import Path

import builder_support as support
import pytest

closure = support.load("_closure.py", "moruna_closure")


def write(root: Path, files: dict[str, str]) -> None:
    for name, text in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(text))


@pytest.fixture
def project(tmp_path: Path) -> Path:
    write(
        tmp_path,
        {
            "pipeline/__init__.py": "",
            "pipeline/tasks.py": '''
                """The pipeline's tasks."""
                import json
                import moruna
                from . import shared
                from .rules import severity

                class Enrich(moruna.Function):
                    """Adds the rule's severity."""

                    def __call__(self, batch):
                        return shared.tag(batch, severity("x"))
            ''',
            "pipeline/shared.py": "import pyarrow as pa\n\ndef tag(batch, level):\n    return batch\n",
            "pipeline/rules/__init__.py": "from .levels import severity\n",
            "pipeline/rules/levels.py": "def severity(name):\n    return 3\n",
            "pipeline/unused.py": "x = 1\n",
            "notes/readme.txt": "not code\n",
        },
    )
    return tmp_path


def code(project: Path, files=()):
    return closure.closure("pipeline.tasks", project / "pipeline" / "tasks.py", files)


def key(found) -> str:
    return closure.build_key(
        found, "pipeline.tasks:Enrich", lock="sha256:" + "1" * 64, runtime="r", shim="s",
        platform="linux/aarch64",
    )  # fmt: skip


def test_the_closure_is_the_modules_the_task_imports(project: Path) -> None:
    """Its module, every project module it imports (relative and absolute, transitively) and
    the packages' __init__.py, and nothing else of the project; third-party names by import."""
    found = code(project)
    assert sorted(found.files) == [
        "pipeline/__init__.py",
        "pipeline/rules/__init__.py",
        "pipeline/rules/levels.py",
        "pipeline/shared.py",
        "pipeline/tasks.py",
    ]
    assert found.root == project.resolve()
    assert found.imports == ("pyarrow",), "json is the standard library's, moruna the shim's"


def test_an_import_a_build_cannot_follow_is_refused_by_name(project: Path) -> None:
    write(project, {"pipeline/shared.py": "import importlib\nm = importlib.import_module(NAME)\n"})
    with pytest.raises(closure.Unresolved, match=r"pipeline.shared line 2: import_module"):
        code(project)
    write(project, {"pipeline/shared.py": "import no_such_distribution_anywhere\n"})
    with pytest.raises(closure.Unresolved, match="`no_such_distribution_anywhere` is neither"):
        code(project)
    write(project, {"pipeline/shared.py": "from . import missing_module\nimport pipeline.gone\n"})
    with pytest.raises(closure.Unresolved, match="`pipeline.gone` is not a module"):
        code(project)


def test_an_optional_import_is_followed_when_it_resolves(project: Path) -> None:
    write(
        project,
        {
            "pipeline/shared.py": """
                try:
                    import no_such_distribution_anywhere
                except ImportError:
                    no_such_distribution_anywhere = None
                from typing import TYPE_CHECKING
                if TYPE_CHECKING:
                    import also_missing
                import importlib
                try:
                    importlib.import_module("pipeline.unused")
                except ImportError:
                    pass
            """
        },
    )
    found = code(project)
    assert "pipeline/unused.py" in found.files, "a constant name is followed"
    assert found.imports == ()


def test_a_docstring_comment_or_layout_edit_keeps_the_key_and_code_changes_it(
    project: Path,
) -> None:
    before = key(code(project))
    tasks = project / "pipeline" / "tasks.py"
    original = tasks.read_text()
    tasks.write_text(
        original.replace('"""Adds the rule\'s severity."""', '"""Adds the severity, again."""')
        .replace("import json", "import json  # a comment")
        + "\n\n# the end\n"
    )
    assert key(code(project)) == before, "docstrings, comments and blank lines are not code"
    tasks.write_text(original.replace('severity("x")', 'severity("y")'))
    assert key(code(project)) != before, "a code edit is a new key"
    tasks.write_text(original)
    (project / "pipeline" / "rules" / "levels.py").write_text("def severity(name):\n    return 4\n")
    assert key(code(project)) != before, "an edit anywhere in the closure is a new key"


def test_data_files_travel_by_their_bytes(project: Path) -> None:
    data = project / "pipeline" / "model.json"
    data.write_text('{"a": 1}')
    with_data = code(project, [data])
    assert "pipeline/model.json" in with_data.data
    first = key(with_data)
    data.write_text('{"a": 2}')
    assert key(code(project, [data])) != first
    with pytest.raises(closure.Unresolved, match="not under"):
        code(project, [Path("/etc/hosts")])


def test_the_key_names_every_input(project: Path) -> None:
    found = code(project)
    base = key(found)
    for change in (
        {"lock": "sha256:" + "2" * 64}, {"runtime": "other"}, {"shim": "other"},
        {"platform": "linux/x86_64"},
    ):  # fmt: skip
        options = {
            "lock": "sha256:" + "1" * 64, "runtime": "r", "shim": "s",
            "platform": "linux/aarch64", **change,
        }  # fmt: skip
        assert closure.build_key(found, "pipeline.tasks:Enrich", **options) != base, change
    assert closure.build_key(
        found, "pipeline.tasks:Other", lock="sha256:" + "1" * 64, runtime="r", shim="s",
        platform="linux/aarch64",
    ) != base  # fmt: skip


def test_provenance_comes_from_git_with_no_credential(project: Path) -> None:
    import subprocess  # noqa: PLC0415

    found = code(project)
    assert closure.provenance(found).revision is None, "outside a repository nothing is known"
    run = lambda *a: subprocess.run(["git", "-C", str(project), *a], check=True, capture_output=True)  # noqa: E731
    run("init", "-q")
    run("-c", "user.name=t", "-c", "user.email=t@t", "add", ".")
    run("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "x")
    run("remote", "add", "origin", "https://user:secret@example.com/team/pipeline.git")
    clean = closure.provenance(found)
    assert clean.source == "https://example.com/team/pipeline.git"
    assert len(clean.revision or "") == 40 and clean.dirty is False
    (project / "pipeline" / "shared.py").write_text("x = 2\n")
    assert closure.provenance(found).dirty is True
    (project / "notes" / "readme.txt").write_text("changed\n")
    assert closure.stripped_tree('"""d"""\nx = 1\n', "t") == closure.stripped_tree("x=1", "t")
