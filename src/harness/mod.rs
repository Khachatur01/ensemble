//! How a harness talks to Ensemble.
//!
//! One surface: an MCP server over stdio. See `mcp` for why that one and not
//! another, and `docs/HARNESS.md` for what was verified to reach that
//! conclusion.

pub mod mcp;
