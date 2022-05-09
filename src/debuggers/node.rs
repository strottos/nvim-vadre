use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use crate::{
    neovim::{CodeBufferContent, NeovimVadreWindow, VadreLogLevel},
    util::{get_unused_localhost_port, log_ret_err, merge_json, ret_err},
};

use anyhow::{bail, Result};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use nvim_rs::{compat::tokio::Compat, Neovim};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Stdout},
    net::TcpStream,
    process::{Child, Command},
    sync::{mpsc, oneshot, Mutex},
    time::timeout,
};
use tokio_tungstenite::{tungstenite::Message as WsMessage, MaybeTlsStream, WebSocketStream};

use super::{DebuggerAPI, DebuggerStepType};

/// Node script, indicated by receiving a 'Debugger.scriptParsed' message from Node
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Script {
    file: String,
    script_id: String,
}

impl Script {
    pub fn new(file: String, script_id: String) -> Self {
        Script { file, script_id }
    }

    pub fn get_script_id(&self) -> &str {
        &self.script_id
    }
}

#[derive(Clone, Debug, Default)]
struct DebuggerData {
    scripts: Vec<Script>,
}

#[derive(Clone)]
pub struct Debugger {
    id: usize,
    command: String,
    command_args: Vec<String>,
    pub neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    process: Arc<Mutex<Option<Child>>>,

    debugger_sender_tx: mpsc::Sender<(
        serde_json::Value,
        Option<oneshot::Sender<serde_json::Value>>,
    )>,
    // Following should be empty most of the time and will be taken by the tcp_connection.
    //
    // We use a mutex here and steal it once for performance reasons, rather than have an
    // unnecessary mutex on the debugger_sender_tx part if this were created when needed. This just
    // gets stolen once and we never use the mutex again.
    debugger_sender_rx: Arc<
        Mutex<
            Option<
                mpsc::Receiver<(
                    serde_json::Value,
                    Option<oneshot::Sender<serde_json::Value>>,
                )>,
            >,
        >,
    >,

    pending_breakpoints: Arc<Mutex<HashMap<String, HashSet<i64>>>>,

    pending_outgoing_requests: Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>,

    stopped_listener_tx: Arc<Mutex<Option<oneshot::Sender<bool>>>>,

    data: Arc<Mutex<DebuggerData>>,
}

