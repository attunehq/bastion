//! The spawn governor: a per-run circuit breaker over agent subprocess launches.
//!
//! Every backend reaches the OS through [`CommandRunner::run`], so wrapping that
//! seam is the one place that sees *every* agent launch a review run makes,
//! including a backend's reprompt turn and a launch that dies at zero tokens. The
//! runner builds one [`SpawnGovernor`] per `bastion review`, shares it across all
//! the reviewers, and wraps each backend's real runner in a [`GovernedRunner`]. The
//! governor enforces [`SpawnLimits`](crate::limits::SpawnLimits): it bounds how
//! many launches run at once, refuses a launch once the per-run total is reached,
//! and trips a breaker when too many launches in a row produce no output. A trip
//! fails every further launch closed, which the runner turns into an aborted run
//! rather than an endless retry (see [`crate::runner`]).
//!
//! This is the safety net for the incident in [`crate::limits`]: it makes a broken,
//! respawning fan-out stop loudly and fast instead of multiplying cost.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use color_eyre::eyre::{Result, eyre};
use tokio::sync::{Semaphore, SemaphorePermit};

use crate::limits::SpawnLimits;

use super::command::{CommandOutput, CommandRunner, CommandSpec, LaunchKind};

/// Why the breaker tripped, carried on the refusal error and surfaced when the
/// runner aborts the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TripReason {
    /// The per-run total-spawn cap was reached: `spawned` launches were admitted,
    /// `limit` is the cap.
    TotalCap {
        /// Launches admitted before the cap refused the next one.
        spawned: u32,
        /// The configured [`SpawnLimits::max_total_spawns`](crate::limits::SpawnLimits::max_total_spawns).
        limit: u32,
    },
    /// Too many launches in a row produced no output: the dead-spawn storm
    /// signature of a broken or unauthenticated agent CLI.
    ConsecutiveFailures {
        /// Consecutive launches that produced no output.
        failures: u32,
        /// The configured [`SpawnLimits::max_consecutive_failures`](crate::limits::SpawnLimits::max_consecutive_failures).
        limit: u32,
        /// Total launches admitted this run when the breaker tripped.
        spawned: u32,
    },
}

impl fmt::Display for TripReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TripReason::TotalCap { spawned, limit } => write!(
                f,
                "the per-run agent-launch cap was reached ({spawned} launched, limit {limit})"
            ),
            TripReason::ConsecutiveFailures {
                failures,
                limit,
                spawned,
            } => write!(
                f,
                "{failures} agent launches in a row produced no output (limit {limit}; \
                 {spawned} launched this run), the signature of a broken or unauthenticated \
                 agent CLI respawning in a loop"
            ),
        }
    }
}

/// The mutable state the governor guards behind a single lock, so the total
/// count, the consecutive-failure count, and the tripped verdict can never drift
/// out of step with each other under concurrent launches.
#[derive(Debug, Default)]
struct State {
    /// Launches admitted so far (each `admit` that returns a permit).
    launched: u32,
    /// Launches that produced no output since the last productive one.
    consecutive_dead: u32,
    /// The reason the breaker tripped, latched on first trip.
    tripped: Option<TripReason>,
}

/// A per-run circuit breaker and concurrency limiter over agent launches.
///
/// One is built per `bastion review` from the run's [`SpawnLimits`] and shared
/// (behind an [`Arc`]) across every reviewer's [`GovernedRunner`].
#[derive(Debug)]
pub struct SpawnGovernor {
    limits: SpawnLimits,
    /// Bounds how many launches hold a slot at once. Sized from
    /// [`SpawnLimits::max_concurrent`](crate::limits::SpawnLimits::max_concurrent),
    /// clamped to at least one so a run can always make progress.
    slots: Semaphore,
    state: Mutex<State>,
}

impl SpawnGovernor {
    /// Build a governor enforcing `limits`.
    #[must_use]
    pub fn new(limits: SpawnLimits) -> Self {
        // A width of zero could never admit a launch, deadlocking the whole run, so
        // clamp to at least one. `usize` for the semaphore; the cap is small.
        let width = usize::try_from(limits.max_concurrent)
            .unwrap_or(usize::MAX)
            .max(1);
        Self {
            limits,
            slots: Semaphore::new(width),
            state: Mutex::new(State::default()),
        }
    }

    /// A shared governor over the default caps, for a call site (or test) that has
    /// no registry-configured limits to hand.
    #[must_use]
    pub fn shared_default() -> Arc<Self> {
        Arc::new(Self::new(SpawnLimits::default()))
    }

