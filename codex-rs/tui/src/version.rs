/// The upstream Codex version this fork is based on.
pub const CODEX_UPSTREAM_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Source-controlled identity for the maintained MCP lifecycle fork.
pub const CODEX_FORK_BUILD_ID: &str = "jkammerland.mcp.16";

/// The user-visible version, including this fork's build identity.
pub const CODEX_CLI_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+jkammerland.mcp.16");
