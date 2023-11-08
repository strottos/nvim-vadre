use std::{collections::HashMap, fmt::Debug, sync::Arc, time::Duration};

use super::{
    breakpoints::Breakpoints, debuggers::DebuggerType, handler::DebuggerHandler,
    processor::DebuggerProcessor, protocol::ProtocolMessageType, DebuggerStepType,
};
use crate::neovim::{NeovimVadreWindow, VadreBufferType, VadreLogLevel};

use anyhow::Result;
use tokio::{
    sync::{oneshot, Mutex},
    time::timeout,
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
        command: String,
        command_args: Vec<String>,
        environment_variables: HashMap<String, String>,
        pending_breakpoints: &Breakpoints,
        existing_debugger_port: Option<String>,
    ) -> Result<()> {
        self.neovim_vadre_window.lock().await.create_ui().await?;

        let (config_done_tx, config_done_rx) = oneshot::channel();

        self.handler
            .lock()
            .await
            .init(existing_debugger_port, None, config_done_tx)
            .await?;

        self.handle_messages().await?;

        let launch_rx = self
            .handler
            .lock()
            .await
            .launch_program(command, command_args, environment_variables)
            .await?;

        self.handler
            .lock()
            .await
            .set_init_breakpoints(pending_breakpoints)
            .await?;

        timeout(Duration::new(60, 0), config_done_rx).await??;

        let (config_tx, config_rx) = oneshot::channel();

        self.handler
            .lock()
            .await
            .configuration_done(config_tx)
            .await?;

        // Check that the launch and config requests were successful
        try_join!(launch_rx, config_rx)?;

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

        tokio::spawn(async move {
            let mut debugger_rx = debugger_rx;

            loop {
                let message = debugger_rx.recv().await?;

                tracing::trace!("Message found: {:?}", message);

                if let ProtocolMessageType::Request(request_args) = message.type_ {
                    debugger_handler
                        .lock()
                        .await
                        .handle_request(*message.seq.first(), request_args)
                        .await?;
                } else if let ProtocolMessageType::Event(event) = message.type_ {
                    debugger_handler.lock().await.handle_event(event).await?;
                }
            }

            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
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
