mod config;
mod install;
mod manifest;
mod transaction_cli;
mod transport;

use anyhow::Result;
use config::{Config, TransportMode};
use konnect_core::mcp::handler::McpHandler;
use std::io::IsTerminal;
use tracing::info;
use tracing_subscriber::{fmt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    // ─── CLI argument parsing (minimal, no clap dependency) ─────────
    let args: Vec<String> = std::env::args().collect();

    // ─── Subcommand dispatch (install, uninstall, status, skill) ────
    match args.get(1).map(String::as_str) {
        Some("init") => {
            let client = install::client_from_args(&args[2..])?;
            return install::run_install(client);
        }
        Some("uninstall") => {
            let client = install::client_from_args(&args[2..])?;
            return install::run_uninstall(client);
        }
        Some("status") => {
            let client = install::client_from_args(&args[2..])?;
            return install::print_status(client);
        }
        Some("skill") => {
            let name = args.get(2).map(String::as_str).unwrap_or("");
            return install::print_skill_content(name);
        }
        Some("transaction") => return transaction_cli::run(&args[2..]),
        Some("--version") | Some("-V") => {
            println!("konnect {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--help") | Some("-h") | Some("help") => {
            print_help();
            return Ok(());
        }
        _ => {}
    }

    // ─── Double-click detection ─────────────────────────────────────
    // If stdin is a terminal (user double-clicked the .exe), run friendly install.
    // If stdin is piped (Claude launched us as MCP server), start server.
    if std::io::stdin().is_terminal() {
        return install::run_double_click_install();
    }

    let client = install::client_from_args(&args[1..])?;

    // ─── Auto-install on first MCP launch (safety net) ──────────────
    if install::needs_install(client) {
        let _ = install::run_install_silent(client);
    }

    // --config <path>: load config from specified file
    let config_path = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|pos| args.get(pos + 1))
        .map(std::path::PathBuf::from);

    let config = if let Some(ref path) = config_path {
        // KiCAD launches the server this way (with KICAD_API_SOCKET set), so
        // the env fallback for a blank ipc_address must apply here too (#39).
        let mut c = Config::load_from(path)?;
        c.apply_env_fallbacks();
        c
    } else {
        Config::load()?
    };

    // ─── Initialize tracing (stderr only — stdout is MCP protocol) ──
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_level));

    fmt::Subscriber::builder()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();

    info!("Konnect v{} starting", env!("CARGO_PKG_VERSION"));

    let server_config = konnect_core::tools::ServerConfig {
        kicad_cli: config.kicad_cli.clone(),
        kicad_binary: config.kicad_binary.clone(),
        ipc_address: config.ipc_address.clone(),
        project_dir: config.project_dir.clone(),
        jlcpcb_db_path: config.jlcpcb_db_path.clone(),
        auto_load_toolsets: config.auto_load_toolsets,
        eager_toolsets: config.eager_toolsets,
    };
    let handler = McpHandler::new(server_config).await?;

    match config.transport {
        TransportMode::Stdio => {
            transport::stdio::run_stdio(handler).await?;
        }
        TransportMode::Http => {
            transport::http::run_http(handler, &config.http_address).await?;
        }
        TransportMode::Both => {
            let handler_http = handler.clone();
            let http_addr = config.http_address.clone();
            let http_task = tokio::spawn(async move {
                transport::http::run_http(handler_http, &http_addr)
                    .await
                    .expect("HTTP transport failed");
            });
            let stdio_task = tokio::spawn(async move {
                transport::stdio::run_stdio(handler)
                    .await
                    .expect("STDIO transport failed");
            });
            tokio::select! {
                _ = http_task => {},
                _ = stdio_task => {},
            }
        }
    }

    Ok(())
}

fn print_help() {
    println!("Konnect v{}", env!("CARGO_PKG_VERSION"));
    println!("MCP server for KiCad EDA with embedded guidance.\n");
    println!("USAGE:");
    println!("  konnect                  Start MCP server (pipe) or install (TTY)");
    println!("  konnect [--client <client>] [--config <path>]");
    println!("  konnect init [--client <client>]");
    println!("  konnect uninstall [--client <client>]");
    println!("  konnect status [--client <client>]");
    println!("  konnect skill <name>     Print skill content (for hooks)");
    println!("  konnect transaction status <project-dir>");
    println!("  konnect transaction recover <project-dir>");
    println!("  konnect transaction abandon <project-dir> <id> --force");
    println!("  konnect --version        Print version");
    println!("  konnect --help           This message");
    println!("\nCLIENTS:");
    println!("  claude (default)         Skills, agents, and hooks under ~/.claude");
    println!("  codex                    Skills under ~/.agents/skills");
}
