//! Command handlers.
//!
//! Each function implements one CLI subcommand. The read-back commands
//! (`transcript`, `show`, `runs`, `clean`) are fully functional over saved runs;
//! `review` does real config discovery, git-based change detection, and routing,
//! then hands off to the [`crate::runner`] to execute the matched reviewers. The
//! runner owns event emission and persistence; this handler renders the stream and
//! reports the aggregate decision so the CLI can set the exit status.
//! `codeowners` is pure generation.

mod attest;
mod codeowners;
mod github_report;
mod read_back;
mod review;
mod skills;
mod update;
mod validate;

pub use attest::attest;
pub use codeowners::codeowners;
pub use github_report::github_report;
pub use read_back::{clean, runs, show, transcript};
pub use review::{GithubSource, ReviewOptions, review};
pub use skills::{skills_check, skills_install, skills_list};
pub use update::{update, update_check_worker};
pub use validate::validate;
