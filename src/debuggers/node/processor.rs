use crate::{
    debuggers::RequestTimeout,
    neovim::{NeovimVadreWindow, VadreLogLevel},
    util::{get_unused_localhost_port, merge_json},
};

use std::{
    collections::HashMap,
    env::consts::EXE_SUFFIX,
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Result};
use futures::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    process::Child,
    sync::{broadcast, mpsc, oneshot, Mutex},
    time::timeout,
};
use tokio_tungstenite::{tungstenite::Message as WsMessage, MaybeTlsStream, WebSocketStream};

/// Responsible for spawning the process and handling the communication with the debugger
pub(crate) struct DebuggerProcessor {
    neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    process: Arc<Mutex<Option<Child>>>,

    /// Allows us to send to debugger
    debugger_sender_tx: Option<mpsc::Sender<serde_json::Value>>,

    /// Mostly to allow new subscribers
    debugger_receiver_tx: Option<broadcast::Sender<serde_json::Value>>,

    /// Allows us to receive from the debugger
    seq_ids: AtomicU32,
    request_timeout: RequestTimeout,
}

impl DebuggerProcessor {
    pub(crate) fn new(neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>) -> Self {
        Self {
            neovim_vadre_window: neovim_vadre_window.clone(),
            process: Arc::new(Mutex::new(None)),

            debugger_sender_tx: None,
            debugger_receiver_tx: None,

            seq_ids: AtomicU32::new(1),
            request_timeout: RequestTimeout::new(neovim_vadre_window),
        }
    }

    pub(crate) async fn setup(
        &mut self,
        command_args: Vec<String>,
        environment_variables: HashMap<String, String>,
    ) -> Result<()> {
        let ws_url = self
            .launch_node_debugger(command_args, environment_variables)
            .await?;

        self.ws_connect(ws_url).await?;

        Ok(())
    }

    pub(crate) fn subscribe_debugger(&self) -> Result<broadcast::Receiver<serde_json::Value>> {
        Ok(self
            .debugger_receiver_tx
            .as_ref()
            .ok_or_else(|| {
                anyhow!("Couldn't get debugger_receiver_tx, was process initialised correctly")
            })?
            .subscribe())
    }

    pub(crate) async fn request(
        &self,
        mut req: serde_json::Value,
        response_sender: Option<oneshot::Sender<serde_json::Value>>,
    ) -> Result<()> {
        let id = self.seq_ids.fetch_add(1, Ordering::SeqCst);
        let id_req = serde_json::json!({
            "id": id,
        });
        merge_json(&mut req, id_req);

        let request_timeout = self.request_timeout.get_or_set().await;

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

        debugger_sender_tx.send(req).await?;

        tokio::spawn(async move {
            let response = loop {
                let response = match timeout(request_timeout, debugger_receiver_rx.recv()).await {
                    Ok(resp) => resp?,
                    Err(e) => bail!("Timed out waiting for a response: {}", e),
                };

                if let Some(resp_id) = response.get("id") {
                    if resp_id == id {
                        break response;
                    }
                }
            };

            if let Some(response_sender) = response_sender {
                tracing::trace!("Sending response back: {:?}", response);
                response_sender.send(response).map_err(|e| {
                    anyhow!(
                        "Couldn't send response to response_sender, was it dropped? {:?}",
                        e
                    )
                })?;
            }

            Ok(())
        });

        Ok(())
    }

    async fn launch_node_debugger(
        &mut self,
        mut command_args: Vec<String>,
        environment_variables: HashMap<String, String>,
    ) -> Result<String> {
        let port = get_unused_localhost_port()?;

        let node_path = if let Some(maybe_program) = command_args.get(0) {
            let maybe_program = PathBuf::from(maybe_program);
            let maybe_program_filename = maybe_program.file_name().map(|x| x.to_str().unwrap());
            if maybe_program_filename == Some("node")
                || maybe_program_filename == Some(format!("node{}", EXE_SUFFIX).as_str())
            {
                command_args.remove(0);

                dunce::canonicalize(which::which("node")?)?
            } else {
                dunce::canonicalize(which::which("node")?)?
            }
        } else {
            bail!("No command specified");
        };

        if !node_path.exists() {
            bail!("NodeJS program doesn't exist: {:?}", node_path);
        }

        let msg = format!(
            "Launching Node process {:?} with args: {:?}",
            node_path, command_args
        );
        self.neovim_vadre_window
            .lock()
            .await
            .log_msg(VadreLogLevel::DEBUG, &msg)
            .await?;

        let mut args = vec![];

        args.push(format!("--inspect-brk={}", port));
        args.extend(command_args);

        let mut child = tokio::process::Command::new(node_path)
            .args(args)
            .envs(environment_variables)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("Failed to spawn NodeJS: {:?}", e))?;

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

        self.neovim_vadre_window
            .lock()
            .await
            .log_msg(VadreLogLevel::DEBUG, "Process spawned".into())
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

    #[tracing::instrument(skip(self, ws_stream))]
    async fn handle_stream(
        &mut self,
        ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    ) -> Result<()> {
        let (debugger_sender_tx, mut debugger_sender_rx) = mpsc::channel(1);
        // Can theoretically get a lot of messages from the debugger. Especially true for node.
        let (debugger_receiver_tx, _debugger_receiver_rx) = broadcast::channel(200);

        self.debugger_sender_tx = Some(debugger_sender_tx);
        self.debugger_receiver_tx = Some(debugger_receiver_tx.clone());

        let neovim_vadre_window = self.neovim_vadre_window.clone();

        tokio::spawn(async move {
            let mut ws_stream = ws_stream;

            loop {
                tokio::select! {
                    msg = ws_stream.next() => {
                        match msg {
                            Some(Ok(msg)) => {
                                tracing::trace!("Received message: {}", msg);
                                match msg {
                                    WsMessage::Text(msg) => {
                                        let msg: serde_json::Value = serde_json::from_str(&msg).expect("can convert WS message text string to json");
                                        debugger_receiver_tx.send(msg)?;
                                    }
                                    WsMessage::Binary(_) |
                                    WsMessage::Ping(_) |
                                    WsMessage::Pong(_) |
                                    WsMessage::Close(_) |
                                    WsMessage::Frame(_) => {
                                        tracing::error!("Received message from debugger, this is not supported: {:?}", msg);
                                        neovim_vadre_window
                                            .clone()
                                            .lock()
                                            .await
                                            .log_msg(VadreLogLevel::ERROR, "Received message from debugger that is not supported")
                                            .await?;
                                    },
                                }
                            },
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
                                    .await?;
                                break;
                            }
                        };
                    }
                    Some(msg) = debugger_sender_rx.recv() => {
                        tracing::trace!("Sending message: {}", msg);
                        let msg = WsMessage::Text(msg.to_string());
                        ws_stream.send(msg).await?;
                    }
                }
            }

            Ok::<(), anyhow::Error>(())
        });

        Ok(())
    }
}
