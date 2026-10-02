//! Computer use over pingpong: an AI agent uses a paired host the way a
//! person uses it through Ping -- the same tunnel, the same picture, the same
//! keyboard and mouse -- as an identity of its own that the host holds to
//! the agent rules (see docs/ai-agents.md).
//!
//! - [`headless`]: a session without a window, pictures decoded in memory.
//! - [`computer`]: the actions (click, type, key, scroll, drag, zoom, ...),
//!   each answered with the settled screen.
//! - [`mcp`]: those actions as an MCP server, for any agent that speaks MCP;
//!   [`install`] adds it to other agents' settings.
//! - [`providers`] and [`runner`]: running an agent on a task from Ping
//!   itself -- through Claude Code or Codex (the user's subscription), or
//!   straight to Anthropic's, OpenAI's or an OpenAI-compatible API (a key).

pub mod computer;
pub mod control;
pub mod conversation;
pub mod decode;
pub mod frame;
pub mod headless;
pub mod install;
pub mod keys;
pub mod mcp;
pub mod providers;
pub mod risk;
pub mod runner;
