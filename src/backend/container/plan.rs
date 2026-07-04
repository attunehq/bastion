use std::path::{Component, Path};

use color_eyre::eyre::{Context, Result, bail};

use crate::reviewer::{Reviewer, RunnerSpec};

use super::ContainerEngine;
use crate::backend::command::{CommandRunner, CommandSpec};

/// How a reviewer is executed, resolved once from its profile.
///
/// Parsing a [`Reviewer`] into this type is the single place that rejects an
/// unprovisioned capability tier, so holding an `ExecutionPlan` is proof the
/// reviewer is runnable in this build.
#[derive(Debug)]
pub enum ExecutionPlan {
    /// Run the backend directly on the host.
    Native,
    /// Run the backend inside a provisioned container.
    Container(ContainerPlan),
}

impl ExecutionPlan {
    /// Resolve a reviewer's execution plan, failing closed on a tier this build
    /// does not yet provision.
    ///
    /// `mcp` and `skills` are container-provisioned tiers that are still unwired
    /// (a later PR each), so they fail closed whether or not a `runner` is present.
    /// Network scoping is likewise unbuilt: the only egress tier a container can be
    /// given today is *general* outbound (`network: true`), so a containerized
    /// reviewer must opt into it. A containerized `network: false` (the default)
    /// fails closed: Bastion cannot scope a container's egress to the model provider
    /// yet, and granting general egress would make `network: false` inaccurate. A
    /// native `network: true` also fails closed: with no container there is nothing to
    /// scope a network into, so honoring it would be meaningless.
    ///
    /// # Errors
    ///
    /// Returns an error naming the unprovisioned opt-in, or describing an invalid
    /// `runner`: one that declares neither a `dockerfile` nor an `image`, a
    /// `dockerfile` path that is absolute or escapes the repository with `..`, or an
    /// `image` reference that begins with `-`. [`dispatch`](super::super::dispatch) turns any
    /// of these into a fail-closed block for a gate and a skip for an advisor.
    pub fn resolve(reviewer: &Reviewer) -> Result<Self> {
        if !reviewer.capabilities.mcp.is_empty() {
            bail!(
                "reviewer '{}' declares MCP servers {:?}, but this build does not provision \
                 MCP; failing closed rather than reviewing without the tools it asked for",
                reviewer.name,
                reviewer.capabilities.mcp
            );
        }
        if !reviewer.capabilities.skills.is_empty() {
            bail!(
                "reviewer '{}' declares skills {:?}, but this build does not load skills into \
                 the agent's context; failing closed rather than reviewing without them",
                reviewer.name,
                reviewer.capabilities.skills
            );
        }

        match reviewer.runner.as_ref() {
            Some(runner) => {
                let source = ImageSource::resolve(runner).wrap_err_with(|| {
                    format!("reviewer '{}' has an unprovisionable runner", reviewer.name)
                })?;
                // A container's egress cannot be scoped to the provider yet, so the
                // only tier it can be given is general outbound (`network: true`). The
                // default `network: false` cannot be enforced, so it fails closed
                // rather than silently attaching general egress, the same way the
                // `mcp`/`skills` arms reject an unprovisioned tier instead of running
                // degraded.
                if !reviewer.capabilities.network {
                    bail!(
                        "reviewer '{}' runs in a container with the default `network: false`, \
                         but this build cannot scope a container's egress to the model provider \
                         (the allowlisting proxy is unbuilt); it fails closed rather than silently \
                         attaching general egress. Set `network: true` to accept general egress \
                         until scoped egress lands.",
                        reviewer.name
                    );
                }
                Ok(Self::Container(ContainerPlan { source }))
            }
            None => {
                if reviewer.capabilities.network {
                    bail!(
                        "reviewer '{}' opts into `network` access without a container `runner`; \
                         this build cannot scope a native reviewer's network, so it fails closed \
                         (add a `runner` to run it in a container)",
                        reviewer.name
                    );
                }
                Ok(Self::Native)
            }
        }
    }
}

