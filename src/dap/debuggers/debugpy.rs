use std::{collections::HashMap, env, fmt::Debug, path::PathBuf, process::Stdio, sync::Arc};

use anyhow::{anyhow, bail, Result};
use reqwest::Url;
use tokio::{
    process::{Child, Command},
    sync::Mutex,
};
use which::which;

use crate::{
    dap::protocol::{Either, RequestArguments},
    neovim::{NeovimVadreWindow, VadreLogLevel},
    util::{download_extract_zip, get_debuggers_dir, get_os_and_cpu_architecture},
};

const VERSION: &str = "1.8.0";

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
        path.push("debugpy");

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
                "https://github.com/microsoft/debugpy/archive/v{}.zip",
                VERSION,
            ))?;

            download_extract_zip(path.as_path(), url).await?;

            let python_path = self.get_python_path().await?;
            path.push(&format!("debugpy-{}", VERSION));
            let working_dir = dunce::canonicalize(path)?;

            tracing::debug!(
                "Running debugpy installation: {:?} {:?}",
                python_path,
                working_dir
            );

            let child = Command::new(python_path.to_str().unwrap())
                .args(vec!["setup.py", "build", "--build-platlib", "build/lib"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .current_dir(working_dir)
                .output()
                .await
                .expect("Failed to spawn debugpy setup");

            let stdout = String::from_utf8_lossy(&child.stdout);
            let stderr = String::from_utf8_lossy(&child.stdout);

            tracing::debug!("debugpy output: {:?}", stdout);
            tracing::debug!("debugpy stderr: {:?}", stderr);

            self.neovim_vadre_window
                .lock()
                .await
                .log_msg(VadreLogLevel::INFO, &format!("debugpy stdout: {}", stdout))
                .await?;
            self.neovim_vadre_window
                .lock()
                .await
                .log_msg(VadreLogLevel::INFO, &format!("debugpy stderr: {}", stderr))
                .await?;
        }

        Ok(())
    }

    pub(crate) async fn spawn_child(
        &self,
        port: Option<u16>,
        _program: Vec<String>,
    ) -> Result<Child> {
        let mut debugger_path = get_debuggers_dir()?;
        debugger_path.push("debugpy");
        debugger_path.push(&format!("debugpy-{}", VERSION));
        debugger_path.push("build");
        debugger_path.push("lib");

        if !debugger_path.exists() {
            bail!("The debugpy adapter doesn't exist: {:?}", debugger_path);
        }

        let debugger_path = debugger_path
            .to_str()
            .ok_or_else(|| anyhow!("Can't change path to string: ${:?}", debugger_path))?
            .to_string();

        let cmd_args = match port {
            Some(port) => {
                let mut ret = vec![
                    "-m".to_string(),
                    "debugpy.adapter".to_string(),
                    "--port".to_string(),
                    port.to_string(),
                ];
                if let Ok(_) = env::var("VADRE_LOG_FILE") {
                    ret.push("--log-stderr".to_string());
                }
                ret
            }
            None => bail!("Need to specify a port for DebugPy"),
        };

        let cmd = self.get_python_path().await?;

        tracing::debug!("Spawning `{:?}` with args: {:?}", cmd, cmd_args);

        Command::new(cmd)
            .args(cmd_args)
            .env("PYTHONPATH", debugger_path)
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
                "cwd": env::current_dir()?,
                "env": environment_variables,
                "name": "debugpy",
                "console": "integratedTerminal",
                "type": "debugpy",
                "request": "launch",
                "program": program,
                "stopOnEntry": true,
            }),
        )))
    }

    pub(crate) async fn get_attach_request(&self, pid: i64) -> Result<RequestArguments> {
        Ok(RequestArguments::attach(serde_json::json!({
            "pid": pid,
        })))
    }

    async fn get_python_path(&self) -> Result<PathBuf> {
        let path = match self
            .neovim_vadre_window
            .lock()
            .await
            .get_var("vadre_python_path")
            .await
        {
            Ok(x) => PathBuf::from(
                x.as_str()
                    .ok_or_else(|| anyhow!("Couldn't convert vadre_python_path to string"))?,
            ),
            Err(_) => which("python3")?,
        };

        let path = dunce::canonicalize(&path)?;

        if !path.exists() {
            bail!("The binary doesn't exist: {}", path.to_str().unwrap());
        }

        Ok(path)
    }
}

impl Debug for Debugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugPyDebugger").finish()
    }
}
