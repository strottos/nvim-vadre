use std::{
    collections::HashMap,
    fmt::Display,
    hash::{DefaultHasher, Hash, Hasher},
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::{anyhow, bail, Result};
use nvim_rs::{compat::tokio::Compat, Buffer, Neovim, Value, Window};
use tokio::fs::File as TokioFile;

// Arbitrary number so no clashes with other plugins (hopefully)
// TODO: Find a better solution (looked into a few times and not sure current neovim API supports
// anything better).
static VADRE_NEXT_SIGN_ID: AtomicUsize = AtomicUsize::new(1157831);

lazy_static! {
    static ref VIM_FILE_TYPES: HashMap<&'static str, &'static str> = {
        let mut m = HashMap::new();
        m.insert("rs", "rust");
        m.insert("c", "c");
        m.insert("js", "javascript");
        m.insert("ts", "typescript");
        m.insert("go", "go");
        m.insert("cpp", "cpp");
        m.insert("py", "python");
        m
    };
}

#[derive(Clone, Debug)]
pub(crate) enum VadreLogLevel {
    CRITICAL,
    ERROR,
    WARN,
    INFO,
    DEBUG,
}

impl VadreLogLevel {
    fn log_level(&self) -> u8 {
        match self {
            VadreLogLevel::CRITICAL => 1,
            VadreLogLevel::ERROR => 2,
            VadreLogLevel::WARN => 3,
            VadreLogLevel::INFO => 4,
            VadreLogLevel::DEBUG => 5,
        }
    }

    fn should_log(&self, level: &str) -> bool {
        let level = match level.to_ascii_uppercase().as_ref() {
            "CRITICAL" => 1,
            "INFO" => 4,
            "DEBUG" => 5,
            _ => level.parse::<u8>().unwrap_or(5),
        };

        self.log_level() <= level
    }
}

impl Display for VadreLogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum VadreWindowType {
    Code,
    Output,
}

impl VadreWindowType {
    fn window_name_prefix(&self) -> &str {
        match self {
            VadreWindowType::Code => "Vadre Code",
            VadreWindowType::Output => "Vadre Program",
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum VadreBufferType {
    Code,
    Logs,
    Terminal,
    CallStack,
    Variables,
    Breakpoints,
}

impl VadreBufferType {
    fn window_type(&self) -> VadreWindowType {
        match self {
            VadreBufferType::Code => VadreWindowType::Code,
            VadreBufferType::Logs => VadreWindowType::Output,
            VadreBufferType::Terminal => VadreWindowType::Output,
            VadreBufferType::CallStack => VadreWindowType::Output,
            VadreBufferType::Variables => VadreWindowType::Output,
            VadreBufferType::Breakpoints => VadreWindowType::Output,
        }
    }

    fn buffer_name_prefix(&self) -> String {
        self.window_type().window_name_prefix().to_string()
    }

    fn buffer_post_display(&self) -> Option<&str> {
        match self {
            VadreBufferType::Code => None,
            VadreBufferType::Logs => Some("Logs"),
            VadreBufferType::Terminal => Some("Terminal"),
            VadreBufferType::CallStack => Some("CallStack"),
            VadreBufferType::Variables => Some("Variables"),
            VadreBufferType::Breakpoints => Some("Breakpoints"),
        }
    }

    fn type_name(&self) -> &str {
        match self {
            VadreBufferType::Code => "Code",
            VadreBufferType::Logs => "Logs",
            VadreBufferType::Terminal => "Terminal",
            VadreBufferType::CallStack => "CallStack",
            VadreBufferType::Variables => "Variables",
            VadreBufferType::Breakpoints => "Breakpoints",
        }
    }

    fn next_output_type(&self) -> VadreBufferType {
        match self {
            VadreBufferType::Code => unreachable!(),
            VadreBufferType::Terminal => VadreBufferType::Logs,
            VadreBufferType::Logs => VadreBufferType::CallStack,
            VadreBufferType::CallStack => VadreBufferType::Variables,
            VadreBufferType::Variables => VadreBufferType::Breakpoints,
            VadreBufferType::Breakpoints => VadreBufferType::Terminal,
        }
    }

    fn previous_output_type(&self) -> VadreBufferType {
        match self {
            VadreBufferType::Code => unreachable!(),
            VadreBufferType::Terminal => VadreBufferType::Breakpoints,
            VadreBufferType::Logs => VadreBufferType::Terminal,
            VadreBufferType::CallStack => VadreBufferType::Logs,
            VadreBufferType::Variables => VadreBufferType::CallStack,
            VadreBufferType::Breakpoints => VadreBufferType::Variables,
        }
    }

    fn lua_name(&self) -> &str {
        match self {
            VadreBufferType::Code => unreachable!(),
            VadreBufferType::Terminal => "terminal",
            VadreBufferType::Logs => "logs",
            VadreBufferType::CallStack => "callstack",
            VadreBufferType::Variables => "variables",
            VadreBufferType::Breakpoints => "breakpoints",
        }
    }
}

#[derive(Debug, Hash)]
pub(crate) enum CodeBufferContent<'a> {
    File(&'a str),
    Content(&'a str),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum VadreOutputBufferSelector {
    Next,
    Previous,
    Logs,
    CallStack,
    Variables,
    Breakpoints,
}

impl VadreOutputBufferSelector {
    fn get_type(type_: &str) -> Result<Self> {
        match type_
            .to_lowercase()
            .chars()
            .nth(0)
            .ok_or_else(|| anyhow!("Empty type"))?
        {
            'n' => Ok(Self::Next),
            'p' => Ok(Self::Previous),
            'l' => Ok(Self::Logs),
            's' => Ok(Self::CallStack),
            'v' => Ok(Self::Variables),
            'b' => Ok(Self::Breakpoints),
            _ => Err(anyhow!("Can't understand type {}", type_)),
        }
    }
}

impl Display for VadreOutputBufferSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VadreOutputBufferSelector::Next => write!(f, "Next"),
            VadreOutputBufferSelector::Previous => write!(f, "Previous"),
            VadreOutputBufferSelector::Logs => write!(f, "Logs"),
            VadreOutputBufferSelector::CallStack => write!(f, "CallStack"),
            VadreOutputBufferSelector::Variables => write!(f, "Variables"),
            VadreOutputBufferSelector::Breakpoints => write!(f, "Breakpoints"),
        }
    }
}

#[derive(Clone)]
pub(crate) struct NeovimVadreWindow {
    neovim: Neovim<Compat<TokioFile>>,
    instance_id: usize,
    current_output: VadreBufferType,
    windows: HashMap<VadreWindowType, Window<Compat<TokioFile>>>,
    buffers: HashMap<VadreBufferType, Buffer<Compat<TokioFile>>>,
    pointer_sign_id: usize,
    previous_buffer_hash: Option<u64>,
}

impl NeovimVadreWindow {
    pub(crate) fn new(neovim: Neovim<Compat<TokioFile>>, instance_id: usize) -> Self {
        Self {
            neovim,
            instance_id,
            current_output: VadreBufferType::Terminal,
            buffers: HashMap::new(),
            windows: HashMap::new(),
            pointer_sign_id: VADRE_NEXT_SIGN_ID.fetch_add(1, Ordering::SeqCst),
            previous_buffer_hash: None,
        }
    }

    pub(crate) async fn create_ui(&mut self) -> Result<()> {
        let eventignore_old = self.neovim.get_var("eventignore").await;
        self.neovim.set_var("eventignore", "all".into()).await?;

        self.neovim.command("tab new").await?;

        // Now setup the current window which must by construction be empty
        let current_tabpage = self.neovim.get_current_tabpage().await?;

        self.neovim.command("vnew").await?;
        let mut windows = current_tabpage.list_wins().await?.into_iter();
        assert_eq!(2, windows.len());

        // Window 1 is Code
        let code_window = windows.next().ok_or_else(|| anyhow!("No code window"))?;
        let code_buffer = code_window.get_buf().await?;
        self.neovim.set_current_win(&code_window).await?;
        self.set_vadre_buffer_options(&code_buffer, &VadreBufferType::Code)
            .await?;
        self.set_vadre_debugger_keys_for_buffer(&code_buffer)
            .await?;

        // Window 2 is Terminal
        let output_window = windows
            .next()
            .ok_or_else(|| anyhow!("No terminal window"))?;
        let terminal_buffer = output_window.get_buf().await?;
        self.set_vadre_buffer_options(&terminal_buffer, &VadreBufferType::Terminal)
            .await?;

        // Extra output buffers
        self.neovim.set_current_win(&output_window).await?;

        for buffer_type in [
            VadreBufferType::Logs,
            VadreBufferType::CallStack,
            VadreBufferType::Variables,
            VadreBufferType::Breakpoints,
        ] {
            let buffer = self.neovim.create_buf(false, false).await?;

            self.set_vadre_buffer_options(&buffer, &buffer_type).await?;

            self.neovim
                .exec_lua(
                    &format!(
                        "require('vadre.ui').set_popup({}, '{}', {})",
                        self.instance_id,
                        buffer_type.lua_name(),
                        buffer.get_number().await?,
                    ),
                    vec![],
                )
                .await
                .map_err(|e| anyhow!("Lua error: {e:?}"))?;

            self.buffers.insert(buffer_type, buffer);
        }

        self.neovim.set_current_win(&code_window).await?;

        self.windows.insert(VadreWindowType::Code, code_window);
        self.windows.insert(VadreWindowType::Output, output_window);
        self.buffers.insert(VadreBufferType::Code, code_buffer);
        self.buffers
            .insert(VadreBufferType::Terminal, terminal_buffer);

        // Special first log line to get rid of the annoying
        self.log_msg(VadreLogLevel::INFO, "Vadre Setup UI").await?;

        let au_group_name = format!("vadre_{}", self.instance_id);

        self.neovim.create_augroup(&au_group_name, vec![]).await?;
        self.neovim
            .exec_lua(
                &format!(
                    r#"vim.api.nvim_create_autocmd(
                        {{"BufHidden"}},
                        {{
                             callback = require("vadre.autocmds").on_close_vadre_window,
                             group = "{}",
                             buffer = {},
                         }}
                    )"#,
                    au_group_name,
                    self.buffers
                        .get(&VadreBufferType::Code)
                        .ok_or_else(|| anyhow!("Can't find Code buffer"))?
                        .get_number()
                        .await?,
                ),
                vec![],
            )
            .await?;

        match eventignore_old {
            Ok(x) => self.neovim.set_var("eventignore", x).await?,
            Err(_) => self.neovim.set_var("eventignore", "".into()).await?,
        };
        self.neovim.command("doautocmd User VadreUICreated").await?;

        Ok(())
    }

    pub(crate) async fn log_msg(&self, level: VadreLogLevel, msg: &str) -> Result<()> {
        // TODO: Cache this for a while?
        let set_level = match self.neovim.get_var("vadre_log_level").await {
            Ok(x) => match x {
                Value::Integer(x) => x.to_string(),
                Value::String(x) => x
                    .as_str()
                    .ok_or_else(|| anyhow!("Can't convert variable g:vadre_log_level to string"))?
                    .to_string(),
                _ => "INFO".to_string(),
            },
            Err(_) => "INFO".to_string(),
        };

        if !level.should_log(&set_level) {
            return Ok(());
        }

        let buffer = self
            .buffers
            .get(&VadreBufferType::Logs)
            .ok_or_else(|| anyhow!("Logs buffer not found, have you setup the UI?"))?;
        let window = self.windows.get(&VadreWindowType::Output);

        let now = chrono::offset::Local::now();

        let msgs = msg
            .split('\n')
            .map(move |msg| format!("{} [{}] {}", now.format("%a %H:%M:%S%.6f"), level, msg))
            .collect();

        let mut cursor_at_end = true;
        let line_count = buffer.line_count().await?;

        if let Some(window) = window {
            let current_cursor = window.get_cursor().await?;
            if current_cursor.0 < line_count {
                cursor_at_end = false;
            }
        }
        self.write_to_buffer(buffer, -1, -1, msgs).await?;

        if let Some(window) = window {
            if cursor_at_end && line_count > 1 {
                window.set_cursor((line_count + 1, 0)).await?;
            }
        }

        Ok(())
    }

    pub(crate) async fn spawn_terminal_command(
        &mut self,
        command: String,
        debug_program_str: &str,
    ) -> Result<()> {
        let original_window = self.neovim.get_current_win().await?;

        let terminal_window = self
            .windows
            .get(&VadreWindowType::Output)
            .ok_or_else(|| anyhow!("Can't find terminal window"))?;
        self.neovim.set_current_win(terminal_window).await?;

        tracing::debug!("Running terminal command {}", command);

        self.neovim
            .command(&format!("terminal! {}", command))
            .await?;

        let terminal_buffer = terminal_window.get_buf().await?;

        let au_group_name = format!("vadre_{}", self.instance_id);
        self.neovim
            .exec_lua(
                &format!(
                    r#"vim.api.nvim_create_autocmd(
                        {{"BufHidden"}},
                        {{
                            callback = require("vadre.autocmds").on_close_vadre_window,
                            group = "{}",
                            buffer = {},
                        }}
                    )"#,
                    au_group_name,
                    terminal_buffer.get_number().await?,
                ),
                vec![],
            )
            .await?;
        self.neovim
            .exec_lua(
                &format!(
                    r#"vim.api.nvim_create_autocmd(
                        {{"BufEnter"}},
                        {{
                            callback = require("vadre.autocmds").on_enter_vadre_output_window,
                            group = "{}",
                            buffer = {},
                        }}
                    )"#,
                    au_group_name,
                    terminal_buffer.get_number().await?,
                ),
                vec![],
            )
            .await?;

        if let Err(e) = terminal_buffer
            .set_name(&format!(
                "Vadre Program ({}) - {}",
                self.instance_id, debug_program_str
            ))
            .await
        {
            // TODO: Find out why we can't do this more than once, I simply don't know at
            // present
            tracing::error!("Can't set name of terminal buffer: {:?}", e);
        }

        self.buffers
            .insert(VadreBufferType::Terminal, terminal_buffer);

        self.neovim.set_current_win(&original_window).await?;

        Ok(())
    }

    pub(crate) async fn set_code_buffer<'a>(
        &mut self,
        content: CodeBufferContent<'a>,
        line_number: i64,
        buffer_name: &str,
        force_replace: bool,
    ) -> Result<()> {
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        let content_hash = hasher.finish();

        let code_buffer = self
            .buffers
            .get(&VadreBufferType::Code)
            .ok_or_else(|| anyhow!("Can't find code window"))?;

        let old_buffer_name = code_buffer.get_name().await?;
        let buffer_name = self.get_buffer_name(&VadreBufferType::Code, Some(buffer_name));

        if self.previous_buffer_hash != Some(content_hash)
            || old_buffer_name.ends_with(&buffer_name)
            || force_replace
        {
            tracing::trace!("Opening in code buffer: {:?}", content);

            let content = match &content {
                CodeBufferContent::File(path_name) => {
                    let path = Path::new(&path_name);

                    tracing::trace!("Resetting file to {:?}", path);

                    if !path.exists() {
                        bail!("Source path {:?} doesn't exist", path_name);
                    }

                    if let Some(file_type) = path.extension() {
                        let file_type = file_type.to_str().ok_or_else(|| {
                            anyhow!("Can't convert file type to string from {:?}", file_type)
                        })?;
                        if let Some(file_type) = VIM_FILE_TYPES.get(&file_type) {
                            self.set_file_type(file_type).await?;
                        }
                    }

                    tokio::fs::read_to_string(path)
                        .await?
                        .split('\n')
                        .map(|x| x.trim_end().to_string())
                        .collect()
                }
                CodeBufferContent::Content(content) => {
                    let split_char = if content.contains("\r\n") {
                        "\r\n"
                    } else {
                        "\n"
                    };
                    content
                        .split(split_char)
                        .map(|x| x.trim_end().to_string())
                        .collect()
                }
            };

            self.write_to_buffer(code_buffer, 0, 0, content).await?;

            code_buffer.set_name(&buffer_name).await?;

            self.previous_buffer_hash = Some(content_hash);
        };

        self.unplace_code_pointer().await?;

        if let Err(e) = self
            .neovim
            .exec(
                &format!(
                    "sign place {} line={} name=VadreDebugPointer buffer={}",
                    self.pointer_sign_id,
                    line_number,
                    code_buffer.get_number().await?,
                ),
                false,
            )
            .await
        {
            tracing::error!("Can't place pointer: {:?}", e);
            self.log_msg(VadreLogLevel::ERROR, "Failed to place pointer")
                .await?;
        }

        let code_window = self
            .windows
            .get(&VadreWindowType::Code)
            .ok_or_else(|| anyhow!("Can't find Code window"))?;
        code_window.set_cursor((line_number, 0)).await?;

        Ok(())
    }

    pub(crate) async fn unplace_code_pointer(&self) -> Result<()> {
        self.neovim
            .exec(&format!("sign unplace {}", self.pointer_sign_id), false)
            .await?;
        Ok(())
    }

    pub(crate) async fn set_file_type(&self, file_type: &str) -> Result<()> {
        self.neovim
            .command(&format!("set filetype={}", file_type))
            .await?;

        Ok(())
    }

    pub(crate) async fn set_call_stack_buffer(&self, content: Vec<String>) -> Result<()> {
        let buffer = self
            .buffers
            .get(&VadreBufferType::CallStack)
            .ok_or_else(|| anyhow!("call stack output buffer doesn't exist"))?;
        self.write_to_buffer(buffer, 0, 0, content).await?;

        Ok(())
    }

    pub(crate) async fn set_variables_buffer(&self, content: Vec<String>) -> Result<()> {
        let buffer = self
            .buffers
            .get(&VadreBufferType::Variables)
            .ok_or_else(|| anyhow!("variables output buffer doesn't exist"))?;
        self.write_to_buffer(buffer, 0, 0, content).await?;

        Ok(())
    }

    pub(crate) async fn set_breakpoints_buffer(&self, content: Vec<String>) -> Result<()> {
        let buffer = self
            .buffers
            .get(&VadreBufferType::Breakpoints)
            .ok_or_else(|| anyhow!("breakpoints output buffer doesn't exist"))?;
        self.write_to_buffer(buffer, 0, 0, content).await?;

        Ok(())
    }

    pub(crate) async fn get_output_window_type(&self) -> Result<VadreBufferType> {
        Ok(self.current_output.clone())
    }

    pub(crate) async fn change_output_window(&mut self, type_: &str) -> Result<()> {
        let type_ = VadreOutputBufferSelector::get_type(type_)?;

        let new_output_type = match type_ {
            VadreOutputBufferSelector::Next => self.current_output.next_output_type(),
            VadreOutputBufferSelector::Previous => self.current_output.previous_output_type(),
            VadreOutputBufferSelector::Logs => VadreBufferType::Logs,
            VadreOutputBufferSelector::CallStack => VadreBufferType::CallStack,
            VadreOutputBufferSelector::Variables => VadreBufferType::Variables,
            VadreOutputBufferSelector::Breakpoints => VadreBufferType::Breakpoints,
        };

        self.neovim
            .exec_lua(
                &format!(
                    "require('vadre.ui').display_output_window({}, '{}')",
                    self.instance_id,
                    new_output_type.lua_name()
                ),
                vec![],
            )
            .await
            .map_err(|e| anyhow!("Lua error: {e:?}"))?;

        self.current_output = new_output_type;

        Ok(())
    }

    async fn write_to_buffer(
        &self,
        buffer: &Buffer<Compat<TokioFile>>,
        start_line: i64,
        mut end_line: i64,
        msgs: Vec<String>,
    ) -> Result<()> {
        let line_count = buffer.line_count().await?;
        if start_line == 0 && end_line == 0 {
            end_line = line_count;
        }
        buffer.set_option("modifiable", true.into()).await?;
        if line_count == 1
            && buffer
                .get_lines(0, 1, true)
                .await?
                .first()
                .ok_or_else(|| anyhow!("Couldn't get first line"))?
                .is_empty()
        {
            buffer.set_lines(0, 1, false, msgs).await?;
        } else {
            buffer.set_lines(start_line, end_line, false, msgs).await?;
        }
        buffer.set_option("modifiable", false.into()).await?;

        Ok(())
    }

    async fn set_vadre_buffer_options(
        &self,
        buffer: &Buffer<Compat<TokioFile>>,
        buffer_type: &VadreBufferType,
    ) -> Result<()> {
        let buffer_name = self.get_buffer_name(buffer_type, None);
        let file_type = buffer_type.type_name();
        buffer.set_name(&buffer_name).await?;
        buffer.set_option("swapfile", false.into()).await?;
        buffer.set_option("buftype", "nofile".into()).await?;
        buffer.set_option("filetype", file_type.into()).await?;
        buffer.set_option("buflisted", false.into()).await?;
        buffer.set_option("modifiable", false.into()).await?;

        Ok(())
    }

    fn get_buffer_name(&self, buffer_type: &VadreBufferType, post_display: Option<&str>) -> String {
        let post_display = post_display.or(buffer_type.buffer_post_display());
        if let Some(post_display) = post_display {
            format!(
                "{} ({}) - {}",
                buffer_type.buffer_name_prefix(),
                self.instance_id,
                post_display,
            )
        } else {
            format!(
                "{} ({})",
                buffer_type.buffer_name_prefix(),
                self.instance_id,
            )
        }
    }

    async fn set_vadre_debugger_keys_for_buffer(
        &self,
        buffer: &Buffer<Compat<TokioFile>>,
    ) -> Result<()> {
        // TODO: Configurable?
        for (key, action) in vec![
            (
                "S",
                // This is a bit of a hack to have to quote the argument after VadreStepIn,
                // ideally we wouldn't do this but neovim seems to get upset if the argument
                // after when you specify a count is an integer.
                &format!(
                    ":<C-U>execute v:count1 . \" VadreStepIn \\\"{}\\\"\"<CR>",
                    self.instance_id
                ),
            ),
            (
                "s",
                // See comment above for why multiple levels of quoting
                &format!(
                    ":<C-U>execute v:count1 . \" VadreStepOver \\\"{}\\\"\"<CR>",
                    self.instance_id
                ),
            ),
            ("c", &format!(":VadreContinue {}<CR>", self.instance_id)),
            ("C", &format!(":VadreContinue {}<CR>", self.instance_id)),
            (
                "<C-c>",
                &format!(":VadreInterrupt {}<CR>", self.instance_id),
            ),
            (
                "<localleader>tt",
                &"<cmd>lua require('vadre.setup').toggle_single_thread()<CR>".to_string(),
            ),
            (
                "<localleader>l",
                &format!(
                    "<cmd>lua require('vadre.setup').output_window({}, 'Logs')<CR>",
                    self.instance_id
                ),
            ),
            (
                "<localleader>s",
                &format!(
                    "<cmd>lua require('vadre.setup').output_window({}, 'CallStack')<CR>",
                    self.instance_id
                ),
            ),
            (
                "<localleader>v",
                &format!(
                    "<cmd>lua require('vadre.setup').output_window({}, 'Variables')<CR>",
                    self.instance_id
                ),
            ),
            (
                "<localleader>b",
                &format!(
                    "<cmd>lua require('vadre.setup').output_window({}, 'Breakpoints')<CR>",
                    self.instance_id
                ),
            ),
            (
                ">",
                &format!(
                    "<cmd>lua require('vadre.setup').output_window({}, '{}')<CR>",
                    self.instance_id,
                    VadreOutputBufferSelector::Next
                ),
            ),
            (
                "<",
                &format!(
                    "<cmd>lua require('vadre.setup').output_window({}, '{}')<CR>",
                    self.instance_id,
                    VadreOutputBufferSelector::Previous
                ),
            ),
        ] {
            buffer
                .set_keymap(
                    "n",
                    key,
                    action,
                    vec![
                        (Value::String("noremap".into()), Value::Boolean(true)),
                        (Value::String("silent".into()), Value::Boolean(true)),
                    ],
                )
                .await?; // nnoremap <silent> <buffer> r :PadreRun<cr>
        }

        Ok(())
    }

    pub(crate) async fn get_var(&self, var_name: &str) -> Result<Value> {
        self.neovim.get_var(var_name).await.map_err(|e| anyhow!(e))
    }

    pub(crate) async fn err_writeln(&self, log_msg: &str) -> Result<()> {
        self.neovim
            .err_writeln(log_msg)
            .await
            .map_err(|e| anyhow!(e))
    }

    /// Get the current buffer lines
    ///
    /// Both start and end are optional and if not specified we assume current line inclusive for
    /// each.
    pub(crate) async fn get_current_buffer_lines(
        &self,
        start: Option<i64>,
        end: Option<i64>,
    ) -> Result<Vec<String>> {
        let window = self
            .neovim
            .get_current_win()
            .await
            .map_err(|e| anyhow!(e))?;
        let buffer = self
            .neovim
            .get_current_buf()
            .await
            .map_err(|e| anyhow!(e))?;

        let current_line = window.get_cursor().await?.0;

        let start = start.unwrap_or(current_line - 1);
        let end = end.unwrap_or(current_line);

        buffer
            .get_lines(start, end, false)
            .await
            .map_err(|e| anyhow!(e))
    }

    pub(crate) async fn get_current_line(&self) -> Result<String> {
        self.neovim.get_current_line().await.map_err(|e| anyhow!(e))
    }
}