/// A resolved plan to run a reviewer in a container: which image to run.
///
/// A container plan is built only for a reviewer that opted into general egress
/// (`network: true`); a containerized `network: false` fails closed in
/// [`ExecutionPlan::resolve`], so the plan carries no network flag (the container
/// always attaches the engine's default network).
#[derive(Debug, Clone)]
pub struct ContainerPlan {
    source: ImageSource,
}

impl ContainerPlan {
    /// Build or resolve the image, returning the reference to run.
    ///
    /// A `dockerfile` source is built with the repo root as the build context; an
    /// `image` source is returned as-is (the engine pulls it on demand at run time).
    /// The returned [`ImageReference`] is the proof the image slot is resolved: it is
    /// minted only here, after `ImageSource::resolve` has rejected an option-like
    /// reference, so [`ContainerRunner`](super::ContainerRunner) cannot be handed an
    /// arbitrary string for the `docker run` image position.
    ///
    /// # Errors
    ///
    /// Returns an error if the Dockerfile cannot be read or canonicalized (which also
    /// covers a missing file), if the repo root cannot be canonicalized, if the
    /// canonical Dockerfile path resolves outside the repository (a symlink escape), if
    /// the build cannot be run (the engine fails to spawn, or its output cannot be
    /// captured, surfaced by the [`CommandRunner`]), or if the build exits non-zero.
    pub async fn ensure_image<R: CommandRunner>(
        &self,
        engine: &ContainerEngine,
        runner: &R,
        repo_root: &Path,
    ) -> Result<ImageReference> {
        match &self.source {
            ImageSource::Image(reference) => Ok(ImageReference(reference.clone())),
            ImageSource::Dockerfile(relative) => {
                let dockerfile = repo_root.join(relative);
                // Confirm the resolved Dockerfile really sits under the repo root.
                // `ImageSource::resolve` already rejected absolute and `..` paths
                // lexically; canonicalizing here also catches a symlink inside the
                // checkout that points outside it, so the build never reads or builds
                // a file beyond the repository. Canonicalizing requires the file to
                // exist, which also gives the missing-Dockerfile fail-closed error.
                let canonical_dockerfile = dockerfile
                    .canonicalize()
                    .wrap_err_with(|| format!("reading dockerfile {}", dockerfile.display()))?;
                let canonical_root = repo_root
                    .canonicalize()
                    .wrap_err_with(|| format!("resolving repo root {}", repo_root.display()))?;
                if !canonical_dockerfile.starts_with(&canonical_root) {
                    bail!(
                        "`runner.dockerfile` resolves to `{}`, outside the repository `{}`; \
                         refusing to build from a path that escapes the checkout",
                        canonical_dockerfile.display(),
                        canonical_root.display()
                    );
                }
                let tag = image_tag(&canonical_root, &canonical_dockerfile)?;
                let mut spec = CommandSpec::new(engine.program().to_os_string(), repo_root);
                spec.arg("build")
                    .arg("-t")
                    .arg(&tag)
                    .arg("-f")
                    .arg(dockerfile.as_os_str().to_os_string())
                    .arg(repo_root.as_os_str().to_os_string());
                let output = runner.run(&spec).await?;
                if !output.success() {
                    let code = output
                        .code
                        .map_or_else(|| "signal".to_string(), |c| c.to_string());
                    bail!(
                        "`{} build` failed for `{}` (exit {}): {}",
                        engine.program().to_string_lossy(),
                        relative.display(),
                        code,
                        output.stderr.trim()
                    );
                }
                Ok(ImageReference(tag))
            }
        }
    }
}

/// A resolved container image reference, ready for the `docker run` image slot.
///
/// Minted only by [`ContainerPlan::ensure_image`] (from a built tag or a prebuilt
/// `image` that `ImageSource::resolve` already proved is not option-like), so a
/// [`ContainerRunner`](super::ContainerRunner) cannot be constructed from an arbitrary
/// or option-like string. It is an opaque wrapper: read it with [`as_str`](Self::as_str).
#[derive(Debug, Clone)]
pub struct ImageReference(String);

