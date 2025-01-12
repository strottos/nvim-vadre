use std::{collections::HashMap, fmt::Debug, sync::Arc};

use super::{handler::DebuggerHandler, processor::DebuggerProcessor};
use crate::{
    debuggers::Breakpoints,
    neovim::{NeovimVadreWindow, VadreLogLevel},
};

use anyhow::Result;
use tokio::sync::Mutex;

pub(crate) struct NodeDebugger {
    id: usize,
    pub setup_complete: bool,
    pub neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
    pub handler: Arc<Mutex<DebuggerHandler>>,
}

impl NodeDebugger {
    pub(crate) fn new(
        id: usize,
        debug_program_string: String,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        breakpoints: Breakpoints,
    ) -> Self {
        let debugger_processor = DebuggerProcessor::new(neovim_vadre_window.clone());

        let debugger_handler = Arc::new(Mutex::new(DebuggerHandler::new(
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
        _existing_debugger_port: Option<u16>,
    ) -> Result<()> {
        self.neovim_vadre_window.lock().await.create_ui().await?;

        self.handler
            .lock()
            .await
            .launch(command_args, environment_variables)
            .await?;

        self.handler.lock().await.init().await?;

        self.handle_messages().await?;

        self.log_msg(
            VadreLogLevel::INFO,
            "Node Debugger and program launched successfully",
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

                let status = if message.get("id").is_some() {
                    // We're a response, handled elsewhere
                    Ok(())
                } else {
                    // We're an event
                    debugger_handler.lock().await.handle_event(&message).await
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

impl Debug for NodeDebugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeDebugger")
            .field("id", &self.id)
            .finish()
    }
}
