use std::{
    collections::HashMap,
    env::{self, consts::EXE_SUFFIX},
    fmt::Debug,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
};

use crate::{
    debuggers::dap::protocol::RequestArguments,
    neovim::{NeovimVadreWindow, VadreLogLevel},
    util::{download_extract_zip, get_debuggers_dir, get_os_and_cpu_architecture, merge_json},
};

use anyhow::{anyhow, bail, Result};
use is_executable::IsExecutable;
use reqwest::Url;
use tokio::{
    process::{Child, Command},
    sync::Mutex,
};
use tracing::debug;
use which::which;

const VERSION: &str = "1.22.1";

#[derive(Clone)]
pub struct Debugger {
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
        path.push("delve");
        path.push(&format!("v{}", VERSION));

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
                "https://github.com/go-delve/delve/archive/refs/tags/v{}.zip",
                VERSION,
            ))?;

            download_extract_zip(path.as_path(), url).await?;

            let go_path = match self
                .neovim_vadre_window
                .lock()
                .await
                .get_var("vadre_go_path")
                .await
            {
                Ok(go_path) => PathBuf::from(
                    go_path
                        .as_str()
                        .ok_or_else(|| anyhow!("vadre_go_path is not a string"))?,
                ),
                Err(_) => which("go").unwrap(),
            };

            let go_path = dunce::canonicalize(&go_path)?;

            let working_dir = self.get_debugger_path()?;

            debug!(
                "Running delve installation: {:?} {:?}",
                go_path, working_dir
            );

            let child = Command::new(go_path.to_str().unwrap())
                .args(vec!["build", "."])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .current_dir(working_dir)
                .output()
                .await
                .expect("Failed to spawn go setup");

            let stdout = String::from_utf8_lossy(&child.stdout);
            let stderr = String::from_utf8_lossy(&child.stdout);

            debug!("go build output: {:?}", stdout);
            debug!("go build stderr: {:?}", stderr);

            self.neovim_vadre_window
                .lock()
                .await
                .log_msg(VadreLogLevel::INFO, &format!("go stdout: {}", stdout))
                .await?;
            self.neovim_vadre_window
                .lock()
                .await
                .log_msg(VadreLogLevel::INFO, &format!("go stderr: {}", stderr))
                .await?;
        }

        Ok(())
    }

    pub(crate) async fn spawn_child(
        &self,
        port: Option<u16>,
        _program: Vec<String>,
    ) -> Result<Child> {
        let debugger_path = self.get_debugger_binary_path()?;

        if !debugger_path.exists() {
            bail!("The Go Delve debugger doesn't exist: {:?}", debugger_path);
        }

        let cmd_args = match port {
            Some(port) => {
                let mut ret = vec![
                    "dap".to_string(),
                    "--listen".to_string(),
                    format!("127.0.0.1:{}", port),
                ];
                if let Ok(_) = env::var("VADRE_LOG_FILE") {
                    ret.push("--log".to_string());
                }
                ret
            }
            None => bail!("Need to specify a port for Go Delve debugger"),
        };

        Command::new(debugger_path)
            .args(cmd_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("Failed to spawn Go Delve: {:?}", e))
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

        let go_dir = env::var("GOPATH")
            .map(PathBuf::from)
            .unwrap_or(dirs::home_dir().unwrap().join("Go"));

        let program_path = PathBuf::from(&program);
        let mode = if program_path.exists() && program_path.is_executable() {
            "exec"
        } else {
            "auto"
        };

        let mut env = serde_json::json!({
            "GOPATH": go_dir,
        });
        merge_json(&mut env, serde_json::json!(environment_variables));
        tracing::debug!("Go environment: {:?}", env);

        Ok(RequestArguments::launch(serde_json::json!({
            "args": command_args,
            "cwd": env::current_dir()?,
            "debugAdapter": "dlv-dap",
            "dlvFlags": [],
            "dlvToolPath": self.get_debugger_binary_path()?,
            "env": env,
            "mode": mode,
            "name": "delve",
            "program": program,
            "request": "launch",
            "type": "go",
        })))
    }

    pub(crate) async fn get_attach_request(&self, pid: i64) -> Result<RequestArguments> {
        Ok(RequestArguments::attach(serde_json::json!({
            "pid": pid,
        })))
    }

    fn get_debugger_path(&self) -> Result<PathBuf> {
        let mut path = get_debuggers_dir()?;
        path.push("delve");
        path.push(&format!("v{}", VERSION));
        path.push(&format!("delve-{}", VERSION));
        path.push("cmd");
        path.push("dlv");
        let path = dunce::canonicalize(path)?;
        Ok(path)
    }

    pub(crate) fn check_breakpoint_enabled(&self, _msg: &str) -> Result<bool> {
        // Successful breakpoints in delve are indicated by verified = true and no message, if
        // we're here then it's false
        Ok(false)
    }

    fn get_debugger_binary_path(&self) -> Result<PathBuf> {
        let binary_name = format!("dlv{}", EXE_SUFFIX);
        let mut path = self.get_debugger_path()?;
        path.push(&binary_name);
        Ok(path)
    }
}

impl Debug for Debugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoDelveDebugger").finish()
    }
}
