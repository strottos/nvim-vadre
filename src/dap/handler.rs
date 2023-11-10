use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    path::Path,
    sync::Arc,
    time::Duration,
};

use super::{
    breakpoints::Breakpoints,
    debuggers::DebuggerType,
    processor::DebuggerProcessor,
    protocol::{
        Breakpoint, BreakpointEventBody, ContinueArguments, Either, EventBody,
        InitializeRequestArguments, NextArguments, PauseArguments, ProtocolMessage,
        ProtocolMessageType, RequestArguments, Response, ResponseBody, ResponseResult,
        RunInTerminalResponseBody, ScopesArguments, SetBreakpointsArguments,
        SetExceptionBreakpointsArguments, SetFunctionBreakpointsArguments, Source, SourceArguments,
        SourceBreakpoint, StackTraceArguments, StepInArguments, StoppedEventBody,
        VariablesArguments,
    },
    DebuggerStepType,
};
use crate::neovim::{CodeBufferContent, NeovimVadreWindow, VadreBufferType, VadreLogLevel};

use anyhow::{anyhow, bail, Result};
use tokio::{
    sync::{broadcast, oneshot, Mutex},
    time::timeout,
    try_join,
};

pub(crate) struct DebuggerHandler {
    pub neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,

    debugger_type: DebuggerType,
    processor: DebuggerProcessor,

    current_thread_id: Option<i64>,
    current_frame_id: Option<i64>,
    breakpoints: Breakpoints,

    stack_expanded_threads: HashSet<i64>,

    debug_program_string: String,

    terminal_spawned_tx: Option<oneshot::Sender<()>>,
    stopped_listener_tx: Option<oneshot::Sender<()>>,
}

impl DebuggerHandler {
    pub(crate) fn new(
        debugger_type: DebuggerType,
        processor: DebuggerProcessor,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        debug_program_string: String,
    ) -> Self {
        DebuggerHandler {
            debugger_type,

            processor,

            neovim_vadre_window,

            current_thread_id: None,
            current_frame_id: None,
            breakpoints: Breakpoints::new(),

            stack_expanded_threads: HashSet::new(),

            debug_program_string,

            terminal_spawned_tx: None,
            stopped_listener_tx: None,
        }
    }

    #[tracing::instrument(skip(self, terminal_spawned_tx))]
    pub(crate) async fn init(
        &mut self,
        existing_debugger_port: Option<u16>,
        dap_command: Option<String>,
        terminal_spawned_tx: oneshot::Sender<()>,
    ) -> Result<()> {
        self.terminal_spawned_tx = Some(terminal_spawned_tx);

        self.processor
            .setup(existing_debugger_port, dap_command)
            .await?;

        Ok(())
    }

    /// Initialise the debugger
    #[tracing::instrument(skip(self))]
    pub(crate) async fn launch_program(
        &mut self,
        command_args: Vec<String>,
        attach_debugger_to_pid: Option<i64>,
        environment_variables: HashMap<String, String>,
    ) -> Result<oneshot::Receiver<Response>> {
        self.request_and_response(RequestArguments::initialize(
            InitializeRequestArguments::new(self.debugger_type.get_debugger_type_name()),
        ))
        .await?;

        let (tx, rx) = oneshot::channel();

        if let Some(attach_debugger_to_pid) = attach_debugger_to_pid {
            let attach_request = self
                .debugger_type
                .get_attach_request(attach_debugger_to_pid)
                .await?;

            self.processor.request(attach_request, Some(tx)).await?;
        } else {
            let launch_request = self
                .debugger_type
                .get_launch_request(command_args, environment_variables)
                .await?;

            self.processor.request(launch_request, Some(tx)).await?;
        }

        Ok(rx)
    }

