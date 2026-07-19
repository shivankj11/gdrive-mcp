from gdrive_mcp.guard import preview_response


def test_preview_shape():
    p = preview_response("clear_range", {"range": "A1:B2", "nonempty_cells_cleared": 4})
    assert p["status"] == "confirmation_required"
    assert p["action"] == "clear_range"
    assert p["impact"]["nonempty_cells_cleared"] == 4
    assert "confirm=true" in p["next"]
