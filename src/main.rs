mod logger;

use std::{
    cmp,
    collections::HashMap,
    env,
    error::Error,
    fmt::Debug,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use anyhow::Result;
use async_trait::async_trait;
use nvim_rs::{compat::tokio::Compat, create::tokio as create, Buffer, Handler, Neovim, Value};
use tokio::{io::Stdout, sync::Mutex};

static VADRE_NEXT_INSTANCE_NUM: AtomicUsize = AtomicUsize::new(1);

type VadreResult = Result<Value, Value>;

enum VadreWindowTypes {
    Code,
    Output,
}

impl VadreWindowTypes {
    fn buffer_name_prefix(&self) -> &str {
        match self {
            VadreWindowTypes::Code => "VadreCode",
            VadreWindowTypes::Output => "VadreOutput",
        }
    }

    fn type_name(&self) -> &str {
        match self {
            VadreWindowTypes::Code => "VadreCode",
            VadreWindowTypes::Output => "VadreOutput",
        }
    }
}

#[derive(Clone, Debug)]
enum VadreDebugger {
    CodeLLDB,
}

#[derive(Clone)]
struct Debugger {
    debugger_type: VadreDebugger,
    id: usize,
    command: String,
    command_args: Vec<String>,
    neovim: Neovim<Compat<Stdout>>,
}

impl Debugger {
    fn new(
        debugger_type: VadreDebugger,
        id: usize,
        command: String,
        command_args: Vec<String>,
        neovim: Neovim<Compat<Stdout>>,
    ) -> Self {
        Debugger {
            debugger_type,
            id,
            command,
            command_args,
            neovim,
        }
    }

    async fn launch(&self) -> VadreResult {
        self.setup_ui()
            .await
            .expect("Expected UI to setup correctly");

        tracing::debug!(
            "Launching following process with lldb: {:?} -- {:?}",
            self.command,
            self.command_args,
        );

        Ok(Value::from("process launched"))
    }

    async fn setup_ui(&self) -> Result<()> {
        let eventignore_old = self.neovim.get_var("eventignore").await;
        self.neovim.set_var("eventignore", "all".into()).await?;

        // Check if we need a new tab first
        let current_buf = self.neovim.get_current_buf().await?;
        let current_tabpage = self.neovim.get_current_tabpage().await?;

        let modified = current_buf.get_option("modified").await?.as_bool().unwrap();
        let line_count = current_buf.line_count().await?;
        let lines = current_buf.get_lines(0, 1, true).await?;

        let window_count = current_tabpage.list_wins().await?.len();
        let buffer_name = current_buf.get_name().await?;

        // New tab if we're not completely single window empty pane, otherwise just use current
        if window_count > 1
            || buffer_name != ""
            || modified
            || line_count > 1
            || lines.get(0).unwrap() != ""
        {
            self.neovim.command("tab new").await?;
            tracing::trace!("setup a new empty tab");
        }

        // Now setup the current window which must by construction be empty
        let current_window = self.neovim.get_current_win().await?;
        let current_tabpage = self.neovim.get_current_tabpage().await?;

        let default_height = current_window
            .get_height()
            .await
            .map_or(10, |x| cmp::max(x / 4, 10));
        tracing::trace!("default height is {:?}", default_height);

        let output_window_height = self
            .neovim
            .get_var("vadre_output_window_height")
            .await
            .map_or(default_height, |x| x.as_i64().unwrap_or(default_height));
        tracing::trace!("output window size {:?}", output_window_height);

        self.neovim.command("new").await?;
        let mut windows = current_tabpage.list_wins().await?.into_iter();
        assert_eq!(2, windows.len());

        // Window 1 is code
        let window = windows.next().unwrap();
        self.neovim.set_current_win(&window).await?;
        let buffer = window.get_buf().await?;
        self.set_vadre_buffer(buffer, VadreWindowTypes::Code)
            .await?;

        // Window 2 is output
        let window = windows.next().unwrap();
        let buffer = window.get_buf().await?;
        self.set_vadre_buffer(buffer, VadreWindowTypes::Output)
            .await?;
        window.set_height(output_window_height).await?;

        match eventignore_old {
            Ok(x) => self.neovim.set_var("eventignore", x).await?,
            Err(_) => self.neovim.set_var("eventignore", "".into()).await?,
        };
        self.neovim.command("doautocmd User VadreUICreated").await?;

        Ok(())
    }

    async fn set_vadre_buffer(
        &self,
        buffer: Buffer<Compat<Stdout>>,
        window_type: VadreWindowTypes,
    ) -> Result<()> {
        let buffer_name = format!("{}Win_{}", window_type.buffer_name_prefix(), self.id);
        let file_type = window_type.type_name();
        buffer.set_name(&buffer_name).await?;
        tracing::trace!("0");
        buffer.set_option("swapfile", false.into()).await?;
        tracing::trace!("1");
        buffer.set_option("buftype", "nofile".into()).await?;
        tracing::trace!("2");
        buffer.set_option("filetype", file_type.into()).await?;
        tracing::trace!("3");
        buffer.set_option("buflisted", false.into()).await?;
        tracing::trace!("4");
        buffer.set_option("modifiable", false.into()).await?;
        tracing::trace!("5");

        Ok(())
    }
}

impl Debug for Debugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Debugger")
            .field("debugger_type", &self.debugger_type)
            .field("id", &self.id)
            .field("command", &self.command)
            .field("command_args", &self.command_args)
            .finish()
    }
}

