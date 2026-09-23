#!/usr/bin/env python3
"""Conventions and link checker for the Moruna documentation book (BOARD.md F7.1).

mdBook has no built-in link check, so this script is the one. It is run by
tools/quality/check.sh and by .github/workflows/docs.yml, and it enforces the
conventions of architecture/README.md over docs/ plus the structure mdBook
needs:

  em-dash               an em dash in any file under docs/
  tab                   a tab character in any file under docs/ (.editorconfig)
  encoding              a file under docs/ that is not valid UTF-8
  no-summary            docs/src/SUMMARY.md is missing
  summary-missing-file  SUMMARY.md lists a page that has no file
  unlisted-file         a markdown file under docs/src/ that SUMMARY.md omits
  link-target           a markdown link whose target file is not in the book
  link-anchor           a markdown link whose anchor is not in the target page
  missing-cites         a page that still holds a placeholder and has no cites

A page holds a placeholder while it says "Filled by F7.x"; such a page must also
carry a line with the word "cites", naming the design sections the feature that
fills it must cite. Once the feature replaces the placeholder paragraph the rule
stops applying to that page.

Usage:
  tools/docs/check_docs.py                check the docs/ directory of this repo
  tools/docs/check_docs.py DIR            check the book rooted at DIR
  tools/docs/check_docs.py --self-test    prove each failure above is detected

Exit status: 0 clean, 1 findings reported, 2 bad input (a clear message, never a
traceback).
"""

from __future__ import annotations

import re
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

EM_DASH = chr(0x2014)
TAB = "\t"

# The build output and editor droppings are not documentation sources.
SKIP_DIRS = {"book", "target", ".git", "node_modules"}
# Files whose bytes are not text; they are not read for conventions.
BINARY_SUFFIXES = {".png", ".jpg", ".jpeg", ".gif", ".ico", ".woff", ".woff2", ".ttf", ".pdf"}

PLACEHOLDER = re.compile(r"\bFilled by (F\d+\.\d+)\b", re.IGNORECASE)
CITES = re.compile(r"\bcites\b", re.IGNORECASE)
# [text](target) and [text]: target, the two link forms the book uses.
INLINE_LINK = re.compile(r"!?\[[^\]\n]*\]\(\s*<?([^)\s>]+)>?(?:\s+\"[^\"]*\")?\s*\)")
REF_LINK = re.compile(r"^\s{0,3}\[[^\]\n]+\]:\s*<?([^\s>]+)>?", re.MULTILINE)
HEADING = re.compile(r"^(#{1,6})\s+(.*?)\s*$", re.MULTILINE)
EXPLICIT_ID = re.compile(r"(?:id|name)\s*=\s*[\"']([^\"']+)[\"']")
HEADING_ATTR = re.compile(r"\{#([^}\s]+)\}\s*$")
FENCE = re.compile(r"^\s{0,3}(```+|~~~+)")
EXTERNAL = re.compile(r"^(?:[a-z][a-z0-9+.-]*:|//)", re.IGNORECASE)


class DocsError(Exception):
    """Bad input: the caller is told what is wrong and the script exits 2."""


@dataclass(frozen=True)
class Finding:
    code: str
    where: str
    message: str

    def render(self) -> str:
        return f"check_docs: FAIL [{self.code}] {self.where}: {self.message}"


def repo_root() -> Path:
    """The git top level, or the current directory when git is unavailable."""
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        return Path.cwd()
    if out.returncode == 0 and out.stdout.strip():
        return Path(out.stdout.strip())
    return Path.cwd()


def walk_files(root: Path) -> list[Path]:
    """Every file under root, skipping build output and version control."""
    found: list[Path] = []
    for path in sorted(root.rglob("*")):
        if any(part in SKIP_DIRS or part.startswith(".") for part in path.relative_to(root).parts):
            continue
        if path.is_file():
            found.append(path)
    return found


