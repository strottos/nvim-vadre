mod codelldb;
mod dap;
// mod debugpy;
// mod dotnet;
// mod generic;
// mod go_delve;
// mod node;

use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use crate::{
    neovim::{NeovimVadreWindow, VadreBufferType, VadreLogLevel},
    util::get_unused_localhost_port,
};
use dap::{
    protocol::{
        self as dap_protocol, BreakpointEventBody, ContinueArguments, DAPCodec, DecoderResult,
        Either, EventBody, InitializeRequestArguments, NextArguments, ProtocolMessage,
        ProtocolMessageType, RequestArguments, Response, ResponseBody, ResponseResult,
        RunInTerminalResponseBody, ScopesArguments, SetBreakpointsArguments,
        SetExceptionBreakpointsArguments, SetFunctionBreakpointsArguments, Source, SourceArguments,
        SourceBreakpoint, StackTraceArguments, StepInArguments, StoppedEventBody,
        VariablesArguments,
    },
    shared as dap_shared,
};

use anyhow::{bail, Result};
use nvim_rs::{compat::tokio::Compat, Neovim};
use tokio::{
    io::{AsyncBufReadExt, BufReader, Stdout},
    process::{Child, Command},
    sync::Mutex,
};

pub(crate) enum DebuggerType {
    CodeLLDB(codelldb::Debugger),
    // DebugPy(DebugPy),
    // GoDelve(GoDelve),
    // DotNet(DotNet),
    // NodeJS(NodeJS),
    // Generic(Generic),
}

impl DebuggerType {
    async fn download_plugin(&mut self) -> Result<()> {
        match self {
            DebuggerType::CodeLLDB(debugger) => debugger.download_plugin().await,
        }
    }

    fn get_binary_name(&self) -> Result<String> {
        match self {
            DebuggerType::CodeLLDB(debugger) => debugger.get_binary_name(),
        }
    }

    fn get_debugger_path(&self) -> Result<PathBuf> {
        match self {
            DebuggerType::CodeLLDB(debugger) => debugger.get_debugger_path(),
        }
    }
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
    pub source_line_number: i64,
    pub actual_line_number: Option<i64>,
    pub enabled: bool,
}

impl Breakpoint {
    fn new(
        id: i64,
        file_path: String,
        source_line_number: i64,
        actual_line_number: Option<i64>,
        enabled: bool,
    ) -> Self {
        Breakpoint {
            id,
            file_path,
            source_line_number,
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

pub(crate) struct Debugger {
    id: usize,
    debugger_type: DebuggerType,
    pub neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    process: Arc<Mutex<Option<Child>>>,
}

impl Debugger {
    pub(crate) fn new(
        id: usize,
        debugger_type: DebuggerType,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    ) -> Self {
        Self {
            id,
            debugger_type,
            neovim_vadre_window,
            process: Arc::new(Mutex::new(None)),
        }
    }

    #[must_use]
    #[tracing::instrument(skip(self))]
    pub(crate) async fn setup(
        &mut self,
        command: String,
        command_args: Vec<String>,
        environment_variables: HashMap<String, String>,
        pending_breakpoints: &HashMap<String, HashSet<i64>>,
        existing_debugger_port: Option<String>,
    ) -> Result<()> {
        self.neovim_vadre_window.lock().await.create_ui().await?;

        let port = match existing_debugger_port {
            Some(port) => port.parse::<u16>().expect("debugger port is u16"),
            None => {
                let port = get_unused_localhost_port()?;

                self.launch(command, command_args, environment_variables, port)
                    .await?;

                port
            }
        };

        Ok(())
    }

    #[tracing::instrument(skip(self, port))]
    async fn launch(
        &mut self,
        command: String,
        command_args: Vec<String>,
        environment_variables: HashMap<String, String>,
        port: u16,
    ) -> Result<()> {
        let msg = format!(
            "Launching process {:?} with {:?} and args: {:?}",
            command, "codelldb", command_args,
        );
        self.log_msg(VadreLogLevel::DEBUG, &msg).await?;

        self.debugger_type.download_plugin().await?;

        let path = self.debugger_type.get_debugger_path()?;

        if !path.exists() {
            bail!("The binary doesn't exist: {}", path.to_str().unwrap());
        }

        let args = vec!["--port".to_string(), port.to_string()];

        tracing::debug!("Spawning process: {:?} {:?}", path, args);

        let mut child = Command::new(path.to_str().unwrap())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to spawn debugger");

        let stdout = child.stdout.take().expect("should have stdout");

        let neovim_vadre_window = self.neovim_vadre_window.clone();

        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Some(line) = reader.next_line().await.expect("can read stdout") {
                tracing::info!("Debugger stdout: {}", line);
                neovim_vadre_window
                    .lock()
                    .await
                    .log_msg(VadreLogLevel::INFO, &format!("Debugger stdout: {}", line))
                    .await
                    .expect("can log to vim");
            }
        });

        let stderr = child.stderr.take().expect("should have stderr");

        let neovim_vadre_window = self.neovim_vadre_window.clone();

        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();

            while let Some(line) = reader.next_line().await.expect("can read stderr") {
                tracing::warn!("Debugger stderr: {}", line);
                neovim_vadre_window
                    .lock()
                    .await
                    .log_msg(VadreLogLevel::WARN, &format!("Debugger stderr: {}", line))
                    .await
                    .expect("can log to vim");
            }
        });

