use std::{cmp, collections::HashMap, fmt::Display};

use anyhow::Result;
use nvim_rs::{compat::tokio::Compat, Buffer, Neovim, Value, Window};
use tokio::io::Stdout;

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
            VadreLogLevel::WARN => 2,
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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum VadreBufferType {
    Code,
    Logs,
    Terminal,
}

impl VadreBufferType {
    fn buffer_name_prefix(&self) -> &str {
        match self {
            VadreBufferType::Code => "Vadre Code",
            VadreBufferType::Logs => "Vadre Output",
            VadreBufferType::Terminal => "Vadre Program",
        }
    }

    fn buffer_post_display(&self) -> &str {
        match self {
            VadreBufferType::Code => "",
            VadreBufferType::Logs => "Logs",
            VadreBufferType::Terminal => "Terminal",
        }
    }

    fn type_name(&self) -> &str {
        match self {
            VadreBufferType::Code => "Code",
            VadreBufferType::Logs => "Logs",
            VadreBufferType::Terminal => "Terminal",
        }
    }
}

#[derive(Clone)]
pub struct NeovimVadreWindow {
    neovim: Neovim<Compat<Stdout>>,
    instance_id: usize,
    windows: HashMap<VadreWindowType, Window<Compat<Stdout>>>,
    buffers: HashMap<VadreBufferType, Buffer<Compat<Stdout>>>,
}

impl NeovimVadreWindow {
    pub fn new(neovim: Neovim<Compat<Stdout>>, instance_id: usize) -> Self {
        Self {
            neovim,
            instance_id,
            buffers: HashMap::new(),
            windows: HashMap::new(),
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
        self.set_vadre_buffer(&buffer, VadreBufferType::Code)
            .await?;
        self.set_keys_for_code_buffer(&buffer).await?;

        self.windows.insert(VadreWindowType::Code, window);
        self.buffers.insert(VadreBufferType::Code, buffer);

        // Window 2 is Output stuff, logs at the moment
        let window = windows.next().unwrap();
        let log_buffer = window.get_buf().await?;
        self.set_vadre_buffer(&log_buffer, VadreBufferType::Logs)
            .await?;
        self.set_keys_for_output_buffer(&log_buffer).await?;

        self.windows.insert(VadreWindowType::Output, window);
        self.buffers.insert(VadreBufferType::Logs, log_buffer);

        // Window 3 is Output stuff, logs at the moment
        let window = windows.next().unwrap();
        let terminal_buffer = window.get_buf().await?;
        self.set_vadre_buffer(&terminal_buffer, VadreBufferType::Terminal)
            .await?;
        window.set_height(output_window_height).await?;

        self.windows.insert(VadreWindowType::Program, window);
        self.buffers
            .insert(VadreBufferType::Terminal, terminal_buffer);

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

        let datetime = chrono::offset::Local::now();

        let msgs = msg
            .split("\n")
            .map(move |msg| format!("{} [{}] {}", datetime.format("%a %H:%M:%S%.6f"), level, msg))
            .collect();

        // Annoying little hack for first log line
        if buffer.get_lines(0, 1, true).await?.get(0).unwrap() == "" {
            self.write_to_window(&buffer, 0, 1, msgs).await?;
        } else {
            let mut cursor_at_end = true;
            let current_cursor = window.get_cursor().await?;
            let line_count = buffer.line_count().await?;

            if current_cursor.0 < line_count {
                cursor_at_end = false;
            }
            self.write_to_window(&buffer, -1, -1, msgs).await?;

            if cursor_at_end {
                window.set_cursor((line_count + 1, 0)).await?;
            }
        }

        Ok(())
    }

    pub async fn spawn_terminal_command(&self, command: String) -> Result<()> {
        let original_window = self.neovim.get_current_win().await?;

        let terminal_window = self.windows.get(&VadreWindowType::Program).unwrap();
        self.neovim.set_current_win(&terminal_window).await?;

        self.neovim
            .command(&format!("terminal! {}", command))
            .await?;

        self.neovim.set_current_win(&original_window).await?;

        Ok(())
    }

    async fn write_to_window(
        &self,
        buffer: &Buffer<Compat<Stdout>>,
        start_line: i64,
        end_line: i64,
        msgs: Vec<String>,
    ) -> Result<()> {
        buffer.set_option("modifiable", true.into()).await?;
        buffer.set_lines(start_line, end_line, false, msgs).await?;
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

    async fn set_vadre_buffer(
        &self,
        buffer: &Buffer<Compat<Stdout>>,
        buffer_type: VadreBufferType,
    ) -> Result<()> {
        let post_display = buffer_type.buffer_post_display();
        let buffer_name = if post_display != "" {
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
        };
        let file_type = buffer_type.type_name();
        buffer.set_name(&buffer_name).await?;
        buffer.set_option("swapfile", false.into()).await?;
        buffer.set_option("buftype", "nofile".into()).await?;
        buffer.set_option("filetype", file_type.into()).await?;
        buffer.set_option("buflisted", false.into()).await?;
        buffer.set_option("modifiable", false.into()).await?;

        Ok(())
    }

    async fn set_keys_for_code_buffer(&self, buffer: &Buffer<Compat<Stdout>>) -> Result<()> {
        buffer.set_keymap("n", "r", ":VadreRun<CR>", vec![]).await?; // nnoremap <silent> <buffer> r :PadreRun<cr>

        Ok(())
    }

    async fn set_keys_for_output_buffer(&self, buffer: &Buffer<Compat<Stdout>>) -> Result<()> {
        buffer
            .set_keymap("n", ">", ":VadreNextOutputWindow<CR>", vec![])
            .await?; // nnoremap <silent> <buffer> r :PadreRun<cr>

        Ok(())
    }
}
