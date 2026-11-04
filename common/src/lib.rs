//! What Steery's drivers share, each keeping only what its own harness writes:
//! finding and asking the harness's executable ([`program`]), reading a transcript's
//! lines within bounds ([`lines`]), following a file the harness only appends to
//! ([`follow`]), a transcript that is a tree of records, read along its path in batches
//! ([`tree`]), a delivery's receipt in a transcript ([`receipt`]), an answer, such as
//! a read that said nothing, given again while the files it stood on stand as they did
//! ([`quiet`]), the MCP servers a harness's JSON configuration names ([`mcp`]), an
//! Anthropic API key as a way to give a harness a sign-in ([`anthropic`]), and a harness's
//! own sign-in run on a terminal of the driver's, given back what its browser returned to
//! ([`login`]).

pub mod anthropic;
pub mod follow;
pub mod lines;
pub mod login;
pub mod mcp;
pub mod program;
pub mod quiet;
pub mod receipt;
pub mod tree;