pub(crate) async fn setup_signs(neovim: &Neovim<Compat<TokioFile>>) -> Result<()> {
    let sign_background_colour_output = neovim
        .get_hl(0.into(), vec![("name".into(), "SignColumn".into())])
        .await?;

    tracing::trace!(
        "sign_background_colour_output: {:?}",
        sign_background_colour_output
    );

    let mut bg = Value::Nil;
    let mut ctermbg = Value::Nil;

    for (key, value) in sign_background_colour_output {
        if key.as_str() == Some("bg") {
            bg = value;
        } else if key.as_str() == Some("ctermbg") {
            ctermbg = value;
        }
    }

    let mut source_breakpoint_options = vec![
        ("fg".into(), "#ff0000".into()),
        ("ctermfg".into(), "red".into()),
        ("bold".into(), true.into()),
    ];

    let mut enabled_breakpoint_options = vec![
        ("fg".into(), "#ff0000".into()),
        ("ctermfg".into(), "red".into()),
        ("bold".into(), true.into()),
    ];

    let mut debug_pointer_options = vec![
        ("fg".into(), "#00ff00".into()),
        ("ctermfg".into(), "green".into()),
        ("bold".into(), true.into()),
    ];

    if bg != Value::Nil {
        source_breakpoint_options.push(("bg".into(), bg.clone()));
        enabled_breakpoint_options.push(("bg".into(), bg.clone()));
        debug_pointer_options.push(("bg".into(), bg));
    }

    if ctermbg != Value::Nil {
        source_breakpoint_options.push(("ctermbg".into(), ctermbg.clone()));
        enabled_breakpoint_options.push(("ctermbg".into(), ctermbg.clone()));
        debug_pointer_options.push(("ctermbg".into(), ctermbg));
    }

    neovim
        .set_hl(
            0,
            "VadreSourceBreakpointHighlight",
            source_breakpoint_options,
        )
        .await?;
    neovim
        .set_hl(
            0,
            "VadreEnabledBreakpointHighlight",
            enabled_breakpoint_options,
        )
        .await?;
    neovim
        .set_hl(0, "VadreDebugPointerHighlight", debug_pointer_options)
        .await?;

    neovim
        .call_function(
            "sign_define",
            vec![
                "VadreSourceBreakpoint".into(),
                Value::Map(vec![
                    ("text".into(), "⬤".into()),
                    ("texthl".into(), "VadreSourceBreakpointHighlight".into()),
                ]),
            ],
        )
        .await?;
    neovim
        .call_function(
            "sign_define",
            vec![
                "VadreDebugPointer".into(),
                Value::Map(vec![
                    ("text".into(), "->".into()),
                    ("texthl".into(), "VadreDebugPointerHighlight".into()),
                ]),
            ],
        )
        .await?;

    Ok(())
}

