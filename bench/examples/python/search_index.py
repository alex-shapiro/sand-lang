"""A persisted inverted index for full-text search.

CPython port of examples/search_index.sand. The index file has the same JSON
layout as the one Sand's `json.encode` writes.

Usage:
    python3 search_index.py index INDEX_FILE DIR
    python3 search_index.py add INDEX_FILE FILE...
    python3 search_index.py search INDEX_FILE QUERY...
    python3 search_index.py stats INDEX_FILE

Documents are ranked by TF-IDF. All query terms are OR-ed, so documents
matching more (and rarer) terms rank higher.
"""

import json
import math
import os
import sys
from collections import defaultdict
from collections.abc import Iterator
from dataclasses import dataclass


@dataclass(slots=True)
class Posting:
    doc: int
    # Positions of the term within the document.
    positions: list[int]


@dataclass(slots=True)
class Doc:
    path: str
    length: int


class Index:
    __slots__ = ("version", "docs", "postings")

    def __init__(self, docs: list[Doc], postings: dict[str, list[Posting]], version: int = 1):
        self.version = version
        self.docs = docs
        self.postings = postings

    @staticmethod
    def empty() -> "Index":
        return Index([], {})

    @staticmethod
    def load(path: str) -> "Index":
        if not os.path.exists(path):
            return Index.empty()
        with open(path, encoding="utf-8") as f:
            data = json.load(f)
        return Index(
            docs=[Doc(d["path"], d["length"]) for d in data["docs"]],
            postings={
                term: [Posting(p["doc"], p["positions"]) for p in ps] for term, ps in data["postings"].items()
            },
            version=data.get("version", 1),
        )

    def to_json(self) -> dict:
        return {
            "version": self.version,
            "docs": [{"path": d.path, "length": d.length} for d in self.docs],
            "postings": {
                term: [{"doc": p.doc, "positions": p.positions} for p in ps] for term, ps in self.postings.items()
            },
        }

    def save(self, path: str) -> None:
        # Write to a temporary file first so a crash never leaves a torn index.
        tmp = path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            f.write(json.dumps(self.to_json(), separators=(",", ":"), ensure_ascii=False))
        os.replace(tmp, path)

    def add(self, path: str, text: str) -> None:
        # Re-indexing a document replaces its old postings.
        old = next((i for i, d in enumerate(self.docs) if d.path == path), None)
        if old is not None:
            self.remove_doc(old)
        doc_id = len(self.docs)
        tokens = tokenize(text)
        self.docs.append(Doc(path, len(tokens)))
        positions: defaultdict[str, list[int]] = defaultdict(list)
        for i, t in enumerate(tokens):
            positions[t].append(i)
        for term, ps in positions.items():
            self.postings.setdefault(term, []).append(Posting(doc_id, ps))

    def remove_doc(self, doc_id: int) -> None:
        for term, postings in list(self.postings.items()):
            kept = [p for p in postings if p.doc != doc_id]
            if not kept:
                del self.postings[term]
            else:
                self.postings[term] = kept
        # Keep ids stable by blanking the slot.
        self.docs[doc_id] = Doc("", 0)

    def live_docs(self) -> int:
        return sum(1 for d in self.docs if d.path != "")

    def search(self, query: str, limit: int = 10) -> list[tuple[str, float]]:
        n = float(self.live_docs())
        scores: dict[int, float] = {}
        for term in dict.fromkeys(tokenize(query)):
            postings = self.postings.get(term)
            if postings is None:
                continue
            idf = math.log(1.0 + n / len(postings))
            for p in postings:
                doc = self.docs[p.doc]
                tf = len(p.positions) / max(doc.length, 1)
                scores[p.doc] = scores.get(p.doc, 0.0) + tf * idf
        ranked = sorted(scores.items(), key=lambda e: (-e[1], self.docs[e[0]].path))
        return [(self.docs[d].path, score) for d, score in ranked[:limit]]


STOP_WORDS = {"a", "an", "and", "the", "of", "to", "in", "is", "it", "on", "for"}


def words(text: str) -> Iterator[str]:
    """The lower-cased words of `text`, produced lazily."""
    current = ""
    for c in text.lower():
        if c.isalnum():
            current += c
        elif current:
            yield current
            current = ""
    if current:
        yield current


def tokenize(text: str) -> list[str]:
    return [w for w in words(text) if w not in STOP_WORDS]


def read_text(path: str) -> str:
    with open(path, encoding="utf-8") as f:
        return f.read()


def index_dir(index: Index, directory: str) -> int:
    count = 0
    for name in sorted(os.listdir(directory)):
        path = os.path.join(directory, name)
        if os.path.isdir(path):
            count += index_dir(index, path)
        elif os.path.splitext(path)[1] in (".txt", ".md"):
            index.add(path, read_text(path))
            count += 1
    return count


def usage():
    print(
        "usage: search_index.py (index INDEX DIR | add INDEX FILE... | search INDEX QUERY... | stats INDEX)",
        file=sys.stderr,
    )
    sys.exit(2)


def main() -> None:
    args = sys.argv[1:]
    if len(args) < 2:
        usage()
    index_path = args[1]
    index = Index.load(index_path)

    match args[0]:
        case "index":
            if len(args) != 3:
                usage()
            n = index_dir(index, args[2])
            index.save(index_path)
            print(f"indexed {n} documents, {len(index.postings)} terms")
        case "add":
            for path in args[2:]:
                index.add(path, read_text(path))
            index.save(index_path)
            print(f"added {len(args) - 2} documents")
        case "search":
            query = " ".join(args[2:])
            results = index.search(query)
            if not results:
                print(f'no results for "{query}"')
            for path, score in results:
                print(f"{score:.4f}  {path}")
        case "stats":
            print(f"documents: {index.live_docs()}")
            print(f"terms: {len(index.postings)}")
            top = sorted(index.postings.items(), key=lambda e: (-len(e[1]), e[0]))[:5]
            print(f"most common: {', '.join(f'{term}({len(ps)})' for term, ps in top)}")
        case _:
            usage()


main()
