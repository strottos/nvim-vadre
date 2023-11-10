mod breakpoints;
mod debugger;
mod debuggers;
mod handler;
mod processor;
mod protocol;
mod schema;
// mod debugpy;
// mod dotnet;
// mod generic;
// mod go_delve;
// mod node;

use std::{fmt::Debug, sync::Arc};

use self::debuggers::DebuggerType;
pub(crate) use self::{breakpoints::Breakpoints, debugger::Debugger};
use crate::neovim::NeovimVadreWindow;

use anyhow::{bail, Result};
use nvim_rs::{compat::tokio::Compat, Neovim};
use tokio::{io::Stdout, sync::Mutex};

#[derive(Clone, Debug)]
pub enum DebuggerStepType {
    Over,
    In,
    Continue,
}

pub(crate) fn new_debugger(
    id: usize,
    debug_program_string: String,
    neovim: Neovim<Compat<Stdout>>,
    debugger_type: String,
) -> Result<Box<Debugger>> {
    let neovim_vadre_window = Arc::new(Mutex::new(NeovimVadreWindow::new(neovim, id)));
    let debugger_type = match debugger_type.as_ref() {
        "lldb" | "codelldb" => DebuggerType::CodeLLDB(debuggers::codelldb::Debugger::new(
            neovim_vadre_window.clone(),
        )),

        "python" | "debugpy" => DebuggerType::DebugPy(debuggers::debugpy::Debugger::new(
            neovim_vadre_window.clone(),
        )),

        // "go" | "delve" => Ok(Box::new(go_delve::Debugger::new(
        //     id,
        //     command,
        //     command_args,
        //     neovim,
        // ))),

        // "dotnet" | ".net" | "net" => Ok(Box::new(dotnet::Debugger::new(
        //     id,
        //     command,
        //     command_args,
        //     neovim,
        // ))),

        // "node" | "nodejs" | "js" => Ok(Box::new(node::Debugger::new(
        //     id,
        //     command,
        //     command_args,
        //     neovim,
        // ))),
        _ => bail!("ERROR: Debugger unknown {}", debugger_type),
    };

    Ok(Box::new(Debugger::new(
        id,
        debug_program_string,
        debugger_type,
        neovim_vadre_window,
    )))
}
