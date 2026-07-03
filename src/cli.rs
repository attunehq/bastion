//! The command-line surface.
//!
//! This module defines the clap argument tree and dispatches to the handlers in
//! [`crate::commands`]. The command set mirrors `docs/developer-guide/local-surface.md`: `review` runs the
//! triggered reviewers, and the read-back commands (`transcript`, `show`, `runs`,
//! `clean`) inspect saved runs. `codeowners` generates the governance block from
//! `docs/developer-guide/github-adapter.md`.

use std::num::NonZeroU64;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use color_eyre::eyre::{Context, Result, bail};

use crate::paths::Layout;
use crate::render::Format;
use crate::verdict::Decision;

/// Agentic code review: single-concern reviewers as fitness functions.
#[derive(Debug, Parser)]
#[command(name = "bastion")]
#[command(about = "Agentic code review: single-concern reviewers as fitness functions.")]
#[command(version = crate::version::VERSION)]
pub struct Cli {
    /// Override the data directory used to store and read runs.
    #[arg(long, global = true, value_name = "PATH", env = crate::paths::DATA_DIR_ENV)]
    pub data_dir: Option<PathBuf>,

    /// Override the user-level config directory searched for a personal
    /// `.bastion.yaml`, whose reviewers are merged with the repository's. Defaults
    /// to the platform config dir (e.g. `~/.config/bastion`).
    #[arg(long, global = true, value_name = "PATH", env = crate::config::CONFIG_DIR_ENV)]
    pub config_dir: Option<PathBuf>,

    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// The top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the triggered reviewers against the working tree and gate the result.
    Review {
        /// Base branch the changeset is computed against.
        #[arg(long, default_value = "main")]
        base: String,
        /// Output format.
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
        /// The `owner/name` repository, used with `--pr` to gather the pull request's
        /// description and discussion as reviewer context. Defaults to
        /// `$GITHUB_REPOSITORY`.
        #[arg(long, value_name = "OWNER/NAME", env = "GITHUB_REPOSITORY")]
        repo: Option<String>,
        /// The pull request number. When set (with `--repo`), the reviewers are given
        /// the PR's description, discussion, and their prior findings as context.
        /// Absent for a purely local review, which uses only the local context. A PR
        /// number is positive, so `--pr 0` is rejected at parse time.
        #[arg(long, value_name = "N")]
        pr: Option<NonZeroU64>,
        /// Run only this triggered reviewer (repeatable). Names must belong to
        /// reviewers the changeset triggered; an unknown or untriggered name is an
        /// error. Excluding a triggered reviewer makes the run partial: it is
        /// marked as such in the output and the stored run, and cannot be attested.
        #[arg(long = "reviewer", visible_alias = "only", value_name = "NAME")]
        reviewers: Vec<String>,
        /// Execute every triggered reviewer even when its trigger-scoped diff is
        /// unchanged since this branch's previous run (disable carrying prior
        /// passes forward).
        #[arg(long)]
        fresh: bool,
    },
    /// Parse the reviewer registry and report any problems, without running a
    /// reviewer or spending a model call.
    Validate {
        /// Registry file to validate. Defaults to discovery: the `.bastion.yaml`
        /// (or `.bastion.yml`) found by walking up from the current directory.
        #[arg(value_name = "FILE")]
        file: Option<PathBuf>,
    },
    /// Print a reviewer's saved session transcript (defaults to the latest run).
    Transcript {
        /// Either `<reviewer>` (latest run) or `<run> <reviewer>`.
        #[arg(value_name = "RUN_OR_REVIEWER")]
        first: String,
        /// The reviewer name, when a run id is given as the first argument.
        #[arg(value_name = "REVIEWER")]
        second: Option<String>,
    },
    /// Re-emit a past run's verdicts and findings (defaults to the latest run).
    Show {
        /// The run id; defaults to the latest run.
        run: Option<String>,
        /// Output format.
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },
    /// List recorded runs, most recent first.
    Runs {
        /// Output format.
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },
    /// Prune saved runs.
    Clean {
        /// Keep only the N most recent runs.
        #[arg(long, value_name = "N", conflicts_with = "older_than")]
        keep: Option<usize>,
        /// Remove runs older than this duration (e.g. `7d`, `12h`).
        #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
        older_than: Option<Duration>,
    },
    /// GitHub-specific helpers (the CI adapter surface).
    Github {
        /// The GitHub subcommand to run.
        #[command(subcommand)]
        command: GithubCommand,
    },
    /// Manage the bundled agent skills that teach coding agents to use Bastion.
    Skills {
        /// The skills subcommand to run.
        #[command(subcommand)]
        command: SkillsCommand,
    },
    /// Sign a sealed local run as an attestation note on HEAD, so CI can
    /// verify and replay it instead of re-executing the reviewers
    /// (`docs/developer-guide/attestation.md`).
    Attest {
        /// The run to attest; defaults to the latest recorded run.
        run: Option<String>,
        /// The SSH signing key to use. Defaults to `git config
        /// user.signingkey`.
        #[arg(long, value_name = "PATH")]
        key: Option<PathBuf>,
    },
}

