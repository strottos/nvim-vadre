mod debugger;
mod debuggers;
mod handler;
mod processor;
mod protocol;
mod schema;

use std::sync::Arc;

use anyhow::{bail, Result};
use tokio::sync::Mutex;

pub(crate) use self::debugger::DapDebugger;
use self::debuggers::DapDebuggerType;

use super::Breakpoints;
use crate::neovim::NeovimVadreWindow;

pub(crate) fn new_dap_debugger(
    id: usize,
    debug_program_string: String,
    neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    debugger_type: String,
    breakpoints: Breakpoints,
) -> Result<DapDebugger> {
    let debugger_type = match debugger_type.as_ref() {
        "lldb" | "codelldb" => DapDebuggerType::CodeLLDB(debuggers::codelldb::Debugger::new(
            neovim_vadre_window.clone(),
        )),

        "generic" => DapDebuggerType::Generic(debuggers::generic::Debugger::new(
            neovim_vadre_window.clone(),
        )),

        "python" | "debugpy" => DapDebuggerType::DebugPy(debuggers::debugpy::Debugger::new(
            neovim_vadre_window.clone(),
        )),

        "go" | "delve" => DapDebuggerType::GoDelve(debuggers::go_delve::Debugger::new(
            neovim_vadre_window.clone(),
        )),

        "dotnet" | ".net" | "net" => DapDebuggerType::DotNet(debuggers::dotnet::Debugger::new(
            neovim_vadre_window.clone(),
        )),

        // "node" | "nodejs" | "js" => Ok(Box::new(node::Debugger::new(
        //     id,
        //     command,
        //     command_args,
        //     neovim,
        // ))),
        _ => bail!("ERROR: Debugger unknown {}", debugger_type),
    };

    Ok(DapDebugger::new(
        id,
        debug_program_string,
        debugger_type,
        neovim_vadre_window,
        breakpoints,
    ))
}
