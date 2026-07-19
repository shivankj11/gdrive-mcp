import pytest

from gdrive_mcp.ids import parse_ref

_SYNTHETIC_DOCUMENT_ID = "TEST_DOCUMENT_ID_0000000001"


def test_document_url():
    r = parse_ref(
        f"https://docs.google.com/document/d/{_SYNTHETIC_DOCUMENT_ID}/edit?tab=t.0"
    )
    assert r.id == _SYNTHETIC_DOCUMENT_ID
    assert r.kind == "document"


def test_spreadsheet_url_with_gid():
    r = parse_ref("https://docs.google.com/spreadsheets/d/ABC123_def-XYZ/edit#gid=0")
    assert r.id == "ABC123_def-XYZ"
    assert r.kind == "spreadsheet"


def test_folder_url():
    assert parse_ref("https://drive.google.com/drive/folders/FOLDERid-1").kind == "folder"


def test_file_url():
    assert parse_ref("https://drive.google.com/file/d/FILEID_1/view").kind == "file"


def test_open_id_url():
    assert parse_ref("https://drive.google.com/open?id=XYZ987_id").id == "XYZ987_id"


def test_bare_id():
    r = parse_ref(_SYNTHETIC_DOCUMENT_ID)
    assert r.kind == "unknown"


def test_unparseable():
    with pytest.raises(ValueError):
        parse_ref("not a link")
