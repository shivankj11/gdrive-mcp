"""Split large text into bounded, natural-boundary chunks for agent-friendly paging.

The read tools return one chunk plus metadata (total size, total chunks, has_more) so an
agent pulls only what it needs instead of flooding its context with a whole document.
"""

from __future__ import annotations

# ~8k chars ≈ ~2k tokens: one read stays well inside an agent's context budget while
# keeping round-trips low. Overridable per call via the tools' max_chars argument.
DEFAULT_MAX_CHARS = 8000


def _atoms(text: str, max_chars: int) -> list[str]:
    """Paragraphs (split on blank lines), each hard-split so none exceeds max_chars."""
    atoms: list[str] = []
    for para in text.split("\n\n"):
        if len(para) <= max_chars:
            atoms.append(para)
        else:
            atoms.extend(para[i : i + max_chars] for i in range(0, len(para), max_chars))
    return atoms


def chunk_text(text: str, max_chars: int = DEFAULT_MAX_CHARS) -> list[str]:
    """Greedily pack paragraphs into chunks of at most max_chars.

    Splits on blank lines first (paragraph boundaries); a paragraph longer than max_chars
    is hard-split so no chunk exceeds the budget. max_chars <= 0 disables chunking (one
    chunk). Empty text yields a single empty chunk.
    """
    if max_chars <= 0:
        return [text]
    if not text:
        return [""]
    chunks: list[str] = []
    current = ""
    for para in _atoms(text, max_chars):
        if current and len(current) + 2 + len(para) > max_chars:
            chunks.append(current)
            current = para
        else:
            current = f"{current}\n\n{para}" if current else para
    if current:
        chunks.append(current)
    return chunks or [""]


def paginate(text: str, chunk: int, max_chars: int) -> dict:
    """Chunk `text` and return the requested chunk plus paging metadata.

    `chunk` is a 0-based index; out-of-range values clamp to the last chunk, and the
    returned chunk_index reflects what was actually served.
    """
    chunks = chunk_text(text, max_chars)
    index = max(0, min(chunk, len(chunks) - 1))
    return {
        "content": chunks[index],
        "chunk_index": index,
        "total_chunks": len(chunks),
        "total_chars": len(text),
        "has_more": index < len(chunks) - 1,
    }
