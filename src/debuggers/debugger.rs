use super::DebuggerStepType;
use crate::neovim::VadreLogLevel;

use std::collections::HashMap;

use anyhow::Result;

#[derive(Debug)]
pub(crate) enum Debugger {
    Dap(super::dap::DapDebugger),
    Node(super::node::NodeDebugger),
}

impl Debugger {
    #[tracing::instrument(skip(self))]
    pub(crate) async fn setup(
        &mut self,
        command_args: Vec<String>,
        environment_variables: HashMap<String, String>,
        existing_debugger_port: Option<u16>,
        attach_debugger_to_pid: Option<i64>,
        dap_command: Option<String>,
    ) -> Result<()> {
        match self {
            Debugger::Dap(debugger) => {
                debugger
                    .setup(
                        command_args,
                        environment_variables,
                        existing_debugger_port,
                        attach_debugger_to_pid,
                        dap_command,
                    )
                    .await
            }
            Debugger::Node(debugger) => {
                debugger
                    .setup(command_args, environment_variables, existing_debugger_port)
                    .await
            }
        }
    }

    pub(crate) async fn add_breakpoint(
        &mut self,
        source_file_path: String,
        source_line_number: i64,
    ) -> Result<()> {
        match self {
            Debugger::Dap(debugger) => {
                debugger
                    .handler
                    .lock()
                    .await
                    .add_breakpoint(source_file_path, source_line_number)
                    .await
            }
            Debugger::Node(debugger) => {
                debugger
                    .handler
                    .lock()
                    .await
                    .add_breakpoint(source_file_path, source_line_number)
                    .await
            }
        }
    }

    pub(crate) async fn remove_breakpoint(
        &mut self,
        source_file_path: String,
        source_line_number: i64,
    ) -> Result<()> {
        match self {
            Debugger::Dap(debugger) => {
                debugger
                    .handler
                    .lock()
                    .await
                    .remove_breakpoint(source_file_path, source_line_number)
                    .await
            }
            Debugger::Node(debugger) => {
                debugger
                    .handler
                    .lock()
                    .await
                    .remove_breakpoint(source_file_path, source_line_number)
                    .await
            }
        }
    }

    pub(crate) async fn do_step(&mut self, step_type: DebuggerStepType, count: u64) -> Result<()> {
        match self {
            Debugger::Dap(debugger) => {
                debugger
                    .handler
                    .lock()
                    .await
                    .do_step(step_type, count)
                    .await
            }
            Debugger::Node(debugger) => {
                debugger
                    .handler
                    .lock()
                    .await
                    .do_step(step_type, count)
                    .await
            }
        }
    }

    pub(crate) async fn pause(&mut self, thread: Option<i64>) -> Result<()> {
        match self {
            Debugger::Dap(debugger) => debugger.handler.lock().await.pause(thread).await,
            Debugger::Node(debugger) => debugger.handler.lock().await.pause(thread).await,
        }
    }

    pub(crate) async fn stop(&mut self) -> Result<()> {
        match self {
            Debugger::Dap(debugger) => debugger.handler.lock().await.stop().await,
            Debugger::Node(debugger) => debugger.handler.lock().await.stop().await,
        }
    }

    pub(crate) async fn change_output_window(&mut self, type_: &str) -> Result<()> {
        match self {
            Debugger::Dap(debugger) => {
                debugger
                    .handler
                    .lock()
                    .await
                    .change_output_window(type_)
                    .await
            }
            Debugger::Node(debugger) => {
                debugger
                    .handler
                    .lock()
                    .await
                    .change_output_window(type_)
                    .await
            }
        }
    }

    pub(crate) async fn handle_output_window_key(&mut self, key: &str) -> Result<()> {
        match self {
            Debugger::Dap(debugger) => {
                debugger
                    .handler
                    .lock()
                    .await
                    .handle_output_window_key(key)
                    .await
            }
            Debugger::Node(debugger) => {
                debugger
                    .handler
                    .lock()
                    .await
                    .handle_output_window_key(key)
                    .await
            }
        }
    }

    pub(crate) async fn log_msg(&self, level: VadreLogLevel, msg: &str) -> Result<()> {
        match self {
            Debugger::Dap(debugger) => debugger.log_msg(level, msg).await,
            Debugger::Node(debugger) => debugger.log_msg(level, msg).await,
        }
    }

    pub(crate) fn is_setup_complete(&self) -> bool {
        match self {
            Debugger::Dap(debugger) => debugger.setup_complete,
            Debugger::Node(debugger) => debugger.setup_complete,
        }
    }
}
