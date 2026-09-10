# Installation and distribution requirements

Read [docs/BINARY_HANDOFF.md](docs/BINARY_HANDOFF.md) before packaging, sending,
installing, or registering a gdrive-mcp binary.

- First-time authentication requires Google **Desktop-app OAuth client JSON**.
  A standalone binary does not contain it, and `auth` does not create it.
- Senders must explicitly provide the JSON through a private handoff or explain
  how the recipient obtains it. Use `scripts/package_binary.py` so the archive
  includes recipient instructions and declares whether JSON is included.
- Recipients must install the JSON at the configured path, consent as themselves,
  and verify `whoami`, loaded MCP tools, and a read-only query through their client.
  A successful registration or health check alone is not installation success.
- Never package a user's `token.json`, audit log, or sandbox files. Never commit
  plaintext OAuth JSON or publish a credential-bearing archive.