    /// Lock the state, recovering the guard rather than panicking if a prior
    /// holder panicked: the counts stay usable, and a poisoned lock must not turn a
    /// safety mechanism into its own crash.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The total launches admitted so far this run.
    #[must_use]
    pub fn launched(&self) -> u32 {
        self.state().launched
    }

    /// The reason the breaker has tripped, or `None` while the run is healthy.
    #[must_use]
    pub fn tripped(&self) -> Option<TripReason> {
        self.state().tripped.clone()
    }

    /// Admit one launch, or refuse it.
    ///
    /// A refusal (a tripped breaker, or the total cap reached) returns an error
    /// carrying the reason, which the backend turns into a fail-closed launch and
    /// the runner into an aborted run. On admission it returns a permit that bounds
    /// concurrency for the launch's duration; dropping the permit frees the slot.
    async fn admit(&self) -> Result<SemaphorePermit<'_>> {
        // Fast path: a tripped breaker refuses without waiting on a slot, so a storm
        // that has already tripped stops queueing further launches immediately.
        if let Some(reason) = self.tripped() {
            return Err(refusal(&reason));
        }
        let permit = match self.slots.acquire().await {
            Ok(permit) => permit,
            // The semaphore is never closed while the governor lives, so this cannot
            // happen; treat it as a refusal rather than reaching for a panic.
            Err(_) => return Err(eyre!("the spawn governor's slot semaphore was closed")),
        };
        // Re-check under the lock: the breaker may have tripped, or the cap filled,
        // while this launch waited for a slot.
        let mut state = self.state();
        if let Some(reason) = &state.tripped {
            return Err(refusal(reason));
        }
        if state.launched >= self.limits.max_total_spawns {
            let reason = TripReason::TotalCap {
                spawned: state.launched,
                limit: self.limits.max_total_spawns,
            };
            state.tripped = Some(reason.clone());
            return Err(refusal(&reason));
        }
        state.launched += 1;
        Ok(permit)
    }

    /// Record the outcome of an admitted launch, advancing the consecutive-failure
    /// breaker. A dead required launch increments it and may trip; a productive
    /// launch resets it. A dead optional resume leaves the streak unchanged because
    /// missing prior session state is expected, especially in CI.
    fn record(&self, dead: bool, kind: LaunchKind) {
        let mut state = self.state();
        if !dead {
            state.consecutive_dead = 0;
            return;
        }
        if kind == LaunchKind::ConversationResume {
            return;
        }
        state.consecutive_dead += 1;
        if state.consecutive_dead >= self.limits.max_consecutive_failures && state.tripped.is_none()
        {
            state.tripped = Some(TripReason::ConsecutiveFailures {
                failures: state.consecutive_dead,
                limit: self.limits.max_consecutive_failures,
                spawned: state.launched,
            });
        }
    }
}

/// Build the error returned when a launch is refused, naming the cap that fired so
/// the fail-closed reviewer row and the run-level abort both read clearly.
fn refusal(reason: &TripReason) -> color_eyre::eyre::Error {
    eyre!("bastion refused to launch another agent: {reason}")
}

/// Whether a finished launch produced nothing: the dead-spawn signature the
/// consecutive-failure breaker counts.
///
/// A launch is dead when it could not spawn at all (an `Err` from the inner
/// runner), or when it exited non-zero without writing any stdout (an exit-127, an
/// auth failure, an agent that died at zero tokens). A launch that wrote output is
/// productive even if it later exited non-zero, since it did real work; that keeps
/// an ordinary reviewer failure (a crash mid-review, a malformed verdict) from
/// counting toward the breaker.
fn is_dead_spawn(result: &Result<CommandOutput>) -> bool {
    match result {
        Err(_) => true,
        Ok(output) => !output.success() && output.stdout.trim().is_empty(),
    }
}

/// A [`CommandRunner`] decorator that governs every launch through a shared
/// [`SpawnGovernor`].
///
/// It admits the launch (bounding concurrency and enforcing the caps), runs the
/// inner runner, then records whether the launch was productive or dead. Because it
/// wraps the runner seam, it counts a backend's reprompt turn and a dead launch
/// exactly like a first, productive one.
#[derive(Debug, Clone)]
pub struct GovernedRunner<R> {
    inner: R,
    governor: Arc<SpawnGovernor>,
}

impl<R> GovernedRunner<R> {
    /// Wrap `inner`, governing its launches through the shared `governor`.
    #[must_use]
    pub fn new(inner: R, governor: Arc<SpawnGovernor>) -> Self {
        Self { inner, governor }
    }
}