    #[tracing::instrument]
    pub(crate) async fn set_init_breakpoints(
        &mut self,
        pending_breakpoints: &Breakpoints,
    ) -> Result<()> {
        // let (tx1, rx1) = oneshot::channel();
        // let (tx2, rx2) = oneshot::channel();

        // self.processor
        //     .request_and_response(
        //         RequestArguments::setFunctionBreakpoints(SetFunctionBreakpointsArguments {
        //             breakpoints: vec![],
        //         }),
        //     )
        //     .await?;
        // self.processor
        //     .request_and_response(
        //         RequestArguments::setExceptionBreakpoints(SetExceptionBreakpointsArguments {
        //             filters: vec![],
        //             exception_options: None,
        //             filter_options: None,
        //         }),
        //         Some(tx2),
        //     )
        //     .await?;

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

    #[tracing::instrument(skip(self))]
    pub(crate) async fn configuration_done(
        &mut self,
        config_tx: oneshot::Sender<Response>,
    ) -> Result<()> {
        self.request(RequestArguments::configurationDone(None), Some(config_tx))
            .await?;

        Ok(())
    }

    pub(crate) async fn add_breakpoint(
        &mut self,
        source_file_path: String,
        source_line_number: i64,
    ) -> Result<()> {
        self.breakpoints
            .add_pending_breakpoint(source_file_path.clone(), source_line_number)?;

        self.set_breakpoints(source_file_path).await?;

        Ok(())
    }

    pub(crate) async fn remove_breakpoint(
        &mut self,
        source_file_path: String,
        source_line_number: i64,
    ) -> Result<()> {
        self.breakpoints
            .remove_breakpoint(source_file_path.clone(), source_line_number)?;

        self.set_breakpoints(source_file_path).await?;

        Ok(())
    }

    pub(crate) async fn do_step(&mut self, step_type: DebuggerStepType, count: u64) -> Result<()> {
        let thread_id = match self.current_thread_id {
            Some(thread_id) => thread_id,
            None => {
                self.neovim_vadre_window
                    .lock()
                    .await
                    .log_msg(
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

            self.stopped_listener_tx = Some(tx);

            let resp = self.request_and_response(request.clone()).await?;

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

        self.request(request, None).await?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub(crate) async fn pause(&mut self, thread: Option<i64>) -> Result<()> {
        match thread {
            Some(thread_id) => {
                self.request(RequestArguments::pause(PauseArguments { thread_id }), None)
                    .await?;
                self.current_thread_id = thread;
            }
            None => {
                let mut thread = self.current_thread_id;

                // If we still haven't got a current thread yet find one from the thread list.
                if thread.is_none() {
                    let response_result = self
                        .request_and_response(RequestArguments::threads(None))
                        .await?;

                    if let ResponseResult::Success { ref body } = response_result {
                        if let ResponseBody::threads(threads_body) = body {
                            thread = threads_body.threads.iter().next().map(|thread| thread.id);
                        }
                    }
                }

                let thread_id = thread.ok_or_else(|| {
                    anyhow!(
                        "No thread specified, no currently active thread and couldn't find a thread, aborting interrupt"
                    )
                })?;

                self.request_and_response(RequestArguments::pause(PauseArguments { thread_id }))
                    .await?;
            }
        }

        Ok(())
    }

    pub(crate) fn subscribe_debugger(&self) -> Result<broadcast::Receiver<ProtocolMessage>> {
        self.processor.subscribe_debugger()
    }

    /// Handle a request from Debugger
    #[tracing::instrument(skip(self))]
    pub(crate) async fn handle_request(
        &mut self,
        request_id: u32,
        args: RequestArguments,
    ) -> Result<()> {
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

                match self.terminal_spawned_tx.take() {
                    Some(x) => {
                        if let Err(_) = x.send(()) {
                            bail!("Couldn't send terminal_spawned_tx");
                        }
                    }
                    None => {}
                };

                Ok(())
            }
            _ => unreachable!(),
        }
    }

    #[tracing::instrument(skip(event))]
    pub(crate) async fn handle_event(&mut self, event: EventBody) -> Result<()> {
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

    pub(crate) async fn handle_output_window_enter(&mut self) -> Result<()> {
        let current_output_window_type = self
            .neovim_vadre_window
            .lock()
            .await
            .get_output_window_type()
            .await?;

        let current_line = self
            .neovim_vadre_window
            .lock()
            .await
            .get_current_line()
            .await?;

        if current_output_window_type == VadreBufferType::CallStack {
            let mut split = current_line.split(" - ");

            if let Some(thread_id) = split.next() {
                match thread_id.parse::<i64>() {
                    Ok(thread_id) => {
                        self.current_thread_id = Some(thread_id);
                        self.display_output_info().await?;
                    }
                    Err(_) => {}
                }
            }
        }

        Ok(())
    }

    pub(crate) async fn handle_output_window_space(&mut self) -> Result<()> {
        let current_output_window_type = self
            .neovim_vadre_window
            .lock()
            .await
            .get_output_window_type()
            .await?;

        let current_line = self
            .neovim_vadre_window
            .lock()
            .await
            .get_current_line()
            .await?;

        if current_output_window_type == VadreBufferType::CallStack {
            let mut split = current_line.split(" - ");

            if let Some(thread_id) = split.next() {
                match thread_id.parse::<i64>() {
                    Ok(thread_id) => {
                        self.pause(Some(thread_id)).await?;
                        if !self.stack_expanded_threads.contains(&thread_id) {
                            self.stack_expanded_threads.insert(thread_id);
                        } else {
                            self.stack_expanded_threads.remove(&thread_id);
                        }
                        self.display_output_info().await?;
                    }
                    Err(_) => {}
                }
            }
        }

        Ok(())
    }

    #[tracing::instrument]
    pub(crate) async fn display_output_info(&mut self) -> Result<()> {
        let mut call_stack_buffer_content = Vec::new();
        let current_thread_id = self.current_thread_id;

        let current_output_window_type = self
            .neovim_vadre_window
            .lock()
            .await
            .get_output_window_type()
            .await?;

        tracing::trace!(
            "Displaying output window type: {:?}",
            current_output_window_type
        );

        if current_output_window_type == VadreBufferType::CallStack
            || current_output_window_type == VadreBufferType::Variables
        {
            let response_result = self
                .request_and_response(RequestArguments::threads(None))
                .await?;

            if let ResponseResult::Success { body } = response_result {
                if let ResponseBody::threads(threads_body) = body {
                    for thread in threads_body.threads {
                        let thread_id = thread.id;
                        let thread_name = &thread.name;

                        if current_thread_id == Some(thread_id) {
                            call_stack_buffer_content
                                .push(format!("{} - `{}` (*)", thread_id, thread_name));
                        } else {
                            call_stack_buffer_content
                                .push(format!("{} - `{}`", thread_id, thread_name));
                        }

                        if self.stack_expanded_threads.contains(&thread_id) {
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

                                    let top_frame = match frames.get(0) {
                                        Some(frame) => frame,
                                        None => {
                                            call_stack_buffer_content
                                                .push(format!("  (no frames)"));
                                            continue;
                                        }
                                    };
                                    let frame_id = top_frame.id;
                                    self.current_frame_id = Some(frame_id);

                                    if current_output_window_type == VadreBufferType::Variables {
                                        self.process_variables(frame_id).await?;
                                    }

                                    if current_output_window_type == VadreBufferType::CallStack {
                                        for frame in frames {
                                            call_stack_buffer_content
                                                .push(format!("+ {}", frame.name));

                                            let line_number = frame.line;
                                            let source =
                                                frame.source.as_ref().ok_or_else(|| {
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
                            // } else if current_output_window_type == VadreBufferType::CallStack {
                            //     let stack_trace_response = self
                            //         .request_and_response(RequestArguments::stackTrace(
                            //             StackTraceArguments {
                            //                 thread_id,
                            //                 format: None,
                            //                 levels: None,
                            //                 start_frame: None,
                            //             },
                            //         ))
                            //         .await?;

                            //     if let ResponseResult::Success { body } = stack_trace_response {
                            //         if let ResponseBody::stackTrace(stack_trace_body) = body {
                            //             let frame_name = &stack_trace_body
                            //                 .stack_frames
                            //                 .get(0)
                            //                 .ok_or_else(|| anyhow!("Coudln't find first stack frame as expected from stack trace response: {:?}", stack_trace_body.stack_frames))?
                            //                 .name;

                            //             call_stack_buffer_content
                            //                 .push(format!("{} - {}", thread_name, frame_name));
                            //         } else {
                            //             call_stack_buffer_content.push(format!("{}", thread_name));
                            //         }
                            //     }
                        }
                    }
                }
            }

            self.neovim_vadre_window
                .lock()
                .await
                .set_call_stack_buffer(call_stack_buffer_content)
                .await?;
        }

        Ok(())
    }

    pub(crate) async fn stop(&mut self) -> Result<()> {
        self.processor.stop().await?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn set_breakpoints(&mut self, file_path: String) -> Result<()> {
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

        tracing::trace!("Got response result: {:?}", response_result);
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

                    if let Some(actual_line_number) = breakpoint_response.line {
                        self.breakpoints.set_breakpoint_resolved(
                            file_path.clone(),
                            source_line_number,
                            actual_line_number,
                            breakpoint_id.to_string(),
                        )?;
                    }

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

    #[tracing::instrument]
    async fn handle_event_stopped(&mut self, stopped_event: StoppedEventBody) -> Result<()> {
        self.current_thread_id = stopped_event.thread_id;
        tracing::trace!("HERE1");
        if let Some(thread_id) = stopped_event.thread_id {
            self.stack_expanded_threads.insert(thread_id);
        }

        tracing::trace!("HERE2");
        if let Some(listener_tx) = self.stopped_listener_tx.take() {
            // If we're here we're about to do more stepping so no need to do more
            tracing::trace!("HERE3");
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

        tracing::trace!("HERE4");
        self.display_output_info().await?;

        tracing::trace!("HERE5");
        if let Some(thread_id) = stopped_event.thread_id {
            tracing::trace!("HERE6");
            if let Err(e) = self.process_stopped(thread_id).await {
                tracing::trace!("HERE7");
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

        if let Some(actual_line_number) = breakpoint_event.breakpoint.line {
            self.breakpoints.set_breakpoint_resolved(
                file_path.clone(),
                source_line_number,
                actual_line_number,
                breakpoint_id.to_string(),
            )?;
        }

        if breakpoint_is_enabled {
            self.breakpoints
                .set_breakpoint_enabled(file_path.clone(), source_line_number)?;
        }

        self.process_breakpoints_output().await?;

        Ok(())
    }

    fn breakpoint_is_enabled(&self, breakpoint: &Breakpoint) -> Result<bool> {
        if breakpoint.verified {
            return Ok(true);
        }

        // OK CodeLLDB, over to your awfulness.
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

    #[tracing::instrument]
    async fn process_stopped(&self, thread_id: i64) -> Result<()> {
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
                            self.neovim_vadre_window
                                .lock()
                                .await
                                .set_code_buffer(
                                    CodeBufferContent::Content(source_reference_body.content),
                                    line_number,
                                    &format!("Disassembled Code {}", source_reference_id),
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

    async fn request(
        &self,
        request_args: RequestArguments,
        tx: Option<oneshot::Sender<Response>>,
    ) -> Result<()> {
        self.processor.request(request_args, tx).await
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