def read_text(path: Path, root: Path) -> tuple[str | None, Finding | None]:
    """Read a file as UTF-8, or return the finding that says it is not text."""
    rel = path.relative_to(root).as_posix()
    try:
        return path.read_text(encoding="utf-8"), None
    except UnicodeDecodeError:
        return None, Finding("encoding", rel, "not valid UTF-8; documentation sources are UTF-8 text")
    except OSError as exc:
        return None, Finding("encoding", rel, f"cannot be read: {exc.strerror or exc}")


def strip_code(text: str) -> str:
    """Blank fenced blocks and inline code spans, keeping line numbering."""
    lines = text.splitlines()
    out: list[str] = []
    fence: str | None = None
    for line in lines:
        match = FENCE.match(line)
        if fence is None and match:
            fence = match.group(1)[0] * 3
            out.append("")
            continue
        if fence is not None:
            if match and match.group(1).startswith(fence):
                fence = None
            out.append("")
            continue
        out.append(re.sub(r"`[^`\n]*`", lambda m: " " * len(m.group(0)), line))
    return "\n".join(out)


def normalise_id(text: str) -> str:
    """The anchor mdBook derives from a heading: lowercase, spaces to dashes."""
    attr = HEADING_ATTR.search(text)
    if attr:
        return attr.group(1).lower()
    out: list[str] = []
    for ch in text.strip().lower():
        if ch.isalnum() or ch in "_-":
            out.append(ch)
        elif ch in " \t":
            out.append("-")
    return "".join(out)


def anchors_of(text: str) -> set[str]:
    """Every anchor a page offers: its headings and its explicit ids."""
    anchors: set[str] = set()
    for _level, raw in HEADING.findall(strip_code(text)):
        base = normalise_id(raw)
        if not base:
            continue
        anchor, n = base, 0
        while anchor in anchors:
            n += 1
            anchor = f"{base}-{n}"
        anchors.add(anchor)
    anchors.update(match.lower() for match in EXPLICIT_ID.findall(text))
    return anchors


def summary_pages(summary: str) -> list[tuple[str, int]]:
    """The page paths SUMMARY.md lists, each with the line it is listed on."""
    pages: list[tuple[str, int]] = []
    body = strip_code(summary)
    for lineno, line in enumerate(body.splitlines(), start=1):
        for target in INLINE_LINK.findall(line):
            path = target.split("#", 1)[0].strip()
            if path and not EXTERNAL.match(path):
                pages.append((path, lineno))
    return pages


def line_of(text: str, index: int) -> int:
    return text.count("\n", 0, index) + 1


def check_conventions(docs: Path) -> list[Finding]:
    """No em dash, no tab, UTF-8 only, over every file under docs/."""
    findings: list[Finding] = []
    for path in walk_files(docs):
        if path.suffix.lower() in BINARY_SUFFIXES:
            continue
        text, problem = read_text(path, docs)
        if problem is not None:
            findings.append(problem)
            continue
        assert text is not None
        rel = path.relative_to(docs).as_posix()
        for lineno, line in enumerate(text.splitlines(), start=1):
            if EM_DASH in line:
                findings.append(
                    Finding(
                        "em-dash",
                        f"{rel}:{lineno}",
                        "em dash; use a comma, a colon or parentheses"
                        " (architecture/README.md, conventions)",
                    )
                )
            if TAB in line:
                findings.append(
                    Finding("tab", f"{rel}:{lineno}", "tab character; indent with spaces (.editorconfig)")
                )
    return findings