/// Skills adapter subcommands. They install the skills bundled into this binary
/// into a repository (so its agents discover how to drive `bastion review`) and
/// check that the checked-in copies are still current.
#[derive(Debug, Subcommand)]
pub enum SkillsCommand {
    /// Write the bundled skills into the repository so agents discover them.
    Install {
        /// Skills directory to install into, relative to the repo root. Repeatable;
        /// defaults to `.claude/skills` and `.agents/skills`.
        #[arg(long = "dir", value_name = "DIR")]
        dirs: Vec<PathBuf>,
        /// Overwrite existing skill files even if they were edited locally.
        #[arg(long)]
        force: bool,
    },
    /// Check that the installed skills match this binary (non-zero exit on drift).
    Check {
        /// Skills directory to check, relative to the repo root. Repeatable;
        /// defaults to `.claude/skills` and `.agents/skills`.
        #[arg(long = "dir", value_name = "DIR")]
        dirs: Vec<PathBuf>,
    },
    /// List the skills bundled into this binary.
    List,
}

/// GitHub adapter subcommands. These are specific to the GitHub surface
/// (`docs/developer-guide/github-adapter.md`); the core review surface stays forge-agnostic.
#[derive(Debug, Subcommand)]
pub enum GithubCommand {
    /// Print a CODEOWNERS block protecting Bastion's reviewer-policy paths.
    Codeowners {
        /// Owner(s) to assign (e.g. `@acme/platform`). Repeatable.
        #[arg(long = "owner", value_name = "OWNER", required = true)]
        owners: Vec<String>,
    },
    /// Post a finished run's results to its pull request (sticky comment plus a
    /// check run per reviewer and the aggregate `bastion` check).
    Report {
        /// The `owner/name` repository. Defaults to `$GITHUB_REPOSITORY`.
        #[arg(long, value_name = "OWNER/NAME", env = "GITHUB_REPOSITORY")]
        repo: String,
        /// The pull request number.
        #[arg(long, value_name = "N")]
        pr: u64,
        /// The head commit SHA the check runs attach to.
        #[arg(long, value_name = "SHA")]
        sha: String,
        /// The run to report; defaults to the latest recorded run.
        #[arg(value_name = "RUN")]
        run: Option<String>,
    },
}

