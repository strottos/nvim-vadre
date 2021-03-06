mod dap_protocol;

use std::{
    collections::{HashMap, HashSet},
    env::{self, consts::EXE_SUFFIX},
    fmt::Debug,
    path::Path,
    process::Stdio,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use crate::{
    debuggers::dap_protocol::SourceArguments,
    neovim::{CodeBufferContent, NeovimVadreWindow, VadreLogLevel},
    util::{self, get_debuggers_dir, log_err, log_ret_err, ret_err},
};
use dap_protocol::{
    BreakpointEventBody, ContinuedEventBody, DAPCodec, EventBody, InitializeRequestArguments,
    ProtocolMessage, ProtocolMessageType, RequestArguments, Response, ResponseBody, ResponseResult,
    RunInTerminalResponseBody, ScopesArguments, SetBreakpointsArguments,
    SetExceptionBreakpointsArguments, SetFunctionBreakpointsArguments, Source, SourceBreakpoint,
    StackTraceArguments, StoppedEventBody, VariablesArguments,
};

use anyhow::{bail, Result};
use futures::prelude::*;
use nvim_rs::{compat::tokio::Compat, Neovim};
use reqwest::Url;
use tokio::{
    io::{AsyncBufReadExt, BufReader, Stdout},
    net::TcpStream,
    process::{Child, Command},
    sync::{mpsc, oneshot, Mutex},
    time::{sleep, timeout},
};
use tokio_util::codec::Decoder;
use tracing::{debug, error};

use self::dap_protocol::{ContinueArguments, NextArguments, StepInArguments};

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

#[derive(Clone, Debug)]
pub enum Debugger {
    CodeLLDB(CodeLLDBDebugger),
}

impl Debugger {
    #[tracing::instrument(skip(self))]
    pub async fn setup(
        &mut self,
        pending_breakpoints: &HashMap<String, HashSet<i64>>,
        codelldb_port: Option<u16>,
    ) -> Result<()> {
        match self {
            Debugger::CodeLLDB(debugger) => {
                debugger.setup(pending_breakpoints, codelldb_port).await
            }
        }
    }

    #[tracing::instrument(skip(self))]
    pub fn neovim_vadre_window(&self) -> Arc<Mutex<NeovimVadreWindow>> {
        match self {
            Debugger::CodeLLDB(debugger) => debugger.neovim_vadre_window.clone(),
        }
    }

    #[tracing::instrument(skip(self))]
    pub async fn set_source_breakpoints(
        &self,
        file_path: String,
        line_numbers: &HashSet<i64>,
    ) -> Result<()> {
        match self {
            Debugger::CodeLLDB(debugger) => debugger.set_breakpoints(file_path, line_numbers).await,
        }
    }

    #[tracing::instrument(skip(self))]
    pub async fn do_step(&self, step_type: DebuggerStepType) -> Result<()> {
        match self {
            Debugger::CodeLLDB(debugger) => debugger.do_step(step_type).await,
        }
    }

    #[tracing::instrument(skip(self))]
    pub async fn print_variable(&self, variable_name: &str) -> Result<()> {
        match self {
            Debugger::CodeLLDB(debugger) => debugger.print_variable(variable_name).await,
        }
    }

    #[tracing::instrument(skip(self))]
    pub async fn change_output_window(&self, ascending: bool) -> Result<()> {
        match self {
            Debugger::CodeLLDB(debugger) => debugger.change_output_window(ascending).await,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct CodeLLDBDebuggerData {
    current_thread_id: Option<i64>,
    current_frame_id: Option<i64>,
    breakpoints: Breakpoints,
}

#[derive(Clone)]
pub struct CodeLLDBDebugger {
    id: usize,
    command: String,
    command_args: Vec<String>,
    pub neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    process: Option<Arc<Child>>,

    debugger_sender_tx:
        mpsc::Sender<(ProtocolMessageType, Option<oneshot::Sender<ResponseResult>>)>,
    // Following should be empty most of the time and will be taken by the tcp_connection.
    //
    // We use a mutex here and steal it once for performance reasons, rather than have an
    // unnecessary mutex on the debugger_sender_tx part if this were created when needed. This just
    // gets stolen once and we never use the mutex again.
    debugger_sender_rx: Arc<
        Mutex<
            Option<mpsc::Receiver<(ProtocolMessageType, Option<oneshot::Sender<ResponseResult>>)>>,
        >,
    >,

    pending_outgoing_requests: Arc<Mutex<HashMap<u32, oneshot::Sender<ResponseResult>>>>,

    codelldb_data: Arc<Mutex<CodeLLDBDebuggerData>>,
}

impl CodeLLDBDebugger {
    #[tracing::instrument(skip(neovim))]
    pub fn new(
        id: usize,
        command: String,
        command_args: Vec<String>,
        neovim: Neovim<Compat<Stdout>>,
    ) -> Self {
        let (debugger_sender_tx, debugger_sender_rx) = mpsc::channel(1);

        CodeLLDBDebugger {
            id,
            command,
            command_args,
            neovim_vadre_window: Arc::new(Mutex::new(NeovimVadreWindow::new(neovim, id))),
            process: None,

            debugger_sender_tx,
            debugger_sender_rx: Arc::new(Mutex::new(Some(debugger_sender_rx))),

            pending_outgoing_requests: Arc::new(Mutex::new(HashMap::new())),

            codelldb_data: Arc::new(Mutex::new(CodeLLDBDebuggerData::default())),
        }
    }

    #[tracing::instrument(skip(self, pending_breakpoints))]
    pub async fn setup(
        &mut self,
        pending_breakpoints: &HashMap<String, HashSet<i64>>,
        codelldb_port: Option<u16>,
    ) -> Result<()> {
        log_ret_err!(
            self.neovim_vadre_window.lock().await.create_ui().await,
            self.neovim_vadre_window,
            "Error setting up Vadre UI"
        );

        let port = codelldb_port.unwrap_or(util::get_unused_localhost_port());

        if codelldb_port.is_none() {
            log_ret_err!(
                self.launch(port).await,
                self.neovim_vadre_window,
                "Error launching process"
            );
        }

        let (config_done_tx, config_done_rx) = oneshot::channel();
        log_ret_err!(
            self.tcp_connect(port, config_done_tx).await,
            self.neovim_vadre_window,
            "Error creating TCP connection to process"
        );
        log_ret_err!(
            self.init_process(pending_breakpoints, config_done_rx).await,
            self.neovim_vadre_window,
            "Error initialising process"
        );
        ret_err!(
            self.neovim_vadre_window
                .lock()
                .await
                .log_msg(VadreLogLevel::INFO, "CodeLLDB launched and setup")
                .await
        );

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn set_breakpoints(
        &self,
        file_path: String,
        line_numbers: &HashSet<i64>,
    ) -> Result<()> {
        tracing::trace!(
            "CodeLLDB setting breakpoints in file {:?} on lines {:?}",
            file_path,
            line_numbers,
        );

        let file_name = Path::new(&file_path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let line_numbers = line_numbers.into_iter().map(|x| *x).collect::<Vec<i64>>();

        let breakpoints: Vec<SourceBreakpoint> = line_numbers
            .clone()
            .into_iter()
            .map(|x| SourceBreakpoint::new(x))
            .collect::<Vec<SourceBreakpoint>>();

        let source = Source::new_file(file_name.clone(), file_path.clone());

        let response_result = self
            .send_request_and_await_response(RequestArguments::setBreakpoints(
                SetBreakpointsArguments {
                    breakpoints: Some(breakpoints),
                    lines: None,
                    source,
                    source_modified: Some(false),
                },
            ))
            .await?;

        if let ResponseResult::Success { body } = response_result {
            if let ResponseBody::setBreakpoints(breakpoints_body) = body {
                let ids_left = breakpoints_body
                    .breakpoints
                    .iter()
                    .map(|x| x.id.unwrap())
                    .collect::<Vec<i64>>();

                tracing::trace!("1 - {:?}", self.codelldb_data.lock().await.breakpoints);

                self.codelldb_data
                    .lock()
                    .await
                    .breakpoints
                    .remove_file_ids(file_path.clone(), ids_left)?;

                tracing::trace!("2 - {:?}", self.codelldb_data.lock().await.breakpoints);

                let mut codelldb_data_lock = self.codelldb_data.lock().await;

                for (i, breakpoint_response) in breakpoints_body.breakpoints.into_iter().enumerate()
                {
                    let original_line_number = line_numbers.get(i).unwrap().clone();

                    let breakpoint_id = breakpoint_response.id.unwrap();

                    let message = breakpoint_response.message.unwrap();
                    let breakpoint_is_resolved = CodeLLDBDebugger::breakpoint_is_resolved(&message);

                    tracing::trace!("3 - {:?}", codelldb_data_lock.breakpoints);

                    codelldb_data_lock.breakpoints.add_breakpoint(
                        breakpoint_id,
                        file_path.clone(),
                        original_line_number,
                        breakpoint_response.line,
                        breakpoint_is_resolved,
                    )?;
                    tracing::trace!("4 - {:?}", codelldb_data_lock.breakpoints);
                }

                let breakpoints = codelldb_data_lock
                    .breakpoints
                    .get_all_breakpoints_for_file(&file_path);

                tracing::trace!("5 - {:?}", breakpoints);
                self.neovim_vadre_window
                    .lock()
                    .await
                    .toggle_breakpoint_in_buffer(&file_path, breakpoints)
                    .await?;
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn do_step(&self, step_type: DebuggerStepType) -> Result<()> {
        let codelldb_data = self.codelldb_data.lock().await;

        let request = match step_type {
            DebuggerStepType::Over => RequestArguments::next(NextArguments {
                granularity: None,
                single_thread: Some(false),
                thread_id: codelldb_data.current_thread_id.unwrap(),
            }),
            DebuggerStepType::In => RequestArguments::stepIn(StepInArguments {
                granularity: None,
                single_thread: Some(false),
                target_id: None,
                thread_id: codelldb_data.current_thread_id.unwrap(),
            }),
            DebuggerStepType::Continue => RequestArguments::continue_(ContinueArguments {
                single_thread: Some(false),
                thread_id: codelldb_data.current_thread_id.unwrap(),
            }),
        };

        self.send_request(request).await?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn print_variable(&self, variable_name: &str) -> Result<()> {
        // self.send_request(
        //     "evaluate",
        //     serde_json::json!({ "expression": &format!("/se frame evaluate {}", variable_name) }),
        // )
        // .await?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn change_output_window(&self, ascending: bool) -> Result<()> {
        self.neovim_vadre_window
            .lock()
            .await
            .change_output_window(ascending)
            .await
    }

    #[tracing::instrument(skip(self, port))]
    async fn launch(&mut self, port: u16) -> Result<()> {
        let msg = format!(
            "Launching process {:?} with lldb and args: {:?}",
            self.command, self.command_args,
        );
        self.neovim_vadre_window
            .lock()
            .await
            .log_msg(VadreLogLevel::DEBUG, &msg)
            .await?;

        self.download_plugin().await?;

        let mut path = get_debuggers_dir()?;
        path.push("codelldb");
        path.push("extension");
        path.push("adapter");
        path.push(format!("codelldb{}", EXE_SUFFIX));

        if !path.exists() {
            bail!("The binary for codelldb.exe doesn't exist, though it should by this point");
        }

        tracing::trace!("Spawning processs {:?}", path);

        let mut child = Command::new(path)
            .args(["--port", &port.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to spawn debugger");

        let stdout = child.stdout.take().expect("should have stdout");
        let stderr = child.stderr.take().expect("should have stderr");

        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Some(line) = reader.next_line().await.expect("can read stdout") {
                tracing::trace!("CodeLLDB stdout: {}", line);
            }
        });

        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Some(line) = reader.next_line().await.expect("can read stderr") {
                tracing::trace!("CodeLLDB stderr: {}", line);
            }
        });

        self.process = Some(Arc::new(child));

        self.neovim_vadre_window
            .lock()
            .await
            .log_msg(VadreLogLevel::DEBUG, "Process spawned".into())
            .await?;

        Ok(())
    }

    #[tracing::instrument(skip(self, port))]
    async fn tcp_connect(&mut self, port: u16, config_done_tx: oneshot::Sender<()>) -> Result<()> {
        tracing::trace!("Connecting to port {}", port);

        let neovim_vadre_window = self.neovim_vadre_window.clone();

        let tcp_stream = self.do_tcp_connect(port).await?;
        let framed_stream = DAPCodec::new().framed(tcp_stream);

        let debugger_sender_tx = self.debugger_sender_tx.clone();
        let debugger_sender_rx = self
            .debugger_sender_rx
            .lock()
            .await
            .take()
            .expect("Should have a debugger_sender_rx to take");

        let pending_outgoing_requests = self.pending_outgoing_requests.clone();
        let codelldb_data = self.codelldb_data.clone();

        let debug_program_str = self.command.clone() + &self.command_args.join(" ");

        tokio::spawn(async move {
            let mut framed_stream = framed_stream;
            let mut config_done_tx = Some(config_done_tx);
            let mut debugger_sender_rx = debugger_sender_rx;

            let seq_ids = AtomicU32::new(1);
            let pending_outgoing_requests = pending_outgoing_requests.clone();
            // ?? let pending_incoming_requests: HashMap<u32, oneshot::Sender<()>> = HashMap::new();

            loop {
                tracing::trace!(
                    "framed_stream {:?}, debugger_sender_rx {:?}",
                    framed_stream,
                    debugger_sender_rx
                );
                tokio::select! {
                    msg = framed_stream.next() => {
                        tracing::trace!("Got message {:?}", msg);
                        // Message from CodeLLDB
                        match msg {
                            Some(Ok(decoder_result)) => {
                                tracing::trace!("Message: {:?}", decoder_result);
                                match decoder_result {
                                    Ok(message) => match message.type_ {
                                        dap_protocol::ProtocolMessageType::Request(request) => {
                                            let config_done_tx = config_done_tx.take().unwrap();
                                            if let Err(e) = timeout(
                                                Duration::new(10, 0),
                                                CodeLLDBDebugger::handle_codelldb_request(
                                                    message.seq,
                                                    request,
                                                    neovim_vadre_window.clone(),
                                                    debugger_sender_tx.clone(),
                                                    Some(&debug_program_str),
                                                    Some(config_done_tx),
                                                ),
                                            )
                                            .await
                                            {
                                                let msg = format!("CodeLLDB Request Error: {}", e);
                                                tracing::error!("{}", msg);
                                                neovim_vadre_window
                                                    .lock()
                                                    .await
                                                    .log_msg(VadreLogLevel::WARN, &msg)
                                                    .await
                                                    .expect("Logging failed");
                                            }
                                        }

                                        dap_protocol::ProtocolMessageType::Response(response) => {
                                            // TODO: configurable timeout?
                                            if let Err(e) = timeout(
                                                Duration::new(10, 0),
                                                CodeLLDBDebugger::handle_codelldb_response(
                                                    response,
                                                    pending_outgoing_requests.clone(),
                                                ),
                                            )
                                            .await
                                            {
                                                let msg = format!("CodeLLDB Response Error: {}", e);
                                                tracing::error!("{}", msg);
                                                neovim_vadre_window
                                                    .lock()
                                                    .await
                                                    .log_msg(VadreLogLevel::WARN, &msg)
                                                    .await
                                                    .expect("Logging failed");
                                            }
                                        },

                                        dap_protocol::ProtocolMessageType::Event(event) => {
                                            // TODO: configurable timeout?
                                            if let Err(e) = timeout(
                                                Duration::new(10, 0),
                                                CodeLLDBDebugger::handle_codelldb_event(
                                                    event,
                                                    neovim_vadre_window.clone(),
                                                    debugger_sender_tx.clone(),
                                                    codelldb_data.clone(),
                                                ),
                                            )
                                            .await
                                            {
                                                let msg = format!("CodeLLDB Event Error: {}", e);
                                                tracing::error!("{}", msg);
                                                neovim_vadre_window
                                                    .lock()
                                                    .await
                                                    .log_msg(VadreLogLevel::WARN, &msg)
                                                    .await
                                                    .expect("Logging failed");
                                            }
                                        }
                                    },

                                    Err(err) => {
                                        error!("TODO: {:?}", err);
                                        todo!();
                                    }
                                };
                            }
                            Some(Err(err)) => {
                                error!("Frame decoder error: {}", err);
                                break;
                            }
                            None => {
                                debug!("Client has disconnected");
                                break;
                            }
                        };
                    },
                    Some((message_type, sender)) = debugger_sender_rx.recv() => {
                        tracing::trace!("Sending message 2 {:?}", message_type);
                        let seq = seq_ids.fetch_add(1, Ordering::SeqCst);
                        let message = ProtocolMessage {
                            seq,
                            type_: message_type
                        };
                        match sender {
                            Some(sender) => { pending_outgoing_requests.lock().await.insert(seq, sender); },
                            None => {},
                        };
                        framed_stream
                            .send(message)
                            .await
                            .expect("write should succeed");
                    }
                };
            }
            unreachable!();
        });

        self.neovim_vadre_window
            .lock()
            .await
            .log_msg(
                VadreLogLevel::DEBUG,
                "Process connection established".into(),
            )
            .await?;

        Ok(())
    }

    #[tracing::instrument(skip(self, port))]
    async fn do_tcp_connect(&mut self, port: u16) -> Result<TcpStream> {
        let number_attempts = 50;

        for _ in 1..number_attempts {
            let tcp_stream = TcpStream::connect(format!("127.0.0.1:{}", port)).await;
            match tcp_stream {
                Ok(x) => return Ok(x),
                Err(e) => {
                    tracing::trace!("Sleeping 100 after error: {}", e);
                    sleep(Duration::from_millis(100)).await;
                }
            };
        }

        bail!(
            "Couldn't connect to server after {} attempts, bailing",
            number_attempts
        );
    }

    #[tracing::instrument(skip(self, pending_breakpoints, config_done_rx))]
    async fn init_process(
        &self,
        pending_breakpoints: &HashMap<String, HashSet<i64>>,
        config_done_rx: oneshot::Receiver<()>,
    ) -> Result<()> {
        let res = self
            .send_request_and_await_response(RequestArguments::initialize(
                InitializeRequestArguments::new("CodeLLDB".to_string()),
            ))
            .await?;

        tracing::trace!("RES: {:?}", res);

        #[cfg(windows)]
        let program = dunce::canonicalize(&self.command)?;
        #[cfg(not(windows))]
        let program = std::fs::canonicalize(&self.command)?;

        self.send_request(RequestArguments::launch(dap_protocol::Either::Second(
            serde_json::json!({
                "args": self.command_args,
                "cargo": {},
                "cwd": env::current_dir()?,
                "env": {},
                "name": "lldb",
                "terminal": "integrated",
                "type": "lldb",
                "request": "launch",
                "program": program,
            }),
        )))
        .await?;

        self.send_request(RequestArguments::setFunctionBreakpoints(
            SetFunctionBreakpointsArguments {
                breakpoints: vec![],
            },
        ))
        .await?;

        self.send_request(RequestArguments::setExceptionBreakpoints(
            SetExceptionBreakpointsArguments {
                filters: vec![],
                exception_options: None,
                filter_options: None,
            },
        ))
        .await?;

        for breakpoint in pending_breakpoints {
            self.set_breakpoints(breakpoint.0.clone(), breakpoint.1)
                .await?;
        }

        timeout(Duration::new(10, 0), config_done_rx).await??;

        self.send_request_and_await_response(RequestArguments::configurationDone(None))
            .await?;

        Ok(())
    }

    #[tracing::instrument(skip(self, request))]
    async fn send_request(&self, request: RequestArguments) -> Result<()> {
        CodeLLDBDebugger::do_send_request(request, self.debugger_sender_tx.clone(), None).await
    }

    #[tracing::instrument(skip(self, request))]
    async fn send_request_and_await_response(
        &self,
        request: RequestArguments,
    ) -> Result<ResponseResult> {
        CodeLLDBDebugger::do_send_request_and_await_response(
            request,
            self.debugger_sender_tx.clone(),
        )
        .await
    }

    #[tracing::instrument(skip(self))]
    async fn download_plugin(&self) -> Result<()> {
        let mut path = get_debuggers_dir()?;
        path.push("codelldb");

        if !path.exists() {
            let (os, arch) = util::get_os_and_cpu_architecture();
            let version = "v1.7.0";

            self.neovim_vadre_window
                .lock()
                .await
                .log_msg(
                    VadreLogLevel::INFO,
                    &format!("Downloading and extracting {} plugin for {}", os, arch),
                )
                .await?;

            let url = Url::parse(&format!(
                "https://github.com/vadimcn/vscode-lldb/releases/download/{}\
                         /codelldb-{}-{}.vsix",
                version, arch, os
            ))?;

            CodeLLDBDebugger::download_extract_zip(path.as_path(), url).await?;
        }

        Ok(())
    }

    // These functions going forward are useful as they don't have a `self` parameter and thus can
    // be used by tokio spawned async functions, though more parameters need passing in to be
    // useful of course.
    /// Handle a request from CodeLLDB
    #[tracing::instrument(skip(args, neovim_vadre_window, debugger_sender_tx, config_done_tx))]
    async fn handle_codelldb_request(
        request_id: u32,
        args: RequestArguments,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        debugger_sender_tx: mpsc::Sender<(
            ProtocolMessageType,
            Option<oneshot::Sender<ResponseResult>>,
        )>,
        debug_program_str: Option<&str>,
        mut config_done_tx: Option<oneshot::Sender<()>>,
    ) -> Result<()> {
        // CodeLLDB is requesting something from us, currently only runTerminal should be received
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
                    .spawn_terminal_command(args.args.join(" "), debug_program_str)
                    .await?;

                let response = ProtocolMessageType::Response(Response {
                    request_seq: request_id,
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

                config_done_tx.take().unwrap().send(()).unwrap();

                Ok(())
            }
            _ => unreachable!(),
        }
    }

    #[tracing::instrument(skip(response, pending_outgoing_requests))]
    async fn handle_codelldb_response(
        response: Response,
        pending_outgoing_requests: Arc<Mutex<HashMap<u32, oneshot::Sender<ResponseResult>>>>,
    ) -> Result<()> {
        let request_id = response.request_seq;

        let mut pending_outgoing_requests = pending_outgoing_requests.lock().await;
        match response.success {
            true => {
                if let Some(sender) = pending_outgoing_requests.remove(&request_id) {
                    sender.send(response.result).unwrap();
                    tracing::trace!("Sent JSON response to request");
                    return Ok(());
                }
            }
            false => {
                tracing::trace!("Unhandled unsuccessful response");
            }
        };

        Ok(())
    }

    #[tracing::instrument(skip(event, neovim_vadre_window, debugger_sender_tx, codelldb_data,))]
    async fn handle_codelldb_event(
        event: EventBody,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        debugger_sender_tx: mpsc::Sender<(
            ProtocolMessageType,
            Option<oneshot::Sender<ResponseResult>>,
        )>,
        codelldb_data: Arc<Mutex<CodeLLDBDebuggerData>>,
    ) -> Result<()> {
        tracing::trace!("Processing event: {:?}", event);
        match event {
            EventBody::output(output) => {
                neovim_vadre_window
                    .lock()
                    .await
                    .log_msg(
                        VadreLogLevel::INFO,
                        &format!("CodeLLDB: {}", output.output.trim_end()),
                    )
                    .await
            }
            EventBody::stopped(stopped_event) => {
                CodeLLDBDebugger::handle_codelldb_event_stopped(
                    stopped_event,
                    neovim_vadre_window,
                    debugger_sender_tx,
                    codelldb_data,
                )
                .await
            }
            EventBody::continued(continued_event) => {
                CodeLLDBDebugger::handle_codelldb_event_continued(
                    continued_event,
                    neovim_vadre_window,
                    debugger_sender_tx,
                    codelldb_data,
                )
                .await
            }
            EventBody::breakpoint(breakpoint_event) => {
                CodeLLDBDebugger::handle_codelldb_event_breakpoint(
                    breakpoint_event,
                    neovim_vadre_window,
                    codelldb_data,
                )
                .await
            }
            _ => {
                tracing::trace!("Got unhandled event {:?}", event);
                Ok(())
            }
        }
    }

    #[tracing::instrument(skip(
        stopped_event,
        neovim_vadre_window,
        debugger_sender_tx,
        codelldb_data,
    ))]
    async fn handle_codelldb_event_stopped(
        stopped_event: StoppedEventBody,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        debugger_sender_tx: mpsc::Sender<(
            ProtocolMessageType,
            Option<oneshot::Sender<ResponseResult>>,
        )>,
        codelldb_data: Arc<Mutex<CodeLLDBDebuggerData>>,
    ) -> Result<()> {
        let neovim_vadre_window = neovim_vadre_window.clone();

        codelldb_data.lock().await.current_thread_id = stopped_event.thread_id;

        tokio::spawn(async move {
            log_err!(
                CodeLLDBDebugger::process_output_info(
                    debugger_sender_tx.clone(),
                    neovim_vadre_window.clone(),
                    codelldb_data,
                )
                .await,
                neovim_vadre_window,
                "can get threads"
            );

            if let Some(thread_id) = stopped_event.thread_id {
                if let Err(e) = CodeLLDBDebugger::process_stopped(
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
                        .await
                        .expect("Can log to Vadre");
                }
            }
        });

        Ok(())
    }

    #[tracing::instrument(skip(
        _continued_event,
        neovim_vadre_window,
        debugger_sender_tx,
        codelldb_data,
    ))]
    async fn handle_codelldb_event_continued(
        _continued_event: ContinuedEventBody,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        debugger_sender_tx: mpsc::Sender<(
            ProtocolMessageType,
            Option<oneshot::Sender<ResponseResult>>,
        )>,
        codelldb_data: Arc<Mutex<CodeLLDBDebuggerData>>,
    ) -> Result<()> {
        tokio::spawn(async move {
            log_err!(
                CodeLLDBDebugger::process_output_info(
                    debugger_sender_tx.clone(),
                    neovim_vadre_window.clone(),
                    codelldb_data,
                )
                .await,
                neovim_vadre_window,
                "can get process info"
            );
        });

        Ok(())
    }

    #[tracing::instrument(skip(breakpoint_event, neovim_vadre_window, codelldb_data,))]
    async fn handle_codelldb_event_breakpoint(
        breakpoint_event: BreakpointEventBody,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        codelldb_data: Arc<Mutex<CodeLLDBDebuggerData>>,
    ) -> Result<()> {
        let breakpoint_id = breakpoint_event.breakpoint.id.unwrap();

        let message = breakpoint_event.breakpoint.message.unwrap();
        let is_resolved = CodeLLDBDebugger::breakpoint_is_resolved(&message);

        for _ in 1..100 {
            if codelldb_data
                .lock()
                .await
                .breakpoints
                .get_breakpoint_for_id(&breakpoint_id)
                .is_some()
            {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }

        let existing_breakpoint = codelldb_data
            .lock()
            .await
            .breakpoints
            .get_breakpoint_for_id(&breakpoint_id)
            .unwrap()
            .clone();

        let file_path = existing_breakpoint.file_path.clone();

        let mut codelldb_data_lock = codelldb_data.lock().await;

        codelldb_data_lock.breakpoints.add_breakpoint(
            breakpoint_id,
            file_path.clone(),
            existing_breakpoint.line_number,
            breakpoint_event.breakpoint.line,
            is_resolved,
        )?;

        let breakpoints = codelldb_data_lock
            .breakpoints
            .get_all_breakpoints_for_file(&file_path);

        neovim_vadre_window
            .lock()
            .await
            .toggle_breakpoint_in_buffer(&file_path, breakpoints)
            .await?;

        Ok(())
    }

    fn breakpoint_is_resolved(message: &str) -> bool {
        // This is awful but I can't see any other way of knowing if a breakpoint
        // was resolved other than checking the message for "Resolved locations: "
        // and ending in either 0 or greater.
        assert!(message.starts_with("Resolved locations: "));
        message.rsplit_once(": ").unwrap().1.parse::<i64>().unwrap() > 0
    }

    /// Actually send the request, should be the only function that does this, even the function
    /// with the `&self` parameter still uses this in turn.
    #[tracing::instrument(skip(request, debugger_sender_tx))]
    async fn do_send_request(
        request: RequestArguments,
        debugger_sender_tx: mpsc::Sender<(
            ProtocolMessageType,
            Option<oneshot::Sender<ResponseResult>>,
        )>,
        sender: Option<oneshot::Sender<ResponseResult>>,
    ) -> Result<()> {
        let message = ProtocolMessageType::Request(request);

        tracing::trace!("Sending message 1 {:?}", message);
        debugger_sender_tx.send((message, sender)).await?;

        Ok(())
    }

    /// Actually send the request and await the response. Used in turn by the equivalent function
    /// with the `&self` parameter.
    #[tracing::instrument(skip(request, debugger_sender_tx))]
    async fn do_send_request_and_await_response(
        request: RequestArguments,
        debugger_sender_tx: mpsc::Sender<(
            ProtocolMessageType,
            Option<oneshot::Sender<ResponseResult>>,
        )>,
    ) -> Result<ResponseResult> {
        let (sender, receiver) = oneshot::channel();

        CodeLLDBDebugger::do_send_request(request, debugger_sender_tx, Some(sender)).await?;

        // TODO: configurable timeout
        let response = match timeout(Duration::new(10, 0), receiver).await {
            Ok(resp) => resp?,
            Err(e) => bail!("Timed out waiting for a response: {}", e),
        };
        tracing::trace!("Response: {:?}", response);

        Ok(response)
    }

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

        let stack_trace_response = CodeLLDBDebugger::do_send_request_and_await_response(
            RequestArguments::stackTrace(StackTraceArguments {
                thread_id,
                format: None,
                levels: Some(1),
                start_frame: None,
            }),
            debugger_sender_tx.clone(),
        )
        .await?;

        if let ResponseResult::Success { body } = stack_trace_response {
            if let ResponseBody::stackTrace(stack_trace_body) = body {
                let stack = stack_trace_body.stack_frames;
                let current_frame = stack.get(0).expect("should have a top frame");
                let source = current_frame.source.as_ref().expect("should have a source");
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
                    let source_reference_response =
                        CodeLLDBDebugger::do_send_request_and_await_response(
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

    #[tracing::instrument(skip(debugger_sender_tx, neovim_vadre_window, codelldb_data))]
    async fn process_output_info(
        debugger_sender_tx: mpsc::Sender<(
            ProtocolMessageType,
            Option<oneshot::Sender<ResponseResult>>,
        )>,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        codelldb_data: Arc<Mutex<CodeLLDBDebuggerData>>,
    ) -> Result<()> {
        tracing::debug!("Getting thread information");

        let mut call_stack_buffer_content = Vec::new();
        let current_thread_id = codelldb_data.lock().await.current_thread_id;

        let response_result = CodeLLDBDebugger::do_send_request_and_await_response(
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

                        let stack_trace_response =
                            CodeLLDBDebugger::do_send_request_and_await_response(
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

                                let top_frame =
                                    frames.get(0).expect("has a frame on the stack trace");
                                let frame_id = top_frame.id;
                                codelldb_data.lock().await.current_frame_id = Some(frame_id);

                                CodeLLDBDebugger::process_variables(
                                    frame_id,
                                    debugger_sender_tx.clone(),
                                    neovim_vadre_window.clone(),
                                )
                                .await?;

                                for frame in frames {
                                    call_stack_buffer_content.push(format!("+ {}", frame.name));

                                    let line_number = frame.line;
                                    let source = frame.source.unwrap();
                                    let source_name = source.name.unwrap();
                                    if let Some(_) = source.path {
                                        call_stack_buffer_content
                                            .push(format!("  - {}:{}", source_name, line_number));
                                    } else {
                                        call_stack_buffer_content.push(format!(
                                            "  - {}:{} (dissassembled)",
                                            source_name, line_number
                                        ));
                                    }
                                }
                            }
                        }
                    } else {
                        let stack_trace_response =
                            CodeLLDBDebugger::do_send_request_and_await_response(
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
                                let frame_name =
                                    &stack_trace_body.stack_frames.get(0).unwrap().name;

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

        neovim_vadre_window
            .lock()
            .await
            .set_call_stack_buffer(call_stack_buffer_content)
            .await?;

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

        let scopes_response_result = CodeLLDBDebugger::do_send_request_and_await_response(
            RequestArguments::scopes(ScopesArguments { frame_id }),
            debugger_sender_tx.clone(),
        )
        .await?;

        if let ResponseResult::Success { body } = scopes_response_result {
            if let ResponseBody::scopes(scopes_body) = body {
                for scope in scopes_body.scopes {
                    variable_content.push(format!("{}:", scope.name));

                    let variables_response_result =
                        CodeLLDBDebugger::do_send_request_and_await_response(
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

    // We just make this synchronous because although it slows things down, it makes it much
    // easier to do. If anyone wants to make this async and cool be my guest but it seems not
    // easy.
    #[tracing::instrument(skip(url))]
    async fn download_extract_zip(full_path: &Path, url: Url) -> Result<()> {
        tracing::trace!("Downloading {} and unzipping to {:?}", url, full_path);
        let zip_contents = reqwest::get(url).await?.bytes().await?;

        let reader = std::io::Cursor::new(zip_contents);
        let mut zip = zip::ZipArchive::new(reader)?;

        zip.extract(full_path)?;

        Ok(())
    }
}

impl Debug for CodeLLDBDebugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeLLDBDebugger")
            .field("id", &self.id)
            .field("command", &self.command)
            .field("command_args", &self.command_args)
            .finish()
    }
}
