use std::{env::consts::EXE_SUFFIX, fmt::Debug, path::Path, process::Stdio, sync::Arc};

use crate::{
    neovim::{NeovimVadreWindow, VadreLogLevel},
    util::{self, get_debuggers_dir, log_err, ret_err},
    VadreResult,
};

use anyhow::{bail, Result};
use nvim_rs::{compat::tokio::Compat, Neovim, Value};
use reqwest::Url;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, Stdout},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream,
    },
    process::{Child, Command},
    sync::Mutex,
};

#[derive(Clone, Debug)]
pub enum Debugger {
    CodeLLDB(CodeLLDBDebugger),
}

impl Debugger {
    pub async fn setup(&mut self) -> VadreResult {
        match self {
            Debugger::CodeLLDB(debugger) => debugger.setup().await,
        }
    }

    pub fn neovim_vadre_window(&self) -> &NeovimVadreWindow {
        match self {
            Debugger::CodeLLDB(debugger) => &debugger.neovim_vadre_window,
        }
    }
}

#[derive(Clone)]
pub struct CodeLLDBDebugger {
    id: usize,
    command: String,
    command_args: Vec<String>,
    pub neovim_vadre_window: NeovimVadreWindow,
    process: Option<Arc<Child>>,
    read_conn: Arc<Mutex<Option<OwnedReadHalf>>>,
    write_conn: Arc<Mutex<Option<OwnedWriteHalf>>>,
}

impl CodeLLDBDebugger {
    pub fn new(
        id: usize,
        command: String,
        command_args: Vec<String>,
        neovim: Neovim<Compat<Stdout>>,
    ) -> Self {
        let neovim_vadre_window = NeovimVadreWindow::new(neovim, id);

        CodeLLDBDebugger {
            id,
            command,
            command_args,
            neovim_vadre_window,
            process: None,
            read_conn: Arc::new(Mutex::new(None)),
            write_conn: Arc::new(Mutex::new(None)),
        }
    }

    #[tracing::instrument(skip(self))]
    pub async fn setup(&mut self) -> VadreResult {
        log_err!(
            self.neovim_vadre_window.create_ui().await,
            self.neovim_vadre_window,
            "Error setting up Vadre UI"
        );

        let port = util::get_unused_localhost_port();

        log_err!(
            self.launch(port).await,
            self.neovim_vadre_window,
            "Error launching process"
        );
        log_err!(
            self.tcp_connect(port).await,
            self.neovim_vadre_window,
            "Error creating TCP connection to process"
        );
        log_err!(
            self.init_process().await,
            self.neovim_vadre_window,
            "Error initialising process"
        );
        ret_err!(
            self.neovim_vadre_window
                .log_msg(VadreLogLevel::INFO, "CodeLLDB launched and setup")
                .await
        );

        Ok(Value::from("CodeLLDB launched and setup"))
    }

    #[tracing::instrument(skip(self, port))]
    async fn launch(&mut self, port: u16) -> Result<()> {
        let msg = format!(
            "Launching process {:?} with lldb and args: {:?}",
            self.command, self.command_args,
        );
        self.neovim_vadre_window
            .log_msg(VadreLogLevel::DEBUG, &msg)
            .await?;

        self.download_plugin().await?;

        let mut path = get_debuggers_dir()?;
        path.push("codelldb");
        path.push("extension");
        path.push("adapter");
        path.push(format!("codelldb{}", EXE_SUFFIX));

        if !path.exists() {
            bail!("The binary for codelldb.exe doesn't exist, though it should by this point");
        }

        tracing::trace!("Spawning processs {:?}", path);
        self.process = Some(Arc::new(
            Command::new(path)
                .args(["--port", &port.to_string()])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("Failed to spawn debugger"),
        ));
        self.neovim_vadre_window
            .log_msg(VadreLogLevel::DEBUG, "Process spawned".into())
            .await?;

        Ok(())
    }

    #[tracing::instrument(skip(self, port))]
    async fn tcp_connect(&mut self, port: u16) -> Result<()> {
        tracing::trace!("Connecting to port {}", port);

        let tcp_conn = TcpStream::connect(format!("127.0.0.1:{}", port)).await?;
        let (read_conn, write_conn) = tcp_conn.into_split();
        *self.read_conn.lock().await = Some(read_conn);
        *self.write_conn.lock().await = Some(write_conn);

        self.neovim_vadre_window
            .log_msg(
                VadreLogLevel::DEBUG,
                "Process connection established".into(),
            )
            .await?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn init_process(&self) -> Result<()> {
        self.send_request(
            r#"{
                "command": "initialize",
                "arguments": {
                    "adapterID": "CodeLLDB",
                    "clientID": "nvim_vadre",
                    "clientName": "nvim_vadre",
                    "linesStartAt1": true,
                    "columnsStartAt1": true,
                    "locale": "en_GB",
                    "pathFormat": "path",
                    "supportsVariableType": true,
                    "supportsVariablePaging": false,
                    "supportsRunInTerminalRequest": true,
                    "supportsMemoryReferences": true
                },
                "seq": 1,
                "type": "request"
            }"#,
        )
        .await?;

        Ok(())
    }

    async fn send_request(&self, msg: &str) -> Result<String> {
        let mut write_conn_lock = self.write_conn.lock().await;
        match write_conn_lock.as_mut() {
            Some(write_conn) => {
                tracing::trace!("Request: {}", msg);
                write_conn
                    .write_all(format!("Content-Length: {}\r\n\r\n{}", msg.len(), msg).as_bytes())
                    .await?;
            }
            None => {
                bail!("Can't find write_conn for CodeLLDB, have you called setup yet?");
            }
        };

        let mut read_conn_lock = self.read_conn.lock().await;
        match read_conn_lock.as_mut() {
            Some(read_conn) => {
                let mut buf = [0u8; 4096];
                let n = read_conn.read(&mut buf).await.unwrap();
                let string = String::from_utf8(buf[0..n].into())?;
                tracing::trace!("Response: {:?}", string);
                Ok(string)
            }
            None => {
                bail!("Can't find read_conn for CodeLLDB, have you called setup yet?")
            }
        }
    }

    #[tracing::instrument(skip(self))]
    async fn download_plugin(&self) -> Result<()> {
        let mut path = get_debuggers_dir()?;
        path.push("codelldb");

        if !path.exists() {
            // TODO: Popup informing

            let url = Url::parse(&format!(
                "https://github.com/vadimcn/vscode-lldb/releases/download/{}\
                         /codelldb-x86_64-{}.vsix",
                "v1.7.0", "windows"
            ))?;
            download_extract_zip(path.as_path(), url).await?;
        }

        Ok(())
    }
}

// We just make this synchronous because although it slows things down, it makes it much
// easier to do. If anyone wants to make this async and cool be my guest but it seems not
// easy.
async fn download_extract_zip(full_path: &Path, url: Url) -> Result<()> {
    tracing::trace!("Downloading {} and unzipping to {:?}", url, full_path);
    let zip_contents = reqwest::get(url).await?.bytes().await?;

    let reader = std::io::Cursor::new(zip_contents);
    let mut zip = zip::ZipArchive::new(reader)?;

    zip.extract(full_path)?;

    Ok(())
}

impl Debug for CodeLLDBDebugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeLLDBDebugger")
            .field("id", &self.id)
            .field("command", &self.command)
            .field("command_args", &self.command_args)
            .finish()
    }
}
