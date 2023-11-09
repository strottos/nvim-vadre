pub(crate) mod codelldb;

use std::{collections::HashMap, path::PathBuf};

use super::protocol::RequestArguments;
use anyhow::Result;

// TODO: Can we meta/macro this file somehow?

#[derive(Debug, Clone)]
pub(crate) enum DebuggerType {
    CodeLLDB(codelldb::Debugger),
    // DebugPy(DebugPy),
    // GoDelve(GoDelve),
    // DotNet(DotNet),
    // NodeJS(NodeJS),
    // Generic(Generic),
}

impl DebuggerType {
    pub(crate) async fn download_plugin(&self) -> Result<()> {
        match self {
            DebuggerType::CodeLLDB(debugger) => debugger.download_plugin().await,
        }
    }

    pub(crate) fn get_cmd_args(&self, port: Option<u16>) -> Result<Vec<String>> {
        match self {
            DebuggerType::CodeLLDB(debugger) => debugger.get_cmd_args(port),
        }
    }

    pub(crate) fn get_launch_request(
        &self,
        command_args: Vec<String>,
        environment_variables: HashMap<String, String>,
    ) -> Result<RequestArguments> {
        match self {
            DebuggerType::CodeLLDB(debugger) => {
                debugger.get_launch_request(command_args, environment_variables)
            }
        }
    }

    pub(crate) fn get_attach_request(&self, pid: i64) -> Result<RequestArguments> {
        match self {
            DebuggerType::CodeLLDB(debugger) => debugger.get_attach_request(pid),
        }
    }

    pub(crate) fn get_debugger_path(&self) -> Result<PathBuf> {
        match self {
            DebuggerType::CodeLLDB(debugger) => debugger.get_debugger_path(),
        }
    }

    pub(crate) fn get_debugger_type_name(&self) -> String {
        match self {
            DebuggerType::CodeLLDB(_) => "CodeLLDB".to_string(),
        }
    }
}
