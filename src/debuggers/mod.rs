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
use dap::{
    protocol::{
        self as dap_protocol, BreakpointEventBody, ContinueArguments, DAPCodec, DecoderResult,
        DisconnectArguments, Either, EventBody, InitializeRequestArguments, NextArguments,
        ProtocolMessage, ProtocolMessageType, RequestArguments, Response, ResponseBody,
        ResponseResult, RunInTerminalResponseBody, ScopesArguments, SetBreakpointsArguments,
        SetExceptionBreakpointsArguments, SetFunctionBreakpointsArguments, Source, SourceArguments,
        SourceBreakpoint, StackTraceArguments, StepInArguments, StoppedEventBody,
        VariablesArguments,
    },
    shared as dap_shared,
};

use anyhow::{anyhow, bail, Result};
use futures::{prelude::*, StreamExt};
use nvim_rs::{compat::tokio::Compat, Neovim};
use tokio::{
    io::{AsyncBufReadExt, BufReader, Stdout},
    net::TcpStream,
    process::{Child, Command},
    sync::{mpsc, oneshot, Mutex},
    time::{sleep, timeout},
};
use tokio_util::codec::Decoder;

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
    async fn download_plugin(&mut self) -> Result<()> {
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
    fn get_binary_name(&self) -> Result<String> {
        match self {
            DebuggerType::CodeLLDB(debugger) => debugger.get_binary_name(),
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

#[derive(Clone, Debug)]
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

    // Allows us to send messages to the debugger
    debugger_sender_tx:
        mpsc::Sender<(ProtocolMessageType, Option<oneshot::Sender<ResponseResult>>)>,

    // Following should be empty most of the time and will be taken by the tcp_connection and used
    // there and only there.
    //
    // We steal it for performance reasons, rather than have an unnecessary mutex on the
    // debugger_sender_tx part if this were created when needed. This just gets stolen once and we
    // never use the mutex again.
    debugger_sender_rx:
        Option<mpsc::Receiver<(ProtocolMessageType, Option<oneshot::Sender<ResponseResult>>)>>,

    // Register if you're interested in listening for notifications about stopped events
    stopped_listener_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,

    data: Arc<Mutex<DebuggerData>>,
}

impl Debugger {
    pub(crate) fn new(
        id: usize,
        debugger_type: DebuggerType,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    ) -> Self {
        let (debugger_sender_tx, debugger_sender_rx) = mpsc::channel(1);

        let data = Arc::new(Mutex::new(DebuggerData {
            current_thread_id: None,
            current_frame_id: None,
            breakpoints: Breakpoints::new(),
        }));

        Self {
            id,
            debugger_type,
            neovim_vadre_window,
            process: Arc::new(Mutex::new(None)),

            debugger_sender_tx,
            debugger_sender_rx: Some(debugger_sender_rx),

            stopped_listener_tx: Arc::new(Mutex::new(None)),

            data,
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
        request_timeout: Duration,
        existing_debugger_port: Option<String>,
    ) -> Result<()> {
        self.neovim_vadre_window.lock().await.create_ui().await?;

        let debug_program_str = command.clone() + " " + &command_args.join(" ");

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

        let (config_done_tx, config_done_rx) = oneshot::channel();
        self.tcp_connect_and_handle(port, config_done_tx, request_timeout, debug_program_str)
            .await?;
        self.init_process(
            command,
            command_args,
            environment_variables,
            pending_breakpoints,
            config_done_rx,
            request_timeout,
        )
        .await?;
        self.log_msg(VadreLogLevel::INFO, "Debugger launched and setup")
            .await?;

        Ok(())
    }

    #[must_use]
    #[tracing::instrument(skip(self, port))]
    async fn launch(&mut self, port: u16) -> Result<()> {
        let msg = format!("Launching {}", self.debugger_type.get_debugger_type_name());
        self.log_msg(VadreLogLevel::DEBUG, &msg).await?;

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

        self.log_msg(VadreLogLevel::DEBUG, "Process spawned".into())
            .await?;

        Ok(())
    }

    /// Handle the TCP connection and setup the frame decoding/encoding and handling
    #[must_use]
    #[tracing::instrument(skip(self, port, config_done_tx))]
    async fn tcp_connect_and_handle(
        &mut self,
        port: u16,
        config_done_tx: oneshot::Sender<()>,
        request_timeout: Duration,
        debug_program_str: String,
    ) -> Result<()> {
        tracing::trace!("Connecting to port {}", port);

        let tcp_stream = self.do_tcp_connect(port).await?;
        let framed_stream = DAPCodec::new().framed(tcp_stream);

        self.handle_framed_stream(
            framed_stream,
            config_done_tx,
            request_timeout,
            debug_program_str,
        )
        .await
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
    #[tracing::instrument(skip(self, framed_stream, config_done_tx))]
    async fn handle_framed_stream<T>(
        &mut self,
        framed_stream: T,
        config_done_tx: oneshot::Sender<()>,
        request_timeout: Duration,
        debug_program_str: String,
    ) -> Result<()>
    where
        T: std::fmt::Debug
            + Stream<Item = Result<DecoderResult, io::Error>>
            + Sink<ProtocolMessage, Error = io::Error>
            + Send
            + Unpin
            + 'static,
    {
        let debugger_sender_tx = self.debugger_sender_tx.clone();
        let debugger_sender_rx = self
            .debugger_sender_rx
            .take()
            .ok_or_else(|| anyhow!("Should have a debugger_sender_rx to take, bad setup"))?;

        let neovim_vadre_window = self.neovim_vadre_window.clone();
        let data = self.data.clone();
        let stopped_listener_tx = self.stopped_listener_tx.clone();

        tokio::spawn(async move {
            let mut framed_stream = framed_stream;
            let mut config_done_tx = Some(config_done_tx);
            let mut debugger_sender_rx = debugger_sender_rx;

            let seq_ids = AtomicU32::new(1);
            let mut pending_responses: HashMap<u32, oneshot::Sender<ResponseResult>> =
                HashMap::new();

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
                            Some(Ok(decoder_result)) => {
                                match decoder_result {
                                    Ok(message) => match message.type_ {
                                        ProtocolMessageType::Request(request) => {
                                            match timeout(
                                                request_timeout,
                                                Debugger::handle_request(
                                                    *message.seq.first(),
                                                    request,
                                                    neovim_vadre_window.clone(),
                                                    debugger_sender_tx.clone(),
                                                    &debug_program_str,
                                                    &mut config_done_tx,
                                                ),
                                            )
                                            .await {
                                                Err(e) => {
                                                    report_error(format!("Debugger Request Timeout: {}", e), neovim_vadre_window.clone()).await?;
                                                }
                                                Ok(inner) => {
                                                    if let Err(e) = inner {
                                                        report_error(format!("Debugger Request Error: {}", e), neovim_vadre_window.clone()).await?;
                                                    }
                                                }
                                            }
                                        }

                                        ProtocolMessageType::Response(response) => {
                                            match timeout(
                                                request_timeout,
                                                Debugger::handle_response(
                                                    response,
                                                    neovim_vadre_window.clone(),
                                                    &mut pending_responses,
                                                ),
                                            )
                                            .await {
                                                Err(e) => {
                                                    report_error(format!("Debugger Response Error: {}", e), neovim_vadre_window.clone()).await?;
                                                },
                                                Ok(inner) => {
                                                    if let Err(e) = inner {
                                                        report_error(format!("Debugger Request Error: {}", e), neovim_vadre_window.clone()).await?;
                                                    }
                                                }
                                            }
                                        },

                                        ProtocolMessageType::Event(event) => {
                                            match timeout(
                                                request_timeout,
                                                Debugger::handle_event(
                                                    event,
                                                    neovim_vadre_window.clone(),
                                                    debugger_sender_tx.clone(),
                                                    stopped_listener_tx.clone(),
                                                    data.clone(),
                                                ),
                                            )
                                            .await {
                                                Err(e) => {
                                                    report_error(format!("Debugger Event Error: {}", e), neovim_vadre_window.clone()).await?;
                                                },
                                                Ok(inner) => {
                                                    if let Err(e) = inner {
                                                        report_error(format!("Debugger Request Error: {}", e), neovim_vadre_window.clone()).await?;
                                                    }
                                                }
                                            }
                                        }
                                    },

                                    Err(err) => {
                                        report_error(format!("An decoder error occurred: {:?}", err), neovim_vadre_window.clone()).await?;
                                    }
                                };
                            }
                            Some(Err(err)) => {
                                report_error(format!("Frame decoder error: {:?}", err), neovim_vadre_window.clone()).await?;
                            }
                            None => {
                                tracing::debug!("Client has disconnected");
                                break;
                            }
                       };
                    },
                    Some((message_type, sender)) = debugger_sender_rx.recv() => {
                        tracing::trace!("Got message to send to debugger {:?}", message_type);
                        let seq = seq_ids.fetch_add(1, Ordering::SeqCst);
                        let message = ProtocolMessage {
                            seq: Either::First(seq),
                            type_: message_type
                        };
                        match sender {
                            Some(sender) => { pending_responses.insert(seq, sender); },
                            None => {},
                        };
                        framed_stream.send(message).await?;
                    }
                };
            }

            Ok::<(), anyhow::Error>(())
        });

        self.log_msg(
            VadreLogLevel::DEBUG,
            "Process connection established".into(),
        )
        .await?;

        Ok(())
    }

    #[must_use]
    #[tracing::instrument(skip(self, config_done_rx))]
    async fn init_process(
        &self,
        command: String,
        command_args: Vec<String>,
        environment_variables: HashMap<String, String>,
        pending_breakpoints: &Breakpoints,
        config_done_rx: oneshot::Receiver<()>,
        request_timeout: Duration,
    ) -> Result<()> {
        dap_shared::do_send_request_and_await_response(
            RequestArguments::initialize(InitializeRequestArguments::new(
                self.debugger_type.get_debugger_type_name(),
            )),
            self.debugger_sender_tx.clone(),
        )
        .await?;

        let launch_request =
            self.debugger_type
                .get_launch_request(command, command_args, environment_variables)?;

        dap_shared::do_send_request(launch_request, self.debugger_sender_tx.clone(), None).await?;

        dap_shared::do_send_request(
            RequestArguments::setFunctionBreakpoints(SetFunctionBreakpointsArguments {
                breakpoints: vec![],
            }),
            self.debugger_sender_tx.clone(),
            None,
        )
        .await?;

        dap_shared::do_send_request(
            RequestArguments::setExceptionBreakpoints(SetExceptionBreakpointsArguments {
                filters: vec![],
                exception_options: None,
                filter_options: None,
            }),
            self.debugger_sender_tx.clone(),
            None,
        )
        .await?;

        for file_path in pending_breakpoints.get_all_files() {
            for line_number in pending_breakpoints
                .get_all_breakpoint_line_numbers_for_file(&file_path)?
                .keys()
            {
                self.data
                    .lock()
                    .await
                    .breakpoints
                    .add_pending_breakpoint(file_path.clone(), *line_number)?;
            }

            self.set_breakpoints(file_path).await?;
        }

        timeout(Duration::new(60, 0), config_done_rx).await??;

        dap_shared::do_send_request_and_await_response(
            RequestArguments::configurationDone(None),
            self.debugger_sender_tx.clone(),
        )
        .await?;

        Ok(())
    }

    #[must_use]
    pub(crate) async fn add_breakpoint(&self, file_path: String, line_number: i64) -> Result<()> {
        self.data
            .lock()
            .await
            .breakpoints
            .add_pending_breakpoint(file_path.clone(), line_number)?;

        self.set_breakpoints(file_path).await?;

        Ok(())
    }

    #[must_use]
    pub(crate) async fn remove_breakpoint(
        &self,
        file_path: String,
        line_number: i64,
    ) -> Result<()> {
        self.data
            .lock()
            .await
            .breakpoints
            .remove_breakpoint(file_path.clone(), line_number)?;

        self.set_breakpoints(file_path).await?;

        Ok(())
    }

    #[must_use]
    #[tracing::instrument(skip(self))]
    pub async fn set_breakpoints(&self, file_path: String) -> Result<()> {
        tracing::trace!("Debugger setting breakpoints in file {:?}", file_path);

        let line_numbers = self
            .data
            .lock()
            .await
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

        let response_result = dap_shared::do_send_request_and_await_response(
            RequestArguments::setBreakpoints(SetBreakpointsArguments {
                breakpoints: Some(breakpoints),
                lines: None,
                source,
                source_modified: Some(false),
            }),
            self.debugger_sender_tx.clone(),
        )
        .await?;

        tracing::trace!(
            "Debugger set breakpoints response_result {:?}",
            response_result
        );

        if let ResponseResult::Success { body } = response_result {
            if let ResponseBody::setBreakpoints(breakpoints_body) = body {
                let mut data_lock = self.data.lock().await;

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

                    let breakpoint_is_enabled =
                        Debugger::breakpoint_is_enabled(&breakpoint_response)?;

                    data_lock.breakpoints.set_breakpoint_resolved(
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
                        data_lock
                            .breakpoints
                            .set_breakpoint_enabled(file_path.clone(), source_line_number)?;
                    }
                }

                Debugger::process_breakpoints_output(
                    self.neovim_vadre_window.clone(),
                    &data_lock.breakpoints,
                )
                .await?;
            }
        }

        Ok(())
    }

    #[must_use]
    #[tracing::instrument(skip(self))]
    pub(crate) async fn do_step(&self, step_type: DebuggerStepType, count: u64) -> Result<()> {
        let thread_id = {
            let data = self.data.lock().await;

            data.current_thread_id.clone()
        };

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

            *self.stopped_listener_tx.lock().await = Some(tx);

            let resp = dap_shared::do_send_request_and_await_response(
                request.clone(),
                self.debugger_sender_tx.clone(),
            )
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

        dap_shared::do_send_request(request, self.debugger_sender_tx.clone(), None).await?;

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
                Debugger::process_output_info(
                    self.debugger_sender_tx.clone(),
                    self.neovim_vadre_window.clone(),
                    self.data.clone(),
                )
                .await?;
            }
            _ => {}
        };

        Ok(())
    }

    #[must_use]
    pub(crate) async fn stop(&self) -> Result<()> {
        if let Some(child) = self.process.lock().await.as_mut() {
            let request = RequestArguments::disconnect(DisconnectArguments {
                restart: Some(false),
                suspend_debuggee: None,
                terminate_debuggee: Some(true),
            });

            let resp = dap_shared::do_send_request_and_await_response(
                request.clone(),
                self.debugger_sender_tx.clone(),
            )
            .await?;

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

    #[must_use]
    pub(crate) async fn log_msg(&self, level: VadreLogLevel, msg: &str) -> Result<()> {
        self.neovim_vadre_window
            .lock()
            .await
            .log_msg(level, msg)
            .await
    }

    // These functions going forward are useful as they don't have a `self` parameter and thus can
    // be used by tokio spawned async functions, though more parameters need passing in to be
    // useful of course.

    /// Handle a request from Debugger
    #[must_use]
    #[tracing::instrument(skip(args, neovim_vadre_window, debugger_sender_tx, config_done_tx))]
    async fn handle_request(
        request_id: u32,
        args: RequestArguments,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        debugger_sender_tx: mpsc::Sender<(
            ProtocolMessageType,
            Option<oneshot::Sender<ResponseResult>>,
        )>,
        debug_program_str: &str,
        config_done_tx: &mut Option<oneshot::Sender<()>>,
    ) -> Result<()> {
        // Debugger is requesting something from us, currently only runTerminal should be received
        match args {
            RequestArguments::runInTerminal(args) => {
                neovim_vadre_window
                    .lock()
                    .await
                    .log_msg(
                        VadreLogLevel::INFO,
                        "Spawning terminal to communicate with program",
                    )
                    .await?;

                neovim_vadre_window
                    .lock()
                    .await
                    .spawn_terminal_command(
                        args.args
                            .into_iter()
                            .map(|x| format!(r#""{}""#, x))
                            .collect::<Vec<String>>()
                            .join(" "),
                        Some(debug_program_str),
                    )
                    .await?;

                let response = ProtocolMessageType::Response(Response {
                    request_seq: Either::First(request_id),
                    success: true,
                    result: ResponseResult::Success {
                        body: ResponseBody::runInTerminal(RunInTerminalResponseBody {
                            process_id: None,
                            shell_process_id: None,
                        }),
                    },
                });

                // One counterexample where we send a response, otherwise this should
                // always use do_send_request to do a send on `debugger_sender_tx`.
                debugger_sender_tx.clone().send((response, None)).await?;

                match config_done_tx.take() {
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
    #[tracing::instrument(skip(response, neovim_vadre_window, pending_responses))]
    async fn handle_response(
        response: Response,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        pending_responses: &mut HashMap<u32, oneshot::Sender<ResponseResult>>,
    ) -> Result<()> {
        let request_id = response.request_seq.first();

        match response.success {
            true => {
                if let Some(sender) = pending_responses.remove(&request_id) {
                    if let Err(e) = sender.send(response.result) {
                        tracing::error!("Couldn't send response to request: {:?}", e);
                        neovim_vadre_window
                            .lock()
                            .await
                            .log_msg(
                                VadreLogLevel::WARN,
                                &format!("Couldn't send response to request: {:?}", e),
                            )
                            .await?;
                    }
                    tracing::trace!("Sent JSON response to request");
                    return Ok(());
                }
            }
            false => {
                tracing::error!("Unhandled unsuccessful response: {:?}", response);
                neovim_vadre_window
                    .lock()
                    .await
                    .log_msg(
                        VadreLogLevel::WARN,
                        &format!("Unhandled unsuccessful response: {:?}", response),
                    )
                    .await?;
            }
        };

        Ok(())
    }

    #[must_use]
    #[tracing::instrument(skip(
        event,
        neovim_vadre_window,
        debugger_sender_tx,
        stopped_listener_tx,
        data,
    ))]
    async fn handle_event(
        event: EventBody,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        debugger_sender_tx: mpsc::Sender<(
            ProtocolMessageType,
            Option<oneshot::Sender<ResponseResult>>,
        )>,
        stopped_listener_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
        data: Arc<Mutex<DebuggerData>>,
    ) -> Result<()> {
        tracing::trace!("Processing event: {:?}", event);
        match event {
            EventBody::initialized(_) => Ok(()),
            EventBody::output(output) => {
                neovim_vadre_window
                    .lock()
                    .await
                    .log_msg(
                        VadreLogLevel::INFO,
                        &format!("Debugger: {}", output.output.trim_end()),
                    )
                    .await
            }
            EventBody::stopped(stopped_event) => {
                Debugger::handle_event_stopped(
                    stopped_event,
                    neovim_vadre_window,
                    debugger_sender_tx,
                    stopped_listener_tx,
                    data,
                )
                .await
            }
            EventBody::continued(_) => Ok(()),
            EventBody::breakpoint(breakpoint_event) => {
                Debugger::handle_event_breakpoint(breakpoint_event, neovim_vadre_window, data).await
            }
            _ => {
                tracing::trace!("Got unhandled event {:?}", event);
                Ok(())
            }
        }
    }

    #[must_use]
    #[tracing::instrument(skip(
        stopped_event,
        neovim_vadre_window,
        debugger_sender_tx,
        stopped_listener_tx,
        data,
    ))]
    async fn handle_event_stopped(
        stopped_event: StoppedEventBody,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        debugger_sender_tx: mpsc::Sender<(
            ProtocolMessageType,
            Option<oneshot::Sender<ResponseResult>>,
        )>,
        stopped_listener_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
        data: Arc<Mutex<DebuggerData>>,
    ) -> Result<()> {
        data.lock().await.current_thread_id = stopped_event.thread_id;

        if let Some(listener_tx) = stopped_listener_tx.lock().await.take() {
            // If we're here we're about to do more stepping so no need to do more
            if let Err(_) = listener_tx.send(()) {
                tracing::error!("Couldn't send stopped_listener_tx");
                neovim_vadre_window
                    .lock()
                    .await
                    .log_msg(VadreLogLevel::WARN, "Couldn't send stopped_listener_tx")
                    .await?;
            }
            return Ok(());
        }

        let neovim_vadre_window = neovim_vadre_window.clone();

        tokio::spawn(async move {
            Debugger::process_output_info(
                debugger_sender_tx.clone(),
                neovim_vadre_window.clone(),
                data,
            )
            .await?;

            if let Some(thread_id) = stopped_event.thread_id {
                if let Err(e) = Debugger::process_stopped(
                    thread_id,
                    debugger_sender_tx,
                    neovim_vadre_window.clone(),
                )
                .await
                {
                    neovim_vadre_window
                        .lock()
                        .await
                        .log_msg(
                            VadreLogLevel::WARN,
                            &format!("An error occurred while displaying code pointer: {}", e),
                        )
                        .await?;
                }
            }

            Ok::<(), anyhow::Error>(())
        });

        Ok(())
    }

    #[tracing::instrument(skip(breakpoint_event, neovim_vadre_window, data,))]
    async fn handle_event_breakpoint(
        breakpoint_event: BreakpointEventBody,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        data: Arc<Mutex<DebuggerData>>,
    ) -> Result<()> {
        let breakpoint_id = breakpoint_event
            .breakpoint
            .id
            .ok_or_else(|| anyhow!("Couldn't find ID from event: {:?}", breakpoint_event))?;

        let breakpoint_is_enabled = Debugger::breakpoint_is_enabled(&breakpoint_event.breakpoint)?;

        let mut data_lock = data.lock().await;

        // Do we need to poll/sleep here to wait for the breakpoint to be resolved?
        let (file_path, source_line_number) = data_lock
            .breakpoints
            .get_breakpoint_for_id(&breakpoint_id)
            .ok_or_else(|| anyhow!("Can't find breakpoint for id {}", breakpoint_id))?;

        data_lock.breakpoints.set_breakpoint_resolved(
            file_path.clone(),
            source_line_number,
            breakpoint_event
                .breakpoint
                .line
                .ok_or_else(|| anyhow!("Couldn't find line from event: {:?}", breakpoint_event))?,
            breakpoint_id.to_string(),
        )?;

        if breakpoint_is_enabled {
            data_lock
                .breakpoints
                .set_breakpoint_enabled(file_path.clone(), source_line_number)?;
        }

        Debugger::process_breakpoints_output(neovim_vadre_window.clone(), &data_lock.breakpoints)
            .await?;

        Ok(())
    }

    fn breakpoint_is_enabled(breakpoint: &dap_protocol::Breakpoint) -> Result<bool> {
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
    #[tracing::instrument(skip(thread_id, debugger_sender_tx, neovim_vadre_window))]
    async fn process_stopped(
        thread_id: i64,
        debugger_sender_tx: mpsc::Sender<(
            ProtocolMessageType,
            Option<oneshot::Sender<ResponseResult>>,
        )>,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    ) -> Result<()> {
        tracing::debug!("Thread id {} stopped", thread_id);

        let stack_trace_response = dap_shared::do_send_request_and_await_response(
            RequestArguments::stackTrace(StackTraceArguments {
                thread_id,
                format: None,
                levels: Some(1),
                start_frame: Some(0), // Should be optional but debugpy breaks without this
            }),
            debugger_sender_tx.clone(),
        )
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
                    neovim_vadre_window
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
                    let source_reference_response = dap_shared::do_send_request_and_await_response(
                        RequestArguments::source(SourceArguments {
                            source: Some(source.clone()),
                            source_reference: source_reference_id,
                        }),
                        debugger_sender_tx.clone(),
                    )
                    .await?;
                    if let ResponseResult::Success { body } = source_reference_response {
                        if let ResponseBody::source(source_reference_body) = body {
                            tracing::trace!("source reference {:?}", source_reference_body);

                            neovim_vadre_window
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

    #[tracing::instrument(skip(debugger_sender_tx, neovim_vadre_window, data))]
    async fn process_output_info(
        debugger_sender_tx: mpsc::Sender<(
            ProtocolMessageType,
            Option<oneshot::Sender<ResponseResult>>,
        )>,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        data: Arc<Mutex<DebuggerData>>,
    ) -> Result<()> {
        tracing::debug!("Getting thread information");

        let mut call_stack_buffer_content = Vec::new();
        let current_thread_id = data.lock().await.current_thread_id;

        let current_output_window_type = neovim_vadre_window
            .lock()
            .await
            .get_output_window_type()
            .await?;

        let response_result = dap_shared::do_send_request_and_await_response(
            RequestArguments::threads(None),
            debugger_sender_tx.clone(),
        )
        .await?;

        if let ResponseResult::Success { body } = response_result {
            if let ResponseBody::threads(threads_body) = body {
                for thread in threads_body.threads {
                    let thread_id = thread.id;
                    let thread_name = thread.name;

                    if current_thread_id == Some(thread_id) {
                        call_stack_buffer_content.push(format!("{} (*)", thread_name));

                        let stack_trace_response = dap_shared::do_send_request_and_await_response(
                            RequestArguments::stackTrace(StackTraceArguments {
                                thread_id,
                                format: None,
                                levels: None,
                                start_frame: None,
                            }),
                            debugger_sender_tx.clone(),
                        )
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
                                data.lock().await.current_frame_id = Some(frame_id);

                                if current_output_window_type == VadreBufferType::Variables {
                                    Debugger::process_variables(
                                        frame_id,
                                        debugger_sender_tx.clone(),
                                        neovim_vadre_window.clone(),
                                    )
                                    .await?;
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
                        let stack_trace_response = dap_shared::do_send_request_and_await_response(
                            RequestArguments::stackTrace(StackTraceArguments {
                                thread_id,
                                format: None,
                                levels: None,
                                start_frame: None,
                            }),
                            debugger_sender_tx.clone(),
                        )
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
            neovim_vadre_window
                .lock()
                .await
                .set_call_stack_buffer(call_stack_buffer_content)
                .await?;
        }

        Ok(())
    }

    #[tracing::instrument(skip(debugger_sender_tx, neovim_vadre_window,))]
    async fn process_variables(
        frame_id: i64,
        debugger_sender_tx: mpsc::Sender<(
            ProtocolMessageType,
            Option<oneshot::Sender<ResponseResult>>,
        )>,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    ) -> Result<()> {
        tracing::debug!("Getting variable information");

        let mut variable_content = Vec::new();

        let scopes_response_result = dap_shared::do_send_request_and_await_response(
            RequestArguments::scopes(ScopesArguments { frame_id }),
            debugger_sender_tx.clone(),
        )
        .await?;

        if let ResponseResult::Success { body } = scopes_response_result {
            if let ResponseBody::scopes(scopes_body) = body {
                for scope in scopes_body.scopes {
                    variable_content.push(format!("{}:", scope.name));

                    let variables_response_result = dap_shared::do_send_request_and_await_response(
                        RequestArguments::variables(VariablesArguments {
                            count: None,
                            filter: None,
                            format: None,
                            start: None,
                            variables_reference: scope.variables_reference,
                        }),
                        debugger_sender_tx.clone(),
                    )
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

                neovim_vadre_window
                    .lock()
                    .await
                    .set_variables_buffer(variable_content)
                    .await?;
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip(neovim_vadre_window))]
    async fn process_breakpoints_output(
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        breakpoints: &Breakpoints,
    ) -> Result<()> {
        let mut breakpoints_buffer_content = Vec::new();

        for file in breakpoints.get_all_files() {
            breakpoints_buffer_content.push(format!("{}:", file));

            for (source_line_number, breakpoint) in
                breakpoints.get_all_breakpoint_line_numbers_for_file(&file)?
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

        neovim_vadre_window
            .lock()
            .await
            .set_breakpoints_buffer(breakpoints_buffer_content)
            .await?;
        Ok(())
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

#[must_use]
pub(crate) fn new_debugger(
    id: usize,
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
        debugger_type,
        neovim_vadre_window,
    )))
}
