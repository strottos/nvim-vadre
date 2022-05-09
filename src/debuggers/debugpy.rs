use std::{
    collections::{HashMap, HashSet},
    env,
    fmt::Debug,
    io,
    marker::Unpin,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use super::{
    dap::protocol::{
        BreakpointEventBody, ContinueArguments, ContinuedEventBody, DAPCodec, DecoderResult,
        Either, EventBody, InitializeRequestArguments, NextArguments, ProtocolMessage,
        ProtocolMessageType, RequestArguments, Response, ResponseBody, ResponseResult,
        RunInTerminalResponseBody, ScopesArguments, SetBreakpointsArguments,
        SetExceptionBreakpointsArguments, SetFunctionBreakpointsArguments, Source, SourceArguments,
        SourceBreakpoint, StackTraceArguments, StepInArguments, StoppedEventBody,
        VariablesArguments,
    },
    dap::shared as dap_shared,
    DebuggerAPI, DebuggerData, DebuggerStepType,
};
use crate::{
    neovim::{CodeBufferContent, NeovimVadreWindow, VadreLogLevel},
    util::{
        get_debuggers_dir, get_os_and_cpu_architecture, get_unused_localhost_port, log_err,
        log_ret_err, ret_err,
    },
};

use anyhow::{bail, Result};
use async_trait::async_trait;
use futures::{prelude::*, StreamExt};
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
use which::which;

const VERSION: &str = "1.6.0";

#[derive(Clone)]
pub struct Debugger {
    id: usize,
    command: String,
    command_args: Vec<String>,
    pub neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    process: Arc<Mutex<Option<Child>>>,

    log_debugger: bool,

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

    stopped_listener_tx: Arc<Mutex<Option<oneshot::Sender<bool>>>>,

    data: Arc<Mutex<DebuggerData>>,
}

#[async_trait]
impl DebuggerAPI for Debugger {
    #[tracing::instrument(skip(self))]
    async fn setup(
        &mut self,
        pending_breakpoints: &HashMap<String, HashSet<i64>>,
        existing_debugger_port: Option<String>,
    ) -> Result<()> {
        log_ret_err!(
            self.neovim_vadre_window.lock().await.create_ui().await,
            self.neovim_vadre_window,
            "Error setting up Vadre UI"
        );

        let port = match existing_debugger_port {
            Some(port) => port.parse::<u16>().expect("debugger port is u16"),
            None => {
                let port = get_unused_localhost_port();

                log_ret_err!(
                    self.launch(port).await,
                    self.neovim_vadre_window,
                    "Error launching process"
                );

                port
            }
        };

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
            self.log_msg(VadreLogLevel::INFO, "Debugger launched and setup")
                .await
        );

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn set_source_breakpoints(
        &self,
        file_path: String,
        line_numbers: &HashSet<i64>,
    ) -> Result<()> {
        self.set_breakpoints(file_path, line_numbers).await
    }

    #[tracing::instrument(skip(self))]
    async fn do_step(&self, step_type: DebuggerStepType, count: u64) -> Result<()> {
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

    #[tracing::instrument(skip(self))]
    async fn change_output_window(&self, type_: &str) -> Result<()> {
        self.neovim_vadre_window
            .lock()
            .await
            .change_output_window(type_)
            .await
    }

    async fn log_msg(&self, level: VadreLogLevel, msg: &str) -> Result<()> {
        self.neovim_vadre_window
            .lock()
            .await
            .log_msg(level, msg)
            .await
    }
}

impl Debugger {
    #[tracing::instrument(skip(neovim))]
    pub fn new(
        id: usize,
        command: String,
        command_args: Vec<String>,
        neovim: Neovim<Compat<Stdout>>,
        log_debugger: bool,
    ) -> Self {
        let (debugger_sender_tx, debugger_sender_rx) = mpsc::channel(1);

        Debugger {
            id,
            command,
            command_args,
            neovim_vadre_window: Arc::new(Mutex::new(NeovimVadreWindow::new(neovim, id))),
            process: Arc::new(Mutex::new(None)),

            log_debugger,

            debugger_sender_tx,
            debugger_sender_rx: Arc::new(Mutex::new(Some(debugger_sender_rx))),

            pending_outgoing_requests: Arc::new(Mutex::new(HashMap::new())),

            stopped_listener_tx: Arc::new(Mutex::new(None)),

            data: Arc::new(Mutex::new(DebuggerData::default())),
        }
    }

    #[tracing::instrument(skip(self))]
    pub async fn set_breakpoints(
        &self,
        file_path: String,
        line_numbers: &HashSet<i64>,
    ) -> Result<()> {
        tracing::trace!(
            "Debugger setting breakpoints in file {:?} on lines {:?}",
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

        if let ResponseResult::Success { body } = response_result {
            if let ResponseBody::setBreakpoints(breakpoints_body) = body {
                let ids_left = breakpoints_body
                    .breakpoints
                    .iter()
                    .map(|x| x.id.unwrap())
                    .collect::<Vec<i64>>();

                self.data
                    .lock()
                    .await
                    .breakpoints
                    .remove_file_ids(file_path.clone(), ids_left)?;

                let mut data_lock = self.data.lock().await;

                for (i, breakpoint_response) in breakpoints_body.breakpoints.into_iter().enumerate()
                {
                    let original_line_number = line_numbers.get(i).unwrap().clone();

                    let breakpoint_id = breakpoint_response.id.unwrap();

                    data_lock.breakpoints.add_breakpoint(
                        breakpoint_id,
                        file_path.clone(),
                        original_line_number,
                        breakpoint_response.line,
                        true,
                    )?;
                }

                let breakpoints = data_lock
                    .breakpoints
                    .get_all_breakpoints_for_file(&file_path);

                self.neovim_vadre_window
                    .lock()
                    .await
                    .toggle_breakpoint_in_buffer(&file_path, breakpoints)
                    .await?;
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip(self, port))]
    async fn launch(&mut self, port: u16) -> Result<()> {
        let msg = format!(
            "Launching process {:?} with debugpy and args: {:?}",
            self.command, self.command_args,
        );
        self.log_msg(VadreLogLevel::DEBUG, &msg).await?;

        self.download_plugin().await?;

        let python_path = self.get_python_path().await?;

        let mut debugger_dir = get_debuggers_dir()?;
        debugger_dir.push("debugpy");
        debugger_dir.push(&format!("debugpy-{}", VERSION));
        debugger_dir.push("build");
        debugger_dir.push("lib");
        debugger_dir.push("debugpy");
        debugger_dir.push("adapter");
        let mut args = vec![
            debugger_dir.to_str().unwrap().to_string(),
            "--port".to_string(),
            port.to_string(),
        ];
        if self.log_debugger {
            if let Ok(vadre_log_file) = env::var("VADRE_LOG_FILE") {
                let mut vadre_log = PathBuf::from(vadre_log_file);
                vadre_log.pop();
                args.push(format!("--log-dir={}", vadre_log.to_str().unwrap()));
            } else {
                args.push("--log-stderr".to_string());
            }
        }

        tracing::trace!("Spawning process: {:?} {:?}", python_path, args);

        let mut child = Command::new(python_path.to_str().unwrap())
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

    #[tracing::instrument(skip(self, port, config_done_tx))]
    async fn tcp_connect(&mut self, port: u16, config_done_tx: oneshot::Sender<()>) -> Result<()> {
        tracing::trace!("Connecting to port {}", port);

        let tcp_stream = self.do_tcp_connect(port).await?;
        let framed_stream = DAPCodec::new().framed(tcp_stream);

        self.handle_framed_stream(framed_stream, config_done_tx)
            .await
    }

    #[tracing::instrument(skip(self, framed_stream, config_done_tx))]
    async fn handle_framed_stream<T>(
        &mut self,
        framed_stream: T,
        config_done_tx: oneshot::Sender<()>,
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
            .lock()
            .await
            .take()
            .expect("Should have a debugger_sender_rx to take");

        let neovim_vadre_window = self.neovim_vadre_window.clone();
        let pending_outgoing_requests = self.pending_outgoing_requests.clone();
        let data = self.data.clone();
        let stopped_listener_tx = self.stopped_listener_tx.clone();
        let debug_program_str = self.command.clone() + &self.command_args.join(" ");

        tokio::spawn(async move {
            let mut framed_stream = framed_stream;
            let config_done_tx = Arc::new(Mutex::new(Some(config_done_tx)));
            let mut debugger_sender_rx = debugger_sender_rx;

            let seq_ids = AtomicU32::new(1);
            let pending_outgoing_requests = pending_outgoing_requests.clone();

            loop {
                tracing::trace!(
                    "framed_stream {:?}, debugger_sender_rx {:?}",
                    framed_stream,
                    debugger_sender_rx
                );
                tokio::select! {
                    msg = framed_stream.next() => {
                        tracing::trace!("Got message {:?}", msg);
                        // Message from Debugger
                        match msg {
                            Some(Ok(decoder_result)) => {
                                tracing::trace!("Message: {:?}", decoder_result);
                                match decoder_result {
                                    Ok(message) => match message.type_ {
                                        ProtocolMessageType::Request(request) => {
                                            let config_done_tx = config_done_tx.clone();
                                            if let Err(e) = timeout(
                                                Duration::new(10, 0),
                                                Debugger::handle_request(
                                                    *message.seq.first(),
                                                    request,
                                                    neovim_vadre_window.clone(),
                                                    debugger_sender_tx.clone(),
                                                    Some(&debug_program_str),
                                                    config_done_tx,
                                                ),
                                            )
                                            .await
                                            {
                                                let msg = format!("Debugger Request Error: {}", e);
                                                tracing::error!("{}", msg);
                                                neovim_vadre_window
                                                    .lock()
                                                    .await
                                                    .log_msg(VadreLogLevel::WARN, &msg)
                                                    .await
                                                    .expect("Logging failed");
                                            }
                                        }

                                        ProtocolMessageType::Response(response) => {
                                            // TODO: configurable timeout?
                                            if let Err(e) = timeout(
                                                Duration::new(10, 0),
                                                Debugger::handle_response(
                                                    response,
                                                    pending_outgoing_requests.clone(),
                                                ),
                                            )
                                            .await
                                            {
                                                let msg = format!("Debugger Response Error: {}", e);
                                                tracing::error!("{}", msg);
                                                neovim_vadre_window
                                                    .lock()
                                                    .await
                                                    .log_msg(VadreLogLevel::WARN, &msg)
                                                    .await
                                                    .expect("Logging failed");
                                            }
                                        },

                                        ProtocolMessageType::Event(event) => {
                                            // TODO: configurable timeout?
                                            if let Err(e) = timeout(
                                                Duration::new(10, 0),
                                                Debugger::handle_event(
                                                    event,
                                                    neovim_vadre_window.clone(),
                                                    debugger_sender_tx.clone(),
                                                    stopped_listener_tx.clone(),
                                                    data.clone(),
                                                ),
                                            )
                                            .await
                                            {
                                                let msg = format!("Debugger Event Error: {}", e);
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
                                        error!("An error occurred, panic: {:?}", err);
                                        panic!("An error occurred, panic: {:?}", err);
                                    }
                                };
                            }
                            Some(Err(err)) => {
                                error!("Frame decoder error: {:?}", err);
                                panic!("Frame decoder error: {}", err);
                            }
                            None => {
                                debug!("Client has disconnected");
                                break;
                            }
                        };
                    },
                    Some((message_type, sender)) = debugger_sender_rx.recv() => {
                        let seq = seq_ids.fetch_add(1, Ordering::SeqCst);
                        let message = ProtocolMessage {
                            seq: Either::First(seq),
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

        self.log_msg(
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
        dap_shared::do_send_request_and_await_response(
            RequestArguments::initialize(InitializeRequestArguments::new("debugpy".to_string())),
            self.debugger_sender_tx.clone(),
        )
        .await?;

        let program = dunce::canonicalize(&self.command)?;

        let args = serde_json::json!({
            "args": &self.command_args,
            "cwd": env::current_dir()?,
            "env": {},
            "name": "debugpy",
            "console": "integratedTerminal",
            "type": "debugpy",
            "request": "launch",
            "program": program,
            "stopOnEntry": true,
        });

        dap_shared::do_send_request(
            RequestArguments::launch(Either::Second(args)),
            self.debugger_sender_tx.clone(),
            None,
        )
        .await?;

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

        for breakpoint in pending_breakpoints {
            self.set_breakpoints(breakpoint.0.clone(), breakpoint.1)
                .await?;
        }

        timeout(Duration::new(10, 0), config_done_rx).await??;

        dap_shared::do_send_request_and_await_response(
            RequestArguments::configurationDone(None),
            self.debugger_sender_tx.clone(),
        )
        .await?;

        Ok(())
    }

    async fn get_python_path(&self) -> Result<PathBuf> {
        let path = self
            .neovim_vadre_window
            .lock()
            .await
            .get_var("vadre_python_path")
            .await;

        let path = path.map(|x| PathBuf::from(x));

        let path = path.unwrap_or(which("python3").expect("Can't find Python 3 binary"));

        let path = dunce::canonicalize(&path)?;

        if !path.exists() {
            bail!("The binary doesn't exist: {}", path.to_str().unwrap());
        }

        Ok(path)
    }

    #[tracing::instrument(skip(self))]
    async fn download_plugin(&self) -> Result<()> {
        let mut path = get_debuggers_dir()?;
        path.push("debugpy");

        if !path.exists() {
            let (os, arch) = get_os_and_cpu_architecture();

            self.log_msg(
                VadreLogLevel::INFO,
                &format!("Downloading and extracting {} plugin for {}", os, arch),
            )
            .await?;

            let url = Url::parse(&format!(
                "https://github.com/microsoft/debugpy/archive/v{}.zip",
                VERSION,
            ))?;

            Debugger::download_extract_zip(path.as_path(), url).await?;

            let python_path = self.get_python_path().await?;
            let mut path = get_debuggers_dir()?;
            path.push("debugpy");
            path.push(&format!("debugpy-{}", VERSION));
            let working_dir = dunce::canonicalize(path)?;

            tracing::debug!(
                "Running debugpy installation: {:?} {:?}",
                python_path,
                working_dir
            );

            let child = Command::new(python_path.to_str().unwrap())
                .args(vec!["setup.py", "build", "--build-platlib", "build/lib"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .current_dir(working_dir)
                .output()
                .await
                .expect("Failed to spawn debugpy setup");

            let stdout = String::from_utf8_lossy(&child.stdout);
            let stderr = String::from_utf8_lossy(&child.stdout);

            tracing::debug!("debugpy output: {:?}", stdout);
            tracing::debug!("debugpy stderr: {:?}", stderr);

            self.neovim_vadre_window
                .lock()
                .await
                .log_msg(VadreLogLevel::INFO, &format!("debugpy stdout: {}", stdout))
                .await?;
            self.neovim_vadre_window
                .lock()
                .await
                .log_msg(VadreLogLevel::INFO, &format!("debugpy stderr: {}", stderr))
                .await?;
        }

        Ok(())
    }

    // These functions going forward are useful as they don't have a `self` parameter and thus can
    // be used by tokio spawned async functions, though more parameters need passing in to be
    // useful of course.
    /// Handle a request from Debugger
    #[tracing::instrument(skip(args, neovim_vadre_window, debugger_sender_tx, config_done_tx))]
    async fn handle_request(
        request_id: u32,
        args: RequestArguments,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        debugger_sender_tx: mpsc::Sender<(
            ProtocolMessageType,
            Option<oneshot::Sender<ResponseResult>>,
        )>,
        debug_program_str: Option<&str>,
        config_done_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
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
                        debug_program_str,
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

                match config_done_tx.lock().await.take() {
                    Some(x) => x.send(()).unwrap(),
                    None => {}
                };

                Ok(())
            }
            _ => unreachable!(),
        }
    }

    #[tracing::instrument(skip(response, pending_outgoing_requests))]
    async fn handle_response(
        response: Response,
        pending_outgoing_requests: Arc<Mutex<HashMap<u32, oneshot::Sender<ResponseResult>>>>,
    ) -> Result<()> {
        let request_id = response.request_seq.first();

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
        stopped_listener_tx: Arc<Mutex<Option<oneshot::Sender<bool>>>>,
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
            EventBody::continued(continued_event) => {
                Debugger::handle_event_continued(
                    continued_event,
                    neovim_vadre_window,
                    debugger_sender_tx,
                    data,
                )
                .await
            }
            EventBody::breakpoint(breakpoint_event) => {
                Debugger::handle_event_breakpoint(breakpoint_event, neovim_vadre_window, data).await
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
        stopped_listener_tx: Arc<Mutex<Option<oneshot::Sender<bool>>>>,
        data: Arc<Mutex<DebuggerData>>,
    ) -> Result<()> {
        let neovim_vadre_window = neovim_vadre_window.clone();

        data.lock().await.current_thread_id = stopped_event.thread_id;

        if let Some(listener_tx) = stopped_listener_tx.lock().await.take() {
            // If we're here we're about to do more stepping so no need to do more
            listener_tx.send(true).unwrap();
            return Ok(());
        }

        tokio::spawn(async move {
            log_err!(
                Debugger::process_output_info(
                    debugger_sender_tx.clone(),
                    neovim_vadre_window.clone(),
                    data,
                )
                .await,
                neovim_vadre_window,
                "can get threads"
            );

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
                        .await
                        .expect("Can log to Vadre");
                }
            }
        });

        Ok(())
    }

    #[tracing::instrument(skip(_continued_event, neovim_vadre_window, debugger_sender_tx, data))]
    async fn handle_event_continued(
        _continued_event: ContinuedEventBody,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        debugger_sender_tx: mpsc::Sender<(
            ProtocolMessageType,
            Option<oneshot::Sender<ResponseResult>>,
        )>,
        data: Arc<Mutex<DebuggerData>>,
    ) -> Result<()> {
        tokio::spawn(async move {
            log_err!(
                Debugger::process_output_info(
                    debugger_sender_tx.clone(),
                    neovim_vadre_window.clone(),
                    data,
                )
                .await,
                neovim_vadre_window,
                "can get process info"
            );
        });

        Ok(())
    }

    #[tracing::instrument(skip(breakpoint_event, neovim_vadre_window, data,))]
    async fn handle_event_breakpoint(
        breakpoint_event: BreakpointEventBody,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        data: Arc<Mutex<DebuggerData>>,
    ) -> Result<()> {
        let breakpoint_id = breakpoint_event.breakpoint.id.unwrap();

        for _ in 1..100 {
            if data
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

        let existing_breakpoint = data
            .lock()
            .await
            .breakpoints
            .get_breakpoint_for_id(&breakpoint_id)
            .unwrap()
            .clone();

        let file_path = existing_breakpoint.file_path.clone();

        let mut data_lock = data.lock().await;

        data_lock.breakpoints.add_breakpoint(
            breakpoint_id,
            file_path.clone(),
            existing_breakpoint.line_number,
            breakpoint_event.breakpoint.line,
            true,
        )?;

        let breakpoints = data_lock
            .breakpoints
            .get_all_breakpoints_for_file(&file_path);

        neovim_vadre_window
            .lock()
            .await
            .toggle_breakpoint_in_buffer(&file_path, breakpoints)
            .await?;

        Ok(())
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

                                let top_frame =
                                    frames.get(0).expect("has a frame on the stack trace");
                                let frame_id = top_frame.id;
                                data.lock().await.current_frame_id = Some(frame_id);

                                Debugger::process_variables(
                                    frame_id,
                                    debugger_sender_tx.clone(),
                                    neovim_vadre_window.clone(),
                                )
                                .await?;

                                for frame in frames {
                                    call_stack_buffer_content.push(format!("+ {}", frame.name));

                                    let line_number = frame.line;
                                    let source = frame.source.unwrap();
                                    if let Some(source_name) = source.name {
                                        if let Some(_) = source.path {
                                            call_stack_buffer_content.push(format!(
                                                "  - {}:{}",
                                                source_name, line_number
                                            ));
                                        } else {
                                            call_stack_buffer_content.push(format!(
                                                "  - {}:{} (dissassembled)",
                                                source_name, line_number
                                            ));
                                        }
                                    } else if let Some(source_path) = source.path {
                                        call_stack_buffer_content
                                            .push(format!("  - {}:{}", source_path, line_number));
                                    } else {
                                        call_stack_buffer_content
                                            .push(format!(" - source not understood {:?}", source));
                                    }
                                }
                            }
                        }
                    } else {
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

    // We just make this synchronous because although it slows things down, it makes it much
    // easier to do. If anyone wants to make this async and cool be my guest but it seems not
    // easy.
    #[tracing::instrument(skip(url))]
    async fn download_extract_zip(full_path: &Path, url: Url) -> Result<()> {
        tracing::trace!("Downloading {} and unzipping to {:?}", url, full_path);
        let zip_contents = reqwest::get(url).await?.bytes().await?;

        let reader = io::Cursor::new(zip_contents);
        let mut zip = zip::ZipArchive::new(reader)?;

        zip.extract(full_path)?;

        Ok(())
    }
}

impl Debug for Debugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugPyDebugger")
            .field("id", &self.id)
            .field("command", &self.command)
            .field("command_args", &self.command_args)
            .field("process", &self.process)
            .finish()
    }
}