        *self.process.lock().await = Some(child);

        self.log_msg(VadreLogLevel::DEBUG, "Process spawned".into())
            .await?;

        Ok(())
    }

    #[must_use]
    #[tracing::instrument(skip(self))]
    pub(crate) async fn set_source_breakpoints(
        &self,
        file_path: String,
        line_numbers: &HashSet<i64>,
    ) -> Result<()> {
        tracing::trace!(
            "Debugger setting breakpoints in file {:?} on lines {:?}",
            file_path,
            line_numbers,
        );

        // let file_name = Path::new(&file_path)
        //     .file_name()
        //     .unwrap()
        //     .to_str()
        //     .unwrap()
        //     .to_string();

        // let line_numbers = line_numbers.into_iter().map(|x| *x).collect::<Vec<i64>>();

        // let breakpoints: Vec<SourceBreakpoint> = line_numbers
        //     .clone()
        //     .into_iter()
        //     .map(|x| SourceBreakpoint::new(x))
        //     .collect::<Vec<SourceBreakpoint>>();

        // let source = Source::new_file(file_name.clone(), file_path.clone());

        // let response_result = dap_shared::do_send_request_and_await_response(
        //     RequestArguments::setBreakpoints(SetBreakpointsArguments {
        //         breakpoints: Some(breakpoints),
        //         lines: None,
        //         source,
        //         source_modified: Some(false),
        //     }),
        //     self.debugger_sender_tx.clone(),
        // )
        // .await?;

        // if let ResponseResult::Success { body } = response_result {
        //     if let ResponseBody::setBreakpoints(breakpoints_body) = body {
        //         let ids_left = breakpoints_body
        //             .breakpoints
        //             .iter()
        //             .map(|x| x.id.unwrap())
        //             .collect::<Vec<i64>>();

        //         self.data
        //             .lock()
        //             .await
        //             .breakpoints
        //             .remove_file_ids(file_path.clone(), ids_left)?;

        //         let mut data_lock = self.data.lock().await;

        //         for (i, breakpoint_response) in breakpoints_body.breakpoints.into_iter().enumerate()
        //         {
        //             let original_line_number = line_numbers.get(i).unwrap().clone();

        //             let breakpoint_id = breakpoint_response.id.unwrap();

        //             let breakpoint_is_resolved =
        //                 Debugger::breakpoint_is_resolved(&breakpoint_response);

        //             data_lock.breakpoints.add_breakpoint(
        //                 breakpoint_id,
        //                 file_path.clone(),
        //                 original_line_number,
        //                 breakpoint_response.line,
        //                 breakpoint_is_resolved,
        //             )?;
        //         }

        //         let breakpoints = data_lock
        //             .breakpoints
        //             .get_all_breakpoints_for_file(&file_path);

        //         self.neovim_vadre_window
        //             .lock()
        //             .await
        //             .toggle_breakpoint_in_buffer(&file_path, breakpoints)
        //             .await?;
        //     }
        // }

        Ok(())
    }

    #[must_use]
    #[tracing::instrument(skip(self))]
    pub(crate) async fn do_step(&self, step_type: DebuggerStepType, count: u64) -> Result<()> {
        // let thread_id = {
        //     let data = self.data.lock().await;

        //     data.current_thread_id.clone()
        // };

        // let thread_id = match thread_id {
        //     Some(thread_id) => thread_id,
        //     None => {
        //         self.log_msg(
        //             VadreLogLevel::ERROR,
        //             "Can't do stepping as no current thread",
        //         )
        //         .await?;
        //         return Ok(());
        //     }
        // };

        // let request = match step_type {
        //     DebuggerStepType::Over => RequestArguments::next(NextArguments {
        //         granularity: None,
        //         single_thread: Some(false),
        //         thread_id,
        //     }),
        //     DebuggerStepType::In => RequestArguments::stepIn(StepInArguments {
        //         granularity: None,
        //         single_thread: Some(false),
        //         target_id: None,
        //         thread_id,
        //     }),
        //     DebuggerStepType::Continue => RequestArguments::continue_(ContinueArguments {
        //         single_thread: Some(false),
        //         thread_id,
        //     }),
        // };

        // for _ in 1..count {
        //     let (tx, rx) = oneshot::channel();

        //     *self.stopped_listener_tx.lock().await = Some(tx);

        //     let resp = dap_shared::do_send_request_and_await_response(
        //         request.clone(),
        //         self.debugger_sender_tx.clone(),
        //     )
        //     .await?;

        //     match resp {
        //         ResponseResult::Success { .. } => {}
        //         ResponseResult::Error {
        //             command: _,
        //             message,
        //             show_user: _,
        //         } => bail!("An error occurred stepping {}", message),
        //     };

        //     timeout(Duration::new(2, 0), rx).await??;
        // }

        // dap_shared::do_send_request(request, self.debugger_sender_tx.clone(), None).await?;

        Ok(())
    }

    #[must_use]
    #[tracing::instrument(skip(self))]
    pub(crate) async fn change_output_window(&self, type_: &str) -> Result<()> {
        self.neovim_vadre_window
            .lock()
            .await
            .change_output_window(type_)
            .await?;

        // let current_output_window_type = self
        //     .neovim_vadre_window
        //     .lock()
        //     .await
        //     .get_output_window_type()
        //     .await?;

        // TODO: Get callstack or variables when needed
        // match current_output_window_type {
        //     VadreBufferType::CallStack | VadreBufferType::Variables => {
        //         log_err!(
        //             Debugger::process_output_info(
        //                 self.debugger_sender_tx.clone(),
        //                 self.neovim_vadre_window.clone(),
        //                 self.data.clone(),
        //             )
        //             .await,
        //             self.neovim_vadre_window.clone(),
        //             "can get thread info"
        //         );
        //     }
        //     _ => {}
        // };

        Ok(())
    }

    #[must_use]
    pub(crate) async fn log_msg(&self, level: VadreLogLevel, msg: &str) -> Result<()> {
        self.neovim_vadre_window
            .lock()
            .await
            .log_msg(level, msg)
            .await
    }
}

