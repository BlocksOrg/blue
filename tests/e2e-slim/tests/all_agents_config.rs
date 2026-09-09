//! (c) Managed-config assertions for codex / kimi / opencode.
//!
//! Each agent runs in its own isolated `$HOME` (nextest runs these in parallel
//! processes; keeping HOMEs distinct avoids cross-talk). Each case selects the
//! agent, runs `blue apply --yes`, and asserts three things land uniformly in
//! that agent's managed config: the governed model, the managed `e2e-remote` MCP
//! server, and the managed `example` skill. Claude gets the same assertions in
//! `claude_config.rs`; together all four are covered identically. Cases skip when
//! the agent CLI is not installed. This is the secret-free, no-inference path.

mod common;

use common::{assert_example_skill_materialized, assert_governed_model, assert_mcp_registered};
use e2e_slim::{AgentSelection, Home, Stack};

/// Bootstrap a fresh HOME and select `agent`; returns `None` (skip) when the
/// agent CLI is not installed/eligible.
fn prepared(stack: &Stack, agent: &str) -> Option<Home> {
    let home = stack.bootstrap_home();
    match home.select_agent(agent) {
        AgentSelection::Selected => {
            home.blue().args(["apply", "--yes"]).assert().success();
            Some(home)
        }
        AgentSelection::NotEligible => {
            eprintln!("{agent} CLI not installed/eligible; skipping");
            None
        }
    }
}

/// Shared body: apply, then assert model + MCP + skill for one agent.
fn assert_managed_config(agent: &str, model: &str) {
    let Some(stack) = e2e_slim::env_or_skip() else {
        return;
    };
    let Some(home) = prepared(&stack, agent) else {
        return;
    };
    assert_governed_model(&home, agent, model);
    assert_mcp_registered(&home, agent);
    assert_example_skill_materialized(&home, agent);
}

#[test]
fn codex_managed_config() {
    assert_managed_config("codex", "gpt-5.6-terra");
}

#[test]
fn kimi_managed_config() {
    assert_managed_config("kimi", "kimi-k2.6");
}

#[test]
fn opencode_managed_config() {
    assert_managed_config("opencode", "gpt-5.6-terra");
}
