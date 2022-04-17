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
    neovim::{NeovimVadreWindow, VadreLogLevel},
    util::{self, get_debuggers_dir, log_err, ret_err},
};

use anyhow::{bail, Result};
use nvim_rs::{compat::tokio::Compat, Neovim};
use reqwest::Url;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, Stdout},
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
    pub async fn setup(
        &mut self,
        pending_breakpoints: &HashMap<String, HashSet<i64>>,
    ) -> Result<()> {
        match self {
            Debugger::CodeLLDB(debugger) => debugger.setup(pending_breakpoints).await,
        }
    }

    pub fn neovim_vadre_window(&self) -> &NeovimVadreWindow {
        match self {
            Debugger::CodeLLDB(debugger) => &debugger.neovim_vadre_window,
        }
    }

    pub async fn set_source_breakpoints(
        &self,
        file_path: String,
        line_numbers: &HashSet<i64>,
    ) -> Result<()> {
        match self {
            Debugger::CodeLLDB(debugger) => {
                debugger.set_breakpoints(file_path, line_numbers).await?
            }
        }
        Ok(())
    }

    pub async fn do_step(&self, step_type: DebuggerStepType) -> Result<()> {
        match self {
            Debugger::CodeLLDB(debugger) => debugger.do_step(step_type).await?,
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct CodeLLDBDebugger {
    id: usize,
    command: String,
    command_args: Vec<String>,
    seq_ids: Arc<AtomicU64>,
    pub neovim_vadre_window: NeovimVadreWindow,
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

    main_thread_id: Arc<Mutex<Option<u64>>>,
}

impl CodeLLDBDebugger {
    pub fn new(
        id: usize,
        command: String,
        command_args: Vec<String>,
        neovim: Neovim<Compat<Stdout>>,
    ) -> Self {
        let neovim_vadre_window = NeovimVadreWindow::new(neovim, id);

        let (write_tx, write_rx) = mpsc::channel(1);

        CodeLLDBDebugger {
            id,
            command,
            command_args,
            seq_ids: Arc::new(AtomicU64::new(1)),
            neovim_vadre_window,
            process: None,

            write_tx,
            write_rx: Arc::new(Mutex::new(Some(write_rx))),

            response_senders: Arc::new(Mutex::new(HashMap::new())),

            main_thread_id: Arc::new(Mutex::new(None)),
        }
    }

    #[tracing::instrument(skip(self, pending_breakpoints))]
    pub async fn setup(
        &mut self,
        pending_breakpoints: &HashMap<String, HashSet<i64>>,
    ) -> Result<()> {
        log_err!(
            self.neovim_vadre_window.create_ui().await,
            self.neovim_vadre_window,
            "Error setting up Vadre UI"
        );

        let port = util::get_unused_localhost_port();

        log_err!(
            self.launch(port).await,
            self.neovim_vadre_window,
            "Error launching process"
        );

        let (config_done_tx, config_done_rx) = oneshot::channel();
        log_err!(
            self.tcp_connect(port, config_done_tx).await,
            self.neovim_vadre_window,
            "Error creating TCP connection to process"
        );
        log_err!(
            self.init_process(pending_breakpoints, config_done_rx).await,
            self.neovim_vadre_window,
            "Error initialising process"
        );
        ret_err!(
            self.neovim_vadre_window
                .log_msg(VadreLogLevel::INFO, "CodeLLDB launched and setup")
                .await
        );

        Ok(())
    }

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

    pub async fn do_step(&self, step_type: DebuggerStepType) -> Result<()> {
        let thread_id = self.main_thread_id.lock().await.unwrap();

        let command = match step_type {
            DebuggerStepType::Over => "next",
            DebuggerStepType::In => "stepIn",
            DebuggerStepType::Continue => "continue",
        };

        self.send_request(
            command,
            serde_json::json!({
                "threadId": thread_id,
                "singleThread": false,
            }),
        )
        .await?;

        Ok(())
    }

    #[tracing::instrument(skip(self, port))]
    async fn launch(&mut self, port: u16) -> Result<()> {
        let msg = format!(
            "Launching process {:?} with lldb and args: {:?}",
            self.command, self.command_args,
        );
        self.neovim_vadre_window
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
        self.process = Some(Arc::new(
            Command::new(path)
                .args(["--port", &port.to_string()])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("Failed to spawn debugger"),
        ));
        self.neovim_vadre_window
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
        let main_thread_id = self.main_thread_id.clone();

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

                let response_json: serde_json::Value =
                    serde_json::from_str(&remainder[0..content_length]).unwrap();

                // TODO: Can we do this more efficiently somehow? Use the `Bytes` crate and
                // consume? Is that actually more efficient?
                buf_str = String::from(&remainder[content_length..]);

                tracing::trace!("JSON from CodeLLDB: {}", response_json);

                let r#type = response_json.get("type").expect("type should be set");
                if r#type == "request" {
                    // CodeLLDB is requesting something from us, currently only runTerminal should be received
                    let command = response_json.get("command").expect("command should be set");
                    if command == "runInTerminal" {
                        neovim_vadre_window
                            .log_msg(
                                VadreLogLevel::INFO,
                                "Spawning terminal to communicate with program",
                            )
                            .await
                            .expect("Logging failed");

                        let args = response_json
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
                            .spawn_terminal_command(args.join(" "))
                            .await
                            .expect("Spawning terminal command failed");

                        let seq_id = seq_ids.clone().fetch_add(1, Ordering::SeqCst);
                        let req_id = response_json.get("seq").expect("seq should be set");

                        let response = serde_json::json!({
                            "seq_id": seq_id,
                            "type": "response",
                            "request_seq": req_id,
                            "command": "runInTerminal",
                            "body": {
                            },
                            "success": true
                        });

                        tracing::trace!("Sending response to runInTerminal: {}", response);

                        write_tx
                            .clone()
                            .send(response)
                            .await
                            .expect("Can send to sender for socket");

                        config_done_tx.take().unwrap().send(()).unwrap();
                    } else {
                        neovim_vadre_window
                            .log_msg(
                                VadreLogLevel::WARN,
                                &format!("Unknown request from CodeLLDB: {}", response_json),
                            )
                            .await
                            .expect("Logging failed");
                    }
                } else if r#type == "response" {
                    let seq_id = response_json
                        .get("request_seq")
                        .expect("request_seq should be set")
                        .as_u64()
                        .unwrap();
                    let mut response_senders_lock = response_senders.lock().await;
                    match response_senders_lock.remove(&seq_id) {
                        Some(sender) => {
                            sender.send(response_json).unwrap();
                            tracing::trace!("Sent JSON response to request");
                        }
                        None => {}
                    };
                } else if r#type == "event" {
                    tracing::debug!("Got event: {}", response_json);
                    let event = response_json.get("event").expect("event should be set");
                    if event == "output" {
                        neovim_vadre_window
                            .log_msg(
                                VadreLogLevel::INFO,
                                &format!(
                                    "CodeLLDB: {}",
                                    response_json
                                        .get("body")
                                        .expect("body should be set")
                                        .get("output")
                                        .expect("output should be set")
                                ),
                            )
                            .await
                            .expect("Logging failed");
                    } else if event == "stopped" {
                        let thread_id = response_json
                            .get("body")
                            .expect("body should be set")
                            .get("threadId")
                            .expect("threadId should be set")
                            .as_u64()
                            .expect("threadId should be u64");

                        *main_thread_id.lock().await = Some(thread_id);

                        let seq_ids = seq_ids.clone();
                        let write_tx = write_tx.clone();
                        let response_senders = response_senders.clone();
                        let neovim_vadre_window = neovim_vadre_window.clone();

                        tokio::spawn(async move {
                            CodeLLDBDebugger::process_stopped(
                                thread_id,
                                seq_ids,
                                write_tx,
                                response_senders,
                                &neovim_vadre_window,
                            )
                            .await;
                        });
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

    #[tracing::instrument(skip(self))]
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
                "stopOnEntry": false
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

    async fn send_request(&self, command: &str, args: serde_json::Value) -> Result<()> {
        let seq_id = self.seq_ids.fetch_add(1, Ordering::SeqCst);

        let request = serde_json::json!({
            "command": command,
            "arguments": args,
            "seq": seq_id,
            "type": "request",
        });

        tracing::debug!("Request: {}", request);
        self.write_tx.send(request).await?;

        Ok(())
    }

    async fn send_request_and_await_response(
        &self,
        command: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let seq_id = self.seq_ids.fetch_add(1, Ordering::SeqCst);

        CodeLLDBDebugger::do_send_request_and_await_response(
            seq_id,
            command,
            args,
            self.write_tx.clone(),
            self.response_senders.clone(),
        )
        .await
    }

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
            self.neovim_vadre_window
                .log_msg(VadreLogLevel::INFO, "Downloading and extracting pLugin")
                .await?;

            let url = Url::parse(&format!(
                "https://github.com/vadimcn/vscode-lldb/releases/download/{}\
                         /codelldb-x86_64-{}.vsix",
                "v1.7.0", "windows"
            ))?;

            download_extract_zip(path.as_path(), url).await?;
        }

        Ok(())
    }

    async fn do_send_request_and_await_response(
        seq_id: u64,
        command: &str,
        args: serde_json::Value,
        write_tx: mpsc::Sender<serde_json::Value>,
        response_senders: Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>,
    ) -> Result<serde_json::Value> {
        let (sender, receiver) = oneshot::channel();

        let request = serde_json::json!({
            "command": command,
            "arguments": args,
            "seq": seq_id,
            "type": "request",
        });

        response_senders.lock().await.insert(seq_id, sender);

        tracing::debug!("Request: {}", request);

        write_tx.send(request).await?;

        // TODO: configurable timeout
        let response = match timeout(Duration::new(10, 0), receiver).await {
            Ok(resp) => resp?,
            Err(e) => bail!("Timed out waiting for a response: {}", e),
        };
        tracing::debug!("Response: {}", response.to_string());

        Ok(response)
    }

    async fn process_stopped(
        thread_id: u64,
        seq_ids: Arc<AtomicU64>,
        write_tx: mpsc::Sender<serde_json::Value>,
        response_senders: Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>,
        neovim_vadre_window: &NeovimVadreWindow,
    ) {
        tracing::debug!("Thread id {} stopped", thread_id);

        let seq_id = seq_ids.clone().fetch_add(1, Ordering::SeqCst);

        let stack_response = CodeLLDBDebugger::do_send_request_and_await_response(
            seq_id,
            "stackTrace",
            serde_json::json!({ "threadId": thread_id }),
            write_tx.clone(),
            response_senders.clone(),
        )
        .await
        .expect("received response");

        let stack = stack_response
            .get("body")
            .expect("should have body")
            .get("stackFrames")
            .expect("should have stackFrames");
        let current_frame = stack.get(0).expect("should have a top frame");
        let source_file = current_frame
            .get("source")
            .expect("should have a source")
            .get("path")
            .expect("should have a path")
            .as_str()
            .expect("path is a string");
        let source_file = Path::new(&source_file);
        let line_number = current_frame
            .get("line")
            .expect("should have a line")
            .as_i64()
            .expect("line should be an i64");

        tracing::trace!("Stop at {:?}:{}", source_file, line_number);

        neovim_vadre_window
            .set_code_buffer(&source_file, line_number)
            .await
            .expect("can set source file in buffer");
    }
}

// We just make this synchronous because although it slows things down, it makes it much
// easier to do. If anyone wants to make this async and cool be my guest but it seems not
// easy.
async fn download_extract_zip(full_path: &Path, url: Url) -> Result<()> {
    tracing::trace!("Downloading {} and unzipping to {:?}", url, full_path);
    let zip_contents = reqwest::get(url).await?.bytes().await?;

    let reader = std::io::Cursor::new(zip_contents);
    let mut zip = zip::ZipArchive::new(reader)?;

    zip.extract(full_path)?;

    Ok(())
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
