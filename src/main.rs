mod debuggers;
mod logger;
mod neovim;
mod util;

use crate::{
    debuggers::{CodeLLDBDebugger, Debugger},
    neovim::VadreLogLevel,
};

use std::{
    collections::{HashMap, HashSet},
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
use nvim_rs::{compat::tokio::Compat, create::tokio as create, Handler, Neovim, Value};
use tokio::{io::Stdout, sync::Mutex};

static VADRE_NEXT_INSTANCE_NUM: AtomicUsize = AtomicUsize::new(1);
// Arbitrary number so no clashes with other plugins (hopefully)
// TODO: Find a better solution
static VADRE_NEXT_SIGN_ID: AtomicUsize = AtomicUsize::new(1157831);

type VadreResult = Result<Value, Value>;

#[derive(Clone, Debug)]
struct NeovimHandler {
    // Having two mutexes in one entry sure isn't nice, but without this it's difficult to get
    // mutable references to either with the way nvim_rs::Handler is setup. Given that we shouldn't
    // really be using more than one debugger at a time and we try and take the second mutex
    // sparingly hopefully this won't be too big a performance hit. I'd prefer to take them out
    // though ideally.
    debuggers: Arc<Mutex<HashMap<usize, Arc<Mutex<Debugger>>>>>,

    breakpoints: Arc<Mutex<HashSet<(String, i64)>>>,
}

impl NeovimHandler {
    fn new() -> Self {
        NeovimHandler {
            debuggers: Arc::new(Mutex::new(HashMap::new())),
            breakpoints: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    #[tracing::instrument(skip(self, args, neovim))]
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
            "lldb" | "codelldb" => "codelldb",
            _ => return Err(format!("ERROR: Debugger unknown {}", debugger_type).into()),
        };

        tracing::trace!(
            "Setting up instance {} of type {}",
            instance_id,
            debugger_type
        );

        let debugger =
            match debugger_type {
                "lldb" | "codelldb" => Arc::new(Mutex::new(Debugger::CodeLLDB(
                    CodeLLDBDebugger::new(instance_id, command, command_args, neovim),
                ))),
                _ => return Err(format!("ERROR: Debugger unknown {}", debugger_type).into()),
            };

        self.debuggers
            .lock()
            .await
            .insert(instance_id, debugger.clone());

        let pending_breakpoints = self.breakpoints.lock().await.clone();

        tokio::spawn(async move {
            let debugger = debugger.clone();
            tracing::trace!("Trying to lock 1");
            let mut debugger_lock = debugger.lock().await;
            tracing::trace!("Locked 1");
            if let Err(e) = debugger_lock.setup(&pending_breakpoints).await {
                let log_msg = format!("Can't setup debugger: {}", e);
                debugger_lock
                    .neovim_vadre_window()
                    .log_msg(VadreLogLevel::CRITICAL, &log_msg)
                    .await
                    .unwrap();
            }
            tracing::trace!("Unlocked 1");
        });

        Ok("process launched".into())
    }

    #[tracing::instrument(skip(self, neovim))]
    async fn breakpoint(&self, neovim: Neovim<Compat<Stdout>>) -> VadreResult {
        let cursor_position = neovim
            .get_current_win()
            .await
            .expect("could get current window")
            .get_cursor()
            .await
            .expect("could get current cursor position");

        let file_path = neovim
            .get_current_buf()
            .await
            .expect("could get current buffer")
            .get_name()
            .await
            .expect("could get current cursor position");

        let line_number = cursor_position.0;
        let position = (file_path.clone(), line_number);

        let adding_breakpoint = {
            let mut breakpoints = self.breakpoints.lock().await;

            if breakpoints.contains(&position) {
                breakpoints.remove(&position);
                false
            } else {
                breakpoints.insert(position);
                true
            }
        };

        let debuggers = self.debuggers.lock().await;

        for debugger in debuggers.values() {
            if adding_breakpoint {
                debugger
                    .lock()
                    .await
                    .set_breakpoint(Path::new(&file_path), cursor_position.0)
                    .await
                    .expect("could set breakpoint");
            } else {
                debugger
                    .lock()
                    .await
                    .remove_breakpoint(Path::new(&file_path), cursor_position.0)
                    .await
                    .expect("could set breakpoint");
            }
        }

        Ok((if adding_breakpoint {
            "breakpoint set"
        } else {
            "breakpoint removed"
        })
        .into())
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
            "breakpoint" => self.breakpoint(neovim).await,
            _ => unimplemented!(),
        }
    }
}

async fn setup_signs(neovim: &Neovim<Compat<Stdout>>) -> Result<()> {
    let sign_background_colour_output = neovim.exec("highlight SignColumn", true).await?;
    let sign_background_colour_output = sign_background_colour_output
        .split("\n")
        .collect::<Vec<&str>>();
    assert_eq!(sign_background_colour_output.len(), 1);

    let mut ctermbg = "";
    let mut guibg = "";

    for snippet in sign_background_colour_output.get(0).unwrap().split(" ") {
        if snippet.len() >= 8 && &snippet[0..8] == "ctermbg=" {
            ctermbg = &snippet[8..];
        } else if snippet.len() >= 6 && &snippet[0..6] == "guibg=" {
            guibg = &snippet[6..];
        }
    }

    if guibg != "" && ctermbg != "" {
        neovim
            .exec(
                &format!(
                    "highlight VadreBreakpointHighlight \
                     guifg=#ff0000 guibg={} ctermfg=red ctermbg={}",
                    guibg, ctermbg
                ),
                false,
            )
            .await?;
        neovim
            .exec(
                &format!(
                    "highlight VadreDebugPointerHighlight \
                     guifg=#00ff00 guibg={} ctermfg=green ctermbg={}",
                    guibg, ctermbg
                ),
                false,
            )
            .await?;
    } else {
        neovim
            .exec(
                "highlight VadreBreakpointHighlight guifg=#ff0000 ctermfg=red",
                false,
            )
            .await?;
        neovim
            .exec(
                "highlight VadreDebugPointerHighlight guifg=#00ff00 ctermfg=green",
                false,
            )
            .await?;
    }

    Ok(())
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
    let (neovim, io_handler) = create::new_parent(handler).await;

    setup_signs(&neovim).await?;

    match io_handler.await {
        Err(joinerr) => tracing::error!("Error joining IO loop: '{}'", joinerr),

        Ok(Err(err)) => {
            if !err.is_reader_error() {
                // One last try, since there wasn't an error with writing to the
                // stream
                neovim
                    .err_writeln(&format!("Error: '{}'", err))
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

        Ok(Ok(())) => {}
    }

    Ok(())
}
