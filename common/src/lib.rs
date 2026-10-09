//! What Steery's official drivers share, each keeping only what its own harness writes:
//! finding and asking the harness's executable ([`program`]), reading a transcript's
//! lines within bounds ([`lines`]), following a file the harness only appends to
//! ([`follow`]), and a transcript that is a tree of records, read along its path in
//! batches ([`tree`]).

pub mod follow;
pub mod lines;
pub mod program;
pub mod tree;
