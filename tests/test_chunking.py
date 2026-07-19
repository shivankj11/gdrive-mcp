from gdrive_mcp.chunking import chunk_text, paginate


def test_small_text_is_one_chunk():
    assert chunk_text("a\n\nb\n\nc", max_chars=1000) == ["a\n\nb\n\nc"]


def test_empty_text():
    assert chunk_text("", max_chars=1000) == [""]


def test_no_chunk_exceeds_budget():
    text = "\n\n".join(f"paragraph number {i} " * 20 for i in range(50))
    chunks = chunk_text(text, max_chars=500)
    assert len(chunks) > 1
    assert all(len(c) <= 500 for c in chunks)


def test_paragraph_input_reconstructs():
    text = "\n\n".join(f"para{i}" for i in range(30))
    chunks = chunk_text(text, max_chars=40)
    assert "\n\n".join(chunks) == text  # packing/splitting on blank lines is lossless here


def test_oversized_paragraph_is_hard_split():
    text = "x" * 2500  # single paragraph, no blank lines
    chunks = chunk_text(text, max_chars=1000)
    assert [len(c) for c in chunks] == [1000, 1000, 500]
    assert "".join(chunks) == text


def test_max_chars_zero_disables_chunking():
    text = "a\n\n" + "b" * 5000
    assert chunk_text(text, max_chars=0) == [text]


def test_paginate_metadata_and_clamping():
    text = "\n\n".join(f"para{i}" for i in range(30))
    total = len(chunk_text(text, max_chars=40))

    first = paginate(text, chunk=0, max_chars=40)
    assert first["chunk_index"] == 0
    assert first["total_chunks"] == total
    assert first["total_chars"] == len(text)
    assert first["has_more"] is (total > 1)

    # out-of-range chunk clamps to the last chunk, with no has_more
    last = paginate(text, chunk=999, max_chars=40)
    assert last["chunk_index"] == total - 1
    assert last["has_more"] is False
