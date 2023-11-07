mod codelldb;
mod dap;
// mod debugpy;
// mod dotnet;
// mod generic;
// mod go_delve;
// mod node;

use std::{
    collections::{BTreeSet, HashMap},
    fmt::Debug,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use crate::{
    neovim::{CodeBufferContent, NeovimVadreWindow, VadreBufferType, VadreLogLevel},
    util::get_unused_localhost_port,
};
use dap::protocol::{
    Breakpoint, BreakpointEventBody, ContinueArguments, DAPCodec, DecoderResult,
    DisconnectArguments, Either, EventBody, InitializeRequestArguments, NextArguments,
    ProtocolMessage, ProtocolMessageType, RequestArguments, Response, ResponseBody, ResponseResult,
    RunInTerminalResponseBody, ScopesArguments, SetBreakpointsArguments,
    SetExceptionBreakpointsArguments, SetFunctionBreakpointsArguments, Source, SourceArguments,
    SourceBreakpoint, StackTraceArguments, StepInArguments, StoppedEventBody, VariablesArguments,
};

use anyhow::{anyhow, bail, Result};
use futures::{prelude::*, StreamExt};
use nvim_rs::{compat::tokio::Compat, Neovim};
use tokio::{
    io::{AsyncBufReadExt, BufReader, Stdout},
    net::TcpStream,
    process::{Child, Command},
    sync::{broadcast, mpsc, oneshot, Mutex},
    time::{sleep, timeout},
    try_join,
};
use tokio_util::codec::Decoder;

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
    #[must_use]
    async fn download_plugin(&self) -> Result<()> {
        match self {
            DebuggerType::CodeLLDB(debugger) => debugger.download_plugin().await,
        }
    }

    #[must_use]
    fn get_launch_request(
        &self,
        command: String,
        command_args: Vec<String>,
        environment_variables: HashMap<String, String>,
    ) -> Result<RequestArguments> {
        match self {
            DebuggerType::CodeLLDB(debugger) => {
                debugger.get_launch_request(command, command_args, environment_variables)
            }
        }
    }

    #[must_use]
    fn get_debugger_path(&self) -> Result<PathBuf> {
        match self {
            DebuggerType::CodeLLDB(debugger) => debugger.get_debugger_path(),
        }
    }

    fn get_debugger_type_name(&self) -> String {
        match self {
            DebuggerType::CodeLLDB(_) => "CodeLLDB".to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum DebuggerStepType {
    Over,
    In,
    Continue,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DebuggerBreakpoint {
    enabled: bool,

    // Collection of placed breakpoints, can be multiple breakpoints placed for one in the source.
    // Here we store the breakpoint id as the key and the line number it was placed on as the
    // value.
    resolved: HashMap<String, i64>,
}

impl DebuggerBreakpoint {
    fn new() -> Self {
        DebuggerBreakpoint {
            enabled: false,
            resolved: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Breakpoints(HashMap<String, HashMap<i64, DebuggerBreakpoint>>);

impl Breakpoints {
    pub(crate) fn new() -> Self {
        Breakpoints(HashMap::new())
    }

    #[must_use]
    pub(crate) fn add_pending_breakpoint(
        &mut self,
        source_file_path: String,
        source_line_number: i64,
    ) -> Result<()> {
        self.0
            .entry(source_file_path)
            .and_modify(|line_map| {
                line_map.insert(source_line_number, DebuggerBreakpoint::new());
            })
            .or_insert({
                let mut line_map = HashMap::new();
                line_map.insert(source_line_number, DebuggerBreakpoint::new());
                line_map
            });

        Ok(())
    }

    #[must_use]
    pub(crate) fn remove_breakpoint(
        &mut self,
        source_file_path: String,
        source_line_number: i64,
    ) -> Result<()> {
        if let Some(line_map) = self.0.get_mut(&source_file_path) {
            assert!(line_map.remove(&source_line_number).is_some());
        } else {
            bail!(
                "Can't find existing breakpoints for file {}",
                source_file_path
            )
        }

        Ok(())
    }

    fn get_all_files(&self) -> BTreeSet<String> {
        self.0.keys().map(|x| x.clone()).collect()
    }

    fn get_breakpoint_for_id(&self, id: &i64) -> Option<(String, i64)> {
        for (source_file_path, line_map) in self.0.iter() {
            for (source_line_number, breakpoint) in line_map.iter() {
                if breakpoint.resolved.contains_key(&id.to_string()) {
                    return Some((source_file_path.clone(), *source_line_number));
                }
            }
        }
        None
    }

    fn set_breakpoint_resolved(
        &mut self,
        file_path: String,
        source_line_number: i64,
        actual_line_number: i64,
        id: String,
    ) -> Result<()> {
        let breakpoint = self
            .0
            .get_mut(&file_path)
            .ok_or_else(|| anyhow!("Can't find existing breakpoints for file {}", file_path))?
            .get_mut(&source_line_number)
            .ok_or_else(|| {
                anyhow!(
                    "Can't find existing breakpoints for file {} and line {}",
                    file_path,
                    source_line_number
                )
            })?;
        breakpoint.resolved.insert(id, actual_line_number);

        Ok(())
    }

    fn set_breakpoint_enabled(&mut self, file_path: String, source_line_number: i64) -> Result<()> {
        let breakpoint = self
            .0
            .get_mut(&file_path)
            .ok_or_else(|| anyhow!("Can't find existing breakpoints for file {}", file_path))?
            .get_mut(&source_line_number)
            .ok_or_else(|| {
                anyhow!(
                    "Can't find existing breakpoints for file {} and line {}",
                    file_path,
                    source_line_number
                )
            })?;
        breakpoint.enabled = true;

        Ok(())
    }

    fn set_breakpoint_disabled(
        &mut self,
        file_path: String,
        source_line_number: i64,
    ) -> Result<()> {
        let breakpoint = self
            .0
            .get_mut(&file_path)
            .ok_or_else(|| anyhow!("Can't find existing breakpoints for file {}", file_path))?
            .get_mut(&source_line_number)
            .ok_or_else(|| {
                anyhow!(
                    "Can't find existing breakpoints for file {} and line {}",
                    file_path,
                    source_line_number
                )
            })?;
        breakpoint.enabled = false;

        Ok(())
    }

    fn get_all_breakpoint_line_numbers_for_file(
        &self,
        file_path: &str,
    ) -> Result<&HashMap<i64, DebuggerBreakpoint>> {
        self.0
            .get(file_path)
            .ok_or_else(|| anyhow!("Can't find existing breakpoints for file {}", file_path))
    }
}

struct DebuggerHandler {
    pub neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,

    debugger_type: DebuggerType,
    processor: DebuggerProcess,

    current_thread_id: Option<i64>,
    current_frame_id: Option<i64>,
    breakpoints: Breakpoints,

    debug_program_string: String,

    config_done_tx: Option<oneshot::Sender<()>>,
    stopped_listener_tx: Option<oneshot::Sender<()>>,
}

impl DebuggerHandler {
    #[must_use]
    #[tracing::instrument(skip(self))]
    async fn setup(
        &mut self,
        existing_debugger_port: Option<String>,
        existing_debugger_pid: Option<String>,
    ) -> Result<()> {
        self.processor
            .setup(existing_debugger_port, existing_debugger_pid)
            .await?;
        Ok(())
    }

    /// Initialise the debugger
    #[must_use]
    #[tracing::instrument(skip(self))]
    async fn init_process(
        &mut self,
        command: String,
        command_args: Vec<String>,
        environment_variables: HashMap<String, String>,
    ) -> Result<oneshot::Receiver<Response>> {
        self.request_and_response(RequestArguments::initialize(
            InitializeRequestArguments::new(self.debugger_type.get_debugger_type_name()),
        ))
        .await?;

        let launch_request =
            self.debugger_type
                .get_launch_request(command, command_args, environment_variables)?;

        let (tx, rx) = oneshot::channel();

        self.processor.request(launch_request, Some(tx)).await?;

        Ok(rx)
    }

    #[must_use]
    #[tracing::instrument]
    async fn set_init_breakpoints(&mut self, pending_breakpoints: &Breakpoints) -> Result<()> {
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();

        self.processor
            .request(
                RequestArguments::setFunctionBreakpoints(SetFunctionBreakpointsArguments {
                    breakpoints: vec![],
                }),
                Some(tx1),
            )
            .await?;
        self.processor
            .request(
                RequestArguments::setExceptionBreakpoints(SetExceptionBreakpointsArguments {
                    filters: vec![],
                    exception_options: None,
                    filter_options: None,
                }),
                Some(tx2),
            )
            .await?;

        try_join!(rx1, rx2)?;

        for file_path in pending_breakpoints.get_all_files() {
            for line_number in pending_breakpoints
                .get_all_breakpoint_line_numbers_for_file(&file_path)?
                .keys()
            {
                self.breakpoints
                    .add_pending_breakpoint(file_path.clone(), *line_number)?;
            }

            self.set_breakpoints(file_path).await?;
        }

        Ok(())
    }

    #[must_use]
    #[tracing::instrument(skip(self))]
    pub async fn set_breakpoints(&mut self, file_path: String) -> Result<()> {
        tracing::trace!("Debugger setting breakpoints in file {:?}", file_path);

        let line_numbers = self
            .breakpoints
            .get_all_breakpoint_line_numbers_for_file(&file_path)?
            .keys()
            .map(|x| *x)
            .into_iter()
            .collect::<Vec<i64>>();

        let file_name = Path::new(&file_path)
            .file_name()
            .ok_or_else(|| anyhow!("Can't find filename for file path: {:?}", file_path))?;

        let file_name = file_name
            .to_str()
            .ok_or_else(|| anyhow!("Can't covert filename to string: {:?}", file_name))?
            .to_string();

        let breakpoints: Vec<SourceBreakpoint> = line_numbers
            .clone()
            .into_iter()
            .map(|x| SourceBreakpoint::new(x))
            .collect::<Vec<SourceBreakpoint>>();

        let source = Source::new_file(file_name.clone(), file_path.clone());

        let response_result = self
            .request_and_response(RequestArguments::setBreakpoints(SetBreakpointsArguments {
                breakpoints: Some(breakpoints),
                lines: None,
                source,
                source_modified: Some(false),
            }))
            .await?;

        tracing::trace!(
            "Debugger set breakpoints response_result {:?}",
            response_result
        );

        if let ResponseResult::Success { body } = response_result {
            if let ResponseBody::setBreakpoints(breakpoints_body) = body {
                for (i, breakpoint_response) in breakpoints_body.breakpoints.into_iter().enumerate()
                {
                    let source_line_number = line_numbers
                        .get(i)
                        .ok_or_else(|| {
                            anyhow!("Can't get {i}th line number from: {:?}", line_numbers)
                        })?
                        .clone();

                    let breakpoint_id = breakpoint_response.id.ok_or_else(|| {
                        anyhow!(
                            "Id wasn't set in breakpoint setting response as expected: {:?}",
                            breakpoint_response
                        )
                    })?;

                    let breakpoint_is_enabled = self.breakpoint_is_enabled(&breakpoint_response)?;

                    self.breakpoints.set_breakpoint_resolved(
                        file_path.clone(),
                        source_line_number,
                        breakpoint_response.line.ok_or_else(|| {
                            anyhow!(
                                "Line wasn't set in breakpoint setting response as expected: {:?}",
                                breakpoint_response
                            )
                        })?,
                        breakpoint_id.to_string(),
                    )?;

                    if breakpoint_is_enabled {
                        self.breakpoints
                            .set_breakpoint_enabled(file_path.clone(), source_line_number)?;
                    }
                }

                self.process_breakpoints_output().await?;
            }
        }

        Ok(())
    }

    /// Handle a request from Debugger
    #[must_use]
    #[tracing::instrument]
    async fn handle_request(
        &mut self,
        request_id: u32,
        args: RequestArguments,
        // config_done_tx: &mut Option<oneshot::Sender<()>>,
    ) -> Result<()> {
        tracing::trace!("Handling request: {:?}", args);

        // Debugger is requesting something from us, currently only runTerminal should be received
        match args {
            RequestArguments::runInTerminal(args) => {
                self.neovim_vadre_window
                    .lock()
                    .await
                    .log_msg(
                        VadreLogLevel::INFO,
                        "Spawning terminal to communicate with program",
                    )
                    .await?;

                self.neovim_vadre_window
                    .lock()
                    .await
                    .spawn_terminal_command(
                        args.args
                            .into_iter()
                            .map(|x| format!(r#""{}""#, x))
                            .collect::<Vec<String>>()
                            .join(" "),
                        &self.debug_program_string,
                    )
                    .await?;

                let response = ProtocolMessage {
                    seq: Either::First(request_id),
                    type_: ProtocolMessageType::Response(Response {
                        request_seq: Either::First(request_id),
                        success: true,
                        result: ResponseResult::Success {
                            body: ResponseBody::runInTerminal(RunInTerminalResponseBody {
                                process_id: None,
                                shell_process_id: None,
                            }),
                        },
                    }),
                };

                self.processor.send_msg(response).await?;

                match self.config_done_tx.take() {
                    Some(x) => {
                        if let Err(_) = x.send(()) {
                            bail!("Couldn't send config_done_tx");
                        }
                    }
                    None => {}
                };

                Ok(())
            }
            _ => unreachable!(),
        }
    }

    #[must_use]
    #[tracing::instrument(skip(event))]
    async fn handle_event(&mut self, event: EventBody) -> Result<()> {
        tracing::trace!("Processing event: {:?}", event);
        match event {
            EventBody::initialized(_) => Ok(()),
            EventBody::output(output) => {
                self.neovim_vadre_window
                    .lock()
                    .await
                    .log_msg(
                        VadreLogLevel::INFO,
                        &format!("Debugger: {}", output.output.trim_end()),
                    )
                    .await
            }
            EventBody::stopped(stopped_event) => self.handle_event_stopped(stopped_event).await,
            EventBody::continued(_) => Ok(()),
            EventBody::breakpoint(breakpoint_event) => {
                self.handle_event_breakpoint(breakpoint_event).await
            }
            _ => {
                tracing::trace!("Got unhandled event {:?}", event);
                Ok(())
            }
        }
    }

    fn set_stopped_listener(&mut self, tx: oneshot::Sender<()>) {
        self.stopped_listener_tx = Some(tx);
    }

    #[must_use]
    #[tracing::instrument]
    async fn handle_event_stopped(&mut self, stopped_event: StoppedEventBody) -> Result<()> {
        self.current_thread_id = stopped_event.thread_id;

        if let Some(listener_tx) = self.stopped_listener_tx.take() {
            // If we're here we're about to do more stepping so no need to do more
            if let Err(_) = listener_tx.send(()) {
                tracing::error!("Couldn't send stopped_listener_tx");
                self.neovim_vadre_window
                    .lock()
                    .await
                    .log_msg(VadreLogLevel::WARN, "Couldn't send stopped_listener_tx")
                    .await?;
            }
            return Ok(());
        }

        self.process_output_info().await?;

        if let Some(thread_id) = stopped_event.thread_id {
            if let Err(e) = self.process_stopped(thread_id).await {
                self.neovim_vadre_window
                    .lock()
                    .await
                    .log_msg(
                        VadreLogLevel::WARN,
                        &format!("An error occurred while displaying code pointer: {}", e),
                    )
                    .await?;
            }
        }

        Ok(())
    }

    #[tracing::instrument]
    async fn handle_event_breakpoint(
        &mut self,
        breakpoint_event: BreakpointEventBody,
    ) -> Result<()> {
        let breakpoint_id = breakpoint_event
            .breakpoint
            .id
            .ok_or_else(|| anyhow!("Couldn't find ID from event: {:?}", breakpoint_event))?;

        let breakpoint_is_enabled = self.breakpoint_is_enabled(&breakpoint_event.breakpoint)?;

        // Do we need to poll/sleep here to wait for the breakpoint to be resolved?
        let (file_path, source_line_number) = self
            .breakpoints
            .get_breakpoint_for_id(&breakpoint_id)
            .ok_or_else(|| anyhow!("Can't find breakpoint for id {}", breakpoint_id))?;

        self.breakpoints.set_breakpoint_resolved(
            file_path.clone(),
            source_line_number,
            breakpoint_event
                .breakpoint
                .line
                .ok_or_else(|| anyhow!("Couldn't find line from event: {:?}", breakpoint_event))?,
            breakpoint_id.to_string(),
        )?;

        if breakpoint_is_enabled {
            self.breakpoints
                .set_breakpoint_enabled(file_path.clone(), source_line_number)?;
        }

        self.process_breakpoints_output().await?;

        Ok(())
    }

    fn breakpoint_is_enabled(&self, breakpoint: &Breakpoint) -> Result<bool> {
        let message = breakpoint
            .message
            .as_ref()
            .ok_or_else(|| anyhow!("Can't find breakpoint message: {:?}", breakpoint))?;

        // This is awful but I can't see any other way of knowing if a breakpoint
        // was resolved other than checking the message for "Resolved locations: "
        // and ending in either 0 or greater.
        Ok(message
            .rsplit_once("Resolved locations: ")
            .ok_or_else(|| anyhow!("Couldn't find Resolved locations message in: {}", message))?
            .1
            .parse::<i64>()?
            > 0)
    }

    #[must_use]
    #[tracing::instrument(skip(thread_id))]
    async fn process_stopped(&self, thread_id: i64) -> Result<()> {
        tracing::debug!("Thread id {} stopped", thread_id);

        let stack_trace_response = self
            .request_and_response(RequestArguments::stackTrace(StackTraceArguments {
                thread_id,
                format: None,
                levels: Some(1),
                start_frame: Some(0), // Should be optional but debugpy breaks without this
            }))
            .await?;

        if let ResponseResult::Success { body } = stack_trace_response {
            if let ResponseBody::stackTrace(stack_trace_body) = body {
                let stack = stack_trace_body.stack_frames;
                let current_frame = stack
                    .get(0)
                    .ok_or_else(|| anyhow!("stack should have a top frame: {:?}", stack))?;
                let source = current_frame
                    .source
                    .as_ref()
                    .ok_or_else(|| anyhow!("stack should have a source: {:?}", current_frame))?;
                let line_number = current_frame.line;

                tracing::trace!("Stop at {:?}:{}", source, line_number);

                if let Some(source_file) = &source.path {
                    self.neovim_vadre_window
                        .lock()
                        .await
                        .set_code_buffer(
                            CodeBufferContent::File(&source_file),
                            line_number,
                            &source_file,
                            false,
                        )
                        .await?;
                } else if let Some(source_reference_id) = source.source_reference {
                    let source_reference_response = self
                        .request_and_response(RequestArguments::source(SourceArguments {
                            source: Some(source.clone()),
                            source_reference: source_reference_id,
                        }))
                        .await?;
                    if let ResponseResult::Success { body } = source_reference_response {
                        if let ResponseBody::source(source_reference_body) = body {
                            tracing::trace!("source reference {:?}", source_reference_body);

                            self.neovim_vadre_window
                                .lock()
                                .await
                                .set_code_buffer(
                                    CodeBufferContent::Content(source_reference_body.content),
                                    line_number,
                                    &format!("Disassembled Code {}", source_reference_id),
                                    // TODO: Do we need to reset this every time, feels like it might update...
                                    true,
                                )
                                .await?;
                        }
                    }
                } else {
                    bail!("Can't find any source to display");
                }
            }
        }

        Ok(())
    }

    #[tracing::instrument]
    async fn process_output_info(&mut self) -> Result<()> {
        tracing::debug!("Getting thread information");

        let mut call_stack_buffer_content = Vec::new();
        let current_thread_id = self.current_thread_id;

        let current_output_window_type = self
            .neovim_vadre_window
            .lock()
            .await
            .get_output_window_type()
            .await?;

        tracing::trace!(
            "Current output window type: {:?}",
            current_output_window_type
        );

        let response_result = self
            .request_and_response(RequestArguments::threads(None))
            .await?;

        tracing::trace!("Response result: {:?}", response_result);

        if let ResponseResult::Success { body } = response_result {
            if let ResponseBody::threads(threads_body) = body {
                for thread in threads_body.threads {
                    let thread_id = thread.id;
                    let thread_name = thread.name;

                    if current_thread_id == Some(thread_id) {
                        call_stack_buffer_content.push(format!("{} (*)", thread_name));

                        let stack_trace_response = self
                            .request_and_response(RequestArguments::stackTrace(
                                StackTraceArguments {
                                    thread_id,
                                    format: None,
                                    levels: None,
                                    start_frame: None,
                                },
                            ))
                            .await?;

                        // Sometimes we don't get a body here as we get a message saying invalid thread,
                        // normally when the thread is doing something in blocking.
                        if let ResponseResult::Success { body } = stack_trace_response {
                            if let ResponseBody::stackTrace(stack_trace_body) = body {
                                let frames = stack_trace_body.stack_frames;

                                let top_frame = frames.get(0).ok_or_else(|| {
                                    anyhow!("Stack trace should have a first frame: {:?}", frames)
                                })?;
                                let frame_id = top_frame.id;
                                self.current_frame_id = Some(frame_id);

                                if current_output_window_type == VadreBufferType::Variables {
                                    self.process_variables(frame_id).await?;
                                }

                                if current_output_window_type == VadreBufferType::CallStack {
                                    for frame in frames {
                                        call_stack_buffer_content.push(format!("+ {}", frame.name));

                                        let line_number = frame.line;
                                        let source = frame.source.as_ref().ok_or_else(|| {
                                            anyhow!(
                                                "Couldn't get source from frame as expected: {:?}",
                                                frame
                                            )
                                        })?;
                                        if let Some(source_name) = &source.name {
                                            if let Some(_) = source.path {
                                                call_stack_buffer_content.push(format!(
                                                    "  - {}:{}",
                                                    source_name, line_number
                                                ));
                                            } else {
                                                call_stack_buffer_content.push(format!(
                                                    "  - {}:{} (disassembled)",
                                                    source_name, line_number
                                                ));
                                            }
                                        } else if let Some(source_path) = &source.path {
                                            call_stack_buffer_content.push(format!(
                                                "  - {}:{}",
                                                source_path, line_number
                                            ));
                                        } else {
                                            call_stack_buffer_content.push(format!(
                                                " - source not understood {:?}",
                                                source
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    } else if current_output_window_type == VadreBufferType::CallStack {
                        let stack_trace_response = self
                            .request_and_response(RequestArguments::stackTrace(
                                StackTraceArguments {
                                    thread_id,
                                    format: None,
                                    levels: None,
                                    start_frame: None,
                                },
                            ))
                            .await?;

                        if let ResponseResult::Success { body } = stack_trace_response {
                            if let ResponseBody::stackTrace(stack_trace_body) = body {
                                let frame_name = &stack_trace_body
                                    .stack_frames
                                    .get(0)
                                    .ok_or_else(|| anyhow!("Coudln't find first stack frame as expected from stack trace response: {:?}", stack_trace_body.stack_frames))?
                                    .name;

                                call_stack_buffer_content
                                    .push(format!("{} - {}", thread_name, frame_name));
                            } else {
                                call_stack_buffer_content.push(format!("{}", thread_name));
                            }
                        }
                    }
                }
            }
        }

        if current_output_window_type == VadreBufferType::CallStack {
            self.neovim_vadre_window
                .lock()
                .await
                .set_call_stack_buffer(call_stack_buffer_content)
                .await?;
        }

        Ok(())
    }

    #[tracing::instrument]
    async fn process_variables(&self, frame_id: i64) -> Result<()> {
        tracing::debug!("Getting variable information");

        let mut variable_content = Vec::new();

        let scopes_response_result = self
            .request_and_response(RequestArguments::scopes(ScopesArguments { frame_id }))
            .await?;

        if let ResponseResult::Success { body } = scopes_response_result {
            if let ResponseBody::scopes(scopes_body) = body {
                for scope in scopes_body.scopes {
                    variable_content.push(format!("{}:", scope.name));

                    let variables_response_result = self
                        .request_and_response(RequestArguments::variables(VariablesArguments {
                            count: None,
                            filter: None,
                            format: None,
                            start: None,
                            variables_reference: scope.variables_reference,
                        }))
                        .await?;

                    if let ResponseResult::Success { body } = variables_response_result {
                        if let ResponseBody::variables(variables_body) = body {
                            for variable in variables_body.variables {
                                let name = variable.name;
                                let value = variable.value;

                                if let Some(type_) = variable.type_ {
                                    variable_content
                                        .push(format!("+ ({}) {} = {:?}", type_, name, value));
                                } else {
                                    variable_content.push(format!("+ {} = {:?}", name, value));
                                }
                            }
                        }
                    }
                }

                self.neovim_vadre_window
                    .lock()
                    .await
                    .set_variables_buffer(variable_content)
                    .await?;
            }
        }

        Ok(())
    }

    #[tracing::instrument]
    async fn process_breakpoints_output(&self) -> Result<()> {
        let mut breakpoints_buffer_content = Vec::new();

        for file in self.breakpoints.get_all_files() {
            breakpoints_buffer_content.push(format!("{}:", file));

            for (source_line_number, breakpoint) in self
                .breakpoints
                .get_all_breakpoint_line_numbers_for_file(&file)?
            {
                let breakpoint_is_enabled = breakpoint.enabled;

                for (breakpoint_id, resolved_line_number) in &breakpoint.resolved {
                    breakpoints_buffer_content.push(format!(
                        "  {}  {}{}",
                        if breakpoint_is_enabled { "○" } else { "⬤" },
                        if source_line_number != resolved_line_number {
                            format!("{} -> {}", source_line_number, resolved_line_number)
                        } else {
                            format!("{}", source_line_number)
                        },
                        if breakpoint_is_enabled {
                            format!(" ({})", breakpoint_id)
                        } else {
                            "".to_string()
                        }
                    ));
                }
            }
        }

        self.neovim_vadre_window
            .lock()
            .await
            .set_breakpoints_buffer(breakpoints_buffer_content)
            .await?;

        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(child) = self.processor.process.lock().await.as_mut() {
            let request = RequestArguments::disconnect(DisconnectArguments {
                restart: Some(false),
                suspend_debuggee: None,
                terminate_debuggee: Some(true),
            });

            let resp = self.request_and_response(request).await?;

            match resp {
                ResponseResult::Success { .. } => {}
                ResponseResult::Error {
                    command: _,
                    message,
                    show_user: _,
                } => bail!("An error occurred stepping {}", message),
            };

            child.kill().await?;
        }

        Ok(())
    }

    async fn request(&self, request_args: RequestArguments) -> Result<()> {
        self.processor.request(request_args, None).await
    }

    async fn request_and_response(&self, request_args: RequestArguments) -> Result<ResponseResult> {
        self.processor.request_and_response(request_args).await
    }
}

impl Debug for DebuggerHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebuggerHandler")
            .field("current_thread_id", &self.current_thread_id)
            .field("current_frame_id", &self.current_frame_id)
            .field("breakpoints", &self.breakpoints)
            .finish()
    }
}

/// Responsible for spawning the process and handling the communication with the debugger
pub(crate) struct DebuggerProcess {
    debugger_type: DebuggerType,

    neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    process: Arc<Mutex<Option<Child>>>,

    /// Allows us to send to debugger
    debugger_sender_tx: Option<mpsc::Sender<ProtocolMessage>>,

    /// Mostly to allow new subscribers
    debugger_receiver_tx: Option<broadcast::Sender<ProtocolMessage>>,

    /// Allows us to receive from the debugger
    debugger_receiver_rx: Option<broadcast::Receiver<ProtocolMessage>>,

    seq_ids: AtomicU32,
}

impl DebuggerProcess {
    fn new(
        debugger_type: DebuggerType,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    ) -> Self {
        Self {
            debugger_type,

            neovim_vadre_window,
            process: Arc::new(Mutex::new(None)),

            debugger_sender_tx: None,
            debugger_receiver_tx: None,
            debugger_receiver_rx: None,

            seq_ids: AtomicU32::new(1),
        }
    }

    async fn get_request_timeout(&self) -> Duration {
        // TODO: Cache this?
        match self
            .neovim_vadre_window
            .lock()
            .await
            .get_var("vadre_request_timeout")
            .await
        {
            Ok(duration) => Duration::new(duration.as_u64().unwrap_or(30), 0),
            Err(_) => Duration::new(30, 0),
        }
    }

    async fn setup(
        &mut self,
        existing_debugger_port: Option<String>,
        _existing_debugger_pid: Option<String>,
    ) -> Result<()> {
        let port = match existing_debugger_port {
            Some(port) => port
                .parse::<u16>()
                .map_err(|e| anyhow!("debugger port not understood: {}", e))?,
            None => {
                let port = get_unused_localhost_port()?;

                self.launch(port).await?;

                port
            }
        };

        let request_timeout = self.get_request_timeout().await;

        self.tcp_connect_and_handle(port, request_timeout).await?;

        self.neovim_vadre_window
            .lock()
            .await
            .log_msg(VadreLogLevel::INFO, "Debugger launched")
            .await?;

        Ok(())
    }

    #[must_use]
    async fn launch(&mut self, port: u16) -> Result<()> {
        let msg = format!("Launching {}", self.debugger_type.get_debugger_type_name());
        self.neovim_vadre_window
            .lock()
            .await
            .log_msg(VadreLogLevel::DEBUG, &msg)
            .await?;

        self.debugger_type.download_plugin().await?;

        let path = self.debugger_type.get_debugger_path()?;

        if !path.exists() {
            bail!("The binary doesn't exist: {:?}", path);
        }

        let args = vec!["--port".to_string(), port.to_string()];

        tracing::debug!("Spawning process: {:?} {:?}", path, args);

        let mut child = Command::new(
            path.to_str()
                .ok_or_else(|| anyhow!("Can't convert path to string: {:?}", path))?,
        )
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("Failed to spawn debugger: {}", e))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to take stdout from debugger"))?;

        let neovim_vadre_window = self.neovim_vadre_window.clone();

        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Some(line) = reader
                .next_line()
                .await
                .map_err(|e| anyhow!("Can't read stdout: {}", e))?
            {
                tracing::info!("Debugger stdout: {}", line);
                neovim_vadre_window
                    .lock()
                    .await
                    .log_msg(VadreLogLevel::INFO, &format!("Debugger stdout: {}", line))
                    .await?;
            }

            Ok::<(), anyhow::Error>(())
        });

        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("Failed to take stderr from debugger"))?;

        let neovim_vadre_window = self.neovim_vadre_window.clone();

        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();

            while let Some(line) = reader
                .next_line()
                .await
                .map_err(|e| anyhow!("Can't read stderr: {}", e))?
            {
                tracing::warn!("Debugger stderr: {}", line);
                neovim_vadre_window
                    .lock()
                    .await
                    .log_msg(VadreLogLevel::WARN, &format!("Debugger stderr: {}", line))
                    .await?;
            }

            Ok::<(), anyhow::Error>(())
        });

        *self.process.lock().await = Some(child);

        self.neovim_vadre_window
            .lock()
            .await
            .log_msg(VadreLogLevel::DEBUG, "Process spawned".into())
            .await?;

        Ok(())
    }

    /// Handle the TCP connection and setup the frame decoding/encoding and handling
    #[must_use]
    #[tracing::instrument(skip(self))]
    async fn tcp_connect_and_handle(&mut self, port: u16, request_timeout: Duration) -> Result<()> {
        tracing::trace!("Connecting to port {}", port);

        let tcp_stream = self.do_tcp_connect(port).await?;
        let framed_stream = DAPCodec::new().framed(tcp_stream);

        self.handle_framed_stream(framed_stream).await?;

        Ok(())
    }

    /// Actually do the TCP connection itself
    #[must_use]
    #[tracing::instrument(skip(self, port))]
    async fn do_tcp_connect(&mut self, port: u16) -> Result<TcpStream> {
        let number_attempts = 50;

        for _ in 1..number_attempts {
            let tcp_stream = TcpStream::connect(format!("127.0.0.1:{}", port)).await;
            match tcp_stream {
                Ok(x) => return Ok(x),
                Err(e) => {
                    tracing::trace!("Sleeping 100ms before retry: {}", e);
                    sleep(Duration::from_millis(100)).await;
                }
            };
        }

        bail!(
            "Couldn't connect to server after {} attempts, bailing",
            number_attempts
        );
    }

    /// Spawn and handle the stream
    #[must_use]
    #[tracing::instrument(skip(self, framed_stream))]
    async fn handle_framed_stream<T>(&mut self, framed_stream: T) -> Result<()>
    where
        T: std::fmt::Debug
            + Stream<Item = Result<DecoderResult, io::Error>>
            + Sink<ProtocolMessage, Error = io::Error>
            + Send
            + Unpin
            + 'static,
    {
        let (debugger_sender_tx, debugger_sender_rx) = mpsc::channel(1);
        // Can theoretically get a lot of messages from the debugger.
        let (debugger_receiver_tx, debugger_receiver_rx) = broadcast::channel(20);

        self.debugger_sender_tx = Some(debugger_sender_tx);
        self.debugger_receiver_tx = Some(debugger_receiver_tx.clone());
        self.debugger_receiver_rx = Some(debugger_receiver_rx);

        let neovim_vadre_window = self.neovim_vadre_window.clone();

        tokio::spawn(async move {
            let mut framed_stream = framed_stream;
            let mut debugger_sender_rx = debugger_sender_rx;

            async fn report_error(
                msg: String,
                neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
            ) -> Result<()> {
                tracing::error!("{}", msg);
                neovim_vadre_window
                    .lock()
                    .await
                    .log_msg(VadreLogLevel::ERROR, &msg)
                    .await?;
                neovim_vadre_window
                    .lock()
                    .await
                    .err_writeln(&msg)
                    .await
                    .unwrap_or_else(|vim_err| {
                        tracing::error!("Couldn't write to neovim: {}", vim_err);
                    });

                Ok(())
            }

            loop {
                tokio::select! {
                    msg = framed_stream.next() => {
                        tracing::trace!("Got message from debugger {:?}", msg);
                        match msg {
                            Some(Ok(decoder_result)) => match decoder_result {
                                Ok(message) => {
                                    debugger_receiver_tx.send(message)?;
                                },
                                Err(err) => {
                                    report_error(
                                        format!("An decoder error occurred: {:?}", err),
                                        neovim_vadre_window.clone(),
                                    )
                                    .await?;
                                }
                            },
                            Some(Err(err)) => {
                                report_error(
                                    format!("Frame decoder error: {:?}", err),
                                    neovim_vadre_window.clone(),
                                )
                                .await?;
                            }
                            None => {
                                tracing::debug!("Client has disconnected");
                                break;
                            }
                        };
                    }
                    Some(message) = debugger_sender_rx.recv() => {
                        tracing::trace!("Got message to send to debugger {:?}", message);
                        framed_stream.send(message).await?;
                    }
                }
            }

            Ok::<(), anyhow::Error>(())
        });

        Ok(())
    }

    async fn send_msg(&mut self, message: ProtocolMessage) -> Result<()> {
        let debugger_sender_tx = self.debugger_sender_tx.as_mut().ok_or_else(|| {
            anyhow!("Couldn't get debugger_sender_tx, was process initialised correctly")
        })?;
        debugger_sender_tx.send(message).await?;

        Ok(())
    }

    async fn request_and_response(&self, request_args: RequestArguments) -> Result<ResponseResult> {
        let (tx, rx) = oneshot::channel();

        self.request(request_args, Some(tx)).await?;

        let response = rx.await?;

        if !response.success {
            bail!("Got unsuccessful response: {:?}", response);
        }

        Ok(response.result)
    }

    async fn request(
        &self,
        request_args: RequestArguments,
        response_sender: Option<oneshot::Sender<Response>>,
    ) -> Result<()> {
        let seq = self.seq_ids.fetch_add(1, Ordering::SeqCst);

        let message = ProtocolMessage {
            seq: Either::First(seq),
            type_: ProtocolMessageType::Request(request_args),
        };

        tracing::trace!("Sending request to send to debugger {:?}", message);

        let request_timeout = self.get_request_timeout().await;

        let mut debugger_receiver_rx = self
            .debugger_receiver_tx
            .as_ref()
            .ok_or_else(|| {
                anyhow!("Couldn't get debugger_receiver_tx, was process initialised correctly")
            })?
            .subscribe();
        let debugger_sender_tx = self.debugger_sender_tx.as_ref().ok_or_else(|| {
            anyhow!("Couldn't get debugger_sender_tx, was process initialised correctly")
        })?;

        debugger_sender_tx.send(message).await?;

        tokio::spawn(async move {
            let response = loop {
                let response = match timeout(request_timeout, debugger_receiver_rx.recv()).await {
                    Ok(resp) => resp?,
                    Err(e) => bail!("Timed out waiting for a response: {}", e),
                };

                tracing::trace!("Checking debugger msg: {:?}", response);

                if let ProtocolMessageType::Response(response) = response.type_ {
                    if *response.request_seq.first() == seq {
                        tracing::trace!("Got expected response with seq {}: {:?}", seq, response);
                        break response;
                    }
                }
            };

            if let Some(response_sender) = response_sender {
                response_sender.send(response).map_err(|e| {
                    anyhow!(
                        "Couldn't send response to response_sender, was it dropped? {:?}",
                        e
                    )
                })?;
            }

            Ok(())
        });

        tracing::trace!("Sent request");

        Ok(())
    }
}

impl Debug for DebuggerProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Debugger")
            .field("process", &self.process)
            .finish()
    }
}

pub(crate) struct Debugger {
    id: usize,
    pub neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    handler: Arc<Mutex<DebuggerHandler>>,
}

impl Debugger {
    pub(crate) fn new(
        id: usize,
        debug_program_string: String,
        debugger_type: DebuggerType,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    ) -> Self {
        let debugger_processor =
            DebuggerProcess::new(debugger_type.clone(), neovim_vadre_window.clone());

        let debugger_handler = Arc::new(Mutex::new(DebuggerHandler {
            debugger_type,

            processor: debugger_processor,

            neovim_vadre_window: neovim_vadre_window.clone(),

            current_thread_id: None,
            current_frame_id: None,
            breakpoints: Breakpoints::new(),

            debug_program_string,

            config_done_tx: None,
            stopped_listener_tx: None,
        }));

        Self {
            id,
            neovim_vadre_window,

            handler: debugger_handler,
        }
    }

    #[must_use]
    #[tracing::instrument(skip(self))]
    pub(crate) async fn setup(
        &mut self,
        command: String,
        command_args: Vec<String>,
        environment_variables: HashMap<String, String>,
        pending_breakpoints: &Breakpoints,
        existing_debugger_port: Option<String>,
    ) -> Result<()> {
        self.neovim_vadre_window.lock().await.create_ui().await?;

        let (config_done_tx, config_done_rx) = oneshot::channel();

        self.handler.lock().await.config_done_tx = Some(config_done_tx);

        self.handler
            .lock()
            .await
            .setup(existing_debugger_port, None)
            .await?;

        self.handle_messages().await?;

        let launch_rx = self
            .handler
            .lock()
            .await
            .init_process(command, command_args, environment_variables)
            .await?;

        self.handler
            .lock()
            .await
            .set_init_breakpoints(pending_breakpoints)
            .await?;

        timeout(Duration::new(60, 0), config_done_rx).await??;

        let (config_tx, config_rx) = oneshot::channel();

        self.handler
            .lock()
            .await
            .processor
            .request(RequestArguments::configurationDone(None), Some(config_tx))
            .await?;

        try_join!(launch_rx, config_rx)?;

        self.log_msg(VadreLogLevel::INFO, "Debugger launched and setup")
            .await?;

        Ok(())
    }

    async fn handle_messages(&mut self) -> Result<()> {
        let debugger_rx = self
            .handler
            .lock()
            .await
            .processor
            .debugger_receiver_rx
            .as_ref()
            .ok_or_else(|| {
                anyhow!("Couldn't get debugger_receiver_rx, was process initialised correctly")
            })?
            .resubscribe();

        let debugger_handler = self.handler.clone();

        tokio::spawn(async move {
            let mut debugger_rx = debugger_rx;

            loop {
                let message = debugger_rx.recv().await?;

                tracing::trace!("Message found: {:?}", message);

                if let ProtocolMessageType::Request(request_args) = message.type_ {
                    debugger_handler
                        .lock()
                        .await
                        .handle_request(*message.seq.first(), request_args)
                        .await?;
                } else if let ProtocolMessageType::Event(event) = message.type_ {
                    debugger_handler.lock().await.handle_event(event).await?;
                }
            }

            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        });

        Ok(())
    }

    #[must_use]
    pub(crate) async fn add_breakpoint(&self, file_path: String, line_number: i64) -> Result<()> {
        self.handler
            .lock()
            .await
            .breakpoints
            .add_pending_breakpoint(file_path.clone(), line_number)?;

        self.handler.lock().await.set_breakpoints(file_path).await?;

        Ok(())
    }

    #[must_use]
    pub(crate) async fn remove_breakpoint(
        &self,
        file_path: String,
        line_number: i64,
    ) -> Result<()> {
        self.handler
            .lock()
            .await
            .breakpoints
            .remove_breakpoint(file_path.clone(), line_number)?;

        self.handler.lock().await.set_breakpoints(file_path).await?;

        Ok(())
    }

    #[must_use]
    #[tracing::instrument(skip(self))]
    pub(crate) async fn do_step(&self, step_type: DebuggerStepType, count: u64) -> Result<()> {
        let thread_id = self.handler.lock().await.current_thread_id.clone();

        let thread_id = match thread_id {
            Some(thread_id) => thread_id,
            None => {
                self.log_msg(
                    VadreLogLevel::ERROR,
                    "Can't do stepping as no current thread",
                )
                .await?;
                return Ok(());
            }
        };

        let request = match step_type {
            DebuggerStepType::Over => RequestArguments::next(NextArguments {
                granularity: None,
                single_thread: Some(false),
                thread_id,
            }),
            DebuggerStepType::In => RequestArguments::stepIn(StepInArguments {
                granularity: None,
                single_thread: Some(false),
                target_id: None,
                thread_id,
            }),
            DebuggerStepType::Continue => RequestArguments::continue_(ContinueArguments {
                single_thread: Some(false),
                thread_id,
            }),
        };

        for _ in 1..count {
            let (tx, rx) = oneshot::channel();

            self.handler.lock().await.set_stopped_listener(tx);

            let resp = self
                .handler
                .lock()
                .await
                .request_and_response(request.clone())
                .await?;

            match resp {
                ResponseResult::Success { .. } => {}
                ResponseResult::Error {
                    command: _,
                    message,
                    show_user: _,
                } => bail!("An error occurred stepping {}", message),
            };

            timeout(Duration::new(2, 0), rx).await??;
        }

        self.handler.lock().await.request(request).await?;

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

        let current_output_window_type = self
            .neovim_vadre_window
            .lock()
            .await
            .get_output_window_type()
            .await?;

        match current_output_window_type {
            VadreBufferType::CallStack | VadreBufferType::Variables => {
                self.handler.lock().await.process_output_info().await?;
            }
            _ => {}
        };

        Ok(())
    }

    #[must_use]
    pub(crate) async fn stop(&self) -> Result<()> {
        self.handler.lock().await.stop().await
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
        f.debug_struct("Debugger").field("id", &self.id).finish()
    }
}

#[must_use]
pub(crate) fn new_debugger(
    id: usize,
    debug_program_string: String,
    neovim: Neovim<Compat<Stdout>>,
    debugger_type: String,
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
        // ))),

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