#[derive(Clone, Debug)]
struct NeovimHandler {
    debuggers: Arc<Mutex<HashMap<usize, Debugger>>>,
}

impl NeovimHandler {
    fn new() -> Self {
        NeovimHandler {
            debuggers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[tracing::instrument(skip(args, neovim))]
    async fn launch(
        &self,
        instance_id: usize,
        args: Vec<Value>,
        neovim: Neovim<Compat<Stdout>>,
    ) -> VadreResult {
        tracing::debug!("Launching instance {} with args: {:?}", instance_id, args);

        let mut debugger_type = None;
        let mut process_vadre_args = true;

        let mut command_args = vec![];
        let mut command = "".to_string();

        for arg in args {
            if Some("--") == arg.as_str() {
                process_vadre_args = false;
                continue;
            }

            let arg_string = arg.as_str().unwrap().to_string();
            if process_vadre_args && arg_string.starts_with("-t=") && arg_string.len() > 3 {
                debugger_type = Some(arg_string[3..].to_string());
            } else {
                if command == "" {
                    command = arg_string.clone();
                } else {
                    command_args.push(arg_string);
                }
            }
        }

        let debugger_type = debugger_type.unwrap_or_else(|| "codelldb".to_string());

        let command_path = Path::new(&command);
        if !command_path.exists() {
            let log_msg = format!("Program not found {}", command);
            tracing::error!("{}", log_msg);
            return Err(format!("ERROR: {}", log_msg).into());
        }

        let debugger_type = match debugger_type.as_ref() {
            "lldb" | "codelldb" => VadreDebugger::CodeLLDB,
            _ => return Err(format!("ERROR: Debugger unknown {}", debugger_type).into()),
        };

        tracing::trace!(
            "Setting up instance {} of type {:?}",
            instance_id,
            debugger_type
        );

        let debugger = Debugger::new(debugger_type, instance_id, command, command_args, neovim);
        debugger.launch().await?;

        self.debuggers.lock().await.insert(instance_id, debugger);

        Ok("process launched".into())
    }
}

#[async_trait]
impl Handler for NeovimHandler {
    type Writer = Compat<Stdout>;

    // This function is either responsible for anything trivial (< 1 line) or handing requests
    // to their appropriate handlers.
    //
    // NB: We should not put any logging in here as it confused the async trait but should
    // defer it to the handlers themselves.
    async fn handle_request(
        &self,
        name: String,
        args: Vec<Value>,
        neovim: Neovim<Compat<Stdout>>,
    ) -> VadreResult {
        match name.as_ref() {
            "ping" => Ok("pong".into()),
            "launch" => {
                let instance_id = VADRE_NEXT_INSTANCE_NUM.fetch_add(1, Ordering::SeqCst);
                self.launch(
                    instance_id,
                    args.get(0)
                        .expect("launch args should be supplied")
                        .as_array()
                        .expect("launch args should be an array")
                        .to_vec(),
                    neovim,
                )
                .await
            }
            _ => unimplemented!(),
        }
    }
}

#[tokio::main]
#[tracing::instrument]
async fn main() -> Result<()> {
    logger::setup_logging(
        env::var("VADRE_LOG_FILE")
            .ok()
            .as_ref()
            .map(|x| Path::new(x)),
        env::var("VADRE_LOG").ok().as_deref(),
    )?;

    let span = tracing::span!(tracing::Level::TRACE, "root");
    let _enter = span.enter();

    tracing::info!("Loading VADRE plugin");
    let handler: NeovimHandler = NeovimHandler::new();
    let (nvim, io_handler) = create::new_parent(handler).await;

    match io_handler.await {
        Err(joinerr) => tracing::error!("Error joining IO loop: '{}'", joinerr),

        Ok(Err(err)) => {
            if !err.is_reader_error() {
                // One last try, since there wasn't an error with writing to the
                // stream
                nvim.err_writeln(&format!("Error: '{}'", err))
                    .await
                    .unwrap_or_else(|e| {
                        // We could inspect this error to see what was happening, and
                        // maybe retry, but at this point it's probably best
                        // to assume the worst and print a friendly and
                        // supportive message to our users
                        tracing::error!("Well, dang... '{}'", e);
                    });
            }

            if !err.is_channel_closed() {
                // Closed channel usually means neovim quit itself, or this plugin was
                // told to quit by closing the channel, so it's not always an error
                // condition.
                tracing::error!("Error: '{}'", err);

                let mut source = err.source();

                while let Some(e) = source {
                    tracing::error!("Caused by: '{}'", e);
                    source = e.source();
                }
            }
        }

        Ok(Ok(())) => {
            tracing::info!("HERE3");
        }
    }

    Ok(())
}
