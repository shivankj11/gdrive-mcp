//! CLI entry points: `auth` (one-time browser consent), `whoami` (verify), `serve` (the MCP).

use clap::{Parser, Subcommand};
use serde_json::Value;

use gdrive_mcp::auth::{load_credentials, run_auth_flow, AuthError, Credentials};
use gdrive_mcp::clients::{GoogleApi, GoogleClient};
use gdrive_mcp::config::token_path;

#[derive(Parser)]
#[command(name = "gdrive-mcp", about = "Google Drive / Docs / Sheets MCP (read/write) — auth + serve.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the one-time browser OAuth consent and cache the token.
    Auth,
    /// Verify cached credentials by printing the authenticated user.
    Whoami,
    /// Run the MCP server over stdio.
    Serve,
}

/// Confirm the token works by reading the authenticated user via drive.about.
async fn authed_user(_creds: &Credentials) -> Result<Value, AuthError> {
    GoogleClient::new()
        .drive_about("user")
        .await
        .map(|about| about.get("user").cloned().unwrap_or(Value::Null))
        .map_err(|e| AuthError(e.to_string()))
}

fn field<'a>(user: &'a Value, key: &str) -> &'a str {
    user.get(key).and_then(Value::as_str).unwrap_or("")
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Auth => {
            let creds = run_auth_flow().await?;
            let user = authed_user(&creds).await?;
            let email = field(&user, "emailAddress");
            println!(
                "Authenticated as {}.\nToken cached at {} (read/write Drive access).",
                if email.is_empty() { "unknown" } else { email },
                token_path().display()
            );
        }
        Command::Whoami => {
            let creds = load_credentials().await?;
            let user = authed_user(&creds).await?;
            let email = field(&user, "emailAddress");
            let email = if email.is_empty() { "unknown" } else { email };
            let name = field(&user, "displayName");
            if name.is_empty() {
                println!("{email}");
            } else {
                println!("{email} ({name})");
            }
        }
        Command::Serve => gdrive_mcp::server::run().await?,
    }
    Ok(())
}
