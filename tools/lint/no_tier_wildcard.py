#!/usr/bin/env python3
"""Scanner behind tools/lint/no_tier_wildcard.sh (contracts CT-T14, CT-I11).

Reads Rust source files and reports every `match` whose arms name a `Tier` or a
`StagingCodec` variant and which also has a `_ =>` arm. Comments, string and
character literals are blanked before scanning so text inside them cannot
trigger or hide a hit; brace depth is tracked so a wildcard arm of a nested,
unrelated `match` is not charged to the enclosing one.

Usage: no_tier_wildcard.py <file.rs>...   (exit 1 on any hit, 0 otherwise)
"""
import re
import sys

VARIANT = re.compile(r"\b(Tier|StagingCodec)::")
WILDCARD = re.compile(r"^_\s*(if\b.*)?$")


def blank_literals(src: str) -> str:
    """Replace comments, strings and char literals with spaces, keeping newlines."""
    out = []
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        two = src[i : i + 2]
        if two == "//":
            j = src.find("\n", i)
            j = n if j < 0 else j
            out.append(" " * (j - i))
            i = j
        elif two == "/*":
            depth, j = 1, i + 2
            while j < n and depth:
                if src[j : j + 2] == "/*":
                    depth, j = depth + 1, j + 2
                elif src[j : j + 2] == "*/":
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            out.append("".join("\n" if ch == "\n" else " " for ch in src[i:j]))
            i = j
        elif c == '"' or (c in "br" and src[i : i + 2] in ('b"', 'r"', "r#")):
            # raw string: r"...", r#"..."#, br"...", br#"..."#
            m = re.match(r'b?r(#*)"', src[i:])
            if m:
                hashes = m.group(1)
                start = i + m.end()
                j = src.find('"' + hashes, start)
                j = n if j < 0 else j + 1 + len(hashes)
            else:
                start = i + (2 if c == "b" else 1)
                j = start
                while j < n and src[j] != '"':
                    j += 2 if src[j] == "\\" else 1
                j = min(j + 1, n)
            out.append("".join("\n" if ch == "\n" else " " for ch in src[i:j]))
            i = j
        elif c == "'" and (m := re.match(r"'(\\.|\\x[0-9a-fA-F]{2}|\\u\{[0-9a-fA-F]+\}|[^\\'])'", src[i:])):
            out.append(" " * m.end())
            i += m.end()
        else:
            out.append(c)
            i += 1
    return "".join(out)


def match_blocks(text: str):
    """Yield (line, block_text) for every `match` expression's arm block."""
    for m in re.finditer(r"\bmatch\b", text):
        i, depth = m.end(), 0
        n = len(text)
        while i < n:
            ch = text[i]
            if ch in "([":
                depth += 1
            elif ch in ")]":
                depth -= 1
            elif ch == "{" and depth == 0:
                break
            elif ch == ";" and depth == 0:
                i = n
            i += 1
        if i >= n:
            continue
        start, depth, j = i, 0, i
        while j < n:
            if text[j] == "{":
                depth += 1
            elif text[j] == "}":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        yield text.count("\n", 0, m.start()) + 1, text[start + 1 : j]


def arm_patterns(block: str):
    """Patterns of the arms at the top level of a match block."""
    depth, cur, i, n = 0, [], 0, len(block)
    while i < n:
        ch = block[i]
        if ch in "([{":
            depth += 1
            cur.append(ch)
        elif ch in ")]}":
            depth -= 1
            cur.append(ch)
            if depth == 0 and ch == "}":
                cur = []
        elif depth == 0 and block[i : i + 2] == "=>":
            yield "".join(cur).strip()
            cur = []
            i += 1
        elif depth == 0 and ch == ",":
            cur = []
        else:
            cur.append(ch)
        i += 1


def scan(path: str):
    with open(path, encoding="utf-8") as fh:
        text = blank_literals(fh.read())
    hits = []
    for line, block in match_blocks(text):
        pats = list(arm_patterns(block))
        if any(VARIANT.search(p) for p in pats) and any(WILDCARD.match(p) for p in pats):
            hits.append(line)
    return hits


def main(argv):
    bad = 0
    for path in argv[1:]:
        for line in scan(path):
            print(f"{path}:{line}: wildcard `_ =>` arm in a match over Tier or StagingCodec (CT-I11)")
            bad += 1
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
