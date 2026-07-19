import pytest

from gdrive_mcp.a1 import (
    build_range,
    col_to_letter,
    letter_to_col,
    parse_cell,
    parse_range,
    quote_tab,
    split_range,
)


def test_col_letter_roundtrip():
    for i in [0, 1, 25, 26, 27, 51, 52, 701, 702]:
        assert letter_to_col(col_to_letter(i)) == i


def test_known_letters():
    assert col_to_letter(0) == "A"
    assert col_to_letter(25) == "Z"
    assert col_to_letter(26) == "AA"
    assert col_to_letter(701) == "ZZ"
    assert col_to_letter(702) == "AAA"


def test_parse_cell():
    assert parse_cell("A1") == (0, 0)
    assert parse_cell("B3") == (1, 2)
    assert parse_cell("AA10") == (26, 9)


def test_build_range():
    assert build_range("Data", "A1", 3, 2) == "'Data'!A1:B3"
    assert build_range("S", "B2", 1, 1) == "'S'!B2:B2"
    assert build_range("T", "C5", 4, 3) == "'T'!C5:E8"


def test_quote_tab_doubles_embedded_apostrophes():
    assert quote_tab("Data") == "'Data'"
    assert quote_tab("John's Data") == "'John''s Data'"
    assert quote_tab("O'Brien's") == "'O''Brien''s'"


def test_build_range_quotes_apostrophe_tab():
    # the inner quote must be doubled, not terminate the name early
    assert build_range("John's Data", "A1", 1, 1) == "'John''s Data'!A1:A1"


def test_split_range():
    assert split_range("Data!A1:B2") == ("Data", "A1:B2")
    assert split_range("A1:B2") == (None, "A1:B2")
    assert split_range("'John''s Data'!B2") == ("John's Data", "B2")  # unquoted + undoubled


def test_parse_range():
    assert parse_range("Data!A2:C10") == ("Data", (0, 1, 3, 10))  # 0-based, half-open ends
    assert parse_range("B2") == (None, (1, 1, 2, 2))  # single cell, no tab
    assert parse_range(" Data!A1:A1 ") == ("Data", (0, 0, 1, 1))


def test_parse_range_rejects_open_ended_and_reversed():
    with pytest.raises(ValueError):
        parse_range("Data!A:C")  # open-ended column range
    with pytest.raises(ValueError):
        parse_range("Data!C10:A2")  # end precedes start
