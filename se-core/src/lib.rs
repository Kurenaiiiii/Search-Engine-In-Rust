//! Search Engine in Rust — core library: analysis, codecs, segments,
//! query parser, search, index management, and a brute-force oracle.

pub mod analysis;
pub mod brute;
pub mod codec;
pub mod index;
pub mod query;
pub mod search;
pub mod segment;

pub use analysis::{Analyzer, Schema};
pub use index::{Index, Stats};
pub use query::{parse, Query};
pub use search::{Hit, Scorer, SearchOptions};
