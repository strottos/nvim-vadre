pub(crate) mod codelldb;
pub(crate) mod debugpy;
pub(crate) mod dotnet;
pub(crate) mod generic;
pub(crate) mod go_delve;

use std::collections::HashMap;

use super::protocol::RequestArguments;

use anyhow::Result;
use tokio::process::Child;

#[derive(Debug, Clone)]
pub(crate) enum DapDebuggerType {
    CodeLLDB(codelldb::Debugger),
    DebugPy(debugpy::Debugger),
    GoDelve(go_delve::Debugger),
    DotNet(dotnet::Debugger),
    Generic(generic::Debugger),
}

macro_rules! impl_debugger_type {
    ($($debugger_type:ident,)+) => {
        impl DapDebuggerType {
            pub(crate) async fn download_plugin(&self) -> Result<()> {
                match self {
                    $(DapDebuggerType::$debugger_type(debugger) => debugger.download_plugin().await,)*
                }
            }

            pub(crate) async fn spawn_child(&self, port: Option<u16>, program: Vec<String>) -> Result<Child> {
                match self {
                    $(DapDebuggerType::$debugger_type(debugger) => debugger.spawn_child(port, program).await,)*
                }
            }

            pub(crate) async fn get_launch_request(
                &self,
                command_args: Vec<String>,
                environment_variables: HashMap<String, String>,
            ) -> Result<RequestArguments> {
                match self {
                    $(DapDebuggerType::$debugger_type(debugger) => {
                        debugger.get_launch_request(command_args, environment_variables).await
                    },)*
                }
            }

            pub(crate) async fn get_attach_request(&self, pid: i64) -> Result<RequestArguments> {
                match self {
                    $(DapDebuggerType::$debugger_type(debugger) => debugger.get_attach_request(pid).await,)*
                }
            }

            pub(crate) fn check_breakpoint_enabled(&self, msg: &str) -> Result<bool> {
                match self {
                    $(DapDebuggerType::$debugger_type(debugger) => debugger.check_breakpoint_enabled(msg),)*
                }
            }

            pub(crate) fn get_debugger_type_name(&self) -> String {
                match self {
                    $(DapDebuggerType::$debugger_type(_) => format!("{}", stringify!($debugger_type)),)*
                }
            }
        }
    };
}

impl_debugger_type!(CodeLLDB, DebugPy, GoDelve, DotNet, Generic,);
