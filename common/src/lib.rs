//! What Steery's drivers share, each keeping only what its own harness writes:
//! finding and asking the harness's executable ([`program`]), reading a transcript's
//! lines within bounds ([`lines`]), following a file the harness only appends to
//! ([`follow`]), a transcript that is a tree of records, read along its path in batches
//! ([`tree`]), a delivery's receipt in a transcript ([`receipt`]), and a read that said
//! nothing answered again while the files it stood on stand as they did ([`quiet`]).

pub mod follow;
pub mod lines;
pub mod program;
pub mod quiet;
pub mod receipt;
pub mod tree;
