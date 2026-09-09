//! (b) Claude managed-config assertion.
//!
//! With Claude selected as the default agent, `blue apply` writes the managed
//! overlay under `~/.config/blue/runtime/claude/`. We assert the same three
//! things the other three agents assert in `all_agents_config.rs` — governed
//! model, the managed `e2e-remote` MCP server, and the managed `example` skill —
//! so no agent is tested more or less than another. Skips when the Claude CLI is
//! not installed (agent ineligible).

mod common;

use common::{assert_example_skill_materialized, assert_governed_model, assert_mcp_registered};
use e2e_slim::AgentSelection;

#[test]
fn claude_managed_config_is_written() {
    let Some(stack) = e2e_slim::env_or_skip() else {
        eprintln!("e2e-slim stack env unset; skipping");
        return;
    };
    let home = stack.bootstrap_home();

    if home.select_agent("claude") == AgentSelection::NotEligible {
        eprintln!("claude CLI not installed/eligible; skipping");
        return;
    }

    home.blue().args(["apply", "--yes"]).assert().success();

    assert_governed_model(&home, "claude", "claude-sonnet-5");
    assert_mcp_registered(&home, "claude");
    assert_example_skill_materialized(&home, "claude");
}