/// Parse arguments and dispatch to the matching command handler.
///
/// Returns the process exit code: `bastion review` exits non-zero when the
/// aggregate verdict is `block` (so an agent loop and CI agree that the gate
/// failed), and every command exits zero on success. Errors are surfaced via the
/// `Result` and rendered by `color_eyre`.
///
/// # Errors
///
/// Returns any error from the dispatched command, or exits early via clap on a
/// parse error or `--help`/`--version`.
pub async fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let layout = match cli.data_dir {
        Some(root) => Layout::with_root(root),
        None => Layout::resolve()?,
    };
    // Resolve the user-level config directory once: the explicit `--config-dir`
    // (or its env) when given, otherwise the platform default. `None` only when no
    // home directory can be determined, which simply means no user-level layer.
    let user_config_dir = cli.config_dir.or_else(crate::config::user_config_dir);

    match cli.command {
        Command::Review {
            base,
            format,
            repo,
            pr,
            reviewers,
            fresh,
        } => {
            let cwd = std::env::current_dir().wrap_err("determining the current directory")?;
            // Parse the `--repo`/`--pr` pair into a GitHub source at the boundary so an
            // impossible combination cannot reach dispatch. Gathering a PR's context
            // needs both a number and a repository; `--repo` alone has no PR to read, and
            // `--pr` without a resolvable repository is a usage error, not a silent local
            // review.
            let github = match (repo, pr) {
                (Some(repo), Some(pr)) => Some(crate::commands::GithubSource::new(&repo, pr)?),
                (None, Some(_)) => bail!(
                    "`--pr` needs a repository: pass `--repo <owner/name>` or set $GITHUB_REPOSITORY"
                ),
                (Some(_), None) | (None, None) => None,
            };
            // User-level reviewers are a local-only convenience. A review carrying a
            // GitHub source (`--repo`/`--pr`, set under Actions) is the governed CI
            // path, so it runs the repository's reviewers alone: a self-hosted runner
            // that happens to have a personal config dir must not merge ungoverned
            // reviewers into a PR's gate, and the `repo:` scope must never reach a
            // check run. Only a purely local review layers in the user registry.
            let review_user_dir = if github.is_some() {
                None
            } else {
                user_config_dir.as_deref()
            };
            let options = crate::commands::ReviewOptions {
                base,
                format,
                github,
                only: reviewers,
                fresh,
            };
            let decision = crate::commands::review(&layout, &cwd, options, review_user_dir).await?;
            // A blocked review is an expected, non-error outcome that must still
            // signal failure to the caller: map `block` to a non-zero exit.
            Ok(match decision {
                Decision::Pass => ExitCode::SUCCESS,
                Decision::Block => ExitCode::FAILURE,
            })
        }
        Command::Validate { file } => {
            let cwd = std::env::current_dir().wrap_err("determining the current directory")?;
            crate::commands::validate(&cwd, file.as_deref(), user_config_dir.as_deref())
                .map(|()| ExitCode::SUCCESS)
        }
        Command::Transcript { first, second } => {
            let (run, reviewer) = match second {
                Some(reviewer) => (Some(first), reviewer),
                None => (None, first),
            };
            crate::commands::transcript(&layout, run.as_deref(), &reviewer)
                .map(|()| ExitCode::SUCCESS)
        }
        Command::Show { run, format } => {
            crate::commands::show(&layout, run.as_deref(), format).map(|()| ExitCode::SUCCESS)
        }
        Command::Runs { format } => {
            crate::commands::runs(&layout, format).map(|()| ExitCode::SUCCESS)
        }
        Command::Clean { keep, older_than } => {
            crate::commands::clean(&layout, keep, older_than).map(|()| ExitCode::SUCCESS)
        }
        Command::Github { command } => match command {
            GithubCommand::Codeowners { owners } => {
                crate::commands::codeowners(&owners).map(|()| ExitCode::SUCCESS)
            }
            GithubCommand::Report { repo, pr, sha, run } => {
                let cwd = std::env::current_dir().wrap_err("determining the current directory")?;
                crate::commands::github_report(&layout, &cwd, &repo, pr, &sha, run.as_deref())
                    .await
                    .map(|()| ExitCode::SUCCESS)
            }
        },
        Command::Skills { command } => match command {
            SkillsCommand::Install { dirs, force } => {
                let cwd = std::env::current_dir().wrap_err("determining the current directory")?;
                crate::commands::skills_install(&cwd, &dirs, force).map(|()| ExitCode::SUCCESS)
            }
            SkillsCommand::Check { dirs } => {
                let cwd = std::env::current_dir().wrap_err("determining the current directory")?;
                // Drifted or missing skills are a fail-closed signal for CI, so map
                // them to a non-zero exit, mirroring how `review` maps a block.
                let current = crate::commands::skills_check(&cwd, &dirs)?;
                Ok(if current {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                })
            }
            SkillsCommand::List => crate::commands::skills_list().map(|()| ExitCode::SUCCESS),
        },
        Command::Attest { run, key } => {
            crate::commands::attest(&layout, run.as_deref(), key.as_deref())
                .map(|()| ExitCode::SUCCESS)
        }
    }
}

