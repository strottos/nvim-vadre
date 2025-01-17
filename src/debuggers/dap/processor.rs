use std::{
    collections::VecDeque,
    fmt::Debug,
    io,
    process::Stdio,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use super::{
    debuggers::DapDebuggerType,
    protocol::{
        DAPCodec, DecoderResult, DisconnectArguments, Either, ProtocolMessage, ProtocolMessageType,
        RequestArguments, Response, ResponseResult,
    },
};
use crate::{
    debuggers::RequestTimeout,
    neovim::{NeovimVadreWindow, VadreLogLevel},
    util::get_unused_localhost_port,
};

use anyhow::{anyhow, bail, Result};
use futures::{prelude::*, StreamExt};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::TcpStream,
    process::{Child, Command},
    sync::{broadcast, mpsc, oneshot, Mutex},
    time::{sleep, timeout},
};
use tokio_util::codec::Decoder;

/// Responsible for spawning the process and handling the communication with the debugger
pub(crate) struct DebuggerProcessor {
    debugger_type: DapDebuggerType,

    neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    process: Arc<Mutex<Option<Child>>>,

    /// Allows us to send to debugger
    debugger_sender_tx: Option<mpsc::Sender<ProtocolMessage>>,

    /// Mostly to allow new subscribers
    debugger_receiver_tx: Option<broadcast::Sender<ProtocolMessage>>,

    /// Allows us to receive from the debugger
    seq_ids: AtomicU32,
    request_timeout: RequestTimeout,
}

impl DebuggerProcessor {
    pub(crate) fn new(
        debugger_type: DapDebuggerType,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    ) -> Self {
        Self {
            debugger_type,

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
        existing_debugger_port: Option<u16>,
        dap_command: Option<String>,
    ) -> Result<()> {
        let port;

        if let Some(p) = existing_debugger_port {
            port = Some(p);
        } else if let Some(cmd) = dap_command {
            let mut args = cmd
                .split_whitespace()
                .map(|x| x.to_string())
                .collect::<VecDeque<_>>();

            let cmd = args
                .pop_front()
                .ok_or_else(|| anyhow!("No command provided"))?;

            port = None;

            self.run(cmd, args.into(), false).await?;
        } else {
            port = Some(get_unused_localhost_port()?);

            self.launch_vadre_debugger(port).await?;
        };

        if let Some(port) = port {
            self.tcp_connect_and_handle(port).await?;
        } else {
            self.stdio_connect().await?;
        }

        self.neovim_vadre_window
            .lock()
            .await
            .log_msg(VadreLogLevel::INFO, "Debugger launched")
            .await?;

        Ok(())
    }

    pub(crate) async fn send_msg(&mut self, message: ProtocolMessage) -> Result<()> {
        let debugger_sender_tx = self.debugger_sender_tx.as_mut().ok_or_else(|| {
            anyhow!("Couldn't get debugger_sender_tx, was process initialised correctly")
        })?;
        debugger_sender_tx.send(message).await?;

        Ok(())
    }

    pub(crate) fn subscribe_debugger(&self) -> Result<broadcast::Receiver<ProtocolMessage>> {
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
        request_args: RequestArguments,
        response_sender: Option<oneshot::Sender<Response>>,
    ) -> Result<u32> {
        let seq_id = self.seq_ids.fetch_add(1, Ordering::SeqCst);

        let message = ProtocolMessage {
            seq: Either::First(seq_id),
            type_: ProtocolMessageType::Request(request_args),
        };

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

        debugger_sender_tx.send(message).await?;

        tokio::spawn(async move {
            let response = loop {
                let response = match timeout(request_timeout, debugger_receiver_rx.recv()).await {
                    Ok(resp) => resp?,
                    Err(e) => bail!("Timed out waiting for a response: {}", e),
                };

                if let ProtocolMessageType::Response(response) = response.type_ {
                    if *response.request_seq.first() == seq_id {
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

        Ok(seq_id)
    }

    pub(crate) async fn stop(&mut self) -> Result<()> {
        if let Some(child) = self.process.lock().await.as_mut() {
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

    async fn launch_vadre_debugger(&mut self, port: Option<u16>) -> Result<()> {
        let msg = format!(
            "Launching Vadre Command for: {}",
            self.debugger_type.get_debugger_type_name(),
        );
        self.neovim_vadre_window
            .lock()
            .await
            .log_msg(VadreLogLevel::DEBUG, &msg)
            .await?;

        self.debugger_type.download_plugin().await?;

        let child = self
            .debugger_type
            .spawn_child(port, vec!["./test_files/test_progs/test.py".to_string()])
            .await?;

        self.setup_stdio(child, true).await
    }

    async fn run(&mut self, cmd: String, args: Vec<String>, take_stdio: bool) -> Result<()> {
        tracing::debug!("Spawning process: {:?} {:?}", cmd, args);

        let child = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("Failed to spawn debugger: {}", e))?;

        self.setup_stdio(child, take_stdio).await
    }

    async fn setup_stdio(&mut self, mut child: Child, take_stdio: bool) -> Result<()> {
        if take_stdio {
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
        }

        *self.process.lock().await = Some(child);

        self.neovim_vadre_window
            .lock()
            .await
            .log_msg(VadreLogLevel::DEBUG, "Process spawned".into())
            .await?;

        Ok(())
    }

    /// Handle the TCP connection and setup the frame decoding/encoding and handling
    #[tracing::instrument(skip(self))]
    async fn tcp_connect_and_handle(&mut self, port: u16) -> Result<()> {
        tracing::trace!("Connecting to port {}", port);

        let tcp_stream = self.do_tcp_connect(port).await?;
        let framed_stream = DAPCodec::new().framed(tcp_stream);

        self.handle_framed_stream(framed_stream).await?;

        Ok(())
    }

    /// Actually do the TCP connection itself
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

    /// Connect over stdio and setup the frame decoding/encoding and handling
    #[tracing::instrument(skip(self))]
    async fn stdio_connect(&mut self) -> Result<()> {
        tracing::trace!("Connecting to stdio");

        let (stdin, stdout) = {
            let mut locked_process = self.process.lock().await;
            let process = locked_process.as_mut().expect("has a process");
            let stdin = process.stdin.take().expect("should have stdin");
            let stdout = process.stdout.take().expect("should have stdin");
            (stdin, stdout)
        };

        let stream = crate::tokio_join::join(stdout, stdin);
        let framed_stream = DAPCodec::new().framed(stream);

        self.handle_framed_stream(framed_stream).await
    }

    /// Spawn and handle the stream
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
        let (debugger_sender_tx, debugger_sender_rx) = mpsc::channel(256);
        // Can theoretically get a lot of messages from the debugger.
        let (debugger_receiver_tx, _debugger_receiver_rx) = broadcast::channel(512);

        self.debugger_sender_tx = Some(debugger_sender_tx);
        self.debugger_receiver_tx = Some(debugger_receiver_tx.clone());

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
                        framed_stream.send(message).await?;
                    }
                }
            }

            Ok::<(), anyhow::Error>(())
        });

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
}

impl Debug for DebuggerProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebuggerProcessor")
            .field("process", &self.process)
            .finish()
    }
}
