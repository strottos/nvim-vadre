use std::{
    collections::HashMap,
    env::{self, consts::EXE_SUFFIX},
    fmt::Debug,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
};

use anyhow::{anyhow, bail, Result};
use reqwest::Url;
use tokio::{
    process::{Child, Command},
    sync::Mutex,
};

use crate::{
    dap::protocol::{Either, RequestArguments},
    neovim::{NeovimVadreWindow, VadreLogLevel},
    util::{download_extract_zip, get_debuggers_dir, get_os_and_cpu_architecture},
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
        let mut path = get_debuggers_dir()?;
        path.push("codelldb");
        path.push(VERSION);

        if !path.exists() {
            let (os, arch) = get_os_and_cpu_architecture();

            self.neovim_vadre_window
                .lock()
                .await
                .log_msg(
                    VadreLogLevel::INFO,
                    &format!("Downloading and extracting {} plugin for {}", os, arch),
                )
                .await?;

            let url = Url::parse(&format!(
                "https://github.com/vadimcn/vscode-lldb/releases/download/v{}\
                                 /codelldb-{}-{}.vsix",
                VERSION, arch, os
            ))?;

            download_extract_zip(path.as_path(), url).await?;
        }

        Ok(())
    }

    pub(crate) async fn spawn_child(
        &self,
        port: Option<u16>,
        _program: Vec<String>,
    ) -> Result<Child> {
        let debugger_path = self.get_debugger_path()?;

        if !debugger_path.exists() {
            bail!("The CodeLLDB debugger doesn't exist: {:?}", debugger_path);
        }

        let cmd_args = match port {
            Some(port) => vec!["--port".to_string(), port.to_string()],
            None => bail!("Need to specify a port for CodeLLDB"),
        };

        Command::new(debugger_path)
            .args(cmd_args)
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

        Ok(RequestArguments::launch(Either::Second(
            serde_json::json!({
                "args": command_args,
                "cargo": {},
                "cwd": env::current_dir()?,
                "env": environment_variables,
                "name": "lldb",
                "terminal": "integrated",
                "type": "lldb",
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

    fn get_debugger_path(&self) -> Result<PathBuf> {
        let mut path = get_debuggers_dir()?;
        path.push("codelldb");
        path.push(VERSION);
        path.push("extension");
        path.push("adapter");
        path.push(&self.get_binary_name()?);
        let path = dunce::canonicalize(path)?;

        Ok(path)
    }

    fn get_binary_name(&self) -> Result<String> {
        Ok(format!("codelldb{}", EXE_SUFFIX))
    }
}

impl Debug for Debugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeLLDBDebugger").finish()
    }
}
