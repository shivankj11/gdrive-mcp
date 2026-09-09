#!/usr/bin/env bash
#
# setup.sh — one-command install of the gdrive MCP.
#
#   git clone https://github.com/shivankj11/gdrive-mcp.git && cd gdrive-mcp && bash setup.sh
#
# It installs uv (if missing), runs the one-time Google browser consent, and
# registers the server with Claude Code — all idempotent, so re-running is safe.
# You supply your own Desktop-app OAuth client JSON (see the README's Setup section).

set -euo pipefail

REPO="git+https://github.com/shivankj11/gdrive-mcp"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/gdrive-mcp"
CLIENT_DEST="$CONFIG_DIR/oauth_client.json"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ---- logging ---------------------------------------------------------------
if [ -t 1 ]; then B=$'\033[1m'; G=$'\033[32m'; Y=$'\033[33m'; R=$'\033[31m'; N=$'\033[0m'; else B=; G=; Y=; R=; N=; fi
info() { printf '%s==>%s %s\n' "$G$B" "$N" "$*"; }
warn() { printf '%s!!%s %s\n' "$Y$B" "$N" "$*" >&2; }
die()  { printf '%serror:%s %s\n' "$R$B" "$N" "$*" >&2; exit 1; }

# ---- 1. dependencies -------------------------------------------------------
command -v git >/dev/null 2>&1 || die "git is required but not found."

if ! command -v uvx >/dev/null 2>&1; then
  if command -v uv >/dev/null 2>&1; then
    die "found 'uv' but not 'uvx' — run 'uv self update' to get uvx, then re-run."
  fi
  info "Installing uv (provides uvx)…"
  curl -LsSf https://astral.sh/uv/install.sh | sh
  export PATH="$HOME/.local/bin:$PATH"
  command -v uvx >/dev/null 2>&1 || die "uv installed but 'uvx' isn't on PATH yet — open a new shell and re-run."
fi

command -v claude >/dev/null 2>&1 || die "Claude Code CLI ('claude') not found. Install it, then re-run."

# ---- 2. OAuth client JSON --------------------------------------------------
# A Desktop-app OAuth client (client_id + client_secret) that you create in your
# own Google Cloud project. You consent individually and get your own per-user token.
mkdir -p "$CONFIG_DIR"
chmod 700 "$CONFIG_DIR" 2>/dev/null || true

if [ -f "$CLIENT_DEST" ]; then
  info "OAuth client already in place ($CLIENT_DEST)."
elif [ -n "${GDRIVE_MCP_OAUTH_CLIENT_CMD:-}" ]; then
  info "Fetching OAuth client via \$GDRIVE_MCP_OAUTH_CLIENT_CMD…"
  eval "$GDRIVE_MCP_OAUTH_CLIENT_CMD" > "$CLIENT_DEST" || die "OAuth client fetch command failed."
  chmod 600 "$CLIENT_DEST"
elif [ -f "$SCRIPT_DIR/oauth_client.json" ]; then
  info "Installing plaintext OAuth client from the repo root."
  install -m 600 "$SCRIPT_DIR/oauth_client.json" "$CLIENT_DEST"
else
  die "No OAuth client JSON found. Create a Desktop-app OAuth client in Google Cloud
    (see the README's 'Setup' section), then provide it one of these ways and re-run:
    • save the client JSON to $CLIENT_DEST, or
    • drop it as oauth_client.json in the repo root, or
    • export GDRIVE_MCP_OAUTH_CLIENT_CMD='<command that prints the client JSON>'."
fi

# ---- 3. one-time browser consent -------------------------------------------
# whoami doubles as a credential probe: it silently refreshes a stale token and
# fails when the token is missing required scopes (for example, one created before Calendar
# support), so we auth exactly when needed and upgrade old grants through incremental consent.
if uvx --from "$REPO" gdrive-mcp whoami >/dev/null 2>&1; then
  info "Already authenticated — skipping browser consent."
else
  info "Opening a browser for the one-time Google consent…"
  uvx --from "$REPO" gdrive-mcp auth
fi

# ---- 4. register with Claude Code (user scope, idempotent) -----------------
info "Registering the 'gdrive' MCP server (user scope)…"
claude mcp remove gdrive -s user >/dev/null 2>&1 || true
claude mcp add -s user gdrive -- uvx --from "$REPO" gdrive-mcp serve

# ---- 5. confirm ------------------------------------------------------------
printf '\n'
info "${G}Done.${N} 'gdrive' is registered and authenticated as: $(uvx --from "$REPO" gdrive-mcp whoami 2>/dev/null || echo '?')"
info "Restart Claude Code (or reload MCP servers) to pick it up."