impl ImageReference {
    /// Borrow the underlying image reference.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Build a reference directly, for tests that construct a runner without resolving
    /// an image through `ensure_image`.
    #[cfg(test)]
    pub(crate) fn for_test(reference: impl Into<String>) -> Self {
        Self(reference.into())
    }
}

impl std::fmt::Display for ImageReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A runner field's trimmed value, or `None` when it is absent or blank. Both image
/// sources treat a whitespace-only field as unset, so a stray newline from a YAML
/// block scalar never becomes part of a path or image reference.
fn non_blank(field: &Option<String>) -> Option<&str> {
    field.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// The provenance of a reviewer's container image, parsed from its `runner` block.
///
/// Exactly one source, with `dockerfile` taking precedence over `image` (per the
/// core design). Parsing here removes the "both / neither" ambiguity the raw
/// [`RunnerSpec`] allows.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ImageSource {
    /// Build the image from a Dockerfile (path relative to the repo root).
    Dockerfile(std::path::PathBuf),
    /// Use a prebuilt image reference (pulled on demand by the engine).
    Image(String),
}

impl ImageSource {
    /// Parse a [`RunnerSpec`] into its single image source.
    ///
    /// # Errors
    ///
    /// Returns an error if the runner declares neither a non-empty `dockerfile` nor
    /// a non-empty `image` (an empty `runner: {}` has nothing to provision), if the
    /// `dockerfile` path is not confined to the repository (absolute, or escaping via
    /// `..`, since it is later joined onto the repo root and built), or if the `image`
    /// reference begins with `-` (it becomes a `docker run` argument, where a leading
    /// dash would be parsed as an option rather than the image).
    fn resolve(runner: &RunnerSpec) -> Result<Self> {
        if let Some(dockerfile) = non_blank(&runner.dockerfile) {
            let path = Path::new(dockerfile);
            if path.is_absolute()
                || path
                    .components()
                    .any(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
            {
                bail!(
                    "`runner.dockerfile` must be a path relative to the repository root, \
                     but `{dockerfile}` is absolute"
                );
            }
            if path.components().any(|c| matches!(c, Component::ParentDir)) {
                bail!(
                    "`runner.dockerfile` must stay within the repository, but `{dockerfile}` \
                     escapes it with `..`"
                );
            }
            return Ok(Self::Dockerfile(path.to_path_buf()));
        }
        if let Some(image) = non_blank(&runner.image) {
            if image.starts_with('-') {
                bail!(
                    "`runner.image` must be an image reference, but `{image}` begins with `-`; \
                     it would be parsed as a `docker run` option rather than the image"
                );
            }
            return Ok(Self::Image(image.to_string()));
        }
        bail!("`runner` declares neither a `dockerfile` nor an `image`; nothing to provision")
    }
}

/// Derive a stable image tag from a build context root and its Dockerfile.
///
/// Both paths are expected to be already canonical (the caller, `ensure_image`,
/// canonicalizes them to confine the Dockerfile to the repo). The tag changes when
/// the Dockerfile content changes (forcing a rebuild) and is stable when it does not
/// (so the engine's layer cache is hit). It also folds in the build context identity
/// (`context_root`): the build context is the whole repo, so two repos or worktrees
/// with byte-identical Dockerfiles must not collide on one global tag and overwrite
/// each other's image between build and run. A non-cryptographic hash is enough: a
/// collision only means a stale cache hit on otherwise-identical content, and the
/// value is a local cache key, not a security boundary.
///
/// # Errors
///
/// Returns an error if the Dockerfile cannot be read.
fn image_tag(context_root: &Path, dockerfile: &Path) -> Result<String> {
    use std::hash::{Hash, Hasher};
    let bytes = std::fs::read(dockerfile)
        .wrap_err_with(|| format!("reading dockerfile {}", dockerfile.display()))?;
    let mut hasher = std::hash::DefaultHasher::new();
    context_root.hash(&mut hasher);
    bytes.hash(&mut hasher);
    Ok(format!("bastion-reviewer:{:016x}", hasher.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::command::CommandOutput;
    use crate::backend::container::testutil::*;
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn least_privilege_resolves_to_native() {
        assert!(matches!(
            ExecutionPlan::resolve(&reviewer()).unwrap(),
            ExecutionPlan::Native
        ));
    }

    #[test]
    fn native_network_optin_fails_closed() {
        let mut r = reviewer();
        r.capabilities.network = true;
        let err = ExecutionPlan::resolve(&r).unwrap_err();
        assert!(err.to_string().contains("network"));
        assert!(err.to_string().contains("runner"));
    }

    #[test]
    fn mcp_fails_closed_even_with_a_runner() {
        let mut r = reviewer();
        r.runner = Some(RunnerSpec {
            dockerfile: Some("d".into()),
            image: None,
        });
        r.capabilities.mcp = vec!["playwright".into()];
        let err = ExecutionPlan::resolve(&r).unwrap_err();
        assert!(err.to_string().contains("MCP"));
    }

    #[test]
    fn skills_fails_closed_even_with_a_runner() {
        let mut r = reviewer();
        r.runner = Some(RunnerSpec {
            dockerfile: Some("d".into()),
            image: None,
        });
        r.capabilities.skills = vec!["stop-slop".into()];
        let err = ExecutionPlan::resolve(&r).unwrap_err();
        assert!(err.to_string().contains("skills"));
    }

    #[test]
    fn containerized_reviewer_resolves_to_a_container_plan() {
        let mut r = reviewer();
        r.runner = Some(RunnerSpec {
            dockerfile: Some("./e2e.Dockerfile".into()),
            image: None,
        });
        // A container needs an explicit `network: true` to run today (general egress);
        // the default `network: false` fails closed (see the test below).
        r.capabilities.network = true;
        match ExecutionPlan::resolve(&r).unwrap() {
            ExecutionPlan::Container(plan) => {
                assert_eq!(
                    plan.source,
                    ImageSource::Dockerfile(PathBuf::from("./e2e.Dockerfile"))
                );
            }
            ExecutionPlan::Native => panic!("expected a container plan"),
        }
    }

    #[test]
    fn containerized_network_false_fails_closed() {
        // A containerized reviewer with the default `network: false` cannot be given
        // provider-only egress (the allowlist proxy is unbuilt), so it fails closed
        // rather than silently attaching general egress under a flag that reads as
        // restricted. Only an explicit `network: true` runs in a container today.
        let mut r = reviewer();
        r.runner = Some(RunnerSpec {
            dockerfile: Some("./e2e.Dockerfile".into()),
            image: None,
        });
        // `capabilities.network` defaults to false.
        let err = ExecutionPlan::resolve(&r).unwrap_err();
        assert!(err.to_string().contains("network: false"), "{err}");
        assert!(err.to_string().contains("fails closed"), "{err}");
    }

    #[test]
    fn an_empty_runner_fails_closed() {
        let mut r = reviewer();
        r.runner = Some(RunnerSpec {
            dockerfile: None,
            image: None,
        });
        // The chain (as the runner records it with `{:#}`) names the empty runner.
        let err = ExecutionPlan::resolve(&r).unwrap_err();
        assert!(format!("{err:#}").contains("neither"));
    }

    #[test]
    fn dockerfile_takes_precedence_over_image() {
        let source = ImageSource::resolve(&RunnerSpec {
            dockerfile: Some("./Dockerfile".into()),
            image: Some("ghcr.io/acme/e2e:latest".into()),
        })
        .unwrap();
        assert_eq!(
            source,
            ImageSource::Dockerfile(PathBuf::from("./Dockerfile"))
        );
    }

    #[test]
    fn dockerfile_rejects_an_absolute_path() {
        // An absolute path is later joined onto the repo root and built; reject it at
        // the parse boundary so a registry cannot point the build outside the checkout.
        #[cfg(windows)]
        let absolute = r"C:\etc\evil.Dockerfile";
        #[cfg(not(windows))]
        let absolute = "/etc/evil.Dockerfile";
        let err = ImageSource::resolve(&RunnerSpec {
            dockerfile: Some(absolute.into()),
            image: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
    }

    #[test]
    fn dockerfile_rejects_parent_traversal() {
        let err = ImageSource::resolve(&RunnerSpec {
            dockerfile: Some("../../etc/evil.Dockerfile".into()),
            image: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("escapes"), "{err}");
    }

    #[test]
    fn image_rejects_an_option_like_reference() {
        // A leading `-` would be parsed as a `docker run` option, shifting the backend
        // program into the image slot; reject it at the parse boundary.
        let err = ImageSource::resolve(&RunnerSpec {
            dockerfile: None,
            image: Some("--privileged".into()),
        })
        .unwrap_err();
        assert!(err.to_string().contains("begins with `-`"), "{err}");
    }

    #[test]
    fn resolve_trims_surrounding_whitespace() {
        // The fields are trimmed before use, so a stray newline from a YAML block
        // scalar does not become part of the path or image reference.
        let dockerfile = ImageSource::resolve(&RunnerSpec {
            dockerfile: Some("  ./Dockerfile\n".into()),
            image: None,
        })
        .unwrap();
        assert_eq!(
            dockerfile,
            ImageSource::Dockerfile(PathBuf::from("./Dockerfile"))
        );
        let image = ImageSource::resolve(&RunnerSpec {
            dockerfile: None,
            image: Some("  ghcr.io/acme/e2e:latest \n".into()),
        })
        .unwrap();
        assert_eq!(image, ImageSource::Image("ghcr.io/acme/e2e:latest".into()));
    }

    #[test]
    fn a_blank_dockerfile_falls_back_to_the_image() {
        // A `dockerfile` that is present but blank (whitespace only) is treated as
        // absent, so a runner that also sets `image` resolves to that image rather
        // than failing closed on an empty dockerfile path.
        let source = ImageSource::resolve(&RunnerSpec {
            dockerfile: Some("   ".into()),
            image: Some("ghcr.io/acme/e2e:latest".into()),
        })
        .unwrap();
        assert_eq!(source, ImageSource::Image("ghcr.io/acme/e2e:latest".into()));
    }

    #[test]
    fn a_runner_with_only_blank_fields_fails_closed() {
        let err = ImageSource::resolve(&RunnerSpec {
            dockerfile: Some("  ".into()),
            image: Some("\n".into()),
        })
        .unwrap_err();
        assert!(err.to_string().contains("neither"), "{err}");
    }

    #[tokio::test]
    async fn ensure_image_uses_a_prebuilt_image_without_building() {
        let plan = ContainerPlan {
            source: ImageSource::Image("ghcr.io/acme/e2e:latest".into()),
        };
        let runner = RecordingRunner::default();
        let engine = ContainerEngine::with_program("docker");
        let image = plan
            .ensure_image(&engine, &runner, Path::new("/repo"))
            .await
            .unwrap();
        assert_eq!(image.as_str(), "ghcr.io/acme/e2e:latest");
        // No build was run for a prebuilt image.
        assert!(runner.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ensure_image_builds_from_a_dockerfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("e2e.Dockerfile"), b"FROM scratch\n").unwrap();
        let plan = ContainerPlan {
            source: ImageSource::Dockerfile(PathBuf::from("e2e.Dockerfile")),
        };
        let runner = RecordingRunner::with(vec![CommandOutput {
            code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        }]);
        let engine = ContainerEngine::with_program("docker");
        let tag = plan
            .ensure_image(&engine, &runner, dir.path())
            .await
            .unwrap();
        assert!(tag.as_str().starts_with("bastion-reviewer:"));
        let build = runner.last();
        assert_eq!(build.program, OsString::from("docker"));
        let args = args_of(&build);
        assert_eq!(args[0], "build");
        // `-t <tag>`: the content-addressed tag follows the flag.
        let t_at = args.iter().position(|a| a == "-t").expect("a -t flag");
        assert_eq!(args[t_at + 1], tag.as_str());
        // `-f <dockerfile>`: the exact resolved Dockerfile path follows the flag, not
        // just the bare flag (a malformed `docker build` could otherwise pass).
        let f_at = args.iter().position(|a| a == "-f").expect("a -f flag");
        let expected_dockerfile = dir.path().join("e2e.Dockerfile");
        assert_eq!(args[f_at + 1], expected_dockerfile.to_string_lossy());
        // The final positional argument is the build context: the repo root.
        assert_eq!(
            args.last().map(String::as_str),
            Some(dir.path().to_string_lossy().as_ref())
        );
    }

    #[test]
    fn image_tag_is_content_and_context_addressed() {
        // Within one build context (repo root): identical Dockerfile contents yield
        // the same tag (cache hit), and any content change yields a different tag
        // (forced rebuild). A constant or path-based tag would violate this.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("A.Dockerfile");
        let b = dir.path().join("B.Dockerfile");
        let c = dir.path().join("C.Dockerfile");
        std::fs::write(&a, b"FROM scratch\nRUN echo one\n").unwrap();
        std::fs::write(&b, b"FROM scratch\nRUN echo one\n").unwrap();
        std::fs::write(&c, b"FROM scratch\nRUN echo two\n").unwrap();
        let tag_a = image_tag(dir.path(), &a).unwrap();
        let tag_b = image_tag(dir.path(), &b).unwrap();
        let tag_c = image_tag(dir.path(), &c).unwrap();
        // Same content, same context -> same tag.
        assert_eq!(tag_a, tag_b);
        // Different content -> different tag.
        assert_ne!(tag_a, tag_c);
        assert!(tag_a.starts_with("bastion-reviewer:"));

        // Same Dockerfile contents but a different build context (a second repo or
        // worktree) must NOT collide on one global tag: otherwise two processes could
        // overwrite each other's image between build and run.
        let other = tempfile::tempdir().unwrap();
        let a2 = other.path().join("A.Dockerfile");
        std::fs::write(&a2, b"FROM scratch\nRUN echo one\n").unwrap();
        let tag_other = image_tag(other.path(), &a2).unwrap();
        assert_ne!(tag_a, tag_other);
    }

    #[tokio::test]
    async fn ensure_image_fails_closed_on_a_failed_build() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), b"FROM scratch\n").unwrap();
        let plan = ContainerPlan {
            source: ImageSource::Dockerfile(PathBuf::from("Dockerfile")),
        };
        let runner = RecordingRunner::with(vec![CommandOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: "build broke".into(),
        }]);
        let engine = ContainerEngine::with_program("docker");
        let err = plan
            .ensure_image(&engine, &runner, dir.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("build broke"));
    }

    #[tokio::test]
    async fn ensure_image_fails_closed_on_an_unreadable_dockerfile() {
        // A missing Dockerfile fails the confinement check (canonicalizing it) before
        // any build runs: the reviewer fails closed rather than building from nothing.
        let dir = tempfile::tempdir().unwrap();
        let plan = ContainerPlan {
            source: ImageSource::Dockerfile(PathBuf::from("does-not-exist.Dockerfile")),
        };
        let runner = RecordingRunner::default();
        let engine = ContainerEngine::with_program("docker");
        let err = plan
            .ensure_image(&engine, &runner, dir.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("reading dockerfile"));
        // No build was attempted: we failed before reaching the engine.
        assert!(runner.seen.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ensure_image_rejects_a_symlink_escaping_the_repo() {
        // A repo-local relative path passes the lexical checks but resolves, through a
        // symlink, to a Dockerfile outside the checkout. Canonicalizing in
        // `ensure_image` catches that and fails closed before building.
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("evil.Dockerfile"), b"FROM scratch\n").unwrap();
        let repo = tempfile::tempdir().unwrap();
        symlink(
            outside.path().join("evil.Dockerfile"),
            repo.path().join("link.Dockerfile"),
        )
        .unwrap();
        let plan = ContainerPlan {
            source: ImageSource::Dockerfile(PathBuf::from("link.Dockerfile")),
        };
        let runner = RecordingRunner::default();
        let engine = ContainerEngine::with_program("docker");
        let err = plan
            .ensure_image(&engine, &runner, repo.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("outside the repository"), "{err}");
        // No build was attempted: we failed before reaching the engine.
        assert!(runner.seen.lock().unwrap().is_empty());
    }
}
