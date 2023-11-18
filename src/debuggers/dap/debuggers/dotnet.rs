use std::{
    collections::HashMap,
    env::{self, consts::EXE_SUFFIX},
    fmt::Debug,
    path::PathBuf,
    sync::Arc,
};

use anyhow::{anyhow, bail, Result};
use reqwest::Url;
use tokio::{process::Child, sync::Mutex};

use crate::{
    debuggers::dap::protocol::RequestArguments,
    neovim::{NeovimVadreWindow, VadreLogLevel},
    util::{download_extract_zip, get_debuggers_dir, get_os_and_cpu_architecture},
};

const VERSION: &str = "3.0.0-1012";

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
        let mut path = get_debuggers_dir()?;
        path.push("netcoredbg");

        if !path.exists() {
            self.neovim_vadre_window
                .lock()
                .await
                .log_msg(VadreLogLevel::INFO, &format!("So downloading it"))
                .await?;
            let (os, arch) = get_os_and_cpu_architecture();

            self.neovim_vadre_window
                .lock()
                .await
                .log_msg(
                    VadreLogLevel::INFO,
                    &format!("Downloading and extracting {} plugin for {}", os, arch),
                )
                .await?;

            // TODO: Does this work on other OS's
            let url = match os {
                "windows" => Url::parse(&format!(
                    "https://github.com/Samsung/netcoredbg/releases/download/{}/netcoredbg-win64.zip",
                    VERSION,
                ))?,
                _ => Url::parse(&format!(
                    "https://github.com/Samsung/netcoredbg/releases/download/{}/netcoredbg-{}-{}.zip",
                    VERSION, os, arch,
                ))?,
            };

            download_extract_zip(path.as_path(), url).await?;
        }

        Ok(())
    }

    pub(crate) async fn spawn_child(
        &self,
        port: Option<u16>,
        _program: Vec<String>,
    ) -> Result<Child> {
        let cmd_args = match port {
            Some(_) => vec!["--interpreter=vscode".to_string()],
            None => bail!("Need to specify a port for netcoredbg"),
        };
        todo!();
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
            "name": "netcoredbg",
            "terminal": "integrated",
            "type": "netcoredbg",
            "request": "launch",
            "program": program,
        })))
    }

    pub(crate) async fn get_attach_request(&self, pid: i64) -> Result<RequestArguments> {
        Ok(RequestArguments::attach(serde_json::json!({
            "pid": pid,
        })))
    }

    pub(crate) async fn get_debugger_path(&self) -> Result<PathBuf> {
        let mut path = get_debuggers_dir()?;
        path.push("netcoredbg");
        path.push(&self.get_binary_name().await?);
        let path = dunce::canonicalize(path)?;

        Ok(path)
    }

    pub(crate) fn check_breakpoint_enabled(&self, msg: &str) -> Result<bool> {
        Ok(msg
            .rsplit_once("Resolved locations: ")
            .ok_or_else(|| anyhow!("Couldn't find Resolved locations message in: {}", msg))?
            .1
            .parse::<i64>()?
            > 0)
    }

    async fn get_binary_name(&self) -> Result<String> {
        Ok(format!("netcoredbg{}", EXE_SUFFIX))
    }
}

impl Debug for Debugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetCoreDbgDebugger").finish()
    }
}
