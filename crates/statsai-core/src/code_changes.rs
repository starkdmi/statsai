//! Local code-change measurement, Git inspection, and conservative trace matching.
//!
//! The pipeline runs in one direction. [`classify`] decides what counts as
//! code, [`patch`] turns a recorded agent edit into counted lines, [`git`]
//! reads what the local repository already contains, [`matching`] decides
//! which commit an edit became, and [`metrics`] aggregates the result into
//! records that may be published. [`types`] holds the vocabulary they share.

mod classify;
mod git;
mod matching;
mod metrics;
mod patch;
#[cfg(test)]
mod test_support;
mod types;

pub use classify::*;
pub use git::*;
pub use matching::*;
pub use metrics::*;
pub use patch::*;
pub use types::*;