/// clap value parser for human-friendly durations.
fn parse_duration(raw: &str) -> std::result::Result<Duration, String> {
    humantime::parse_duration(raw).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn review_defaults_to_human_against_main() {
        let cli = Cli::parse_from(["bastion", "review"]);
        match cli.command {
            // `repo`/`pr` are ignored here: `repo` reads `$GITHUB_REPOSITORY`, which is
            // set under Actions (including Bastion's own CI), so it is not deterministic.
            Command::Review { base, format, .. } => {
                assert_eq!(base, "main");
                assert_eq!(format, Format::Human);
            }
            other => panic!("expected review, got {other:?}"),
        }
    }

    #[test]
    fn review_accepts_a_pull_request_for_context() {
        let cli = Cli::parse_from(["bastion", "review", "--repo", "acme/app", "--pr", "42"]);
        match cli.command {
            Command::Review { repo, pr, .. } => {
                assert_eq!(repo.as_deref(), Some("acme/app"));
                assert_eq!(pr, NonZeroU64::new(42));
            }
            other => panic!("expected review, got {other:?}"),
        }
    }

    #[test]
    fn review_collects_repeatable_reviewer_selections_and_fresh() {
        let cli = Cli::parse_from([
            "bastion",
            "review",
            "--reviewer",
            "tenant-isolation",
            "--reviewer",
            "perf",
            "--fresh",
        ]);
        match cli.command {
            Command::Review {
                reviewers, fresh, ..
            } => {
                assert_eq!(reviewers, ["tenant-isolation", "perf"]);
                assert!(fresh);
            }
            other => panic!("expected review, got {other:?}"),
        }
    }

    #[test]
    fn review_accepts_only_as_an_alias_for_reviewer() {
        let cli = Cli::parse_from(["bastion", "review", "--only", "perf"]);
        match cli.command {
            Command::Review { reviewers, .. } => assert_eq!(reviewers, ["perf"]),
            other => panic!("expected review, got {other:?}"),
        }
    }

    #[test]
    fn review_defaults_to_the_full_set_with_carry_enabled() {
        let cli = Cli::parse_from(["bastion", "review"]);
        match cli.command {
            Command::Review {
                reviewers, fresh, ..
            } => {
                assert!(reviewers.is_empty());
                assert!(!fresh);
            }
            other => panic!("expected review, got {other:?}"),
        }
    }

    #[test]
    fn review_rejects_a_zero_pull_request_number() {
        // A PR number is positive; `--pr 0` is rejected at parse time by `NonZeroU64`.
        let err = Cli::try_parse_from(["bastion", "review", "--pr", "0"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn config_dir_is_a_global_flag() {
        // `--config-dir` is global, like `--data-dir`, so it parses on any subcommand
        // and feeds the user-level registry merge.
        let cli = Cli::parse_from(["bastion", "--config-dir", "/etc/bastion", "validate"]);
        assert_eq!(cli.config_dir, Some(PathBuf::from("/etc/bastion")));
        let after = Cli::parse_from(["bastion", "review", "--config-dir", "/etc/bastion"]);
        assert_eq!(after.config_dir, Some(PathBuf::from("/etc/bastion")));
    }

    #[test]
    fn validate_takes_an_optional_file_argument() {
        let none = Cli::parse_from(["bastion", "validate"]);
        assert!(matches!(none.command, Command::Validate { file: None }));

        let some = Cli::parse_from(["bastion", "validate", "config/.bastion.yaml"]);
        match some.command {
            Command::Validate { file: Some(path) } => {
                assert_eq!(path, PathBuf::from("config/.bastion.yaml"));
            }
            other => panic!("expected validate with a file, got {other:?}"),
        }
    }

    #[test]
    fn transcript_accepts_one_or_two_positionals() {
        let one = Cli::parse_from(["bastion", "transcript", "tenant-isolation"]);
        assert!(matches!(
            one.command,
            Command::Transcript { second: None, .. }
        ));

        let two = Cli::parse_from(["bastion", "transcript", "r-0f3a", "tenant-isolation"]);
        assert!(matches!(
            two.command,
            Command::Transcript {
                second: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn codeowners_lives_under_the_github_subcommand() {
        // The old flat form is gone.
        assert!(Cli::try_parse_from(["bastion", "codeowners", "--owner", "@x"]).is_err());

        let cli = Cli::parse_from([
            "bastion",
            "github",
            "codeowners",
            "--owner",
            "@a",
            "--owner",
            "@b",
        ]);
        match cli.command {
            Command::Github {
                command: GithubCommand::Codeowners { owners },
            } => {
                assert_eq!(owners, ["@a", "@b"]);
            }
            other => panic!("expected github codeowners, got {other:?}"),
        }
    }

    #[test]
    fn clean_keep_and_older_than_conflict() {
        let result = Cli::try_parse_from(["bastion", "clean", "--keep", "3", "--older-than", "7d"]);
        assert!(result.is_err());
    }

    #[test]
    fn skills_install_collects_repeatable_dirs_and_force() {
        let cli = Cli::parse_from([
            "bastion",
            "skills",
            "install",
            "--dir",
            ".claude/skills",
            "--dir",
            "vendor/skills",
            "--force",
        ]);
        match cli.command {
            Command::Skills {
                command: SkillsCommand::Install { dirs, force },
            } => {
                assert_eq!(
                    dirs,
                    [
                        PathBuf::from(".claude/skills"),
                        PathBuf::from("vendor/skills")
                    ]
                );
                assert!(force);
            }
            other => panic!("expected skills install, got {other:?}"),
        }
    }

    #[test]
    fn skills_install_defaults_to_no_dirs_and_no_force() {
        // With no `--dir`, the handler fills in the defaults; clap leaves it empty.
        let cli = Cli::parse_from(["bastion", "skills", "install"]);
        match cli.command {
            Command::Skills {
                command: SkillsCommand::Install { dirs, force },
            } => {
                assert!(dirs.is_empty());
                assert!(!force);
            }
            other => panic!("expected skills install, got {other:?}"),
        }
    }

    #[test]
    fn attest_defaults_to_the_latest_run_and_no_explicit_key() {
        let cli = Cli::parse_from(["bastion", "attest"]);
        match cli.command {
            Command::Attest { run, key } => {
                assert_eq!(run, None);
                assert_eq!(key, None);
            }
            other => panic!("expected attest, got {other:?}"),
        }
    }

    #[test]
    fn attest_accepts_a_run_id_and_a_key_path() {
        let cli = Cli::parse_from(["bastion", "attest", "r-0f3a", "--key", "/home/me/.ssh/id"]);
        match cli.command {
            Command::Attest { run, key } => {
                assert_eq!(run.as_deref(), Some("r-0f3a"));
                assert_eq!(key, Some(PathBuf::from("/home/me/.ssh/id")));
            }
            other => panic!("expected attest, got {other:?}"),
        }
    }

    #[test]
    fn skills_has_check_and_list_subcommands() {
        assert!(matches!(
            Cli::parse_from(["bastion", "skills", "check"]).command,
            Command::Skills {
                command: SkillsCommand::Check { .. }
            }
        ));
        assert!(matches!(
            Cli::parse_from(["bastion", "skills", "list"]).command,
            Command::Skills {
                command: SkillsCommand::List
            }
        ));
    }
}
