use std::{collections::HashMap, fmt::Debug, sync::Arc, time::Duration};

use super::{
    breakpoints::Breakpoints, debuggers::DebuggerType, handler::DebuggerHandler,
    processor::DebuggerProcessor, protocol::ProtocolMessageType,
};
use crate::neovim::{NeovimVadreWindow, VadreBufferType, VadreLogLevel};

use anyhow::Result;
use tokio::{
    sync::{oneshot, Mutex},
    time::{sleep, timeout},
    try_join,
};

pub(crate) struct Debugger {
    id: usize,
    pub neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    pub handler: Arc<Mutex<DebuggerHandler>>,
}

impl Debugger {
    pub(crate) fn new(
        id: usize,
        debug_program_string: String,
        debugger_type: DebuggerType,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    ) -> Self {
        let debugger_processor =
            DebuggerProcessor::new(debugger_type.clone(), neovim_vadre_window.clone());

        let debugger_handler = Arc::new(Mutex::new(DebuggerHandler::new(
            debugger_type,
            debugger_processor,
            neovim_vadre_window.clone(),
            debug_program_string,
        )));

        Self {
            id,
            neovim_vadre_window,

            handler: debugger_handler,
        }
    }

    #[tracing::instrument(skip(self))]
    pub(crate) async fn setup(
        &mut self,
        command_args: Vec<String>,
        environment_variables: HashMap<String, String>,
        pending_breakpoints: &Breakpoints,
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

        let launch_rx = self
            .handler
            .lock()
            .await
            .launch_program(command_args, attach_debugger_to_pid, environment_variables)
            .await?;

        if !attach_debugger_to_pid.is_some() {
            timeout(Duration::new(60, 0), terminal_spawned_rx).await??;
        }

        self.handler
            .lock()
            .await
            .set_init_breakpoints(pending_breakpoints)
            .await?;

        let (config_done_tx, config_done_rx) = oneshot::channel();

        self.handler
            .lock()
            .await
            .configuration_done(config_done_tx)
            .await?;

        // Check that the launch and config requests were successful
        try_join!(launch_rx, config_done_rx)?;

        self.log_msg(
            VadreLogLevel::INFO,
            "Debugger and program launched successfully",
        )
        .await?;

        Ok(())
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

    // TODO: Handler or here?
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
                self.handler.lock().await.display_output_info().await?;
            }
            _ => {}
        };

        Ok(())
    }

    pub(crate) async fn log_msg(&self, level: VadreLogLevel, msg: &str) -> Result<()> {
        self.neovim_vadre_window
            .lock()
            .await
            .log_msg(level, msg)
            .await
    }
}

impl Debug for Debugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Debugger").field("id", &self.id).finish()
    }
}