impl Debug for Debugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoDelveDebugger")
            .field("id", &self.id)
            .field("process", &self.process)
            .finish()
    }
}

pub(crate) fn new_debugger(
    id: usize,
    neovim: Neovim<Compat<Stdout>>,
    debugger_type: String,
    log_debugger: bool,
) -> Result<Box<Debugger>> {
    let neovim_vadre_window = Arc::new(Mutex::new(NeovimVadreWindow::new(neovim, id)));
    let debugger_type = match debugger_type.as_ref() {
        "lldb" | "codelldb" => {
            DebuggerType::CodeLLDB(codelldb::Debugger::new(neovim_vadre_window.clone()))
        }

        // "python" | "debugpy" => Ok(Box::new(debugpy::Debugger::new(
        //     id,
        //     command,
        //     command_args,
        //     neovim,
        //     log_debugger,
        // ))),

        // "go" | "delve" => Ok(Box::new(go_delve::Debugger::new(
        //     id,
        //     command,
        //     command_args,
        //     neovim,
        //     log_debugger,
        // ))),

        // "dotnet" | ".net" | "net" => Ok(Box::new(dotnet::Debugger::new(
        //     id,
        //     command,
        //     command_args,
        //     neovim,
        //     log_debugger,
        // ))),

        // "node" | "nodejs" | "js" => Ok(Box::new(node::Debugger::new(
        //     id,
        //     command,
        //     command_args,
        //     neovim,
        //     log_debugger,
        // ))),
        _ => bail!("ERROR: Debugger unknown {}", debugger_type),
    };

    Ok(Box::new(Debugger::new(
        id,
        debugger_type,
        neovim_vadre_window,
    )))
}