impl<R: CommandRunner> CommandRunner for GovernedRunner<R> {
    async fn run(&self, spec: &CommandSpec) -> Result<CommandOutput> {
        // A refused admission never runs the inner runner and is not recorded: it is
        // not a launch, so it must not advance either counter.
        let _permit = self.governor.admit().await?;
        let result = self.inner.run(spec).await;
        self.governor.record(is_dead_spawn(&result), spec.kind);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::backend::command::CommandSpec;

    /// A [`CommandRunner`] returning canned outcomes in sequence, tracking how many
    /// launches it actually ran and the peak number in flight at once.
    #[derive(Debug, Default)]
    struct FakeRunner {
        script: Mutex<std::collections::VecDeque<CommandOutput>>,
        ran: AtomicUsize,
        in_flight: AtomicUsize,
        peak_in_flight: AtomicUsize,
        /// Optional per-launch delay so the concurrency test can hold slots open.
        hold: std::time::Duration,
    }

    impl FakeRunner {
        fn with(outputs: impl IntoIterator<Item = CommandOutput>) -> Self {
            Self {
                script: Mutex::new(outputs.into_iter().collect()),
                hold: std::time::Duration::ZERO,
                ..Default::default()
            }
        }

        /// A runner that always returns a dead launch (exit 127, no stdout), for the
        /// respawn-storm scenarios.
        fn always_dead() -> Self {
            Self {
                script: Mutex::new(std::collections::VecDeque::new()),
                hold: std::time::Duration::ZERO,
                ..Default::default()
            }
        }

        fn ran(&self) -> usize {
            self.ran.load(Ordering::SeqCst)
        }
    }

    impl CommandRunner for FakeRunner {
        async fn run(&self, _spec: &CommandSpec) -> Result<CommandOutput> {
            self.ran.fetch_add(1, Ordering::SeqCst);
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak_in_flight.fetch_max(now, Ordering::SeqCst);
            if !self.hold.is_zero() {
                tokio::time::sleep(self.hold).await;
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            let canned = self.script.lock().unwrap().pop_front();
            Ok(canned.unwrap_or(CommandOutput {
                // No script entry means "a dead launch": exit 127, no stdout.
                code: Some(127),
                stdout: String::new(),
                stderr: "command not found".into(),
            }))
        }
    }

    fn productive() -> CommandOutput {
        CommandOutput {
            code: Some(0),
            stdout: "an agent event stream".into(),
            stderr: String::new(),
        }
    }

    fn spec() -> CommandSpec {
        CommandSpec::new("agent", ".")
    }

    fn resume_spec() -> CommandSpec {
        let mut spec = spec();
        spec.conversation_resume();
        spec
    }

    fn limits(concurrent: u32, total: u32, consecutive: u32) -> SpawnLimits {
        SpawnLimits {
            max_concurrent: concurrent,
            max_total_spawns: total,
            max_consecutive_failures: consecutive,
        }
    }

    #[test]
    fn a_failed_or_dead_launch_is_classified_dead_but_a_productive_one_is_not() {
        // The three dead signatures: could not spawn, exit 127 with no output, any
        // non-zero exit with no output.
        assert!(is_dead_spawn(&Err(eyre!("could not spawn"))));
        assert!(is_dead_spawn(&Ok(CommandOutput {
            code: Some(127),
            stdout: String::new(),
            stderr: "not found".into(),
        })));
        assert!(is_dead_spawn(&Ok(CommandOutput {
            code: None,
            stdout: "   \n".into(),
            stderr: String::new(),
        })));
        // Productive: exit zero, or any exit that still wrote output (it did work).
        assert!(!is_dead_spawn(&Ok(productive())));
        assert!(!is_dead_spawn(&Ok(CommandOutput {
            code: Some(1),
            stdout: "reviewed, then crashed".into(),
            stderr: String::new(),
        })));
    }

    #[tokio::test]
    async fn the_concurrency_cap_bounds_simultaneous_launches() {
        // Ten launches fired at once against a cap of three must never have more
        // than three in flight together, though all ten still run.
        let governor = Arc::new(SpawnGovernor::new(limits(3, 100, 100)));
        let runner = Arc::new(GovernedRunner::new(
            {
                let mut fake = FakeRunner::with(std::iter::repeat_with(productive).take(10));
                fake.hold = std::time::Duration::from_millis(25);
                fake
            },
            governor.clone(),
        ));

        let mut handles = Vec::new();
        for _ in 0..10 {
            let runner = runner.clone();
            handles.push(tokio::spawn(async move { runner.run(&spec()).await }));
        }
        for handle in handles {
            handle.await.unwrap().expect("productive launches succeed");
        }

        assert_eq!(runner.inner.ran(), 10, "every launch eventually runs");
        assert!(
            runner.inner.peak_in_flight.load(Ordering::SeqCst) <= 3,
            "never more than the cap in flight, saw peak {}",
            runner.inner.peak_in_flight.load(Ordering::SeqCst)
        );
        assert_eq!(governor.launched(), 10);
        assert!(governor.tripped().is_none());
    }

    #[tokio::test]
    async fn the_total_cap_aborts_after_the_configured_number_of_launches() {
        // A total cap of four admits exactly four launches; the fifth is refused and
        // trips the breaker, so the inner runner never runs a fifth time.
        let governor = Arc::new(SpawnGovernor::new(limits(1, 4, 100)));
        let runner = GovernedRunner::new(
            FakeRunner::with(std::iter::repeat_with(productive).take(10)),
            governor.clone(),
        );

        for _ in 0..4 {
            runner.run(&spec()).await.expect("within the cap");
        }
        let err = runner
            .run(&spec())
            .await
            .expect_err("the fifth launch is refused");
        assert!(
            err.to_string().contains("per-run agent-launch cap"),
            "{err}"
        );
        assert_eq!(runner.inner.ran(), 4, "the refused launch never ran");
        assert_eq!(governor.launched(), 4);
        assert!(matches!(
            governor.tripped(),
            Some(TripReason::TotalCap {
                spawned: 4,
                limit: 4
            })
        ));
    }

    #[tokio::test]
    async fn the_breaker_trips_after_consecutive_dead_launches_and_refuses_the_rest() {
        // Three dead launches (the exit-127 / auth-failure signature) in a row trip
        // the breaker; the next launch is refused with a clear error rather than
        // looping forever.
        let governor = Arc::new(SpawnGovernor::new(limits(1, 100, 3)));
        let runner = GovernedRunner::new(FakeRunner::always_dead(), governor.clone());

        // Each of the first three dead launches runs (a real launch that died); the
        // third trips the breaker.
        for _ in 0..3 {
            let out = runner
                .run(&spec())
                .await
                .expect("a dead launch still returns");
            assert!(!out.success());
        }
        assert!(matches!(
            governor.tripped(),
            Some(TripReason::ConsecutiveFailures {
                failures: 3,
                limit: 3,
                ..
            })
        ));

        let err = runner
            .run(&spec())
            .await
            .expect_err("a tripped breaker refuses further launches");
        assert!(err.to_string().contains("produced no output"), "{err}");
        assert_eq!(
            runner.inner.ran(),
            3,
            "the refused launch after the trip never ran"
        );
    }

    #[tokio::test]
    async fn a_productive_launch_resets_the_consecutive_failure_count() {
        // An occasional transient failure must not trip the breaker: a productive
        // launch between failures resets the run, so it takes a fresh streak to trip.
        let governor = Arc::new(SpawnGovernor::new(limits(1, 100, 3)));
        // dead, dead, productive, dead, dead: never three dead in a row.
        let runner = GovernedRunner::new(
            FakeRunner::with([
                CommandOutput {
                    code: Some(127),
                    stdout: String::new(),
                    stderr: String::new(),
                },
                CommandOutput {
                    code: Some(127),
                    stdout: String::new(),
                    stderr: String::new(),
                },
                productive(),
                CommandOutput {
                    code: Some(127),
                    stdout: String::new(),
                    stderr: String::new(),
                },
                CommandOutput {
                    code: Some(127),
                    stdout: String::new(),
                    stderr: String::new(),
                },
            ]),
            governor.clone(),
        );

        for _ in 0..5 {
            runner.run(&spec()).await.expect("launch returns");
        }
        assert!(
            governor.tripped().is_none(),
            "interleaved productive launches must keep the breaker from tripping"
        );
        assert_eq!(runner.inner.ran(), 5);
    }

    #[tokio::test]
    async fn missing_conversations_do_not_trip_the_dead_spawn_breaker() {
        let governor = Arc::new(SpawnGovernor::new(limits(1, 100, 2)));
        let runner = GovernedRunner::new(FakeRunner::always_dead(), governor.clone());

        for _ in 0..4 {
            let output = runner
                .run(&resume_spec())
                .await
                .expect("an attempted resume still launches");
            assert!(!output.success());
        }

        assert_eq!(governor.launched(), 4, "resume attempts still count");
        assert!(
            governor.tripped().is_none(),
            "an unavailable optional conversation must leave room for fresh fallback"
        );

        for _ in 0..2 {
            runner.run(&spec()).await.expect("required launch runs");
        }
        assert!(matches!(
            governor.tripped(),
            Some(TripReason::ConsecutiveFailures { failures: 2, .. })
        ));
    }
}
