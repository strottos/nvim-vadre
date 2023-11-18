use super::{
    processor::DebuggerProcessor,
    util::{canonical_filename, Script},
};
use crate::{
    debuggers::{Breakpoints, DebuggerStepType},
    neovim::{CodeBufferContent, NeovimVadreWindow, VadreBufferType, VadreLogLevel},
};

use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, bail, Result};
use tokio::{
    sync::{broadcast, oneshot, Mutex},
    time::timeout,
};

pub(crate) struct DebuggerHandler {
    pub neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,

    processor: DebuggerProcessor,

    scripts: Vec<Script>,
    breakpoints: Breakpoints,

    _debug_program_string: String,

    stopped_listener_tx: Option<oneshot::Sender<()>>,
}

impl DebuggerHandler {
    pub(crate) fn new(
        processor: DebuggerProcessor,
        neovim_vadre_window: Arc<Mutex<NeovimVadreWindow>>,
        debug_program_string: String,
        breakpoints: Breakpoints,
    ) -> Self {
        DebuggerHandler {
            processor,

            neovim_vadre_window,

            scripts: vec![],
            breakpoints,

            _debug_program_string: debug_program_string,

            stopped_listener_tx: None,
        }
    }

    pub(crate) async fn launch(
        &mut self,
        command_args: Vec<String>,
        environment_variables: HashMap<String, String>,
    ) -> Result<()> {
        self.processor
            .setup(command_args, environment_variables)
            .await?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub(crate) async fn init(&mut self) -> Result<()> {
        self.request_and_response(serde_json::json!({"method":"Runtime.enable"}))
            .await?;

        self.request(serde_json::json!({"method":"Debugger.enable"}), None)
            .await?;

        self.request(
            serde_json::json!({"method":"Runtime.runIfWaitingForDebugger"}),
            None,
        )
        .await?;

        Ok(())
    }

    pub(crate) async fn add_breakpoint(
        &mut self,
        source_file_path: String,
        source_line_number: i64,
    ) -> Result<()> {
        self.breakpoints
            .add_pending_breakpoint(source_file_path.clone(), source_line_number)?;

        self.set_breakpoints(source_file_path).await?;

        Ok(())
    }

    pub(crate) async fn remove_breakpoint(
        &mut self,
        source_file_path: String,
        source_line_number: i64,
    ) -> Result<()> {
        self.breakpoints
            .remove_breakpoint(source_file_path.clone(), source_line_number)?;

        self.set_breakpoints(source_file_path).await?;

        Ok(())
    }

    pub(crate) async fn do_step(&mut self, step_type: DebuggerStepType, count: u64) -> Result<()> {
        let request = match step_type {
            DebuggerStepType::Over => serde_json::json!({"method":"Debugger.stepOver"}),
            DebuggerStepType::In => serde_json::json!({"method":"Debugger.stepInto"}),
            DebuggerStepType::Continue => serde_json::json!({"method":"Debugger.resume"}),
        };

        for _ in 1..count {
            let (tx, rx) = oneshot::channel();

            self.stopped_listener_tx = Some(tx);

            let resp = self.request_and_response(request.clone()).await?;

            // TODO: Do we care?
            tracing::trace!("Resp: {:#?}", resp);

            timeout(Duration::new(2, 0), rx).await??;
        }

        self.request(request, None).await?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub(crate) async fn pause(&mut self, thread: Option<i64>) -> Result<()> {
        bail!("TODO: Pause");
    }

    pub(crate) async fn stop(&mut self) -> Result<()> {
        bail!("TODO: Stop");
    }

    pub(crate) async fn change_output_window(&mut self, type_: &str) -> Result<()> {
        self.neovim_vadre_window
            .lock()
            .await
            .change_output_window(type_)
            .await?;

        Ok(())
    }

    pub(crate) async fn handle_output_window_key(&mut self, key: &str) -> Result<()> {
        let current_output_window_type = self
            .neovim_vadre_window
            .lock()
            .await
            .get_output_window_type()
            .await?;

        if current_output_window_type == VadreBufferType::Breakpoints {
            self.handle_breakpoints_output_window_key(key).await?;
        }

        Ok(())
    }

    pub(crate) fn subscribe_debugger(&self) -> Result<broadcast::Receiver<serde_json::Value>> {
        self.processor.subscribe_debugger()
    }

    pub(crate) async fn handle_event(&mut self, event: &serde_json::Value) -> Result<()> {
        tracing::trace!("Processing event: {:?}", event);
        match event.get("method") {
            Some(method) => {
                let method = method.as_str().expect("WS event method is a string");
                match method {
                    "Runtime.consoleAPICalled" => {}
                    "Runtime.executionContextCreated" => {}
                    "Runtime.executionContextDestroyed" => {
                        self.neovim_vadre_window
                            .lock()
                            .await
                            .log_msg(VadreLogLevel::ERROR, "Finished")
                            .await
                            .expect("Can log to vim");
                    }
                    "Debugger.scriptParsed" => {
                        self.analyze_script_parsed(event).await?;
                    }
                    "Debugger.paused" => {
                        self.analyse_debugger_paused(event)
                            .await
                            .expect("Can analyze pausing debugger");
                    }
                    "Debugger.resumed" => {}
                    _ => {
                        bail!("Debugger doesn't support event method `{}`", method);
                    }
                }
            }
            None => bail!("Couldn't get method of event: {}", event),
        }

        Ok(())
    }

    async fn analyze_script_parsed(&mut self, event: &serde_json::Value) -> Result<()> {
        let file: String = match serde_json::from_value(event["params"]["url"].clone()) {
            Ok(s) => s,
            Err(e) => {
                panic!("Can't understand file: {:?}", e);
            }
        };

        let script_id: String = match serde_json::from_value(event["params"]["scriptId"].clone()) {
            Ok(s) => s,
            Err(e) => {
                panic!("Can't understand script_id: {:?}", e);
            }
        };

        self.scripts.push(Script::new(file.clone(), script_id));

        self.set_breakpoints(file).await?;

        Ok(())
    }

    pub(crate) async fn analyse_debugger_paused(
        &mut self,
        event: &serde_json::Value,
    ) -> Result<()> {
        if let Some(listener_tx) = self.stopped_listener_tx.take() {
            // If we're here we're about to do more stepping so no need to do more
            listener_tx.send(()).unwrap();
            return Ok(());
        }

        let line_number: i64 = match serde_json::from_value(
            event["params"]["callFrames"][0]["location"]["lineNumber"].clone(),
        ) {
            Ok(s) => {
                let s: i64 = s;
                s + 1
            }
            Err(e) => {
                bail!("Can't understand line_num: {:?}", e);
            }
        };

        let script = if let Some(script_id) = event["params"]["callFrames"][0]["location"]
            ["scriptId"]
            .as_str()
            .map(|s| s.to_string())
        {
            self.scripts
                .iter()
                .find(|s| s.get_script_id() == script_id)
                .expect("Can find script")
        } else {
            let file: String =
                match serde_json::from_value(event["params"]["callFrames"][0]["url"].clone()) {
                    Ok(s) => s,
                    Err(e) => {
                        // TODO: How do we get here? Handle when we see it.
                        bail!("JSON: {}, err: {}", event, e);
                    }
                };

            if let Some(script) = self.get_script_from_filename(&file).await? {
                script
            } else {
                bail!("Can't find script for {}", file);
            }
        };

        let msg = serde_json::json!({
            "method": "Debugger.getScriptSource",
            "params": {
                "scriptId": script.get_script_id(),
            }
        });

        let resp = self.request_and_response(msg).await?;

        let contents = resp["result"]["scriptSource"]
            .as_str()
            .expect("can convert to str")
            .to_string();

        self.neovim_vadre_window
            .lock()
            .await
            .set_code_buffer(
                CodeBufferContent::Content(contents),
                line_number,
                script.get_file(),
                false,
            )
            .await?;

        if script.get_file().ends_with(".ts") {
            self.neovim_vadre_window
                .lock()
                .await
                .set_file_type("typescript")
                .await?;
        } else {
            self.neovim_vadre_window
                .lock()
                .await
                .set_file_type("javascript")
                .await?;
        }

        self.display_output_info(event).await?;

        Ok(())
    }

    async fn handle_breakpoints_output_window_key(&mut self, _key: &str) -> Result<()> {
        let lines = self
            .neovim_vadre_window
            .lock()
            .await
            .get_current_buffer_lines(Some(0), None)
            .await?;

        let mut file = None;

        for line in lines.iter().rev() {
            let first_char = line
                .chars()
                .next()
                .ok_or_else(|| anyhow!("Can't get first char of line: {:?}", line))?;

            // Surely never get a file with full path beginning with a space.
            if first_char != ' ' {
                let found_file = line[0..line.len() - 1].to_string();
                if &found_file == "" {
                    bail!("Can't find file in line: {:?}", file);
                }

                file = Some(found_file);

                break;
            }
        }

        let file =
            file.ok_or_else(|| anyhow!("Can't find file for breakpoint in lines: {:?}", lines))?;

        let source_line = lines.iter().rev().next().ok_or_else(|| {
            anyhow!(
                "Can't find source line for breakpoint in lines: {:?}",
                lines
            )
        })?;

        let mut breakpoint_enabled = false;
        let mut num = "".to_string();

        for ch in source_line.chars() {
            if ch == '⬤' {
                breakpoint_enabled = true;
            }

            if ch.is_numeric() {
                num.push(ch);
            }

            if &num != "" && ch == ' ' {
                break;
            }
        }

        let num = num
            .parse::<i64>()
            .map_err(|_| anyhow!("can't parse source line from `{}`: {:?}", num, source_line))?;

        if breakpoint_enabled {
            tracing::debug!("Disabling breakpoint: {}, {}", file, num);

            self.breakpoints
                .set_breakpoint_disabled(file.clone(), num)?;
        } else {
            tracing::debug!("Enabling breakpoint: {}, {}", file, num);

            self.breakpoints.set_breakpoint_enabled(file.clone(), num)?;
        }

        self.set_breakpoints(file).await?;

        Ok(())
    }

    pub(crate) async fn display_output_info(&mut self, event: &serde_json::Value) -> Result<()> {
        let mut call_stack_buffer_content = Vec::new();

        for frame in event["params"]["callFrames"]
            .as_array()
            .expect("callFrames is an array")
        {
            let script = self
                .scripts
                .iter()
                .find(|s| {
                    s.get_script_id()
                        == frame["location"]["scriptId"]
                            .as_str()
                            .expect("location has a string script id")
                })
                .expect("Can find script");

            call_stack_buffer_content.push(format!(
                "{}:{}",
                script.get_file(),
                frame["location"]["lineNumber"]
                    .as_i64()
                    .expect("line number is integer")
                    + 1
            ));
            call_stack_buffer_content.push(format!("- {}", frame["functionName"]));
        }

        self.neovim_vadre_window
            .lock()
            .await
            .set_call_stack_buffer(call_stack_buffer_content)
            .await?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn set_breakpoints(&mut self, file: String) -> Result<()> {
        let file_canonical_path = canonical_filename(&file)?;

        tracing::trace!("Looking for {} for breakpoints", file_canonical_path);

        let line_numbers = if let Ok(breakpoint_lines) = self
            .breakpoints
            .get_all_breakpoints_for_file(&file_canonical_path)
        {
            breakpoint_lines
                .iter()
                .filter(|(_, v)| v.enabled)
                .map(|(k, _)| *k)
                .collect::<HashSet<i64>>()
        } else {
            return Ok(());
        };

        tracing::debug!("Setting breakpoint in {} at lines {:?}", file, line_numbers);

        if let Some(script) = self.get_script_from_filename(&file).await? {
            let script_id = script.get_script_id();
            for line_number in line_numbers {
                let req = serde_json::json!({
                    "method": "Debugger.setBreakpoint",
                    "params":{
                        "location":{
                            "scriptId": script_id,
                            "lineNumber": line_number - 1,
                        }
                    },
                });
                let resp = self.request_and_response(req).await?;
                tracing::trace!("Breakpoint resp: {:?}", resp);

                // TODO: Enabling breakpoints
            }
        }

        Ok(())
    }

    async fn get_script_from_filename(&self, filename: &str) -> Result<Option<&Script>> {
        let file_path = canonical_filename(filename)?;

        for script in self.scripts.iter() {
            let script_file_path = canonical_filename(script.get_file())?;
            if script_file_path == file_path {
                return Ok(Some(&script));
            }
        }
        Ok(None)
    }

    async fn request(
        &self,
        message: serde_json::Value,
        tx: Option<oneshot::Sender<serde_json::Value>>,
    ) -> Result<()> {
        self.processor.request(message, tx).await
    }

    async fn request_and_response(&self, message: serde_json::Value) -> Result<serde_json::Value> {
        let (tx, rx) = oneshot::channel();

        self.request(message, Some(tx)).await?;

        let response = rx.await?;

        Ok(response)
    }
}

impl Debug for DebuggerHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebuggerHandler")
            // .field("current_thread_id", &self.current_thread_id)
            // .field("current_frame_id", &self.current_frame_id)
            // .field("breakpoints", &self.breakpoints)
            .finish()
    }
}