#[async_trait]
impl DebuggerAPI for Debugger {
    #[tracing::instrument(skip(self))]
    async fn setup(
        &mut self,
        pending_breakpoints: &HashMap<String, HashSet<i64>>,
        existing_debugger_location: Option<String>,
    ) -> Result<()> {
        log_ret_err!(
            self.neovim_vadre_window.lock().await.create_ui().await,
            self.neovim_vadre_window,
            "Error setting up Vadre UI"
        );

        {
            let mut pending_breakpoints_lock = self.pending_breakpoints.lock().await;

            for (key, value) in pending_breakpoints {
                let file = Debugger::canonicalize_filename(key)?;
                pending_breakpoints_lock.insert(file, value.clone());
            }
        }

        let ws_url = match existing_debugger_location {
            Some(ws_url) => ws_url,
            None => {
                let port = get_unused_localhost_port();

                match self.launch(port).await {
                    Ok(ws_url) => ws_url,
                    Err(e) => {
                        let msg = format!("Generic error: {}", e);
                        tracing::error!("{}", msg);
                        self.neovim_vadre_window
                            .lock()
                            .await
                            .log_msg(VadreLogLevel::ERROR, &msg)
                            .await
                            .expect("logs should be logged");

                        return Err(e);
                    }
                }
            }
        };

        log_ret_err!(
            self.ws_connect(ws_url).await,
            self.neovim_vadre_window,
            "Error creating WS connection to process"
        );
        log_ret_err!(
            self.init_process().await,
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
        Debugger::set_breakpoints(
            file_path,
            line_numbers,
            self.debugger_sender_tx.clone(),
            self.pending_breakpoints.clone(),
            self.neovim_vadre_window.clone(),
            self.data.clone(),
        )
        .await
    }

    #[tracing::instrument(skip(self))]
    async fn do_step(&self, step_type: DebuggerStepType, count: u64) -> Result<()> {
        let request = match step_type {
            DebuggerStepType::Over => serde_json::json!({"method":"Debugger.stepOver"}),
            DebuggerStepType::In => serde_json::json!({"method":"Debugger.stepInto"}),
            DebuggerStepType::Continue => serde_json::json!({"method":"Debugger.resume"}),
        };

        for _ in 1..count {
            let (tx, rx) = oneshot::channel();

            *self.stopped_listener_tx.lock().await = Some(tx);

            let resp = do_send_request_and_await_response(
                request.clone(),
                self.debugger_sender_tx.clone(),
            )
            .await?;

            tracing::trace!("Resp: {:#?}", resp);

            timeout(Duration::new(2, 0), rx).await??;
        }

        self.debugger_sender_tx.send((request, None)).await?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn change_output_window(&self, type_: &str) -> Result<()> {
        self.neovim_vadre_window
            .lock()
            .await
            .change_output_window(type_)
            .await?;

        Ok(())
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
        _log_debugger: bool,
    ) -> Self {
        let (debugger_sender_tx, debugger_sender_rx) = mpsc::channel(1);

        Debugger {
            id,
            command,
            command_args,
            neovim_vadre_window: Arc::new(Mutex::new(NeovimVadreWindow::new(neovim, id))),
            process: Arc::new(Mutex::new(None)),

            debugger_sender_tx,
            debugger_sender_rx: Arc::new(Mutex::new(Some(debugger_sender_rx))),

            pending_breakpoints: Arc::new(Mutex::new(HashMap::new())),
            pending_outgoing_requests: Arc::new(Mutex::new(HashMap::new())),

            stopped_listener_tx: Arc::new(Mutex::new(None)),

            data: Arc::new(Mutex::new(DebuggerData::default())),
        }
    }

    #[tracing::instrument(skip(self, port))]
    async fn launch(&mut self, port: u16) -> Result<String> {
        let msg = format!(
            "Launching Node process {:?} with args: {:?}",
            self.command, self.command_args,
        );
        self.log_msg(VadreLogLevel::DEBUG, &msg).await?;

        let path = dunce::canonicalize(PathBuf::from(self.command.clone()))?;

        if !path.exists() {
            bail!("The binary doesn't exist: {}", path.to_str().unwrap());
        }

        let mut args = vec![format!("--inspect-brk={}", port)];
        args.extend(self.command_args.clone());

        tracing::debug!("Spawning process: {:?} {:?}", path, args);

        let mut child = Command::new(path.to_str().unwrap())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to spawn debugger");

        let stderr = child.stderr.take().expect("should have stderr");

        let (ws_url_tx, ws_url_rx) = oneshot::channel();

        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            let mut log_stderr = false;
            let mut stderr = tokio::io::stderr();

            let mut ws_url_tx = Some(ws_url_tx);

            while let Some(line) = reader.next_line().await.expect("can read stderr") {
                if log_stderr {
                    stderr
                        .write_all(line.as_bytes())
                        .await
                        .expect("Can write to stderr");
                } else {
                    if line.starts_with("Debugger listening on ") {
                        let url = &line[22..];

                        ws_url_tx
                            .take()
                            .expect("Should be able to take oneshot channel once")
                            .send(url.to_string())
                            .expect("Should be able to send WS Url");
                    } else if line
                        == "Debugger stderr: For help, see: https://nodejs.org/en/docs/inspector"
                    {
                        tracing::trace!("Going to log stderr");

                        log_stderr = true;
                    }
                }
            }
        });

        *self.process.lock().await = Some(child);

        self.log_msg(VadreLogLevel::DEBUG, "Process spawned".into())
            .await?;

        let ws_url = timeout(Duration::new(60, 0), ws_url_rx).await??;

        Ok(ws_url)
    }

    #[tracing::instrument(skip(self, ws_url))]
    async fn ws_connect(&mut self, ws_url: String) -> Result<()> {
        let ws_stream = self.do_ws_connect(ws_url).await?;
        self.handle_stream(ws_stream).await?;

        Ok(())
    }

