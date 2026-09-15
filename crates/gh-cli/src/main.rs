// `blue` — the governance meta-harness CLI.
//
// Three jobs and nothing more: connect + authenticate to the service, load
// governance config, and transparently wrap the chosen harness. See the plan
// and each crate's docs for the design.

mod commands;
mod repair;
mod supervisor;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "blue",
    version,
    about = "Governance wrapper for supported coding-agent CLIs",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print the Blue metaharness version.
    Version,
    /// Connect or reconnect this client to a metaharness deployment.
    Setup,
    /// Disconnect the active deployment while retaining its non-secret state.
    Reset {
        /// Skip the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Authenticate to the provisioned service and store the session.
    Login {
        /// Re-run the browser authorization even when a session is stored.
        #[arg(long)]
        force: bool,
    },
    /// Remove the locally stored OAuth session.
    Logout,
    /// Report installed harnesses, versions, and whether policy allows them.
    Doctor,
    /// Show or change the preferred coding agent.
    Agent {
        /// Agent to make the default (run `blue doctor` to list supported harnesses).
        name: Option<String>,
    },
    /// Show login, desired revision, and whether managed files are current.
    Status,
    /// Verify managed files match the last successfully applied revision.
    Verify,
    /// Wrap and launch a harness: `blue run codex -- "…"`.
    Run {
        /// Harness name (run `blue doctor` to list supported harnesses).
        name: String,
        /// Args forwarded verbatim to the harness (after `--`).
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Print the resolved governance config and its source.
    Config,
    /// Provision or reconcile the managed gateway key for the current user.
    Gateway,
    /// Reconcile the default agent's managed configuration once.
    Apply {
        /// Accept all existing-config merge prompts (backups are still made).
        #[arg(long)]
        yes: bool,
    },
    /// Run the reconcile daemon: keep the default agent current on revision/TTL.
    Daemon {
        /// Max seconds between polls (defaults to the config TTL).
        #[arg(long)]
        interval: Option<u64>,
    },
    /// Install/remove PATH shims for claude, codex, kimi, and opencode.
    Shim {
        #[command(subcommand)]
        action: ShimAction,
    },
    /// Internal entrypoint invoked by managed agent lifecycle hooks.
    #[command(hide = true)]
    SessionUpload {
        /// Harness that produced the hook payload.
        name: String,
        /// Compiled compatibility profile that installed this hook.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Detached worker that drains durable session-upload spool records.
    #[command(hide = true)]
    SessionUploadWorker,
    /// Internal entrypoint invoked by managed agent session-start hooks.
    #[command(hide = true)]
    SessionStart {
        /// Harness that produced the hook payload.
        name: String,
        /// Compiled compatibility profile that installed this hook.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Verify and restore a downloaded portable session bundle.
    #[command(hide = true)]
    SessionRestore {
        #[arg(long)]
        bundle: std::path::PathBuf,
        /// Check compatibility and collisions without writing native state.
        #[arg(long)]
        preflight: bool,
    },
    /// Shorthand: `blue codex …` ≡ `blue run codex -- …`.
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Subcommand)]
enum ShimAction {
    /// Install shims into a directory (default: ~/.local/bin on Unix, LocalAppData\\Blue\\bin on Windows).
    Install {
        #[arg(long)]
        dir: Option<String>,
    },
    /// Remove previously-installed shims.
    Uninstall {
        #[arg(long)]
        dir: Option<String>,
    },
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("HARNESS_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .without_time()
        .init();

    let cli = Cli::parse();
    let result = match cli.command {
        None => commands::start(),
        Some(Command::Version) => commands::version(),
        Some(Command::Setup) => commands::setup(),
        Some(Command::Reset { yes }) => commands::reset(yes),
        Some(Command::Login { force }) => commands::login(force),
        Some(Command::Logout) => commands::logout(),
        Some(Command::Doctor) => commands::doctor(),
        Some(Command::Agent { name }) => commands::agent(name.as_deref()),
        Some(Command::Status) => commands::status(false),
        Some(Command::Verify) => commands::status(true),
        Some(Command::Run { name, args }) => commands::run(&name, &args),
        Some(Command::Config) => commands::config(),
        Some(Command::Gateway) => commands::gateway(),
        Some(Command::Apply { yes }) => commands::apply(yes),
        Some(Command::Daemon { interval }) => commands::daemon(interval),
        Some(Command::Shim { action }) => match action {
            ShimAction::Install { dir } => commands::shim_install(dir.as_deref()),
            ShimAction::Uninstall { dir } => commands::shim_uninstall(dir.as_deref()),
        },
        Some(Command::SessionUpload { name, profile }) => {
            commands::session_upload(&name, profile.as_deref())
        }
        Some(Command::SessionUploadWorker) => commands::session_upload_worker(),
        Some(Command::SessionStart { name, profile }) => {
            commands::session_start(&name, profile.as_deref())
        }
        Some(Command::SessionRestore { bundle, preflight }) => {
            commands::session_restore(&bundle, preflight)
        }
        Some(Command::External(argv)) => commands::run_external(&argv),
    };

    if let Err(e) = result {
        print_error(&e);
        std::process::exit(1);
    }
}

fn print_error(error: &anyhow::Error) {
    eprintln!();
    eprintln!(
        "  {}",
        console::style("Blue ran into a problem").red().bold()
    );
    eprintln!();
    for line in format!("{error:#}").lines() {
        eprintln!("  {line}");
    }
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agent_with_and_without_a_name() {
        let cli = Cli::try_parse_from(["blue", "agent"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Agent { name: None })));

        let cli = Cli::try_parse_from(["blue", "agent", "claude"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Agent { name: Some(name) }) if name == "claude"
        ));
    }

    #[test]
    fn parses_login_with_and_without_the_forced_reauthorization() {
        let cli = Cli::try_parse_from(["blue", "login"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Login { force: false })));

        let cli = Cli::try_parse_from(["blue", "login", "--force"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Login { force: true })));
    }

    #[test]
    fn parses_reset_with_and_without_confirmation_bypass() {
        let cli = Cli::try_parse_from(["blue", "reset"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Reset { yes: false })));

        let cli = Cli::try_parse_from(["blue", "reset", "--yes"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Reset { yes: true })));
    }

    #[test]
    fn parses_profile_scoped_and_legacy_session_upload_hooks() {
        let cli =
            Cli::try_parse_from(["blue", "session-upload", "claude", "--profile", "claude-v1"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::SessionUpload { name, profile: Some(profile) })
                if name == "claude" && profile == "claude-v1"
        ));

        let cli = Cli::try_parse_from(["blue", "session-upload", "claude"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::SessionUpload { profile: None, .. })
        ));
    }

    #[test]
    fn parses_session_start_hook_with_and_without_profile() {
        let cli = Cli::try_parse_from([
            "blue",
            "session-start",
            "codex",
            "--profile",
            "codex-v0_114_0",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::SessionStart { name, profile: Some(profile) })
                if name == "codex" && profile == "codex-v0_114_0"
        ));

        let cli = Cli::try_parse_from(["blue", "session-start", "codex"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::SessionStart { profile: None, .. })
        ));
    }

    #[test]
    fn parses_blue_codex_shorthand() {
        let cli = Cli::try_parse_from(["blue", "codex", "--help"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::External(argv)) if argv == ["codex", "--help"]
        ));
    }
}
