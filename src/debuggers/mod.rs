use std::{
    collections::{HashMap, HashSet},
    env::{self, consts::EXE_SUFFIX},
    fmt::Debug,
    path::Path,
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use crate::{
    neovim::{CodeBufferContent, NeovimVadreWindow, VadreLogLevel},
    util::{self, get_debuggers_dir, log_err, log_ret_err, ret_err},
};

use anyhow::{bail, Result};
use nvim_rs::{compat::tokio::Compat, Neovim};
use reqwest::Url;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Stdout},
    net::TcpStream,
    process::{Child, Command},
    sync::{mpsc, oneshot, Mutex},
    time::{sleep, timeout},
};

#[derive(Clone, Debug)]
pub enum DebuggerStepType {
    Over,
    In,
    Continue,
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
    ) -> Result<()> {
        match self {
            Debugger::CodeLLDB(debugger) => debugger.setup(pending_breakpoints).await,
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
    current_thread_id: Option<u64>,
    current_frame_id: Option<u64>,
}

#[derive(Clone)]
pub struct CodeLLDBDebugger {
    id: usize,
    command: String,
    command_args: Vec<String>,
    seq_ids: Arc<AtomicU64>,
    pub neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    process: Option<Arc<Child>>,

    write_tx: mpsc::Sender<serde_json::Value>,
    // Following should be empty most of the time and will be taken by the tcp_connection,
    // shouldn't be used.
    //
    // We use a mutex here and steal it once for performance reasons, rather than have an
    // unnecessary mutex on the write_tx part if this were created when needed. This just gets
    // stolen once and we never use the mutex again.
    write_rx: Arc<Mutex<Option<mpsc::Receiver<serde_json::Value>>>>,

    response_senders: Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>,

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
        let (write_tx, write_rx) = mpsc::channel(1);

        CodeLLDBDebugger {
            id,
            command,
            command_args,
            seq_ids: Arc::new(AtomicU64::new(1)),
            neovim_vadre_window: Arc::new(Mutex::new(NeovimVadreWindow::new(neovim, id))),
            process: None,

            write_tx,
            write_rx: Arc::new(Mutex::new(Some(write_rx))),

            response_senders: Arc::new(Mutex::new(HashMap::new())),

            codelldb_data: Arc::new(Mutex::new(CodeLLDBDebuggerData::default())),
        }
    }

    #[tracing::instrument(skip(self, pending_breakpoints))]
    pub async fn setup(
        &mut self,
        pending_breakpoints: &HashMap<String, HashSet<i64>>,
    ) -> Result<()> {
        log_ret_err!(
            self.neovim_vadre_window.lock().await.create_ui().await,
            self.neovim_vadre_window,
            "Error setting up Vadre UI"
        );

        let port = util::get_unused_localhost_port();

        log_ret_err!(
            self.launch(port).await,
            self.neovim_vadre_window,
            "Error launching process"
        );

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

        self.send_breakpoints_request(file_path, line_numbers)
            .await?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn do_step(&self, step_type: DebuggerStepType) -> Result<()> {
        let codelldb_data = self.codelldb_data.lock().await;

        let command = match step_type {
            DebuggerStepType::Over => "next",
            DebuggerStepType::In => "stepIn",
            DebuggerStepType::Continue => "continue",
        };

        self.send_request(
            command,
            serde_json::json!({
                "threadId": codelldb_data.current_thread_id.unwrap(),
                "singleThread": false,
            }),
        )
        .await?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn print_variable(&self, variable_name: &str) -> Result<()> {
        self.send_request(
            "evaluate",
            serde_json::json!({ "expression": &format!("/se frame evaluate {}", variable_name) }),
        )
        .await?;

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

        let tcp_conn = self.do_tcp_connect(port).await?;
        let (read_conn, write_conn) = tcp_conn.into_split();

        let response_senders = self.response_senders.clone();
        let write_tx = self.write_tx.clone();
        let seq_ids = self.seq_ids.clone();
        let codelldb_data = self.codelldb_data.clone();

        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let mut buf_str = String::new();
            let mut read_conn = read_conn;
            let mut previous_string_length = 0;
            let mut config_done_tx = Some(config_done_tx);

            loop {
                // If we already have something in buffer try a non-blocking read but try and
                // process it anyway. Prevents the situation where you read two messages in one
                // read operation not reading the second immediately. Also keep track of the string
                // length to make sure something was processed, don't keep trying.
                let n = if buf_str != "" || previous_string_length > buf_str.len() {
                    previous_string_length = buf_str.len();
                    read_conn.try_read(&mut buf).unwrap_or(0)
                } else {
                    read_conn.read(&mut buf).await.unwrap()
                };
                buf_str += &String::from_utf8(buf[0..n].into()).unwrap();

                tracing::trace!("From CodeLLDB: {:?}", buf_str);
                let output: Vec<&str> = buf_str.splitn(3, "\r\n").collect();
                if output.len() < 3 {
                    continue;
                }
                let content_length = output.get(0).unwrap()[16..].parse::<usize>().unwrap();
                let remainder = output.get(2).unwrap();
                if remainder.len() < content_length {
                    continue;
                }

                let incoming_json: serde_json::Value =
                    serde_json::from_str(&remainder[0..content_length]).unwrap();

                // TODO: Can we do this more efficiently somehow? Use the `Bytes` crate and
                // consume? Is that actually more efficient?
                buf_str = String::from(&remainder[content_length..]);

                tracing::debug!("<-- {}", incoming_json);

                let r#type = incoming_json.get("type").expect("type should be set");
                if r#type == "request" {
                    let config_done_tx = config_done_tx.take().unwrap();
                    if let Err(e) = CodeLLDBDebugger::handle_codelldb_request(
                        incoming_json,
                        seq_ids.clone(),
                        neovim_vadre_window.clone(),
                        write_tx.clone(),
                        Some(config_done_tx),
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
                } else if r#type == "response" {
                    if let Err(e) = CodeLLDBDebugger::handle_codelldb_response(
                        incoming_json,
                        response_senders.clone(),
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
                } else if r#type == "event" {
                    if let Err(e) = CodeLLDBDebugger::handle_codelldb_event(
                        incoming_json,
                        seq_ids.clone(),
                        neovim_vadre_window.clone(),
                        write_tx.clone(),
                        response_senders.clone(),
                        codelldb_data.clone(),
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
            }
        });

        let write_rx = self
            .write_rx
            .lock()
            .await
            .take()
            .expect("Should have a write_rx to take");

        tokio::spawn(async move {
            let mut write_rx = write_rx;
            let mut write_conn = write_conn;
            while let Some(msg) = write_rx.recv().await {
                let msg = msg.to_string();
                let msg = format!("Content-Length: {}\r\n\r\n{}", msg.len(), msg);
                tracing::trace!("Sending to CodeLLDB: {:?}", msg);
                write_conn
                    .write_all(msg.as_bytes())
                    .await
                    .expect("write should succeed");
            }
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
        self.send_request_and_await_response(
            "initialize",
            serde_json::json!({
                "adapterID": "CodeLLDB",
                "clientID": "nvim_vadre",
                "clientName": "nvim_vadre",
                "linesStartAt1": true,
                "columnsStartAt1": true,
                "locale": "en_GB",
                "pathFormat": "path",
                "supportsVariableType": true,
                "supportsVariablePaging": false,
                "supportsRunInTerminalRequest": true,
                "supportsMemoryReferences": true
            }),
        )
        .await?;

        #[cfg(windows)]
        let program = dunce::canonicalize(&self.command)?;
        #[cfg(not(windows))]
        let program = std::fs::canonicalize(&self.command)?;

        self.send_request(
            "launch",
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
        )
        .await?;

        self.send_request(
            "setFunctionBreakpoints",
            serde_json::json!({
                "breakpoints": [],
            }),
        )
        .await?;

        self.send_request(
            "setExceptionBreakpoints",
            serde_json::json!({
                "filters": []
            }),
        )
        .await?;

        for breakpoint in pending_breakpoints {
            self.send_breakpoints_request(breakpoint.0.clone(), breakpoint.1)
                .await?;
        }

        timeout(Duration::new(10, 0), config_done_rx).await??;

        self.send_request_and_await_response("configurationDone", serde_json::json!({}))
            .await?;

        Ok(())
    }

    #[tracing::instrument(skip(self, args))]
    async fn send_request(&self, command: &str, args: serde_json::Value) -> Result<()> {
        let seq_id = self.seq_ids.fetch_add(1, Ordering::SeqCst);

        CodeLLDBDebugger::do_send_request(seq_id, command, args, self.write_tx.clone()).await
    }

    #[tracing::instrument(skip(self, args))]
    async fn send_request_and_await_response(
        &self,
        command: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value> {
        CodeLLDBDebugger::do_send_request_and_await_response(
            self.seq_ids.clone(),
            command,
            args,
            self.write_tx.clone(),
            self.response_senders.clone(),
        )
        .await
    }

    #[tracing::instrument(skip(self))]
    async fn send_breakpoints_request(
        &self,
        file_path: String,
        line_numbers: &HashSet<i64>,
    ) -> Result<()> {
        let file_name = Path::new(&file_path).file_name().unwrap().to_str().unwrap();

        let breakpoints = line_numbers
            .into_iter()
            .map(|x| return serde_json::json!({ "line": x }))
            .collect::<Vec<serde_json::Value>>();
        let breakpoints = serde_json::json!(breakpoints);

        self.send_request(
            "setBreakpoints",
            serde_json::json!({
                "source": {
                    "name": file_name,
                    "path": file_path,
                },
                "breakpoints": breakpoints,
                "sourceModified": false,
            }),
        )
        .await?;

        Ok(())
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
    #[tracing::instrument(skip(json, seq_ids, neovim_vadre_window, write_tx, config_done_tx))]
    async fn handle_codelldb_request(
        json: serde_json::Value,
        seq_ids: Arc<AtomicU64>,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        write_tx: mpsc::Sender<serde_json::Value>,
        mut config_done_tx: Option<oneshot::Sender<()>>,
    ) -> Result<()> {
        // CodeLLDB is requesting something from us, currently only runTerminal should be received
        let command = json.get("command").expect("command should be set");
        if command != "runInTerminal" {
            bail!("Unknown request from CodeLLDB: {}", json);
        }

        neovim_vadre_window
            .lock()
            .await
            .log_msg(
                VadreLogLevel::INFO,
                "Spawning terminal to communicate with program",
            )
            .await?;

        let args = json
            .get("arguments")
            .expect("arguments should be set")
            .get("args")
            .expect("args should be set")
            .as_array()
            .expect("should be an array")
            .into_iter()
            .map(|x| x.to_string())
            .collect::<Vec<String>>();

        neovim_vadre_window
            .lock()
            .await
            .spawn_terminal_command(args.join(" "))
            .await?;

        let seq_id = seq_ids.clone().fetch_add(1, Ordering::SeqCst);
        let req_id = json.get("seq").expect("seq should be set");

        let response = serde_json::json!({
            "seq_id": seq_id,
            "type": "response",
            "request_seq": req_id,
            "command": "runInTerminal",
            "body": {
            },
            "success": true
        });

        tracing::debug!("--> {}", response);

        // One counterexample where we send a response, otherwise this should
        // always use do_send_request to do a send on `write_tx`.
        write_tx.clone().send(response).await?;

        config_done_tx.take().unwrap().send(()).unwrap();

        Ok(())
    }

    #[tracing::instrument(skip(json, response_senders))]
    async fn handle_codelldb_response(
        json: serde_json::Value,
        response_senders: Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>,
    ) -> Result<()> {
        let seq_id = json
            .get("request_seq")
            .expect("request_seq should be set")
            .as_u64()
            .expect("request_seq should be u64");
        let mut response_senders_lock = response_senders.lock().await;
        match response_senders_lock.remove(&seq_id) {
            Some(sender) => {
                sender.send(json).unwrap();
                tracing::trace!("Sent JSON response to request");
            }
            None => {}
        };
        Ok(())
    }

    #[tracing::instrument(skip(
        json,
        seq_ids,
        neovim_vadre_window,
        write_tx,
        response_senders,
        codelldb_data,
    ))]
    async fn handle_codelldb_event(
        json: serde_json::Value,
        seq_ids: Arc<AtomicU64>,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        write_tx: mpsc::Sender<serde_json::Value>,
        response_senders: Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>,
        codelldb_data: Arc<Mutex<CodeLLDBDebuggerData>>,
    ) -> Result<()> {
        tracing::trace!("Processing event: {}", json);
        let event = json
            .get("event")
            .expect("event should be set")
            .as_str()
            .expect("event should be a str");
        match event {
            "output" => {
                neovim_vadre_window
                    .lock()
                    .await
                    .log_msg(
                        VadreLogLevel::INFO,
                        &format!(
                            "CodeLLDB: {}",
                            json.get("body")
                                .expect("body should be set")
                                .get("output")
                                .expect("output should be set")
                        ),
                    )
                    .await
            }
            "stopped" => {
                CodeLLDBDebugger::handle_codelldb_event_stopped(
                    json,
                    seq_ids,
                    neovim_vadre_window,
                    write_tx,
                    response_senders,
                    codelldb_data,
                )
                .await
            }
            "continued" => {
                CodeLLDBDebugger::handle_codelldb_event_continued(
                    json,
                    seq_ids,
                    neovim_vadre_window,
                    write_tx,
                    response_senders,
                    codelldb_data,
                )
                .await
            }
            _ => Ok(()),
        }
    }

    #[tracing::instrument(skip(
        json,
        seq_ids,
        neovim_vadre_window,
        write_tx,
        response_senders,
        codelldb_data,
    ))]
    async fn handle_codelldb_event_stopped(
        json: serde_json::Value,
        seq_ids: Arc<AtomicU64>,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        write_tx: mpsc::Sender<serde_json::Value>,
        response_senders: Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>,
        codelldb_data: Arc<Mutex<CodeLLDBDebuggerData>>,
    ) -> Result<()> {
        let neovim_vadre_window = neovim_vadre_window.clone();

        let thread_id = match json
            .get("body")
            .expect("body should be set")
            .get("threadId")
        {
            Some(thread_id) => thread_id.as_u64(),
            None => None,
        };

        codelldb_data.lock().await.current_thread_id = thread_id;

        tokio::spawn(async move {
            log_err!(
                CodeLLDBDebugger::process_output_info(
                    seq_ids.clone(),
                    write_tx.clone(),
                    response_senders.clone(),
                    neovim_vadre_window.clone(),
                    codelldb_data,
                )
                .await,
                neovim_vadre_window,
                "can get threads"
            );

            if let Some(thread_id) = thread_id {
                if let Err(e) = CodeLLDBDebugger::process_stopped(
                    thread_id,
                    seq_ids,
                    write_tx,
                    response_senders,
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

    #[tracing::instrument(skip(_json, seq_ids, neovim_vadre_window, write_tx, response_senders,))]
    async fn handle_codelldb_event_continued(
        _json: serde_json::Value,
        seq_ids: Arc<AtomicU64>,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        write_tx: mpsc::Sender<serde_json::Value>,
        response_senders: Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>,
        codelldb_data: Arc<Mutex<CodeLLDBDebuggerData>>,
    ) -> Result<()> {
        let neovim_vadre_window = neovim_vadre_window.clone();

        tokio::spawn(async move {
            log_err!(
                CodeLLDBDebugger::process_output_info(
                    seq_ids.clone(),
                    write_tx.clone(),
                    response_senders.clone(),
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

    /// Actually send the request, should be the only function that does this, even the function
    /// with the `&self` parameter still uses this in turn.
    #[tracing::instrument(skip(seq_id, command, args, write_tx))]
    async fn do_send_request(
        seq_id: u64,
        command: &str,
        args: serde_json::Value,
        write_tx: mpsc::Sender<serde_json::Value>,
    ) -> Result<()> {
        let request = serde_json::json!({
            "command": command,
            "arguments": args,
            "seq": seq_id,
            "type": "request",
        });

        tracing::debug!("--> {}", request);
        write_tx.send(request).await?;

        Ok(())
    }

    /// Actually send the request and await the response. Used in turn by the equivalent function
    /// with the `&self` parameter.
    #[tracing::instrument(skip(seq_ids, command, args, write_tx, response_senders))]
    async fn do_send_request_and_await_response(
        seq_ids: Arc<AtomicU64>,
        command: &str,
        args: serde_json::Value,
        write_tx: mpsc::Sender<serde_json::Value>,
        response_senders: Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>,
    ) -> Result<serde_json::Value> {
        let (sender, receiver) = oneshot::channel();

        let seq_id = seq_ids.clone().fetch_add(1, Ordering::SeqCst);

        response_senders.lock().await.insert(seq_id, sender);

        CodeLLDBDebugger::do_send_request(seq_id, command, args, write_tx).await?;

        // TODO: configurable timeout
        let response = match timeout(Duration::new(10, 0), receiver).await {
            Ok(resp) => resp?,
            Err(e) => bail!("Timed out waiting for a response: {}", e),
        };
        tracing::trace!("Response: {}", response.to_string());

        Ok(response)
    }

    #[tracing::instrument(skip(
        thread_id,
        seq_ids,
        write_tx,
        response_senders,
        neovim_vadre_window
    ))]
    async fn process_stopped(
        thread_id: u64,
        seq_ids: Arc<AtomicU64>,
        write_tx: mpsc::Sender<serde_json::Value>,
        response_senders: Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    ) -> Result<()> {
        tracing::debug!("Thread id {} stopped", thread_id);

        let stack_response = CodeLLDBDebugger::do_send_request_and_await_response(
            seq_ids.clone(),
            "stackTrace",
            serde_json::json!({ "threadId": thread_id, "levels": 1 }),
            write_tx.clone(),
            response_senders.clone(),
        )
        .await?;

        let stack = stack_response
            .get("body")
            .expect("should have body")
            .get("stackFrames")
            .expect("should have stackFrames");
        let current_frame = stack.get(0).expect("should have a top frame");
        let source = current_frame.get("source").expect("should have a source");
        let line_number = current_frame
            .get("line")
            .expect("should have a line")
            .as_i64()
            .expect("line should be an i64");

        tracing::trace!("Stop at {:?}:{}", source, line_number);

        if let Some(source_file) = source.get("path") {
            let source_file = source_file.as_str().expect("path is a string").to_string();
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
        } else if let Some(source_reference_id) = current_frame
            .get("source")
            .expect("should have a source")
            .get("sourceReference")
        {
            let source_reference_response = CodeLLDBDebugger::do_send_request_and_await_response(
                seq_ids,
                "source",
                serde_json::json!({
                    "source": { "sourceReference": source_reference_id },
                    "sourceReference": source_reference_id
                }),
                write_tx.clone(),
                response_senders.clone(),
            )
            .await?;

            tracing::trace!("source reference {}", source_reference_response);

            let content = source_reference_response
                .get("body")
                .expect("body should be set")
                .get("content")
                .expect("content should be set")
                .as_str()
                .expect("content should be a string")
                .to_string();

            neovim_vadre_window
                .lock()
                .await
                .set_code_buffer(
                    CodeBufferContent::Content(content),
                    line_number,
                    &format!("Disassembled Code {}", source_reference_id),
                    // TODO: Do we need to reset this every time, feels like it might update...
                    true,
                )
                .await?;
        } else {
            bail!("Can't find any source to display");
        }

        Ok(())
    }

    #[tracing::instrument(skip(
        seq_ids,
        write_tx,
        response_senders,
        neovim_vadre_window,
        codelldb_data
    ))]
    async fn process_output_info(
        seq_ids: Arc<AtomicU64>,
        write_tx: mpsc::Sender<serde_json::Value>,
        response_senders: Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        codelldb_data: Arc<Mutex<CodeLLDBDebuggerData>>,
    ) -> Result<()> {
        tracing::debug!("Getting thread information");

        let mut call_stack_buffer_content = Vec::new();
        let current_thread_id = codelldb_data.lock().await.current_thread_id;

        let threads_response = CodeLLDBDebugger::do_send_request_and_await_response(
            seq_ids.clone(),
            "threads",
            serde_json::json!({}),
            write_tx.clone(),
            response_senders.clone(),
        )
        .await?;

        for thread in threads_response
            .get("body")
            .expect("body is set")
            .get("threads")
            .expect("threads is set")
            .as_array()
            .expect("threads is array")
        {
            let thread_id = thread
                .get("id")
                .expect("thread should have id")
                .as_u64()
                .expect("thread is should be u64");
            let thread_name = thread
                .get("name")
                .expect("thread should have name")
                .as_str()
                .expect("thread is should be str");

            if current_thread_id == Some(thread_id) {
                call_stack_buffer_content.push(format!("{} (*)", thread_name));

                let stack_trace_response = CodeLLDBDebugger::do_send_request_and_await_response(
                    seq_ids.clone(),
                    "stackTrace",
                    serde_json::json!({
                        "threadId": current_thread_id,
                    }),
                    write_tx.clone(),
                    response_senders.clone(),
                )
                .await?;

                // Sometimes we don't get a body here as we get a message saying invalid thread,
                // normally when the thread is doing something in blocking.
                if let Some(body) = stack_trace_response.get("body") {
                    let frames = body
                        .get("stackFrames")
                        .expect("stackFrames should be set")
                        .as_array()
                        .expect("stackFrames should be an array");

                    let top_frame = frames.get(0).expect("has a frame on the stack trace");
                    let frame_id = top_frame
                        .get("id")
                        .expect("frame has id")
                        .as_u64()
                        .expect("frame_id is u64");
                    codelldb_data.lock().await.current_frame_id = Some(frame_id);

                    CodeLLDBDebugger::process_variables(
                        frame_id,
                        seq_ids.clone(),
                        write_tx.clone(),
                        response_senders.clone(),
                        neovim_vadre_window.clone(),
                    )
                    .await?;

                    for frame in frames {
                        let frame_name = frame
                            .get("name")
                            .expect("should have a name")
                            .as_str()
                            .expect("frame name should be a string");
                        call_stack_buffer_content.push(format!("+ {}", frame_name));

                        let line_number = frame
                            .get("line")
                            .expect("line number should be set")
                            .as_u64()
                            .expect("line number should be u64");
                        let source = frame.get("source").expect("should have a source");
                        let source_name = source
                            .get("name")
                            .expect("source name should be set")
                            .as_str()
                            .expect("source name should be a string");
                        if let Some(_) = source.get("path") {
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
            } else {
                let stack_trace_response = CodeLLDBDebugger::do_send_request_and_await_response(
                    seq_ids.clone(),
                    "stackTrace",
                    serde_json::json!({
                        "levels": 1,
                        "threadId": thread_id,
                    }),
                    write_tx.clone(),
                    response_senders.clone(),
                )
                .await?;

                // Sometimes we don't get a body here as we get a message saying invalid thread,
                // normally when the thread is doing something in blocking.
                if let Some(body) = stack_trace_response.get("body") {
                    let frame_name = body
                        .get("stackFrames")
                        .expect("stackFrames should be set")
                        .get(0)
                        .expect("should have a frame")
                        .get("name")
                        .expect("should have a name")
                        .as_str()
                        .expect("frame name should be a string");

                    call_stack_buffer_content.push(format!("{} - {}", thread_name, frame_name));
                } else {
                    call_stack_buffer_content.push(format!("{}", thread_name));
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

    #[tracing::instrument(skip(seq_ids, write_tx, response_senders, neovim_vadre_window,))]
    async fn process_variables(
        frame_id: u64,
        seq_ids: Arc<AtomicU64>,
        write_tx: mpsc::Sender<serde_json::Value>,
        response_senders: Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    ) -> Result<()> {
        tracing::debug!("Getting variable information");

        let mut variable_content = Vec::new();

        let scopes_response = CodeLLDBDebugger::do_send_request_and_await_response(
            seq_ids.clone(),
            "scopes",
            serde_json::json!({
                "frameId": frame_id,
            }),
            write_tx.clone(),
            response_senders.clone(),
        )
        .await?;

        let scopes = scopes_response
            .get("body")
            .expect("body should be set")
            .get("scopes")
            .expect("scopes should be set")
            .as_array()
            .expect("scopes should be an array");

        for scope in scopes {
            let scope_name = scope
                .get("name")
                .expect("scope name should be set")
                .as_str()
                .expect("scope name should be a string");

            variable_content.push(format!("{}:", scope_name));

            let variable_reference = scope
                .get("variablesReference")
                .expect("variablesReference should be set")
                .as_u64()
                .expect("variablesReference should be u64");

            let variables_response = CodeLLDBDebugger::do_send_request_and_await_response(
                seq_ids.clone(),
                "variables",
                serde_json::json!({
                    "variablesReference": variable_reference,
                }),
                write_tx.clone(),
                response_senders.clone(),
            )
            .await?;

            let variables = variables_response
                .get("body")
                .expect("body is set")
                .get("variables")
                .expect("variables is set")
                .as_array()
                .expect("variables is an array");

            for variable in variables {
                let name = variable
                    .get("name")
                    .expect("variable name should be set")
                    .as_str()
                    .expect("variable name should be a string");

                let value = variable
                    .get("value")
                    .expect("variable value should be set")
                    .as_str()
                    .expect("variable value should be a string");

                if let Some(r#type) = variable.get("type") {
                    let r#type = r#type.as_str().expect("variable type should be a string");
                    variable_content.push(format!("+ ({}) {} = {:?}", r#type, name, value));
                } else {
                    variable_content.push(format!("+ {} = {:?}", name, value));
                }
            }
        }

        neovim_vadre_window
            .lock()
            .await
            .set_variables_buffer(variable_content)
            .await?;

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
