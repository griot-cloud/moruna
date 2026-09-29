#!/usr/bin/env python3
"""Conventions and link checker for the Moruna documentation site.

The site is built with Sphinx, and CI builds it with warnings as errors, which catches
broken cross-references. This script is the fast half that needs no Sphinx installed, so
the pre-commit gate (tools/quality/check.sh) can run it on every commit:

  em-dash       an em dash in any page under docs/
  tab           a tab character in any page under docs/ (.editorconfig)
  encoding      a page under docs/ that is not valid UTF-8
  orphan        a page no toctree lists, which Sphinx would build but never link to
  link-target   a relative link to a .md page that does not exist

Usage:
  tools/docs/check_docs.py              check the docs/ directory of this repository
  tools/docs/check_docs.py DIR          check the site rooted at DIR
  tools/docs/check_docs.py --self-test  prove each failure above is detected

Exit status: 0 clean, 1 findings reported, 2 bad input.
"""

from __future__ import annotations

import re
import sys
import tempfile
from pathlib import Path

EM_DASH = "\u2014"
SKIP_DIRS = {"_build", "_static", "assets"}
TOCTREE = re.compile(r"```\{toctree\}(.*?)```", re.S)
LINK = re.compile(r"\]\(([^)#\s]+\.md)(#[^)\s]*)?\)")


def pages(root: Path) -> list[Path]:
    return sorted(
        p for p in root.rglob("*.md") if not SKIP_DIRS.intersection(p.relative_to(root).parts)
    )


def toctree_entries(text: str) -> list[str]:
    entries = []
    for block in TOCTREE.findall(text):
        for line in block.splitlines():
            line = line.strip()
            if line and not line.startswith(":"):
                entries.append(line)
    return entries


def check(root: Path) -> list[str]:
    findings: list[str] = []
    listed = {"index"}
    texts: dict[Path, str] = {}
    for page in pages(root):
        rel = page.relative_to(root)
        try:
            text = page.read_bytes().decode("utf-8")
        except UnicodeDecodeError:
            findings.append(f"encoding: {rel} is not valid UTF-8")
            continue
        texts[page] = text
        for number, line in enumerate(text.splitlines(), 1):
            if EM_DASH in line:
                findings.append(f"em-dash: {rel}:{number}: use a comma, a colon or parentheses")
            if "\t" in line:
                findings.append(f"tab: {rel}:{number}")
        base = rel.parent
        listed.update(str((base / entry).as_posix()) for entry in toctree_entries(text))
    for page, text in texts.items():
        rel = page.relative_to(root)
        name = rel.with_suffix("").as_posix()
        if name not in listed:
            findings.append(f"orphan: {rel} is in no toctree")
        for target, _anchor in LINK.findall(text):
            if "://" in target:
                continue
            if not (page.parent / target).exists():
                findings.append(f"link-target: {rel} links to {target}, which does not exist")
    return findings


def self_test() -> int:
    cases = {
        "em-dash": {"index.md": "# Home\n\na " + EM_DASH + " b\n"},
        "tab": {"index.md": "# Home\n\n\tindented\n"},
        "orphan": {"index.md": "# Home\n", "lost.md": "# Lost\n"},
        "link-target": {"index.md": "# Home\n\nSee [x](missing.md).\n"},
        "encoding": {"index.md": b"# Home\n\xff\xfe\n"},
    }
    failed = 0
    for rule, files in cases.items():
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name, body in files.items():
                path = root / name
                if isinstance(body, bytes):
                    path.write_bytes(body)
                else:
                    path.write_text(body, encoding="utf-8")
            found = check(root)
            if not any(f.startswith(rule + ":") for f in found):
                print(f"self-test: {rule} was not detected (got {found})")
                failed += 1
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "index.md").write_text(
            "# Home\n\n[a](a.md)\n\n```{toctree}\n:hidden:\n\na\n```\n", encoding="utf-8"
        )
        (root / "a.md").write_text("# A\n", encoding="utf-8")
        if check(root):
            print(f"self-test: a clean site reported {check(root)}")
            failed += 1
    print("self-test: ok" if not failed else f"self-test: {failed} failed")
    return 1 if failed else 0


def main(argv: list[str]) -> int:
    if argv[:1] == ["--self-test"]:
        return self_test()
    root = Path(argv[0]) if argv else Path(__file__).resolve().parents[2] / "docs"
    if not (root / "index.md").is_file():
        print(f"check_docs: {root} has no index.md", file=sys.stderr)
        return 2
    findings = check(root)
    for finding in findings:
        print(finding)
    if findings:
        return 1
    print(f"check_docs: ok ({len(pages(root))} pages under {root}, no findings)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
