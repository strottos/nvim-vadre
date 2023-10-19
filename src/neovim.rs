use std::{
    cmp,
    collections::HashMap,
    fmt::Display,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::{bail, Result};
use nvim_rs::{compat::tokio::Compat, Buffer, Neovim, Value, Window};
use tokio::io::Stdout;

use crate::debuggers::Breakpoint;

// Arbitrary number so no clashes with other plugins (hopefully)
// TODO: Find a better solution
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
pub enum VadreLogLevel {
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
            _ => match level.parse::<u8>() {
                Ok(x) => x,
                Err(_) => 5,
            },
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
    Program,
}

impl VadreWindowType {
    fn window_name_prefix(&self) -> &str {
        match self {
            VadreWindowType::Code => "Vadre Code",
            VadreWindowType::Output => "Vadre Output",
            VadreWindowType::Program => "Vadre Program",
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum VadreBufferType {
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
            VadreBufferType::Terminal => VadreWindowType::Program,
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

    fn buffer_should_wrap(&self) -> bool {
        // TODO: Take into account default
        match &self {
            VadreBufferType::Code => true,
            VadreBufferType::Logs => true,
            VadreBufferType::Terminal => true,
            VadreBufferType::CallStack => false,
            VadreBufferType::Variables => false,
            VadreBufferType::Breakpoints => true,
        }
    }

    fn get_output_buffer(
        current_buf_name: &str,
        type_: VadreOutputBufferSelector,
    ) -> VadreBufferType {
        let mut split = current_buf_name.rsplit(" - ");
        let output_buffer_type = split
            .next()
            .expect("should be able to retrieve output buffer type");
        let output_buffer_type = VadreBufferType::get_buffer_type_from_str(output_buffer_type);

        let buffer_order = vec![
            VadreBufferType::Logs,
            VadreBufferType::CallStack,
            VadreBufferType::Variables,
            VadreBufferType::Breakpoints,
        ];

        let mut index = buffer_order
            .iter()
            .position(|r| *r == output_buffer_type)
            .unwrap();

        match type_ {
            VadreOutputBufferSelector::Next => index += 1,
            VadreOutputBufferSelector::Previous => {
                if index == 0 {
                    index = buffer_order.len();
                }
                index -= 1;
            }
            VadreOutputBufferSelector::Logs => index = 0,
            VadreOutputBufferSelector::CallStack => index = 1,
            VadreOutputBufferSelector::Variables => index = 2,
            VadreOutputBufferSelector::Breakpoints => index = 3,
        };
        let index: usize = index % buffer_order.len();

        buffer_order.get(index).unwrap().clone()
    }

    fn get_buffer_type_from_str(s: &str) -> VadreBufferType {
        match s {
            "Code" => VadreBufferType::Code,
            "Logs" => VadreBufferType::Logs,
            "Terminal" => VadreBufferType::Terminal,
            "CallStack" => VadreBufferType::CallStack,
            "Variables" => VadreBufferType::Variables,
            "Breakpoints" => VadreBufferType::Breakpoints,
            _ => panic!("Can't understand string {}", s),
        }
    }
}

#[derive(Debug)]
pub enum CodeBufferContent<'a> {
    File(&'a str),
    Content(String),
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
    fn get_type(type_: &str) -> Self {
        tracing::trace!("Type: {}", type_);
        match type_.to_lowercase().chars().nth(0).unwrap() {
            'n' => Self::Next,
            'p' => Self::Previous,
            'l' => Self::Logs,
            's' => Self::CallStack,
            'v' => Self::Variables,
            'b' => Self::Breakpoints,
            _ => panic!("Can't understand type {}", type_),
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
pub struct NeovimVadreWindow {
    neovim: Neovim<Compat<Stdout>>,
    instance_id: usize,
    windows: HashMap<VadreWindowType, Window<Compat<Stdout>>>,
    buffers: HashMap<VadreBufferType, Buffer<Compat<Stdout>>>,
    pointer_sign_id: usize,
}

impl NeovimVadreWindow {
    pub fn new(neovim: Neovim<Compat<Stdout>>, instance_id: usize) -> Self {
        Self {
            neovim,
            instance_id,
            buffers: HashMap::new(),
            windows: HashMap::new(),
            pointer_sign_id: VADRE_NEXT_SIGN_ID.fetch_add(1, Ordering::SeqCst),
        }
    }

    pub async fn create_ui(&mut self) -> Result<()> {
        let eventignore_old = self.neovim.get_var("eventignore").await;
        self.neovim.set_var("eventignore", "all".into()).await?;

        self.check_if_new_tab_needed().await?;

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
        let window = windows.next().unwrap();
        self.neovim.set_current_win(&window).await?;

        self.neovim.command("vnew").await?;
        let mut windows = current_tabpage.list_wins().await?.into_iter();
        assert_eq!(3, windows.len());

        // Window 1 is Code
        let window = windows.next().unwrap();
        let buffer = window.get_buf().await?;
        self.neovim.set_current_win(&window).await?;
        self.set_vadre_buffer_options(&buffer, &VadreBufferType::Code)
            .await?;
        self.set_vadre_debugger_keys_for_buffer(&buffer).await?;

        self.windows.insert(VadreWindowType::Code, window);
        self.buffers.insert(VadreBufferType::Code, buffer);

        // Window 2 is Output stuff, logs at the moment
        let window = windows.next().unwrap();
        window.set_option("wrap", true.into()).await?;
        let log_buffer = window.get_buf().await?;
        self.set_vadre_buffer_options(&log_buffer, &VadreBufferType::Logs)
            .await?;
        self.set_vadre_debugger_keys_for_buffer(&log_buffer).await?;

        self.windows.insert(VadreWindowType::Output, window);
        self.buffers.insert(VadreBufferType::Logs, log_buffer);

        // Window 3 is Output stuff, logs at the moment
        let window = windows.next().unwrap();
        let terminal_buffer = window.get_buf().await?;
        self.set_vadre_buffer_options(&terminal_buffer, &VadreBufferType::Terminal)
            .await?;
        window.set_height(output_window_height).await?;

        self.windows.insert(VadreWindowType::Program, window);
        self.buffers
            .insert(VadreBufferType::Terminal, terminal_buffer);

        // Extra output buffers
        for buffer_type in vec![
            VadreBufferType::CallStack,
            VadreBufferType::Variables,
            VadreBufferType::Breakpoints,
        ] {
            let buffer = self.neovim.create_buf(false, false).await?;
            self.set_vadre_buffer_options(&buffer, &buffer_type).await?;
            self.buffers.insert(buffer_type, buffer);
        }

        // Special first log line to get rid of the annoying
        self.log_msg(VadreLogLevel::INFO, "Vadre Setup UI").await?;

        match eventignore_old {
            Ok(x) => self.neovim.set_var("eventignore", x).await?,
            Err(_) => self.neovim.set_var("eventignore", "".into()).await?,
        };
        self.neovim.command("doautocmd User VadreUICreated").await?;

        Ok(())
    }

    pub async fn log_msg(&self, level: VadreLogLevel, msg: &str) -> Result<()> {
        tracing::trace!("Logging message: {}", msg);

        // TODO: Cache this for a while?
        let set_level = match self.neovim.get_var("vadre_log_level").await {
            Ok(x) => match x {
                Value::Integer(x) => x.to_string(),
                Value::String(x) => x.as_str().clone().unwrap().to_string(),
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
            .expect("Logs buffer not found, have you setup the UI?");
        let window = self
            .windows
            .get(&VadreWindowType::Output)
            .expect("Logs window not found, have you setup the UI?");

        let now = chrono::offset::Local::now();

        let msgs = msg
            .split("\n")
            .map(move |msg| format!("{} [{}] {}", now.format("%a %H:%M:%S%.6f"), level, msg))
            .collect();

        let mut cursor_at_end = true;
        let current_cursor = window.get_cursor().await?;
        let line_count = buffer.line_count().await?;

        if current_cursor.0 < line_count {
            cursor_at_end = false;
        }
        self.write_to_buffer(&buffer, -1, -1, msgs).await?;

        if cursor_at_end && line_count > 1 {
            window.set_cursor((line_count + 1, 0)).await?;
        }

        Ok(())
    }

    pub async fn spawn_terminal_command(
        &self,
        command: String,
        debug_program_str: Option<&str>,
    ) -> Result<()> {
        let original_window = self.neovim.get_current_win().await?;

        let terminal_window = self.windows.get(&VadreWindowType::Program).unwrap();
        self.neovim.set_current_win(&terminal_window).await?;

        tracing::debug!("Running terminal command {}", command);

        self.neovim
            .command(&format!("terminal! {}", command))
            .await?;

        match debug_program_str {
            Some(debug_program_str) => {
                self.neovim
                    .get_current_win()
                    .await?
                    .get_buf()
                    .await?
                    .set_name(debug_program_str)
                    .await?;
            }
            None => {}
        };

        self.neovim.set_current_win(&original_window).await?;

        Ok(())
    }

    pub async fn set_code_buffer<'a>(
        &self,
        content: CodeBufferContent<'a>,
        line_number: i64,
        buffer_name: &str,
        force_replace: bool,
    ) -> Result<()> {
        tracing::trace!("Opening in code buffer: {:?}", content);

        let code_buffer = self.buffers.get(&VadreBufferType::Code).unwrap();

        let old_buffer_name = code_buffer.get_name().await?;
        let buffer_name = self.get_buffer_name(&VadreBufferType::Code, Some(buffer_name));

        if !old_buffer_name.ends_with(&buffer_name) || force_replace {
            let content = match &content {
                CodeBufferContent::File(path_name) => {
                    let path = Path::new(&path_name);

                    tracing::trace!("Resetting file to {:?}", path);

                    if !path.exists() {
                        bail!("Source path {:?} doesn't exist", path_name);
                    }

                    if let Some(file_type) = path.extension().map(|x| x.to_str().unwrap()) {
                        match VIM_FILE_TYPES.get(&file_type) {
                            Some(file_type) => {
                                self.set_file_type(file_type).await?;
                            }
                            None => {}
                        };
                    }

                    tokio::fs::read_to_string(path)
                        .await?
                        .split("\n")
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

            self.write_to_buffer(&code_buffer, 0, 0, content).await?;

            code_buffer.set_name(&buffer_name).await?;
        };

        let pointer_sign_id = self.pointer_sign_id;
        self.neovim
            .exec(&format!("sign unplace {}", pointer_sign_id), false)
            .await?;

        self.neovim
            .exec(
                &format!(
                    "sign place {} line={} name=VadreDebugPointer buffer={}",
                    pointer_sign_id,
                    line_number,
                    code_buffer.get_number().await?,
                ),
                false,
            )
            .await?;

        let code_window = self.windows.get(&VadreWindowType::Code).unwrap();
        code_window.set_cursor((line_number, 0)).await?;

        Ok(())
    }

    pub async fn set_file_type(&self, file_type: &str) -> Result<()> {
        self.neovim
            .command(&format!("set filetype={}", file_type))
            .await?;

        Ok(())
    }

    pub async fn set_call_stack_buffer(&self, content: Vec<String>) -> Result<()> {
        let buffer = self
            .buffers
            .get(&VadreBufferType::CallStack)
            .expect("call stack output buffer exists");
        self.write_to_buffer(buffer, 0, 0, content).await?;

        Ok(())
    }

    pub async fn set_variables_buffer(&self, content: Vec<String>) -> Result<()> {
        let buffer = self
            .buffers
            .get(&VadreBufferType::Variables)
            .expect("variables output buffer exists");
        self.write_to_buffer(buffer, 0, 0, content).await?;

        Ok(())
    }

    pub async fn toggle_breakpoint_in_buffer(
        &self,
        file_path: &str,
        breakpoints: Vec<&Breakpoint>,
    ) -> Result<()> {
        let buffer = self
            .buffers
            .get(&VadreBufferType::Breakpoints)
            .expect("breakpoints output buffer exists");

        let mut line_count = buffer.line_count().await?;
        let mut current_contents = buffer.get_lines(0, line_count, false).await?;

        for breakpoint in breakpoints {
            if breakpoint.file_path != file_path {
                panic!(
                    "Breakpoints with wrong file_path: {:?} != {:?}",
                    breakpoint.file_path, file_path
                );
            }
            let breakpoint_info = format!("{}:{}", file_path, breakpoint.line_number);
            let resolved_to_info = if breakpoint.actual_line_number.is_some()
                && breakpoint.line_number != breakpoint.actual_line_number.unwrap()
            {
                format!(
                    " (resolved to line {})",
                    breakpoint.actual_line_number.unwrap()
                )
            } else {
                "".to_string()
            };
            let line = format!(
                "({}) {}{}",
                if breakpoint.enabled { "*" } else { " " },
                breakpoint_info,
                resolved_to_info,
            );
            if let Some(pos) = current_contents
                .iter()
                .position(|x| x.contains(&breakpoint_info))
            {
                self.write_to_buffer(buffer, pos.try_into()?, pos.try_into()?, vec![line])
                    .await?;
                current_contents = buffer.get_lines(0, line_count, false).await?;
            } else {
                self.write_to_buffer(buffer, line_count, line_count, vec![line])
                    .await?;
                line_count = buffer.line_count().await?;
            }
        }

        Ok(())
    }

    pub async fn get_output_window_type(&self) -> Result<VadreBufferType> {
        let output_window = self
            .windows
            .get(&VadreWindowType::Output)
            .expect("can get output window");
        let output_buffer_name = &output_window.get_buf().await?.get_name().await?;
        let mut split = output_buffer_name.rsplit(" - ");
        let output_buffer_type = split
            .next()
            .expect("should be able to retrieve output buffer type");
        let output_buffer_type = VadreBufferType::get_buffer_type_from_str(output_buffer_type);

        Ok(output_buffer_type)
    }

    pub async fn change_output_window(&self, type_: &str) -> Result<()> {
        let type_ = VadreOutputBufferSelector::get_type(type_);

        let output_window = self
            .windows
            .get(&VadreWindowType::Output)
            .expect("can get output window");
        let new_buffer_type = VadreBufferType::get_output_buffer(
            &output_window.get_buf().await?.get_name().await?,
            type_,
        );
        let new_buffer = self
            .buffers
            .get(&new_buffer_type)
            .expect("can retrieve next buffer");
        output_window.set_buf(new_buffer).await?;
        output_window
            .set_option("wrap", new_buffer_type.buffer_should_wrap().into())
            .await?;

        Ok(())
    }

    async fn write_to_buffer(
        &self,
        buffer: &Buffer<Compat<Stdout>>,
        start_line: i64,
        mut end_line: i64,
        msgs: Vec<String>,
    ) -> Result<()> {
        let line_count = buffer.line_count().await?;
        if start_line == 0 && end_line == 0 {
            end_line = line_count;
        }
        buffer.set_option("modifiable", true.into()).await?;
        if line_count == 1 && buffer.get_lines(0, 1, true).await?.get(0).unwrap() == "" {
            buffer.set_lines(0, 1, false, msgs).await?;
        } else {
            buffer.set_lines(start_line, end_line, false, msgs).await?;
        }
        buffer.set_option("modifiable", false.into()).await?;

        Ok(())
    }

    async fn check_if_new_tab_needed(&self) -> Result<()> {
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

        Ok(())
    }

    async fn set_vadre_buffer_options(
        &self,
        buffer: &Buffer<Compat<Stdout>>,
        buffer_type: &VadreBufferType,
    ) -> Result<()> {
        let buffer_name = self.get_buffer_name(&buffer_type, None);
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
        buffer: &Buffer<Compat<Stdout>>,
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
                "<localleader>l",
                &format!(":VadreOutputWindow {} Logs<CR>", self.instance_id),
            ),
            (
                "<localleader>s",
                &format!(":VadreOutputWindow {} CallStack<CR>", self.instance_id),
            ),
            (
                "<localleader>v",
                &format!(":VadreOutputWindow {} Variables<CR>", self.instance_id),
            ),
            (
                "<localleader>b",
                &format!(":VadreOutputWindow {} Breakpoints<CR>", self.instance_id),
            ),
            (
                ">",
                &format!(
                    ":VadreOutputWindow {} {}<CR>",
                    self.instance_id,
                    VadreOutputBufferSelector::Next
                ),
            ),
            (
                "<",
                &format!(
                    ":VadreOutputWindow {} {}<CR>",
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

    pub async fn get_var(&self, var_name: &str) -> Option<String> {
        self.neovim
            .get_var(var_name)
            .await
            .ok()
            .map(|x| x.to_string())
    }
}

pub async fn setup_signs(neovim: &Neovim<Compat<Stdout>>) -> Result<()> {
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
            .command(&format!(
                "highlight VadreBreakpointHighlight \
                     guifg=#ff0000 guibg={} ctermfg=red ctermbg={}",
                guibg, ctermbg
            ))
            .await?;
        neovim
            .command(&format!(
                "highlight VadreDebugPointerHighlight \
                     guifg=#00ff00 guibg={} ctermfg=green ctermbg={}",
                guibg, ctermbg
            ))
            .await?;
    } else {
        neovim
            .command("highlight VadreBreakpointHighlight guifg=#ff0000 ctermfg=red")
            .await?;
        neovim
            .command("highlight VadreDebugPointerHighlight guifg=#00ff00 ctermfg=green")
            .await?;
    }

    neovim
        .command("sign define VadreBreakpoint text=() texthl=VadreBreakpointHighlight")
        .await?;
    neovim
        .command("sign define VadreDebugPointer text=-> texthl=VadreDebugPointerHighlight")
        .await?;

    Ok(())
}

pub async fn add_breakpoint_sign(neovim: &Neovim<Compat<Stdout>>, line_number: i64) -> Result<()> {
    tracing::trace!("HERE1");
    let buffer_number_output = neovim.exec(&format!("echo bufnr()"), true).await?;
    tracing::trace!("HERE2");
    let buffer_number = buffer_number_output.trim();
    tracing::trace!("HERE3 {}", buffer_number);
    let pointer_sign_id = VADRE_NEXT_SIGN_ID.fetch_add(1, Ordering::SeqCst);

    tracing::trace!("HERE4");
    neovim
        .exec(
            &format!(
                "sign place {} line={} name=VadreBreakpoint buffer={}",
                pointer_sign_id, line_number, buffer_number,
            ),
            false,
        )
        .await?;
    tracing::trace!("HERE5");

    Ok(())
}

pub async fn remove_breakpoint_sign(
    neovim: &Neovim<Compat<Stdout>>,
    file_name: String,
    line_number: i64,
) -> Result<()> {
    let signs_in_file = neovim
        .exec(&format!("sign place file={}", file_name), true)
        .await?;
    let signs_in_file_on_line = signs_in_file
        .split("\n")
        .into_iter()
        .filter(|line| {
            line.contains("name=VadreBreakpoint") && line.contains(&format!("line={}", line_number))
        })
        .collect::<Vec<&str>>();

    assert_eq!(signs_in_file_on_line.len(), 1);

    let signs_in_file_on_line = signs_in_file_on_line.get(0).unwrap();
    let mut breakpoint_id = 0;
    for snippet in signs_in_file_on_line.split(" ") {
        if snippet.len() >= 3 && &snippet[0..3] == "id=" {
            breakpoint_id = snippet[3..].parse::<u64>().expect("id is a u64");
        }
    }

    tracing::trace!("breakpoint_id: {:?}", breakpoint_id);

    neovim
        .command(&format!(
            "sign unplace {} file={}",
            breakpoint_id, file_name
        ))
        .await?;

    Ok(())
}
