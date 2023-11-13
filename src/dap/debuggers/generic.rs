use std::{collections::HashMap, env, fmt::Debug, sync::Arc};

use anyhow::{anyhow, bail, Result};
use tokio::{process::Child, sync::Mutex};

use crate::{dap::protocol::RequestArguments, neovim::NeovimVadreWindow};

#[derive(Clone)]
pub(crate) struct Debugger {}

impl Debugger {
    pub(crate) fn new(_neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>) -> Self {
        Self {}
    }

    pub(crate) async fn download_plugin(&self) -> Result<()> {
        bail!("Need to specify the command to run generic debugger, try running with `-d=<cmd>\\ <options>` (you need to backslash escape spaces in the command)");
    }

    pub(crate) async fn spawn_child(
        &self,
        _port: Option<u16>,
        _program: Vec<String>,
    ) -> Result<Child> {
        bail!("Need to specify the command to run generic debugger, try running with `-d=<cmd>\\ <options>` (you need to backslash escape spaces in the command)");
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

        Ok(RequestArguments::launch(serde_json::json!({
            "args": command_args,
            "cwd": env::current_dir()?,
            "env": environment_variables,
            "name": "dap",
            "terminal": "integrated",
            "type": "dap",
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
