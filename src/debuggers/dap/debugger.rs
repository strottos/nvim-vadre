use std::{collections::HashMap, fmt::Debug, sync::Arc, time::Duration};

use super::{
    debuggers::DapDebuggerType, handler::DebuggerHandler, processor::DebuggerProcessor,
    protocol::ProtocolMessageType,
};
use crate::{
    debuggers::Breakpoints,
    neovim::{NeovimVadreWindow, VadreLogLevel},
};

use anyhow::{bail, Result};
use tokio::{
    sync::{oneshot, Mutex},
    time::timeout,
};

pub(crate) struct DapDebugger {
    id: usize,
    pub setup_complete: bool,
    pub neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    pub handler: Arc<Mutex<DebuggerHandler>>,
}

impl DapDebugger {
    pub(crate) fn new(
        id: usize,
        debug_program_string: String,
        debugger_type: DapDebuggerType,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        breakpoints: Breakpoints,
    ) -> Self {
        // TODO: assert all breakpoints in pending state
        let debugger_processor =
            DebuggerProcessor::new(debugger_type.clone(), neovim_vadre_window.clone());

        let debugger_handler = Arc::new(Mutex::new(DebuggerHandler::new(
            debugger_type,
            debugger_processor,
            neovim_vadre_window.clone(),
            debug_program_string,
            breakpoints,
        )));

        Self {
            id,
            setup_complete: false,

            neovim_vadre_window,

            handler: debugger_handler,
        }
    }

    pub(crate) async fn setup(
        &mut self,
        command_args: Vec<String>,
        environment_variables: HashMap<String, String>,
        existing_debugger_port: Option<u16>,
        attach_debugger_to_pid: Option<i64>,
        dap_command: Option<String>,
    ) -> Result<()> {
        self.neovim_vadre_window.lock().await.create_ui().await?;

        let (terminal_spawned_tx, terminal_spawned_rx) = oneshot::channel();

        self.handler
            .lock()
            .await
            .init(existing_debugger_port, dap_command, terminal_spawned_tx)
            .await?;

        self.handle_messages().await?;

        let (launch_rx, is_integrated_terminal) = self
            .handler
            .lock()
            .await
            .launch_program(command_args, attach_debugger_to_pid, environment_variables)
            .await?;

        // Turn launch into a broadcast channel
        let (launch_watch_tx, mut launch_watch_rx) = tokio::sync::broadcast::channel(1);
        let launch_watch_tx_clone = launch_watch_tx.clone();

        tokio::spawn(async move {
            let launch_rx = launch_rx;
            let launch_watch_tx = launch_watch_tx_clone;

            let recv = launch_rx.await;
            launch_watch_tx.send(recv)?;

            Ok::<(), anyhow::Error>(())
        });

        // Turn terminal spawn into a broadcast channel
        if is_integrated_terminal {
            let (terminal_watch_tx, mut terminal_watch_rx) = tokio::sync::broadcast::channel(1);

            tokio::spawn(async move {
                let terminal_spawned_rx = terminal_spawned_rx;
                let terminal_watch_tx = terminal_watch_tx;

                let recv = terminal_spawned_rx.await;
                terminal_watch_tx.send(recv)?;

                Ok::<(), anyhow::Error>(())
            });

            loop {
                tokio::select!(
                    launch_ret = launch_watch_rx.recv() => {
                        match launch_ret {
                            Ok(ret) => {
                                match ret {
                                    Ok(res) => {
                                        if res.success {
                                            continue;
                                        } else {
                                            tracing::error!("Error launching program: {:?}", res);
                                            bail!("Error launching program: {:?}", res);
                                        }
                                    },
                                    Err(e) => {
                                        tracing::error!("Error launching program: {:?}", e);
                                        bail!("Error launching program: {:?}", e);
                                    }
                                }
                            }
                            Err(err) => {
                                tracing::trace!("Error launching program: {:?}", err);
                                bail!("Error launching program: {:?}", err);
                            }
                        }
                    },
                    output = timeout(Duration::new(60, 0), terminal_watch_rx.recv()) => {
                        output???;
                        break;
                    }
                );
            }
        }

        self.handler.lock().await.init_breakpoints().await?;

        let (config_done_tx, config_done_rx) = oneshot::channel();

        self.handler
            .lock()
            .await
            .configuration_done(config_done_tx)
            .await?;

        // Check that the launch and config requests were successful
        launch_watch_rx.recv().await??;
        config_done_rx.await?;

        self.log_msg(
            VadreLogLevel::INFO,
            "DAP Debugger and program launched successfully",
        )
        .await?;
        self.setup_complete = true;

        Ok(())
    }

    pub(crate) async fn log_msg(&self, level: VadreLogLevel, msg: &str) -> Result<()> {
        self.neovim_vadre_window
            .lock()
            .await
            .log_msg(level, msg)
            .await
    }

    async fn handle_messages(&mut self) -> Result<()> {
        let debugger_rx = self.handler.lock().await.subscribe_debugger()?;

        let debugger_handler = self.handler.clone();
        let neovim_vadre_window = self.neovim_vadre_window.clone();

        tokio::spawn(async move {
            let mut debugger_rx = debugger_rx;

            loop {
                let message = match debugger_rx.recv().await {
                    Ok(message) => message,
                    Err(err) => {
                        let msg = format!("Error receiving message: {:?}", err);
                        tracing::error!("{}", msg);
                        neovim_vadre_window
                            .lock()
                            .await
                            .log_msg(VadreLogLevel::ERROR, &msg)
                            .await
                            .unwrap();
                        continue;
                    }
                };

                let status = match message.type_ {
                    ProtocolMessageType::Request(request_args) => {
                        debugger_handler
                            .lock()
                            .await
                            .handle_request(*message.seq.first(), request_args)
                            .await
                    }
                    ProtocolMessageType::Response(_) => Ok(()),
                    ProtocolMessageType::Event(event) => {
                        debugger_handler.lock().await.handle_event(event).await
                    }
                };

                if let Err(err) = status {
                    let msg = format!("Error handling message: {:?}", err);
                    tracing::error!("{}", msg);
                    neovim_vadre_window
                        .lock()
                        .await
                        .log_msg(VadreLogLevel::ERROR, &msg)
                        .await
                        .unwrap();
                }
            }
        });

        Ok(())
    }
}

impl Debug for DapDebugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DapDebugger").field("id", &self.id).finish()
    }
}
