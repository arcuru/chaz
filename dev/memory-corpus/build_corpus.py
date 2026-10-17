#!/usr/bin/env python3
"""Build the memory-recall corpus fixture from the project's own docs.

Every level-2/3 heading in `docs/src/**/*.md` becomes one memory entry:

    key   = "<relative path>#<heading slug>"
    value = the section body, whitespace-collapsed and truncated
    tags  = [top-level docs dir, second-level dir or file stem]

Each entry also carries a `title_query`: the heading text on its own.
That gives a known-item retrieval set where the gold answer for a query
is the entry it came from, with no hand labelling.

Paraphrase queries — the ones written to avoid the document's own
vocabulary — are not generated here. They live in `paraphrase_queries.json`
and are merged into the fixture.

Usage (from the repo root):

    python3 dev/memory-corpus/build_corpus.py

Writes `crates/lib/src/tools/testdata/memory_corpus.json`.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
DOCS = REPO / "docs" / "src"
HERE = Path(__file__).resolve().parent
OUT = REPO / "crates" / "lib" / "src" / "tools" / "testdata" / "memory_corpus.json"

MIN_VALUE_CHARS = 80
MAX_VALUE_CHARS = 480
TARGET_ENTRIES = 200


def slug(text: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")


def collapse(body: list[str]) -> str:
    """Drop fenced code and tables, collapse the rest to one line."""
    out: list[str] = []
    fenced = False
    for line in body:
        if line.lstrip().startswith("```"):
            fenced = not fenced
            continue
        if fenced:
            continue
        stripped = line.strip()
        if not stripped or stripped.startswith("|") or stripped.startswith("<"):
            continue
        out.append(stripped)
    text = " ".join(out)
    text = re.sub(r"[*_`\[\]]", "", text)
    text = re.sub(r"\s+", " ", text).strip()
    return text[:MAX_VALUE_CHARS]


def sections(path: Path):
    heading = None
    body: list[str] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        m = re.match(r"^(#{2,3})\s+(.*)$", line)
        if m:
            if heading:
                yield heading, body
            heading, body = m.group(2).strip(), []
        elif heading:
            body.append(line)
    if heading:
        yield heading, body


def sample(entries: list[dict], target: int) -> list[dict]:
    """Round-robin across source files so no single doc dominates."""
    if len(entries) <= target:
        return entries
    by_file: dict[str, list[dict]] = {}
    for e in entries:
        by_file.setdefault(e["key"].split("#", 1)[0], []).append(e)
    picked: list[dict] = []
    depth = 0
    while len(picked) < target:
        added = 0
        for file in sorted(by_file):
            bucket = by_file[file]
            if depth < len(bucket):
                picked.append(bucket[depth])
                added += 1
                if len(picked) == target:
                    break
        if added == 0:
            break
        depth += 1
    return sorted(picked, key=lambda e: e["key"])


def main() -> int:
    entries = []
    seen: set[str] = set()
    for path in sorted(DOCS.rglob("*.md")):
        rel = path.relative_to(DOCS).as_posix()
        parts = rel.split("/")
        tags = [parts[0].replace(".md", "")]
        if len(parts) > 1:
            tags.append(parts[-1].removesuffix(".md"))
        for heading, body in sections(path):
            value = collapse(body)
            if len(value) < MIN_VALUE_CHARS:
                continue
            key = f"{rel}#{slug(heading)}"
            if key in seen:
                continue
            seen.add(key)
            entries.append(
                {
                    "key": key,
                    "value": value,
                    "tags": tags,
                    "title_query": heading,
                }
            )

    entries = sample(entries, TARGET_ENTRIES)
    if len(entries) < TARGET_ENTRIES:
        print(
            f"warning: only {len(entries)} entries (< {TARGET_ENTRIES})",
            file=sys.stderr,
        )

    paraphrases = json.loads((HERE / "paraphrase_queries.json").read_text())
    keys = {e["key"] for e in entries}
    missing = [q for q in paraphrases if q["gold"] not in keys]
    if missing:
        for q in missing:
            print(f"error: gold key not in corpus: {q['gold']}", file=sys.stderr)
        return 1

    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(
        json.dumps(
            {
                "source": "docs/src/**/*.md, one entry per level-2/3 heading",
                "generator": "dev/memory-corpus/build_corpus.py",
                "entries": entries,
                "paraphrase_queries": paraphrases,
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    print(f"{len(entries)} entries, {len(paraphrases)} paraphrase queries -> {OUT}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
