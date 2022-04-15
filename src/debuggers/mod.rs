use std::{
    collections::HashMap,
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
    VadreResult,
};

use anyhow::{bail, Result};
use nvim_rs::{compat::tokio::Compat, Neovim, Value};
use reqwest::Url;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, Stdout},
    net::{tcp::OwnedWriteHalf, TcpStream},
    process::{Child, Command},
    sync::{oneshot, Mutex},
    time::{sleep, timeout},
};

#[derive(Clone, Debug)]
pub enum Debugger {
    CodeLLDB(CodeLLDBDebugger),
}

impl Debugger {
    pub async fn setup(&mut self) -> VadreResult {
        match self {
            Debugger::CodeLLDB(debugger) => debugger.setup().await,
        }
    }

    pub fn neovim_vadre_window(&self) -> &NeovimVadreWindow {
        match self {
            Debugger::CodeLLDB(debugger) => &debugger.neovim_vadre_window,
        }
    }
}

#[derive(Clone)]
pub struct CodeLLDBDebugger {
    id: usize,
    command: String,
    command_args: Vec<String>,
    seq_id: Arc<AtomicU64>,
    pub neovim_vadre_window: NeovimVadreWindow,
    process: Option<Arc<Child>>,
    write_conn: Arc<Mutex<Option<OwnedWriteHalf>>>,
    response_senders: Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>, // TODO: Clear up these, possibly needs a mutex
}

impl CodeLLDBDebugger {
    pub fn new(
        id: usize,
        command: String,
        command_args: Vec<String>,
        neovim: Neovim<Compat<Stdout>>,
    ) -> Self {
        let neovim_vadre_window = NeovimVadreWindow::new(neovim, id);

        CodeLLDBDebugger {
            id,
            command,
            command_args,
            seq_id: Arc::new(AtomicU64::new(100)),
            neovim_vadre_window,
            process: None,
            write_conn: Arc::new(Mutex::new(None)),
            response_senders: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[tracing::instrument(skip(self))]
    pub async fn setup(&mut self) -> VadreResult {
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
        log_err!(
            self.tcp_connect(port).await,
            self.neovim_vadre_window,
            "Error creating TCP connection to process"
        );
        log_err!(
            self.init_process().await,
            self.neovim_vadre_window,
            "Error initialising process"
        );
        ret_err!(
            self.neovim_vadre_window
                .log_msg(VadreLogLevel::INFO, "CodeLLDB launched and setup")
                .await
        );

        Ok(Value::from("CodeLLDB launched and setup"))
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
    async fn tcp_connect(&mut self, port: u16) -> Result<()> {
        tracing::trace!("Connecting to port {}", port);

        let neovim_vadre_window = self.neovim_vadre_window.clone();

        let tcp_conn = self.do_tcp_connect(port).await?;
        let (read_conn, write_conn) = tcp_conn.into_split();

        let response_senders = self.response_senders.clone();

        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let mut buf_str = String::new();
            let mut read_conn = read_conn;
            let mut previous_string_length = 0;

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

                tracing::debug!("JSON from CodeLLDB: {}", response_json);

                let r#type = response_json.get("type").expect("type should be set");
                if r#type == "request" {
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
                    } else {
                        neovim_vadre_window
                            .log_msg(
                                VadreLogLevel::INFO,
                                &format!("Need to code in request: {}", command),
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
                        }
                        None => {}
                    };
                    tracing::trace!("Sent JSON response to request");
                } else if r#type == "event" {
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
                    }
                }
            }
        });
        *self.write_conn.lock().await = Some(write_conn);

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
    async fn init_process(&self) -> Result<()> {
        self.send_request_and_retrieve_response(
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
                "stopOnEntry": true
            }),
            None,
        )
        .await?;

        self.send_request(
            "setFunctionBreakpoints",
            serde_json::json!({
                "breakpoints": [],
            }),
            None,
        )
        .await?;

        self.send_request(
            "setExceptionBreakpoints",
            serde_json::json!({
                "filters": []
            }),
            None,
        )
        .await?;

        Ok(())
    }

    async fn send_request(
        &self,
        command: &str,
        args: serde_json::Value,
        mut sender: Option<oneshot::Sender<serde_json::Value>>,
    ) -> Result<()> {
        let seq_id = self.seq_id.fetch_add(1, Ordering::SeqCst);

        if let Some(sender) = sender.take() {
            self.response_senders.lock().await.insert(seq_id, sender);
        }

        let request = serde_json::json!({
            "command": command,
            "arguments": args,
            "seq": seq_id,
            "type": "request",
        });
        let msg = request.to_string();

        let mut write_conn_lock = self.write_conn.lock().await;
        match write_conn_lock.as_mut() {
            Some(write_conn) => {
                tracing::trace!("Request: {}", msg);
                write_conn
                    .write_all(format!("Content-Length: {}\r\n\r\n{}", msg.len(), msg).as_bytes())
                    .await?;
            }
            None => {
                bail!("Can't find write_conn for CodeLLDB, have you called setup yet?");
            }
        };

        Ok(())
    }

    async fn send_request_and_retrieve_response(
        &self,
        command: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let (sender, receiver) = oneshot::channel();

        self.send_request(command, args, Some(sender)).await?;

        // TODO: configurable timeout
        let response = match timeout(Duration::new(10, 0), receiver).await {
            Ok(resp) => resp?,
            Err(e) => bail!("Timed out waiting for a response: {}", e),
        };
        tracing::trace!("Got response: {}", response.to_string());

        Ok(response)
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
