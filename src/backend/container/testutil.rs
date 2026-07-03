use std::sync::Mutex;

use color_eyre::eyre::Result;

use crate::backend::command::{CommandOutput, CommandRunner, CommandSpec};
use crate::reviewer::{Backend, Capabilities, Mode, Reviewer};

/// A [`CommandRunner`] that records the specs it is handed and returns canned
/// outputs in order. A real fake, not a mocking framework.
#[derive(Default)]
pub(crate) struct RecordingRunner {
    pub(crate) outputs: Mutex<std::collections::VecDeque<CommandOutput>>,
    pub(crate) seen: Mutex<Vec<CommandSpec>>,
}

impl RecordingRunner {
    pub(crate) fn with(outputs: Vec<CommandOutput>) -> Self {
        Self {
            outputs: Mutex::new(outputs.into()),
            seen: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn last(&self) -> CommandSpec {
        self.seen.lock().unwrap().last().unwrap().clone()
    }
}

impl CommandRunner for RecordingRunner {
    async fn run(&self, spec: &CommandSpec) -> Result<CommandOutput> {
        self.seen.lock().unwrap().push(spec.clone());
        Ok(self
            .outputs
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(CommandOutput {
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            }))
    }
}

pub(crate) fn args_of(spec: &CommandSpec) -> Vec<String> {
    spec.args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

pub(crate) fn reviewer() -> Reviewer {
    Reviewer {
        name: "demo".into(),
        trigger: vec!["**".into()],
        mode: Mode::Gate,
        backend: Backend::ClaudeCode,
        model: None,
        effort: None,
        timeout: None,
        runner: None,
        env: Default::default(),
        capabilities: Capabilities::default(),
        inputs: Default::default(),
        attestation: None,
        prompt: "p".into(),
    }
}
