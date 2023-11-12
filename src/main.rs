#[macro_use]
extern crate lazy_static;

mod dap;
mod logger;
mod neovim;
mod tokio_join;
mod util;

use crate::neovim::VadreLogLevel;

use std::{
    collections::HashMap,
    env,
    error::Error,
    fmt::Debug,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use clap::Parser;
use dap::{Breakpoints, Debugger, DebuggerStepType};
use nvim_rs::{compat::tokio::Compat, create::tokio as create, Handler, Neovim, Value};
use tokio::{
    io::Stdout,
    sync::{Mutex, MutexGuard},
    time::timeout,
};

static VADRE_NEXT_INSTANCE_NUM: AtomicUsize = AtomicUsize::new(1);

type VadreResult = Result<Value, Value>;

#[derive(Parser, Debug)]
struct LaunchArgs {
    #[arg(short = 'l', long, default_value_t = 10)]
    launch_timeout: u64,

    #[arg(short = 't', long)]
    debugger_type: Option<String>,

    #[arg(short = 'p', long)]
    debugger_port: Option<u16>,

    #[arg(short = 'a', long)]
    attach: Option<i64>,

    #[arg(short = 'd', long)]
    dap_command: Option<String>,

    #[arg(short = 'e', long = "env")]
    environment_variables: Vec<String>,

    command_args: Vec<String>,
}

#[derive(Clone, Debug)]
struct NeovimHandler {
    // Having two mutexes in one entry sure isn't nice, but without this it's difficult to get
    // mutable references to either with the way nvim_rs::Handler is setup. Given that we shouldn't
    // really be using more than one debugger at a time and we try and take the second mutex
    // sparingly hopefully this won't be too big a performance hit. I'd prefer to take them out
    // though ideally.
    debuggers: Arc<Mutex<HashMap<usize, Arc<Mutex<Box<Debugger>>>>>>,

    // Record of breakpoints for when new debuggers are spawned
    pending_breakpoints: Arc<Mutex<Breakpoints>>,
}

impl NeovimHandler {
    fn new() -> Self {
        NeovimHandler {
            debuggers: Arc::new(Mutex::new(HashMap::new())),
            pending_breakpoints: Arc::new(Mutex::new(Breakpoints::new())),
        }
    }

    #[tracing::instrument(skip(self, neovim))]
    async fn launch(&self, mut args: Vec<String>, neovim: Neovim<Compat<Stdout>>) -> VadreResult {
        if args.len() == 0 {
            tracing::debug!("Launching Vadre create Debugger UI");

            neovim
                .exec_lua(
                    r#"
                    local vadre_ui = require('vadre.ui')

                    vadre_ui.start_debugger_ui()
                    "#,
                    vec![],
                )
                .await
                .map_err(|e| format!("Lua error: {e:?}"))?;

            return Ok("".into());
        }

        let instance_id = VADRE_NEXT_INSTANCE_NUM.fetch_add(1, Ordering::SeqCst);

        tracing::debug!("Launching instance {} with args: {:?}", instance_id, args);

        // Claps `try_parse_from` assumes that the first argument is the program
        args.insert(0, "nvim_vadre".to_string());

        let args =
            LaunchArgs::try_parse_from(args).map_err(|x| format!("Couldn't parse args: {x}"))?;

        let debugger_type = args.debugger_type.unwrap_or("codelldb".to_string());
        let mut environment_variables = HashMap::new();

        for env_var in args.environment_variables {
            let mut split = env_var.split("=");
            let key = split
                .next()
                .ok_or("Key not found for environment variable")?;
            let value = split
                .next()
                .ok_or("Value not found for environment variable")?;
            if !split.next().is_none() {
                return Err("Too many '=' in environment variable".into());
            }
            environment_variables.insert(key.to_string(), value.to_string());
        }

        tracing::trace!(
            "Setting up instance {} of type {:?}",
            instance_id,
            debugger_type
        );

        let command_args = args.command_args.join(" ");

        let pending_breakpoints = self.pending_breakpoints.lock().await.clone();

        let debugger = match dap::new_debugger(
            instance_id,
            command_args.clone(),
            neovim.clone(),
            debugger_type.clone(),
            pending_breakpoints,
        ) {
            Ok(x) => Arc::new(Mutex::new(x)),
            Err(e) => return Err(format!("Can't setup debugger: {}", e).into()),
        };

        self.debuggers
            .lock()
            .await
            .insert(instance_id, debugger.clone());

        tokio::spawn(async move {
            let neovim = neovim.clone();

            let mut debugger_lock = debugger.lock().await;

            async fn report_error(
                msg: &str,
                debugger: MutexGuard<'_, Box<Debugger>>,
                neovim: Neovim<Compat<Stdout>>,
            ) -> anyhow::Result<()> {
                tracing::error!("{}", msg);
                if let Err(log_err) = debugger.log_msg(VadreLogLevel::CRITICAL, &msg).await {
                    tracing::error!("Couldn't write to neovim logs: {}", log_err);
                    neovim.err_writeln(&msg).await.unwrap_or_else(|vim_err| {
                        tracing::error!("Couldn't write to neovim: {}", vim_err);
                    });
                };
                neovim.err_writeln(&msg).await.unwrap_or_else(|vim_err| {
                    tracing::error!("Couldn't write to neovim: {}", vim_err);
                });

                Ok(())
            }

            match timeout(
                Duration::new(args.launch_timeout, 0),
                debugger_lock.setup(
                    args.command_args,
                    environment_variables,
                    args.debugger_port,
                    args.attach,
                    args.dap_command,
                ),
            )
            .await
            {
                Ok(x) => {
                    if let Err(ref e) = x {
                        report_error(
                            &format!("Critical error setting up debugger: {}", e),
                            debugger_lock,
                            neovim.clone(),
                        )
                        .await?;
                    }
                }
                Err(e) => {
                    report_error(
                        &format!("Critical error, timed out setting up debugger: {}", e),
                        debugger_lock,
                        neovim.clone(),
                    )
                    .await?;
                }
            }

            neovim
                .set_var(
                    &format!(r#"vadre_last_command_{}"#, debugger_type),
                    command_args.into(),
                )
                .await
                .unwrap_or_else(|e| {
                    tracing::error!("Couldn't create vadre last command: {}", e);
                });

            Ok::<(), anyhow::Error>(())
        });

        Ok("process launched".into())
    }

    #[tracing::instrument(skip(self, neovim))]
    async fn breakpoint(&self, neovim: Neovim<Compat<Stdout>>) -> VadreResult {
        let cursor_position = neovim
            .get_current_win()
            .await
            .map_err(|e| format!("Couldn't get current window: {e}"))?
            .get_cursor()
            .await
            .map_err(|e| format!("Couldn't get current cursor position: {e}"))?;

        let file_path = neovim
            .get_current_buf()
            .await
            .map_err(|e| format!("Couldn't get current buffer: {e}"))?
            .get_name()
            .await
            .map_err(|e| format!("Couldn't get current cursor position: {e}"))?;

        let line_number = cursor_position.0;
        let file_path = match dunce::canonicalize(Path::new(&file_path)) {
            Ok(x) => x,
            Err(_) => return Err("Path not found for setting breakpoint".into()),
        };

        if !file_path.exists() {
            return Err(format!(
                "Requested to set breakpoints in non-existent file: {:?}",
                file_path
            )
            .into());
        }

        let file_path = file_path
            .to_str()
            .ok_or_else(|| format!("Couldn't get file path as string: {:?}", file_path))?
            .to_string();

        let adding_breakpoint = crate::neovim::toggle_breakpoint_sign(&neovim, line_number)
            .await
            .map_err(|e| format!("Breakpoint sign didn't place: {e}"))?;

        {
            let mut breakpoints_lock = self.pending_breakpoints.lock().await;

            if adding_breakpoint {
                breakpoints_lock
                    .add_pending_breakpoint(file_path.clone(), line_number)
                    .map_err(|e| format!("Couldn't add breakpoint to pending breakpoints: {e}"))?;
            } else {
                breakpoints_lock
                    .remove_breakpoint(file_path.clone(), line_number)
                    .map_err(|e| {
                        format!("Couldn't remove breakpoint from pending breakpoints: {e}")
                    })?;
            }
        }

        let debuggers = self.debuggers.clone();
        let spawn_file_path = file_path.clone();

        tokio::spawn(async move {
            let debuggers = debuggers.lock().await;

            for debugger in debuggers.values() {
                let debugger_lock = debugger.lock().await;

                if !debugger_lock.setup_complete {
                    continue;
                }

                let breakpoint_result = if adding_breakpoint {
                    debugger_lock
                        .handler
                        .lock()
                        .await
                        .add_breakpoint(spawn_file_path.clone(), line_number)
                        .await
                } else {
                    debugger_lock
                        .handler
                        .lock()
                        .await
                        .remove_breakpoint(spawn_file_path.clone(), line_number)
                        .await
                };

                if let Err(e) = breakpoint_result {
                    let msg = format!(
                        "Couldn't {} breakpoints: {e}",
                        if adding_breakpoint { "set" } else { "remove" }
                    );
                    tracing::error!("{}", msg);

                    neovim.err_writeln(&msg).await.unwrap_or_else(|e| {
                        tracing::error!("Couldn't write to neovim: {}", e);
                    });
                    if let Err(e) = debugger_lock.log_msg(VadreLogLevel::CRITICAL, &msg).await {
                        tracing::error!("Couldn't write to neovim logs: {}", e);
                        if let Err(e) = neovim.err_writeln("Vadre couldn't write to neovim").await {
                            tracing::error!("Couldn't write to neovim: {}", e);
                        }
                    }
                }
            }
        });

        Ok((if adding_breakpoint {
            tracing::trace!("Adding breakpoint sign {file_path} {line_number}");
            "breakpoint set"
        } else {
            tracing::trace!("Removing breakpoint sign {file_path} {line_number}");
            "breakpoint removed"
        })
        .into())
    }

    async fn do_step(
        &self,
        step_type: DebuggerStepType,
        instance_id: usize,
        count: u64,
    ) -> VadreResult {
        let debuggers = match self.debuggers.try_lock() {
            Ok(x) => x,
            Err(e) => {
                tracing::error!("Couldn't get debuggers lock: {e}");
                return Ok("Vadre busy, please retry shortly".into());
            }
        };

        let debugger = debuggers.get(&instance_id).ok_or("Debugger didn't exist")?;

        let debugger = match debugger.try_lock() {
            Ok(x) => x,
            Err(e) => {
                tracing::error!("Couldn't get debugger lock: {e}");
                return Ok("Vadre busy, please retry shortly".into());
            }
        };

        if !debugger.setup_complete {
            return Err("Can't do step, debugger not setup".into());
        }

        debugger
            .handler
            .lock()
            .await
            .do_step(step_type, count)
            .await
            .map_err(|e| format!("Couldn't do code step: {e}"))?;

        Ok("".into())
    }

    async fn interrupt(&self, instance_id: usize, thread_id: Option<i64>) -> VadreResult {
        let debuggers = match self.debuggers.try_lock() {
            Ok(x) => x,
            Err(e) => {
                tracing::error!("Couldn't get debuggers lock: {e}");
                return Ok("Vadre busy, please retry shortly".into());
            }
        };

        let debugger = debuggers.get(&instance_id).ok_or("Debugger didn't exist")?;

        let debugger = match debugger.try_lock() {
            Ok(x) => x,
            Err(e) => {
                tracing::error!("Couldn't get debugger lock: {e}");
                return Ok("Vadre busy, please retry shortly".into());
            }
        };

        if !debugger.setup_complete {
            return Err("Can't do interrupt, debugger not setup correctly".into());
        }

        debugger
            .handler
            .lock()
            .await
            .pause(thread_id)
            .await
            .map_err(|e| format!("Couldn't do pause: {e}"))?;

        Ok("interrupted".into())
    }

    async fn stop_debugger(&self, instance_id: usize) -> VadreResult {
        let mut debuggers_lock = match self.debuggers.try_lock() {
            Ok(x) => x,
            Err(e) => {
                tracing::error!("Couldn't get debuggers lock: {e}");
                return Ok("Vadre busy, please retry shortly".into());
            }
        };

        let debugger = debuggers_lock
            .get(&instance_id)
            .ok_or("Debugger didn't exist")?;

        let debugger = match debugger.try_lock() {
            Ok(x) => x,
            Err(e) => {
                tracing::error!("Couldn't get debugger lock: {e}");
                return Ok("Vadre busy, please retry shortly".into());
            }
        };

        let ret;

        if let Err(e) = timeout(Duration::new(5, 0), debugger.handler.lock().await.stop())
            .await
            .map_err(|e| format!("Couldn't stop debugger: {e}"))?
        {
            tracing::error!("Timed out stopping debugger: {e}");
            ret = Err(format!("Debugger instance {} stopped", instance_id).into());
        } else {
            ret = Ok(format!("Debugger instance {} failed to stop", instance_id).into());
        }

        drop(debugger);

        debuggers_lock.remove(&instance_id);

        ret
    }

    async fn change_output_window(&self, instance_id: usize, mut type_: &str) -> VadreResult {
        let debuggers = match self.debuggers.try_lock() {
            Ok(x) => x,
            Err(e) => {
                tracing::error!("Couldn't get debuggers lock: {e}");
                return Ok("Vadre busy, please retry shortly".into());
            }
        };

        let debugger = debuggers.get(&instance_id).ok_or("Debugger didn't exist")?;

        let debugger = match debugger.try_lock() {
            Ok(x) => x,
            Err(e) => {
                tracing::error!("Couldn't get debugger lock: {e}");
                return Ok("Vadre busy, please retry shortly".into());
            }
        };

        if !debugger.setup_complete {
            // Force us to show logs if debugger not complete, only thing we can be interested in
            type_ = "Logs";
        }

        debugger
            .change_output_window(type_)
            .await
            .map_err(|e| format!("Couldn't show output window: {e}"))?;

        Ok("".into())
    }

    async fn handle_output_window_key(&self, instance_id: usize, key: &str) -> VadreResult {
        let debuggers = match self.debuggers.try_lock() {
            Ok(x) => x,
            Err(e) => {
                tracing::error!("Couldn't get debuggers lock: {e}");
                return Ok("Vadre busy, please retry shortly".into());
            }
        };

        let debugger = debuggers.get(&instance_id).ok_or("Debugger didn't exist")?;

        let debugger = match debugger.try_lock() {
            Ok(x) => x,
            Err(e) => {
                tracing::error!("Couldn't get debugger lock: {e}");
                return Ok("Vadre busy, please retry shortly".into());
            }
        };

        debugger
            .handler
            .lock()
            .await
            .handle_output_window_key(key)
            .await
            .map_err(|e| format!("Couldn't show output window: {e}"))?;

        Ok("".into())
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
        tracing::debug!("Handling request: {} {:?}", name, args);
        match name.as_ref() {
            "ping" => Ok("pong".into()),
            "launch" => {
                self.launch(
                    args.get(0)
                        .ok_or("Launch args must be supplied")?
                        .as_array()
                        .ok_or("Launch args must be an array")?
                        .to_vec()
                        .iter()
                        // unwrap should be OK here as we always have strings
                        .map(|x| x.as_str().unwrap().to_string())
                        .collect(),
                    neovim,
                )
                .await
            }
            "breakpoint" => self.breakpoint(neovim).await,
            "step_in" => {
                let args = args
                    .get(0)
                    .ok_or("Step args must be supplied")?
                    .as_array()
                    .ok_or("Step args must be an array")?
                    .to_vec();
                let instance_id: usize = args
                    .get(0)
                    .ok_or("Instance id must be supplied")?
                    .as_str()
                    .ok_or("Instance id must be a string")?
                    .replace("\"", "")
                    .parse::<usize>()
                    .map_err(|e| format!("Instance id must be usize: {e}"))?;

                let count = match args.get(1) {
                    Some(x) => x.as_u64().ok_or("Count must be u64")?,
                    None => 1,
                };

                self.do_step(DebuggerStepType::In, instance_id, count).await
            }
            "step_over" => {
                let args = args
                    .get(0)
                    .ok_or("Step args must be supplied")?
                    .as_array()
                    .ok_or("Step args must be an array")?
                    .to_vec();
                let instance_id: usize = args
                    .get(0)
                    .ok_or("Instance id must be supplied")?
                    .as_str()
                    .ok_or("Instance id must be a string")?
                    .replace("\"", "")
                    .parse::<usize>()
                    .map_err(|e| format!("Instance id is usize: {e}"))?;

                let count = match args.get(1) {
                    Some(x) => x.as_u64().ok_or("Count is u64")?,
                    None => 1,
                };

                self.do_step(DebuggerStepType::Over, instance_id, count)
                    .await
            }
            "continue" => {
                let instance_id: usize = args
                    .get(0)
                    .ok_or("Instance id must be supplied")?
                    .as_str()
                    .ok_or("Instance id must be a string")?
                    .parse::<usize>()
                    .map_err(|e| format!("Instance id is usize: {e}"))?;

                self.do_step(DebuggerStepType::Continue, instance_id, 1)
                    .await
            }
            "interrupt" => {
                let args = args
                    .get(0)
                    .ok_or("Interrupt args must be supplied")?
                    .as_array()
                    .ok_or("Interrupt args must be an array")?
                    .to_vec();

                let instance_id: usize = args
                    .get(0)
                    .ok_or("Instance id must be supplied")?
                    .as_str()
                    .ok_or("Instance id must be a string")?
                    .parse::<usize>()
                    .map_err(|e| format!("Instance id is usize: {e}"))?;

                let thread_id: Option<i64> = args.get(1).and_then(|x| x.as_i64());

                self.interrupt(instance_id, thread_id).await
            }
            "stop_debugger" => {
                let instance_id: usize = args
                    .get(0)
                    .ok_or("Instance id must be supplied")?
                    .as_str()
                    .ok_or("Instance id must be a string")?
                    .parse::<usize>()
                    .map_err(|e| format!("Instance id is usize: {e}"))?;

                self.stop_debugger(instance_id).await
            }
            "output_window" => {
                let instance_id: usize =
                    args.get(0)
                        .ok_or("Instance id must be supplied")?
                        .as_u64()
                        .ok_or("Instance id must be a u64")? as usize;

                let type_: &str = args
                    .get(1)
                    .ok_or("Type must be supplied")?
                    .as_str()
                    .ok_or("Type must be a string")?;

                self.change_output_window(instance_id, type_).await
            }
            "handle_output_window_key" => {
                let instance_id: usize =
                    args.get(0)
                        .ok_or("Instance id must be supplied")?
                        .as_u64()
                        .ok_or("Instance id must be a u64")? as usize;

                let key: &str = args
                    .get(1)
                    .ok_or("Key must be supplied")?
                    .as_str()
                    .ok_or("Key must be a string")?;

                self.handle_output_window_key(instance_id, key).await
            }
            _ => Err(format!("Unimplemented {}", name).into()),
        }
    }
}

#[tokio::main]
#[tracing::instrument]
async fn main() -> anyhow::Result<()> {
    logger::setup_logging(
        env::var("VADRE_LOG_FILE").ok().as_deref(),
        env::var("VADRE_LOG").ok().as_deref(),
    )?;

    let span = tracing::span!(tracing::Level::TRACE, "root");
    let _enter = span.enter();

    tracing::info!("Loading VADRE plugin");
    let handler: NeovimHandler = NeovimHandler::new();
    let (neovim, io_handler) = create::new_parent(handler).await;

    crate::neovim::setup_signs(&neovim).await?;

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
                        tracing::error!("Nvim quit... '{}'", e);
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
