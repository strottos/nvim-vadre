mod breakpoints;
mod dap;
mod debugger;
mod node;

use std::{fmt::Debug, sync::Arc, time::Duration};

use crate::neovim::NeovimVadreWindow;
pub(crate) use breakpoints::Breakpoints;
pub(crate) use debugger::Debugger;

use anyhow::{bail, Result};
use nvim_rs::{compat::tokio::Compat, Neovim};
use tokio::{
    io::Stdout,
    sync::{Mutex, RwLock},
    time::Instant,
};

#[derive(Clone, Debug)]
pub enum DebuggerStepType {
    Over,
    In,
    Continue,
}

struct RequestTimeout {
    inner: RwLock<RequestTimeoutInner>,
    neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
}

struct RequestTimeoutInner {
    value: Duration,
    last_set: Instant,
}

impl RequestTimeout {
    fn new(neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>) -> Self {
        RequestTimeout {
            inner: RwLock::new(RequestTimeoutInner {
                value: Duration::new(30, 0),
                last_set: Instant::now(),
            }),
            neovim_vadre_window,
        }
    }

    async fn get_or_set(&self) -> Duration {
        let now = Instant::now();
        if now - self.inner.read().await.last_set > Duration::from_secs(60) {
            let value = match self
                .neovim_vadre_window
                .lock()
                .await
                .get_var("vadre_request_timeout")
                .await
            {
                Ok(duration) => Duration::new(duration.as_u64().unwrap_or(30), 0),
                Err(_) => Duration::new(30, 0),
            };
            let mut writer = self.inner.write().await;
            writer.value = value;
            writer.last_set = now;
        }
        self.inner.read().await.value.clone()
    }
}

pub(crate) fn new_debugger(
    id: usize,
    debug_program_string: String,
    neovim: Neovim<Compat<Stdout>>,
    debugger_type: String,
    breakpoints: Breakpoints,
) -> Result<Box<Debugger>> {
    let neovim_vadre_window = Arc::new(Mutex::new(NeovimVadreWindow::new(neovim, id)));
    let debugger = match debugger_type.as_ref() {
        "lldb" | "codelldb" | "python" | "debugpy" | "go" | "delve" | "dotnet" | ".net" | "net"
        | "generic" => {
            let dap_debugger = dap::new_dap_debugger(
                id,
                debug_program_string,
                neovim_vadre_window.clone(),
                debugger_type,
                breakpoints,
            )?;

            Debugger::Dap(dap_debugger)
        }
        "node" | "nodejs" | "js" => Debugger::Node(node::NodeDebugger::new(
            id,
            debug_program_string,
            neovim_vadre_window.clone(),
            breakpoints,
        )),
        _ => bail!("ERROR: Debugger unknown {}", debugger_type),
    };

    Ok(Box::new(debugger))
}