    #[tracing::instrument(skip(self, ws_stream))]
    async fn handle_stream(
        &mut self,
        ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    ) -> Result<()> {
        let debugger_sender_tx = self.debugger_sender_tx.clone();
        let debugger_sender_rx = self
            .debugger_sender_rx
            .lock()
            .await
            .take()
            .expect("Should have a debugger_sender_rx to take");

        let neovim_vadre_window = self.neovim_vadre_window.clone();
        let pending_breakpoints = self.pending_breakpoints.clone();
        let pending_outgoing_requests = self.pending_outgoing_requests.clone();
        let data = self.data.clone();
        let stopped_listener_tx = self.stopped_listener_tx.clone();

        tokio::spawn(async move {
            let mut ws_stream = ws_stream;
            let mut debugger_sender_rx = debugger_sender_rx;

            let seq_ids = AtomicU64::new(1);

            loop {
                tokio::select! {
                    msg = ws_stream.next() => {
                        tracing::trace!("Received message: {:?}", msg);
                        match msg {
                            Some(Ok(msg)) => {
                                tracing::trace!("Message: {:?}", msg);
                                match msg {
                                    WsMessage::Text(msg) => {
                                        let msg: serde_json::Value = serde_json::from_str(&msg).expect("can convert WS message text string to json");
                                        tracing::trace!("JSON: {:?}", msg);

                                        match msg.get("id") {
                                            Some(id) => {
                                                let id = id.as_u64().expect("WS message id is u64");
                                                let mut pending_outgoing_requests = pending_outgoing_requests.lock().await;
                                                if let Some(sender) = pending_outgoing_requests.remove(&id) {
                                                    tracing::trace!("Sending JSON response to request");
                                                    sender.send(msg).expect("Message can be sent");
                                                    tracing::trace!("Sent JSON response to request");
                                                }
                                            },
                                            None => {
                                                if let Err(e) = timeout(
                                                    Duration::new(30, 0),
                                                    Debugger::handle_event(
                                                        msg,
                                                        neovim_vadre_window.clone(),
                                                        pending_breakpoints.clone(),
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
                                            },
                                        }
                                    },
                                    WsMessage::Binary(_) => todo!(),
                                    WsMessage::Ping(_) => todo!(),
                                    WsMessage::Pong(_) => todo!(),
                                    WsMessage::Close(_) => todo!(),
                                    WsMessage::Frame(_) => todo!(),
                                }
                            }
                            Some(Err(err)) => {
                                tracing::error!("Frame decoder error: {:?}", err);
                                panic!("Frame decoder error: {}", err);
                            }
                            None => {
                                tracing::info!("Client has disconnected");
                                neovim_vadre_window
                                    .clone()
                                    .lock()
                                    .await
                                    .log_msg(VadreLogLevel::INFO, "Client has disconnected")
                                    .await
                                    .expect("can log to vim");
                                break;
                            }
                        }
                    }
                    Some((message, sender)) = debugger_sender_rx.recv() => {
                        let id = seq_ids.fetch_add(1, Ordering::SeqCst);
                        let mut req = serde_json::json!({
                            "id": id,
                        });
                        merge_json(&mut req, message);
                        let message = WsMessage::Text(req.to_string());
                        tracing::trace!("Sending message: {:?}", message);
                        match sender {
                            Some(sender) => { pending_outgoing_requests.lock().await.insert(id, sender); },
                            None => {},
                        };
                        ws_stream.send(message).await.expect("Message should send to WS");
                    }
                }
            }
        });

        Ok(())
    }

    #[tracing::instrument(skip(self, ws_url))]
    async fn do_ws_connect(
        &self,
        ws_url: String,
    ) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
        let msg = format!("Connecting to WS Url {}", ws_url);
        tracing::trace!("{}", msg);

        self.neovim_vadre_window
            .lock()
            .await
            .log_msg(VadreLogLevel::DEBUG, &msg)
            .await
            .expect("can log to vim");

        let (conn, resp) = tokio_tungstenite::connect_async(ws_url)
            .await
            .expect("Can connect to NodeJS WebSocket");

        assert!(resp.status() == 101);

        Ok(conn)
    }

    #[tracing::instrument(skip(self))]
    async fn init_process(&self) -> Result<()> {
        do_send_request_and_await_response(
            serde_json::json!({"method":"Runtime.enable"}),
            self.debugger_sender_tx.clone(),
        )
        .await?;

        do_send_request_and_await_response(
            serde_json::json!({"method":"Debugger.enable"}),
            self.debugger_sender_tx.clone(),
        )
        .await?;

        do_send_request_and_await_response(
            serde_json::json!({"method":"Runtime.runIfWaitingForDebugger"}),
            self.debugger_sender_tx.clone(),
        )
        .await?;

        Ok(())
    }

    #[tracing::instrument(skip(
        event,
        neovim_vadre_window,
        pending_breakpoints,
        debugger_sender_tx,
        stopped_listener_tx,
        data,
    ))]
    async fn handle_event(
        event: serde_json::Value,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        pending_breakpoints: Arc<Mutex<HashMap<String, HashSet<i64>>>>,
        debugger_sender_tx: mpsc::Sender<(
            serde_json::Value,
            Option<oneshot::Sender<serde_json::Value>>,
        )>,
        stopped_listener_tx: Arc<Mutex<Option<oneshot::Sender<bool>>>>,
        data: Arc<Mutex<DebuggerData>>,
    ) -> Result<()> {
        tracing::trace!("Processing event: {:?}", event);
        match event.get("method") {
            Some(method) => {
                let method = method.as_str().expect("WS event method is a string");
                match method {
                    "Runtime.consoleAPICalled" => {}
                    "Runtime.executionContextCreated" => {}
                    "Runtime.executionContextDestroyed" => {
                        neovim_vadre_window
                            .lock()
                            .await
                            .log_msg(VadreLogLevel::ERROR, "Finished")
                            .await
                            .expect("Can log to vim");
                    }
                    "Debugger.scriptParsed" => {
                        Debugger::analyse_script_parsed(
                            event,
                            pending_breakpoints,
                            debugger_sender_tx,
                            neovim_vadre_window,
                            data,
                        )
                        .await?
                    }
                    "Debugger.paused" => {
                        tokio::spawn(async {
                            Debugger::analyse_debugger_paused(
                                event,
                                debugger_sender_tx,
                                stopped_listener_tx,
                                neovim_vadre_window,
                                data,
                            )
                            .await
                            .expect("Can analyse pausing debugger");
                        });
                    }
                    "Debugger.resumed" => {}
                    _ => {
                        neovim_vadre_window
                            .lock()
                            .await
                            .log_msg(
                                VadreLogLevel::ERROR,
                                &format!("Debugger doesn't support event method `{}`", method),
                            )
                            .await
                            .expect("Can log to vim");
                    }
                }
            }
            None => unreachable!(),
        }
        Ok(())
    }

    #[tracing::instrument(skip(
        event,
        pending_breakpoints,
        debugger_sender_tx,
        neovim_vadre_window,
        data
    ))]
    async fn analyse_script_parsed(
        event: serde_json::Value,
        pending_breakpoints: Arc<Mutex<HashMap<String, HashSet<i64>>>>,
        debugger_sender_tx: mpsc::Sender<(
            serde_json::Value,
            Option<oneshot::Sender<serde_json::Value>>,
        )>,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        data: Arc<Mutex<DebuggerData>>,
    ) -> Result<()> {
        let file: String = match serde_json::from_value(event["params"]["url"].clone()) {
            Ok(s) => s,
            Err(e) => {
                panic!("Can't understand file: {:?}", e);
            }
        };

        let script_id: String = match serde_json::from_value(event["params"]["scriptId"].clone()) {
            Ok(s) => s,
            Err(e) => {
                panic!("Can't understand script_id: {:?}", e);
            }
        };

        data.lock()
            .await
            .scripts
            .push(Script::new(file.clone(), script_id));

        let file_canonical_path = Debugger::canonicalize_filename(&file)?;

        tracing::trace!("Looking for {} for breakpoints", file_canonical_path);

        let breakpoint_lines = if let Some(breakpoint_lines) =
            pending_breakpoints.lock().await.get(&file_canonical_path)
        {
            Some(breakpoint_lines.clone())
        } else {
            None
        };

        if let Some(breakpoint_lines) = breakpoint_lines {
            tokio::spawn(async move {
                Debugger::set_breakpoints(
                    file.clone(),
                    &breakpoint_lines,
                    debugger_sender_tx.clone(),
                    pending_breakpoints.clone(),
                    neovim_vadre_window.clone(),
                    data.clone(),
                )
                .await
                .expect("Can set breakpoints");
            });
        };

        Ok(())
    }

    #[tracing::instrument(skip(
        event,
        debugger_sender_tx,
        stopped_listener_tx,
        neovim_vadre_window,
        data
    ))]
    async fn analyse_debugger_paused(
        event: serde_json::Value,
        debugger_sender_tx: mpsc::Sender<(
            serde_json::Value,
            Option<oneshot::Sender<serde_json::Value>>,
        )>,
        stopped_listener_tx: Arc<Mutex<Option<oneshot::Sender<bool>>>>,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        data: Arc<Mutex<DebuggerData>>,
    ) -> Result<()> {
        if let Some(listener_tx) = stopped_listener_tx.lock().await.take() {
            // If we're here we're about to do more stepping so no need to do more
            listener_tx.send(true).unwrap();
            return Ok(());
        }

        let file: String =
            match serde_json::from_value(event["params"]["callFrames"][0]["url"].clone()) {
                Ok(s) => s,
                Err(e) => {
                    // TODO: How do we get here? Handle when we see it.
                    panic!("JSON: {}, err: {}", event, e);
                }
            };

        let line_number: i64 = match serde_json::from_value(
            event["params"]["callFrames"][0]["location"]["lineNumber"].clone(),
        ) {
            Ok(s) => {
                let s: i64 = s;
                s + 1
            }
            Err(e) => {
                panic!("Can't understand line_num: {:?}", e);
            }
        };

        let script = Debugger::get_script_from_filename(&file, data.clone()).await?;

        match script {
            Some(x) => {
                let msg = serde_json::json!({
                    "method": "Debugger.getScriptSource",
                    "params": {
                        "scriptId": x.script_id,
                    }
                });

                let resp =
                    do_send_request_and_await_response(msg, debugger_sender_tx.clone()).await?;

                let contents = resp["result"]["scriptSource"]
                    .as_str()
                    .expect("can convert to str")
                    .to_string();

                neovim_vadre_window
                    .lock()
                    .await
                    .set_code_buffer(
                        CodeBufferContent::Content(contents),
                        line_number,
                        &file,
                        false,
                    )
                    .await?;

                if file.ends_with(".ts") {
                    neovim_vadre_window
                        .lock()
                        .await
                        .set_file_type("typescript")
                        .await?;
                } else {
                    neovim_vadre_window
                        .lock()
                        .await
                        .set_file_type("javascript")
                        .await?;
                }
            }
            None => {
                neovim_vadre_window
                    .lock()
                    .await
                    .log_msg(
                        VadreLogLevel::ERROR,
                        &format!("Can't find script for {}", file),
                    )
                    .await?
            }
        }

        tokio::spawn(async move {
            let event = event.clone();
            let neovim_vadre_window = neovim_vadre_window.clone();
            // let debugger_sender_tx = debugger_sender_tx.clone();
            let data = data.clone();

            Debugger::get_call_stack(event.clone(), neovim_vadre_window.clone(), data.clone())
                .await
                .expect("Can get call stack");

            // Debugger::get_variables(event, debugger_sender_tx, neovim_vadre_window, data)
            //     .await
            //     .expect("Can get variables");
        });

        Ok(())
    }

    #[tracing::instrument(skip(event, neovim_vadre_window, data))]
    async fn get_call_stack(
        event: serde_json::Value,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        data: Arc<Mutex<DebuggerData>>,
    ) -> Result<()> {
        let mut call_stack_buffer_content = Vec::new();

        for frame in event["params"]["callFrames"]
            .as_array()
            .expect("callFrames is an array")
        {
            let script = Debugger::get_script_from_id(
                frame["location"]["scriptId"]
                    .as_str()
                    .expect("location has a string script id"),
                data.clone(),
            )
            .await
            .expect("have script_id in data");

            call_stack_buffer_content.push(format!(
                "{}:{}",
                script.file,
                frame["location"]["lineNumber"]
                    .as_i64()
                    .expect("line number is integer")
                    + 1
            ));
            call_stack_buffer_content.push(format!("- {}", frame["functionName"]));
        }

        neovim_vadre_window
            .lock()
            .await
            .set_call_stack_buffer(call_stack_buffer_content)
            .await?;

        Ok(())
    }

    // async fn get_variables(
    //     event: serde_json::Value,
    //     debugger_sender_tx: mpsc::Sender<(
    //         serde_json::Value,
    //         Option<oneshot::Sender<serde_json::Value>>,
    //     )>,
    //     neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    //     data: Arc<Mutex<DebuggerData>>,
    // ) -> Result<()> {
    //     Ok(())
    // }

    async fn set_breakpoints(
        file_path: String,
        line_numbers: &HashSet<i64>,
        debugger_sender_tx: mpsc::Sender<(
            serde_json::Value,
            Option<oneshot::Sender<serde_json::Value>>,
        )>,
        pending_breakpoints: Arc<Mutex<HashMap<String, HashSet<i64>>>>,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        data: Arc<Mutex<DebuggerData>>,
    ) -> Result<()> {
        tracing::trace!(
            "Setting breakpoint in {} at lines {:?}",
            file_path,
            line_numbers
        );
        match Debugger::get_script_from_filename(&file_path, data.clone()).await? {
            Some(script) => {
                let script_id = script.get_script_id();
                for line_number in line_numbers {
                    let msg = serde_json::json!({
                        "method": "Debugger.setBreakpoint",
                        "params":{
                            "location":{
                                "scriptId": script_id,
                                "lineNumber": line_number - 1,
                            }
                        },
                    });
                    let resp =
                        do_send_request_and_await_response(msg, debugger_sender_tx.clone()).await?;
                    tracing::trace!("Breakpoint resp: {:?}", resp);
                }
            }
            None => {
                let file_path = Debugger::canonicalize_filename(&file_path)?;

                {
                    let mut pending_breakpoints_lock = pending_breakpoints.lock().await;
                    // TODO: Think about if the file is already in there
                    pending_breakpoints_lock.insert(file_path.clone(), line_numbers.clone());
                }

                neovim_vadre_window
                    .lock()
                    .await
                    .log_msg(
                        VadreLogLevel::ERROR,
                        &format!("Can't find script for {}", file_path),
                    )
                    .await?;
            }
        }

        Ok(())
    }

    async fn get_script_from_id(script_id: &str, data: Arc<Mutex<DebuggerData>>) -> Option<Script> {
        let data = data.lock().await;

        for script in data.scripts.iter() {
            if &script.script_id == script_id {
                return Some(script.clone());
            }
        }
        None
    }

    async fn get_script_from_filename(
        filename: &str,
        data: Arc<Mutex<DebuggerData>>,
    ) -> Result<Option<Script>> {
        let data = data.lock().await;

        let file_path = Debugger::canonicalize_filename(filename)?;

        for script in data.scripts.iter() {
            let script_file_path = Debugger::canonicalize_filename(&script.file)?;
            if script_file_path == file_path {
                return Ok(Some(script.clone()));
            }
        }
        Ok(None)
    }

    fn canonicalize_filename(filename: &str) -> Result<String> {
        if filename.starts_with("file:///") {
            let file_path = if cfg!(windows) {
                PathBuf::from(&filename[8..])
            } else {
                PathBuf::from(&filename[7..])
            };
            let file_path = dunce::canonicalize(file_path)?;
            Ok(file_path
                .to_str()
                .expect("can convert path to string")
                .to_string())
        } else {
            Ok(filename.to_string())
        }
    }
}

impl Debug for Debugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeLLDBDebugger")
            .field("id", &self.id)
            .field("command", &self.command)
            .field("command_args", &self.command_args)
            .field("process", &self.process)
            .finish()
    }
}

/// Actually send the request and await the response. Used in turn by the equivalent function
/// with the `&self` parameter.
#[tracing::instrument(skip(request, debugger_sender_tx))]
pub async fn do_send_request_and_await_response(
    request: serde_json::Value,
    debugger_sender_tx: mpsc::Sender<(
        serde_json::Value,
        Option<oneshot::Sender<serde_json::Value>>,
    )>,
) -> Result<serde_json::Value> {
    let (sender, receiver) = oneshot::channel();

    debugger_sender_tx.send((request, Some(sender))).await?;

    // TODO: configurable timeout
    let response = match timeout(Duration::new(30, 0), receiver).await {
        Ok(resp) => resp?,
        Err(e) => bail!("Timed out waiting for a response: {}", e),
    };
    tracing::trace!("Response: {:?}", response);

    Ok(response)
}
