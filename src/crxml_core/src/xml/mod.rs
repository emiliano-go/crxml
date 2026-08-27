//! Crystal Reports XML adapter for `rypipe-core`, embedded in crxml.
//!
//! Provides a [`CrystalXmlDecoder`] that emits field events for Crystal Reports
//! XML rows and a [`CrystalXmlSplitter`] that finds row-boundary split points
//! for parallel/bounded parsing.

pub mod decoder;
pub mod error;
pub(crate) mod scanner;
pub mod splitter;

pub use decoder::CrystalXmlDecoder;
pub use splitter::CrystalXmlSplitter;
