use std::{collections::HashMap, env, fmt::Debug, path::PathBuf, process::Stdio, sync::Arc};

use anyhow::{anyhow, bail, Result};
use tokio::{
    process::{Child, Command},
    sync::Mutex,
};

use crate::{debuggers::dap::protocol::RequestArguments, neovim::NeovimVadreWindow};

#[derive(Clone)]
pub(crate) struct Debugger {}

impl Debugger {
    pub(crate) fn new(_neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>) -> Self {
        Self {}
    }

    pub(crate) async fn download_plugin(&self) -> Result<()> {
        bail!("Downloading plugin for generic type is not supported, please run with `-d=/path/to/debugger`")
    }

    pub(crate) async fn spawn_child(
        &self,
        port: Option<u16>,
        mut program: Vec<String>,
    ) -> Result<Child> {
        if port.is_some() {
            bail!("Doesn't currently support attaching to a port");
        }

        let debugger_path = PathBuf::from(program.get(0).ok_or_else(|| {
            anyhow!(
                "Expected program to have at least one element, but it was empty: {:?}",
                program
            )
        })?);

        program.remove(0);

        if !debugger_path.exists() {
            bail!("The debugger doesn't exist: {:?}", debugger_path);
        }

        Command::new(debugger_path)
            .args(program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("Failed to spawn debugpy: {:?}", e))
    }

    pub(crate) async fn get_launch_request(
        &self,
        mut command_args: Vec<String>,
        environment_variables: HashMap<String, String>,
    ) -> Result<RequestArguments> {
        if command_args.is_empty() {
            bail!("No command to run");
        }

        let command = command_args.remove(0);

        let program = dunce::canonicalize(command)?;

        let program_name = program
            .file_name()
            .ok_or_else(|| anyhow!("Couldn't get file name from: {:?}", program))?
            .to_string_lossy()
            .to_string();

        Ok(RequestArguments::launch(serde_json::json!({
            "args": command_args,
            "cwd": env::current_dir()?,
            "env": environment_variables,
            "name": &program_name,
            "type": &program_name,
            "request": "launch",
            "program": program,
        })))
    }

    pub(crate) async fn get_attach_request(&self, pid: i64) -> Result<RequestArguments> {
        Ok(RequestArguments::attach(serde_json::json!({
            "pid": pid,
        })))
    }

    pub(crate) fn check_breakpoint_enabled(&self, msg: &str) -> Result<bool> {
        Ok(msg
            .rsplit_once("Resolved locations: ")
            .ok_or_else(|| anyhow!("Couldn't find Resolved locations message in: {}", msg))?
            .1
            .parse::<i64>()?
            > 0)
    }
}

impl Debug for Debugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenericDebugger").finish()
    }
}