def check_structure(docs: Path) -> tuple[list[Finding], dict[Path, str]]:
    """SUMMARY.md and the files under src/ list each other, exactly."""
    src = docs / "src"
    findings: list[Finding] = []
    summary_path = src / "SUMMARY.md"
    if not summary_path.is_file():
        return [Finding("no-summary", "src/SUMMARY.md", "the book has no table of contents")], {}

    summary, problem = read_text(summary_path, docs)
    if problem is not None or summary is None:
        return [problem or Finding("encoding", "src/SUMMARY.md", "cannot be read")], {}

    listed: set[Path] = set()
    for page, lineno in summary_pages(summary):
        target = (src / page).resolve()
        try:
            rel = target.relative_to(src.resolve())
        except ValueError:
            findings.append(
                Finding("summary-missing-file", f"src/SUMMARY.md:{lineno}", f"{page} is outside src/")
            )
            continue
        listed.add(rel)
        if not target.is_file():
            findings.append(
                Finding("summary-missing-file", f"src/SUMMARY.md:{lineno}", f"{page} has no file")
            )

    texts: dict[Path, str] = {}
    for path in walk_files(src):
        if path.suffix.lower() != ".md":
            continue
        rel = path.resolve().relative_to(src.resolve())
        text, problem = read_text(path, docs)
        if problem is None and text is not None:
            texts[rel] = text
        if rel.as_posix() == "SUMMARY.md":
            continue
        if rel not in listed:
            findings.append(
                Finding(
                    "unlisted-file",
                    f"src/{rel.as_posix()}",
                    "no entry in SUMMARY.md, so mdBook will not render it",
                )
            )
    return findings, texts


def check_links(docs: Path, texts: dict[Path, str]) -> list[Finding]:
    """Every in-book link resolves to a file in the book and to an anchor in it."""
    src = (docs / "src").resolve()
    findings: list[Finding] = []
    anchors = {rel: anchors_of(text) for rel, text in texts.items()}
    for rel, text in sorted(texts.items()):
        body = strip_code(text)
        targets = [(m.group(1), line_of(body, m.start())) for m in INLINE_LINK.finditer(body)]
        targets += [(m.group(1), line_of(body, m.start())) for m in REF_LINK.finditer(body)]
        for raw, lineno in targets:
            target = raw.strip()
            if not target or EXTERNAL.match(target):
                continue
            path_part, _, anchor = target.partition("#")
            where = f"src/{rel.as_posix()}:{lineno}"
            if not path_part:
                page = rel
            else:
                resolved = (src / rel).parent.joinpath(path_part).resolve()
                try:
                    page = resolved.relative_to(src)
                except ValueError:
                    findings.append(
                        Finding("link-target", where, f"{target} points outside the book")
                    )
                    continue
                if not resolved.is_file():
                    findings.append(Finding("link-target", where, f"{target} has no file in the book"))
                    continue
                page = Path(page.as_posix())
            if anchor and page in anchors and anchor.lower() not in anchors[page]:
                findings.append(
                    Finding("link-anchor", where, f"{target} has no such heading in {page.as_posix()}")
                )
    return findings


def check_citations(texts: dict[Path, str]) -> list[Finding]:
    """A page that still holds a placeholder names the sections it must cite."""
    findings: list[Finding] = []
    for rel, text in sorted(texts.items()):
        if rel.as_posix() == "SUMMARY.md":
            continue
        placeholder = PLACEHOLDER.search(text)
        if placeholder and not CITES.search(text):
            findings.append(
                Finding(
                    "missing-cites",
                    f"src/{rel.as_posix()}",
                    f"placeholder for {placeholder.group(1)} with no cites line;"
                    " name the design sections the feature must cite",
                )
            )
    return findings


def check_book(docs: Path) -> list[Finding]:
    """Run every check over one book directory."""
    if not docs.is_dir():
        raise DocsError(f"{docs} is not a directory")
    if not (docs / "src").is_dir():
        raise DocsError(f"{docs} is not an mdBook: no src directory")
    findings = check_conventions(docs)
    structure, texts = check_structure(docs)
    findings += structure
    findings += check_links(docs, texts)
    findings += check_citations(texts)
    return findings


