"""Exercise deliverables with synthetic credentials; never read local Google auth."""

import importlib.util
import json
from pathlib import Path
import stat
import subprocess
import sys
import tarfile
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "package_binary.py"
SPEC = importlib.util.spec_from_file_location("package_binary", SCRIPT)
packager = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(packager)


class PackagingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.binary = self.root / "gdrive-mcp"
        self.binary.write_bytes(b"synthetic binary")
        self.output = self.root / "bundle.tar.gz"
        self.client = self.root / "client.json"
        self.client.write_text(json.dumps({"installed": {
            "client_id": "test-client", "client_secret": "test-secret",
            "refresh_token": "must-never-ship",
        }}))

    def test_private_bundle_excludes_user_data_and_restricts_permissions(self):
        (self.root / "token.json").write_text("must-never-ship")
        (self.root / "audit.log").write_text("must-never-ship")
        packager.package(self.binary, self.output, self.client)
        self.assertEqual(stat.S_IMODE(self.output.stat().st_mode), 0o600)
        with tarfile.open(self.output) as archive:
            self.assertEqual(set(archive.getnames()), {
                "gdrive-mcp-bundle", "gdrive-mcp-bundle/gdrive-mcp",
                "gdrive-mcp-bundle/oauth_client.json", "gdrive-mcp-bundle/START_HERE.md",
                "gdrive-mcp-bundle/AGENTS.md",
            })
            for member in archive.getmembers():
                self.assertEqual(member.mode, 0o700 if member.isdir() or member.name.endswith("/gdrive-mcp") else 0o600)
                if member.isfile():
                    self.assertNotIn(b"must-never-ship", archive.extractfile(member).read())
            client = json.load(archive.extractfile("gdrive-mcp-bundle/oauth_client.json"))
            self.assertEqual(client["installed"]["client_id"], "test-client")
            self.assertEqual(client["installed"]["client_secret"], "test-secret")

    def test_public_bundle_declares_missing_json_and_carries_instructions(self):
        packager.package(self.binary, self.output, None)
        with tarfile.open(self.output) as archive:
            self.assertFalse(any(name.endswith(".json") for name in archive.getnames()))
            self.assertIn(b"NOT INCLUDED", archive.extractfile("gdrive-mcp-bundle/AGENTS.md").read())
            guide = archive.extractfile("gdrive-mcp-bundle/START_HERE.md").read()
            self.assertIn(b"Sender:", guide)
            self.assertIn(b"Recipient:", guide)

    def test_invalid_credentials_fail_before_creating_a_deliverable(self):
        for doc in (
            {"refresh_token": "not-app-config"},
            {"web": {"client_id": "web", "client_secret": "secret"}},
            {"type": "service_account"},
            {"installed": {"client_id": "incomplete"}},
            {"installed": {"client_id": "id", "client_secret": "secret", "token_uri": "https://example.invalid"}},
        ):
            with self.subTest(doc=doc):
                self.client.write_text(json.dumps(doc))
                with self.assertRaises(ValueError):
                    packager.package(self.binary, self.output, self.client)
                self.assertFalse(self.output.exists())

    def test_existing_deliverable_is_not_overwritten(self):
        self.output.write_bytes(b"previous deliverable")
        with self.assertRaises(FileExistsError):
            packager.package(self.binary, self.output, self.client)
        self.assertEqual(self.output.read_bytes(), b"previous deliverable")

    def test_cli_requires_explicit_credential_choice(self):
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--binary", str(self.binary), "--output", str(self.output)],
            capture_output=True, text=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--oauth-client", result.stderr)
        self.assertIn("--without-oauth-client", result.stderr)
        self.assertFalse(self.output.exists())


if __name__ == "__main__":
    unittest.main()
