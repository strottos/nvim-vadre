mod codelldb;
mod dap;
mod debugpy;
mod dotnet;
mod go_delve;
mod node;

use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
};

use crate::neovim::VadreLogLevel;

use anyhow::{bail, Result};
use async_trait::async_trait;
use dyn_clone::DynClone;
use nvim_rs::{compat::tokio::Compat, Neovim};
use tokio::io::Stdout;

pub fn new_debugger(
    id: usize,
    command: String,
    command_args: Vec<String>,
    environment_variables: HashMap<String, String>,
    neovim: Neovim<Compat<Stdout>>,
    debugger_type: String,
    log_debugger: bool,
) -> Result<Box<dyn DebuggerAPI + Send + Sync + 'static>> {
    match debugger_type.as_ref() {
        "lldb" | "codelldb" => Ok(Box::new(codelldb::Debugger::new(
            id,
            command,
            command_args,
            environment_variables,
            neovim,
            log_debugger,
        ))),

        "python" | "debugpy" => Ok(Box::new(debugpy::Debugger::new(
            id,
            command,
            command_args,
            neovim,
            log_debugger,
        ))),

        "go" | "delve" => Ok(Box::new(go_delve::Debugger::new(
            id,
            command,
            command_args,
            neovim,
            log_debugger,
        ))),

        "dotnet" | ".net" | "net" => Ok(Box::new(dotnet::Debugger::new(
            id,
            command,
            command_args,
            neovim,
            log_debugger,
        ))),

        "node" | "nodejs" | "js" => Ok(Box::new(node::Debugger::new(
            id,
            command,
            command_args,
            neovim,
            log_debugger,
        ))),

        _ => bail!("ERROR: Debugger unknown {}", debugger_type),
    }
}

#[async_trait]
pub trait DebuggerAPI: DynClone + Debug {
    async fn setup(
        &mut self,
        pending_breakpoints: &HashMap<String, HashSet<i64>>,
        existing_debugger_port: Option<String>,
    ) -> Result<()>;

    async fn set_source_breakpoints(
        &self,
        file_path: String,
        line_numbers: &HashSet<i64>,
    ) -> Result<()>;

    async fn do_step(&self, step_type: DebuggerStepType, count: u64) -> Result<()>;

    async fn change_output_window(&self, type_: &str) -> Result<()>;

    async fn log_msg(&self, level: VadreLogLevel, msg: &str) -> Result<()>;
}

#[derive(Clone, Debug)]
pub enum DebuggerStepType {
    Over,
    In,
    Continue,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct Breakpoint {
    pub id: i64,
    pub file_path: String,
    pub line_number: i64,
    pub actual_line_number: Option<i64>,
    pub enabled: bool,
}

impl Breakpoint {
    fn new(
        id: i64,
        file_path: String,
        line_number: i64,
        actual_line_number: Option<i64>,
        enabled: bool,
    ) -> Self {
        Breakpoint {
            id,
            file_path,
            line_number,
            actual_line_number,
            enabled,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Breakpoints(HashMap<i64, Breakpoint>);

impl Breakpoints {
    fn add_breakpoint(
        &mut self,
        id: i64,
        file_path: String,
        line_number: i64,
        actual_line_number: Option<i64>,
        enabled: bool,
    ) -> Result<()> {
        self.0.insert(
            id,
            Breakpoint::new(id, file_path, line_number, actual_line_number, enabled),
        );

        Ok(())
    }

    fn get_breakpoint_for_id(&self, id: &i64) -> Option<&Breakpoint> {
        self.0.get(id)
    }

    fn remove_file_ids(&mut self, file_path: String, ids: Vec<i64>) -> Result<()> {
        let mut remove_ids = vec![];
        for (id, breakpoint) in self.0.iter() {
            if breakpoint.file_path == file_path && !ids.contains(&id) {
                remove_ids.push(id.clone());
            }
        }
        for id in remove_ids {
            self.0.remove(&id);
        }
        Ok(())
    }

    fn get_all_breakpoints_for_file(&self, file_path: &str) -> Vec<&Breakpoint> {
        self.0
            .values()
            .filter(|x| x.file_path == file_path)
            .collect()
    }
}

#[derive(Clone, Debug, Default)]
struct DebuggerData {
    current_thread_id: Option<i64>,
    current_frame_id: Option<i64>,
    breakpoints: Breakpoints,
}