def report(docs: Path, findings: list[Finding], pages: int) -> int:
    for finding in findings:
        print(finding.render(), file=sys.stderr)
    if findings:
        print(f"check_docs: FAIL: {len(findings)} finding(s) in {docs}", file=sys.stderr)
        return 1
    print(f"check_docs: ok ({pages} pages under {docs}, no findings)")
    return 0


def materialise(fixture: Path, into: Path) -> Path:
    """Copy a fixture book, substituting the tokens the repository cannot store.

    tools/quality/check.sh fails on an em dash in any tracked file, so the em
    dash and tab fixtures hold the tokens {{EMDASH}} and {{TAB}} instead of the
    characters, and the self-test puts the characters back here.
    """
    dest = into / fixture.name
    shutil.copytree(fixture / "docs", dest / "docs")
    for path in walk_files(dest):
        try:
            text = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue
        swapped = text.replace("{{EMDASH}}", EM_DASH).replace("{{TAB}}", TAB)
        if swapped != text:
            path.write_text(swapped, encoding="utf-8")
    return dest / "docs"


def self_test(fixtures: Path) -> int:
    """Every bad fixture is rejected with its own code; every good one passes."""
    if not fixtures.is_dir():
        raise DocsError(f"no fixtures directory at {fixtures}")
    cases = sorted(p for p in fixtures.iterdir() if p.is_dir())
    if not cases:
        raise DocsError(f"no fixtures in {fixtures}")
    failures: list[str] = []
    rejected = accepted = 0
    with tempfile.TemporaryDirectory(prefix="moruna-check-docs-") as tmp:
        for case in cases:
            expect_file = case / "expect"
            if not (case / "docs").is_dir():
                failures.append(f"{case.name}: fixture has no docs/ directory")
                continue
            book = materialise(case, Path(tmp))
            try:
                findings = check_book(book)
            except DocsError as exc:
                failures.append(f"{case.name}: {exc}")
                continue
            codes = {f.code for f in findings}
            if case.name.startswith("good_"):
                accepted += 1
                if findings:
                    failures.append(f"{case.name}: expected no findings, got {sorted(codes)}")
                continue
            rejected += 1
            if not findings:
                failures.append(f"{case.name}: expected a finding, got none")
                continue
            if not expect_file.is_file():
                failures.append(f"{case.name}: fixture has no expect file naming its code")
                continue
            wanted = expect_file.read_text(encoding="utf-8").split()
            missing = [code for code in wanted if code not in codes]
            if missing:
                failures.append(f"{case.name}: expected {missing}, got {sorted(codes)}")

        # Bad input is reported, never raised at the user as a traceback.
        for bad, label in ((Path(tmp) / "no-such-book", "a missing directory"),
                           (Path(tmp), "a directory that is not a book")):
            try:
                check_book(bad)
            except DocsError:
                pass
            else:
                failures.append(f"bad input: {label} was accepted")

    for line in failures:
        print(f"check_docs: self-test FAILED: {line}", file=sys.stderr)
    if failures:
        return 1
    print(f"check_docs: self-test ok ({rejected} rejected, {accepted} accepted, bad input reported)")
    return 0


def main(argv: list[str]) -> int:
    args = argv[1:]
    if args and args[0] in {"-h", "--help"}:
        print(__doc__)
        return 0
    here = Path(__file__).resolve().parent
    try:
        if args and args[0] == "--self-test":
            if len(args) > 1:
                raise DocsError("--self-test takes no other argument")
            return self_test(here / "fixtures")
        if len(args) > 1:
            raise DocsError("at most one directory may be given")
        docs = Path(args[0]).resolve() if args else (repo_root() / "docs").resolve()
        findings = check_book(docs)
        pages = sum(
            1
            for p in walk_files(docs / "src")
            if p.suffix.lower() == ".md" and p.name != "SUMMARY.md"
        )
        return report(docs, findings, pages)
    except DocsError as exc:
        print(f"check_docs: bad input: {exc}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print("check_docs: interrupted", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
