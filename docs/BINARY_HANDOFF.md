# Sending and receiving a gdrive-mcp binary

**First-time setup requires a Google OAuth Desktop-app client JSON as well as the
binary.** Building, downloading, or registering the binary does not provision it.
A JSON file sitting next to the binary is not discovered automatically: install it
at the configuration path below before running `auth`.

| Item | Provided by | Purpose |
| --- | --- | --- |
| `gdrive-mcp` | Sender | Binary built for the recipient's OS and CPU architecture |
| `oauth_client.json` | Sender through a private handoff, or recipient's Google Cloud project | Identifies the OAuth app; contains an `installed` object with `client_id` and `client_secret` |
| `token.json` | Created locally when the recipient runs `auth` | Recipient's access/refresh credentials; never include in a handoff |

An OAuth client JSON does not sign the recipient in as the sender. The recipient
must consent in their own browser. The app's audience/test-user configuration and
the recipient's administrator policy must allow that account.

## Sender: prepare the handoff

1. Build and test the current Rust source. Check that the binary targets the
   recipient's OS/architecture (`file rust/target/release/gdrive-mcp` on macOS/Linux).
   Do not send an older build that omits cache metadata from `tools/list`.
2. Decide how the recipient obtains the **Desktop-app** OAuth JSON. For a private
   handoff, supply your deployment's client through the approved private channel
   or include it using the command below. For a public download, omit it and give
   the recipient the Google Cloud setup instructions below. An encrypted client
   alone is insufficient unless the recipient also has a working decrypt path.
3. Confirm the project enables Drive, Docs, Sheets, and Calendar APIs and allows
   the intended recipient to consent. Never send your `token.json`, config
   directory, audit log, or downloaded sandbox files.
4. Send the archive and explicitly state the target OS/architecture, whether the
   client JSON is included, and where to obtain it if omitted. Include this guide
   even when sending a manually assembled zip.

From the source checkout (Python 3.10+ is needed only to create the archive):

```bash
cargo test --manifest-path rust/Cargo.toml
cargo build --release --manifest-path rust/Cargo.toml

# Private handoff: the OAuth app configuration is included alongside the binary.
python3 scripts/package_binary.py \
  --binary rust/target/release/gdrive-mcp \
  --oauth-client /absolute/path/to/desktop-client.json \
  --output dist/gdrive-mcp-handoff.tar.gz

# Public download: no OAuth client configuration is included.
python3 scripts/package_binary.py \
  --binary rust/target/release/gdrive-mcp \
  --without-oauth-client \
  --output dist/gdrive-mcp-public.tar.gz
```

Choose one packaging command. The packager requires an explicit credential
choice, validates Desktop-app JSON, and copies only the binary, this guide,
`AGENTS.md`, and (when requested) the OAuth fields used by the binary. It never
searches for credentials or includes a user's token. It produces a `0600` archive
with a `0700` directory, `0700` binary, and `0600` JSON. An archive's file permissions
do not encrypt it: distribute a credential-bearing archive through a private channel.

Suggested handoff message:

> This archive contains gdrive-mcp for **[OS/architecture]**. Desktop-app OAuth JSON
> is **[included / available from this private source / to be created in your project]**.
> Read `START_HERE.md` and `AGENTS.md`. Install the JSON, run `auth` as yourself,
> verify `whoami`, then register and verify tools plus a read-only Drive query.
> Do not use anyone else's `token.json`.

## Recipient: install and authenticate

Read the bundle's `AGENTS.md` for whether JSON is included. If it is missing,
obtain it from the sender or create a Desktop-app OAuth client as described below.
Do not substitute a service-account key, a web-app client, or another user's token.

On macOS/Linux, extract the archive and enter `gdrive-mcp-bundle`. The commands
below assume the default credential paths (unset `GDRIVE_MCP_OAUTH_CLIENT` and
`GDRIVE_MCP_TOKEN`). If you intentionally use overrides, install at those paths
and pass the same absolute paths to the MCP client's server environment.

```bash
umask 077
mkdir -p "$HOME/.local/bin"
install -m 700 ./gdrive-mcp "$HOME/.local/bin/gdrive-mcp"

GDRIVE_HANDOFF_CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/gdrive-mcp"
mkdir -p "$GDRIVE_HANDOFF_CONFIG"
chmod 700 "$GDRIVE_HANDOFF_CONFIG"

# Set this to the separately obtained file if the bundle omitted the JSON.
GDRIVE_HANDOFF_CLIENT="$PWD/oauth_client.json"
# Preserve an existing app configuration; do not silently switch OAuth apps.
if [ ! -e "$GDRIVE_HANDOFF_CONFIG/oauth_client.json" ]; then
  install -m 600 "$GDRIVE_HANDOFF_CLIENT" "$GDRIVE_HANDOFF_CONFIG/oauth_client.json"
fi
chmod 600 "$GDRIVE_HANDOFF_CONFIG/oauth_client.json"
```

**Stop if the JSON is missing or installation fails.** Request it from the sender
before proceeding. Neither MCP registration nor repeated authentication attempts
will create a missing Desktop-app client configuration.

```bash
"$HOME/.local/bin/gdrive-mcp" auth
"$HOME/.local/bin/gdrive-mcp" whoami
```

The recipient completes browser consent and checks that `whoami` reports their
intended Google account. For an already authenticated installation, `whoami` can
be tried first; run `auth` if credentials are missing or need renewed consent.
Keep `token.json` private to that user. Once the installed copy works, remove
unneeded downloaded/extracted JSON and credential-bearing archives, or restrict
retained copies to owner access (`chmod 600`).

## Recipient: register and verify the actual MCP path

```bash
# For a new registration:
claude mcp add -s user gdrive -- "$HOME/.local/bin/gdrive-mcp" serve
```

If `gdrive` is already registered, inspect it with `claude mcp get gdrive` and
update the existing registration to this binary, preserving any intentional
environment settings. A reload/restart may be needed. A nondefault
`XDG_CONFIG_HOME` must also be present in the MCP server's environment.

The installing agent must verify all three before reporting success:

1. `whoami` succeeds and the user confirms the intended account.
2. The actual MCP client loads tools, including `search_files`. A connected/healthy
   server with zero tools is a failed installation.
3. A read-only `search_files` call through that client succeeds. An empty result
   is valid; a protocol/authentication error is not. Do not create/edit files as
   an installation test.

If the client rejects `tools/list` for missing `ttlMs`/`cacheScope`, obtain a fixed
build from the sender. Verify the replacement through the actual client before
removing an existing compatibility shim. Do not call that failure a credential problem.

## Obtaining your own Desktop-app JSON

In a Google Cloud project you control, configure the OAuth consent screen/audience,
enable the Drive, Docs, Sheets, and Calendar APIs, create an OAuth client with
application type **Desktop app**, and download its JSON. Follow your administrator's
access policy and Google's verification requirements. Store it as `oauth_client.json`
at the path above; Google may download it as `client_secret_….json`.

See Google's [Create access credentials](https://developers.google.com/workspace/guides/create-credentials#desktop-app)
and [OAuth for installed apps](https://developers.google.com/identity/protocols/oauth2/native-app).
The OAuth client identifies the application; it does not replace per-user consent.
