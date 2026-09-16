//! Native payload behind an npm-style .cmd wrapper. No network or agent login.
use std::io::{BufRead, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--version"] {
        println!("codex-cli 0.145.0");
        return;
    }
    let log = std::env::var_os("E2E_WINDOWS_AGENT_LOG").expect("fixture log path");
    std::fs::write(log, serde_json::to_vec(&args).unwrap()).unwrap();
    println!("WINDOWS_AGENT_READY");
    std::io::stdout().flush().unwrap();
    if std::env::var_os("E2E_WINDOWS_READ_INPUT").is_some() {
        let line = std::io::stdin().lock().lines().next().unwrap().unwrap();
        println!("WINDOWS_AGENT_INPUT:{line}");
    }
    std::process::exit(7);
}