pub(crate) async fn toggle_breakpoint_sign(
    neovim: &Neovim<Compat<TokioFile>>,
    line_number: i64,
) -> Result<bool> {
    let buffer_number_output = neovim.exec("echo bufnr()", true).await?;
    let buffer_number = buffer_number_output.trim();

    if let Some(breakpoint_id) = line_is_breakpoint(neovim, buffer_number, line_number).await? {
        neovim
            .command(&format!(
                "sign unplace {} buffer={}",
                breakpoint_id, buffer_number,
            ))
            .await?;

        Ok(false)
    } else {
        let pointer_sign_id = VADRE_NEXT_SIGN_ID.fetch_add(1, Ordering::SeqCst);

        neovim
            .exec(
                &format!(
                    "sign place {} line={} name=VadreSourceBreakpoint buffer={}",
                    pointer_sign_id, line_number, buffer_number,
                ),
                false,
            )
            .await?;

        Ok(true)
    }
}

pub(crate) async fn line_is_breakpoint(
    neovim: &Neovim<Compat<TokioFile>>,
    buffer_number: &str,
    line_number: i64,
) -> Result<Option<u64>> {
    let signs_in_file = neovim
        .exec(&format!("sign place buffer={}", buffer_number), true)
        .await?;

    let signs_in_file_on_line = signs_in_file
        .split('\n')
        .filter(|line| {
            line.contains("name=VadreSourceBreakpoint")
                && line.contains(&format!("line={}", line_number))
        })
        .collect::<Vec<&str>>();

    assert!(signs_in_file_on_line.len() <= 1);

    if let Some(signs_in_file_on_line) = signs_in_file_on_line.first() {
        let mut breakpoint_id = 0;
        for snippet in signs_in_file_on_line.split(' ') {
            if snippet.len() >= 3 && &snippet[0..3] == "id=" {
                breakpoint_id = snippet[3..]
                    .parse::<u64>()
                    .map_err(|e| anyhow!("id is a u64: {e}"))?;
            }
        }

        assert_ne!(breakpoint_id, 0);

        Ok(Some(breakpoint_id))
    } else {
        Ok(None)
    }
}
