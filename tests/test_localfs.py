import os
import time

import pytest

from gdrive_mcp.localfs import (
    MARKER_NAME,
    files_root,
    safe_read_path,
    safe_write_path,
    sweep_expired,
)


@pytest.fixture
def sandbox(tmp_path, monkeypatch):
    # Point at a nonexistent dir so files_root() creates and marks it — the real server flow.
    monkeypatch.setenv("GDRIVE_MCP_FILES_DIR", str(tmp_path / "files"))
    return files_root()


def test_write_relative_stays_within_and_makes_parents(sandbox):
    p = safe_write_path("sub/dir/out.bin", "ignored")
    assert p == sandbox / "sub" / "dir" / "out.bin"
    assert p.parent.is_dir()


def test_write_default_name_is_sanitized_basename(sandbox):
    p = safe_write_path(None, "../../etc/pa ss/wd")
    assert p.parent == sandbox
    assert "/" not in p.name and ".." not in p.name


def test_write_rejects_absolute(sandbox):
    with pytest.raises(RuntimeError):
        safe_write_path("/etc/cron.d/x", "d")


def test_write_rejects_dotdot_escape(sandbox):
    with pytest.raises(RuntimeError):
        safe_write_path("../../../etc/x", "d")


def test_read_within_ok(sandbox):
    (sandbox / "in.txt").write_text("hi")
    assert safe_read_path("in.txt") == sandbox / "in.txt"


def test_read_rejects_absolute(sandbox):
    with pytest.raises(RuntimeError):
        safe_read_path("/etc/hosts")


def test_read_rejects_escape(sandbox):
    with pytest.raises(RuntimeError):
        safe_read_path("../../etc/hosts")


def test_read_missing_file_in_sandbox(sandbox):
    with pytest.raises(RuntimeError):
        safe_read_path("nope.txt")


def _age(path, hours):
    past = time.time() - hours * 3600
    os.utime(path, (past, past))


def test_sweep_deletes_old_keeps_fresh(sandbox):
    old, fresh = sandbox / "old.csv", sandbox / "fresh.csv"
    old.write_text("x")
    fresh.write_text("y")
    _age(old, 48)
    sweep_expired(ttl_hours=24)
    assert not old.exists() and fresh.exists()


def test_sweep_ttl_zero_disables(sandbox):
    old = sandbox / "old.csv"
    old.write_text("x")
    _age(old, 100)
    sweep_expired(ttl_hours=0)
    assert old.exists()


def test_sweep_bad_ttl_env_does_not_raise(sandbox, monkeypatch):
    monkeypatch.setenv("GDRIVE_MCP_FILES_TTL_HOURS", "forever")
    sweep_expired()  # must fall back to default, not raise


def test_sweep_bad_files_dir_does_not_raise(tmp_path, monkeypatch):
    afile = tmp_path / "afile"
    afile.write_text("x")
    monkeypatch.setenv("GDRIVE_MCP_FILES_DIR", str(afile / "under-a-file"))
    sweep_expired(ttl_hours=1)  # files_root() mkdir fails -> must be swallowed (no crash on boot)


# ---- sweep ownership gate: never delete inside a directory gdrive-mcp didn't create ---------


def test_created_sandbox_is_marked(sandbox):
    assert (sandbox / MARKER_NAME).is_file()


def test_sweep_refuses_preexisting_unmarked_dir(tmp_path, monkeypatch, capsys):
    pre = tmp_path / "downloads"  # simulates GDRIVE_MCP_FILES_DIR aimed at a real, existing dir
    pre.mkdir()
    keep = pre / "thesis-draft.txt"
    keep.write_text("precious")
    _age(keep, 24 * 365)
    monkeypatch.setenv("GDRIVE_MCP_FILES_DIR", str(pre))
    sweep_expired(ttl_hours=1)
    assert keep.exists()
    assert not (pre / MARKER_NAME).exists()  # refusing must not itself opt the dir in
    assert "not sweeping" in capsys.readouterr().err


def test_writes_do_not_opt_preexisting_dir_into_sweeping(tmp_path, monkeypatch):
    pre = tmp_path / "downloads"
    pre.mkdir()
    monkeypatch.setenv("GDRIVE_MCP_FILES_DIR", str(pre))
    p = safe_write_path("spill.csv", "d")  # containment still applies to a pre-existing dir
    assert p == pre.resolve() / "spill.csv"
    assert not (pre / MARKER_NAME).exists()


def test_operator_marker_opts_existing_dir_into_sweep(tmp_path, monkeypatch):
    pre = tmp_path / "chosen"
    pre.mkdir()
    (pre / MARKER_NAME).touch()
    old = pre / "old.csv"
    old.write_text("x")
    _age(old, 48)
    monkeypatch.setenv("GDRIVE_MCP_FILES_DIR", str(pre))
    sweep_expired(ttl_hours=24)
    assert not old.exists()


def test_sweep_spares_the_marker_itself(sandbox):
    marker = sandbox / MARKER_NAME
    _age(marker, 24 * 30)
    old = sandbox / "old.csv"
    old.write_text("x")
    _age(old, 24 * 30)
    sweep_expired(ttl_hours=24)
    assert marker.exists() and not old.exists()


def test_default_location_is_owned_even_if_preexisting(tmp_path, monkeypatch):
    # An existing install's default dir (created before markers existed) keeps getting swept.
    monkeypatch.delenv("GDRIVE_MCP_FILES_DIR", raising=False)
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path))
    default_root = tmp_path / "gdrive-mcp" / "files"
    default_root.mkdir(parents=True)
    old = default_root / "old.csv"
    old.write_text("x")
    _age(old, 48)
    sweep_expired(ttl_hours=24)
    assert not old.exists()
    assert (default_root / MARKER_NAME).is_file()
