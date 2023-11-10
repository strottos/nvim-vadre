use std::{collections::HashMap, env, fmt::Debug, path::PathBuf, sync::Arc};

use anyhow::{bail, Result};
use tokio::{process::Child, sync::Mutex};

use crate::{
    dap::protocol::{Either, RequestArguments},
    neovim::NeovimVadreWindow,
};

const VERSION: &str = "1.10.0";

#[derive(Clone)]
pub(crate) struct Debugger {
    pub neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
}

impl Debugger {
    pub(crate) fn new(neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>) -> Self {
        Self {
            neovim_vadre_window,
        }
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

        Ok(RequestArguments::launch(Either::Second(
            serde_json::json!({
                "args": command_args,
                "cwd": env::current_dir()?,
                "env": environment_variables,
                "name": "dap",
                "terminal": "integrated",
                "type": "dap",
                "request": "launch",
                "program": program,
            }),
        )))
    }

    pub(crate) async fn get_attach_request(&self, pid: i64) -> Result<RequestArguments> {
        Ok(RequestArguments::attach(serde_json::json!({
            "pid": pid,
        })))
    }

    pub(crate) async fn get_debugger_path(&self) -> Result<PathBuf> {
        bail!("Need to specify the command to run generic debugger");
    }
}

impl Debug for Debugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenericDebugger").finish()
    }
}