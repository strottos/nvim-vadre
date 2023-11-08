use std::{
    env,
    error::Error,
    fmt::{self, Display, Formatter},
    io::{Cursor, Seek, SeekFrom},
    path::PathBuf,
    process::Stdio,
};

use anyhow::Result;
use rmpv::{decode::read_value, encode::write_value, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{ChildStdin, ChildStdout, Command},
};

#[derive(Debug)]
pub enum RpcMessage {
    RpcRequest {
        msgid: u64,
        method: String,
        params: Vec<Value>,
    }, // 0
    RpcResponse {
        msgid: u64,
        error: Value,
        result: Value,
    }, // 1
    RpcNotification {
        method: String,
        params: Vec<Value>,
    }, // 2
}

#[derive(Debug, PartialEq, Clone)]
pub enum InvalidMessage {
    /// The value read was not an array
    NotAnArray(Value),
    InvalidParams(Value, String),
}

impl Error for InvalidMessage {}

impl Display for InvalidMessage {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            InvalidMessage::NotAnArray(value) => {
                write!(f, "Expected an array, got {:?}", value)
            }
            InvalidMessage::InvalidParams(value, method) => write!(
                f,
                "Expected an array of params for method {}, got {:?}",
                method, value
            ),
        }
    }
}

#[tokio::test]
async fn test_run_nvim_vadre() -> Result<()> {
    compile_test_program().await?;

    // let (mut child, mut stdin, mut stdout) = get_process().await?;

    // init_process(&mut stdin, &mut stdout).await?;

    // set_breakpoint(&mut stdin, &mut stdout).await?;

    // run_debugger(&mut stdin, &mut stdout).await?;

    // child.kill().await?;

    Ok(())
}

async fn compile_test_program() -> Result<()> {
    eprintln!("Compiling test program");
    escargot::CargoBuild::new()
        .manifest_path("./test_files/test_progs/test_rust/Cargo.toml")
        .target_dir("./target/testing_examples")
        .exec()
        .unwrap();
    eprintln!("Compiled test program");

    Ok(())
}

async fn get_process() -> Result<(tokio::process::Child, ChildStdin, ChildStdout)> {
    let run = escargot::CargoBuild::new()
        .bin("nvim-vadre")
        .current_target()
        .manifest_path("Cargo.toml")
        .target_dir("./target/testing")
        .run()
        .unwrap();
    println!("artifact={}", run.path().display());

    let mut child = Command::new(run.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .env("VADRE_LOG", "trace")
        .env("VADRE_LOG_FILE", "./target/testing/vadre_log.txt")
        .spawn()?;

    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    Ok((child, stdin, stdout))
}

async fn init_process(stdin: &mut ChildStdin, stdout: &mut ChildStdout) -> Result<()> {
    rpc_expect_request_and_respond(
        stdin,
        stdout,
        "nvim_exec",
        Some(vec!["highlight SignColumn".into(), true.into()]),
        Value::from("SignColumn     xxx guifg=#938aa9 guibg=#2a2a37"),
    )
    .await?;

    rpc_expect_request_and_respond(
        stdin,
        stdout,
        "nvim_command",
        Some(vec![
            "highlight VadreBreakpointHighlight guifg=#ff0000 ctermfg=red".into(),
        ]),
        Value::Nil,
    )
    .await?;

    rpc_expect_request_and_respond(
        stdin,
        stdout,
        "nvim_command",
        Some(vec![
            "highlight VadreDebugPointerHighlight guifg=#00ff00 ctermfg=green".into(),
        ]),
        Value::Nil,
    )
    .await?;

    rpc_expect_request_and_respond(
        stdin,
        stdout,
        "nvim_command",
        Some(vec![
            "sign define VadreBreakpoint text=() texthl=VadreBreakpointHighlight".into(),
        ]),
        Value::Nil,
    )
    .await?;

    rpc_expect_request_and_respond(
        stdin,
        stdout,
        "nvim_command",
        Some(vec![
            "sign define VadreDebugPointer text=-> texthl=VadreDebugPointerHighlight".into(),
        ]),
        Value::Nil,
    )
    .await?;

    Ok(())
}

fn get_source_file() -> Result<PathBuf> {
    let mut path = env::current_exe()?;
    path.pop();
    path.pop();
    path.pop();
    path.pop();
    path.push("test_files/test_progs/test_rust/src/main.rs");
    Ok(path)
}

pub async fn set_breakpoint(stdin: &mut ChildStdin, stdout: &mut ChildStdout) -> Result<()> {
    let params: Vec<Value> = vec![];
    send_message(1, Value::from("breakpoint"), Value::from(params), stdin).await?;

    let test_main_source = get_source_file()?;

    for (expected_method, expected_params, response) in [
        (
            "nvim_get_current_win",
            Some(vec![]),
            Value::Ext(1, vec![205, 3, 232]),
        ),
        (
            "nvim_win_get_cursor",
            Some(vec![Value::Ext(1, vec![205, 3, 232])]),
            Value::from(vec![Value::from(25), Value::from(0)]),
        ),
        ("nvim_get_current_buf", Some(vec![]), Value::Ext(0, vec![1])),
        (
            "nvim_buf_get_name",
            Some(vec![Value::Ext(0, vec![1])]),
            Value::from(test_main_source.as_os_str().to_str().unwrap()),
        ),
        (
            "nvim_exec",
            Some(vec![Value::from("echo bufnr()"), Value::from(true)]),
            Value::from("1"),
        ),
        (
            "nvim_exec",
            Some(vec![
                Value::from("sign place 1157831 line=25 name=VadreBreakpoint buffer=1"),
                Value::from(false),
            ]),
            Value::from(""),
        ),
    ] {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }

    rpc_expect_response(stdout, 1, Value::Nil, Value::from("breakpoint set")).await?;

    Ok(())
}

async fn run_debugger(stdin: &mut ChildStdin, stdout: &mut ChildStdout) -> Result<()> {
    // TODO: Not sure why this needs double encoding, but seems to only be accepted if it is??
    let params: Value = Value::from(vec![Value::from(vec![
        Value::from("--"),
        Value::from("./target/testing_examples/debug/test_rust"),
    ])]);
    send_message(2, Value::from("launch"), Value::from(params), stdin).await?;

    rpc_expect_response(stdout, 2, Value::Nil, Value::from("process launched")).await?;

    check_create_ui(stdin, stdout).await?;

    Ok(())
}

async fn check_create_ui(stdin: &mut ChildStdin, stdout: &mut ChildStdout) -> Result<()> {
    // Initialize UI
    for (expected_method, expected_params, response) in vec![
        (
            "nvim_get_var",
            Some(vec![Value::from("eventignore")]),
            Value::from(vec![
                Value::from(1),
                Value::from("Key not found: eventignore"),
            ]),
        ),
        (
            "nvim_set_var",
            Some(vec![Value::from("eventignore"), Value::from("all")]),
            Value::Nil,
        ),
        (
            "nvim_command",
            Some(vec![Value::from("tab new")]),
            Value::Nil,
        ),
    ] {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }

    // Create Code buffer
    for (expected_method, expected_params, response) in vec![
        (
            "nvim_get_current_tabpage",
            Some(vec![]),
            Value::Ext(2, vec![3]),
        ),
        ("nvim_command", Some(vec![Value::from("vnew")]), Value::Nil),
        (
            "nvim_tabpage_list_wins",
            Some(vec![Value::Ext(2, vec![3])]),
            Value::from(vec![
                Value::Ext(1, vec![205, 3, 234]),
                Value::Ext(1, vec![205, 3, 233]),
            ]),
        ),
        (
            "nvim_win_get_buf",
            Some(vec![Value::Ext(1, vec![205, 3, 234])]),
            Value::Ext(0, vec![3]),
        ),
        (
            "nvim_set_current_win",
            Some(vec![Value::Ext(1, vec![205, 3, 234])]),
            Value::Nil,
        ),
        (
            "nvim_buf_set_name",
            Some(vec![Value::Ext(0, vec![3]), Value::from("Vadre Code (1)")]),
            Value::Nil,
        ),
    ] {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }

    for (expected_method, expected_params, response) in
        get_expected_buffer_set_vadre_option_items(3, "Code")?
    {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }
    for (expected_method, expected_params, response) in
        get_expected_buffer_set_vadre_keymap_items(3)?
    {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }

    // Create Terminal buffer
    for (expected_method, expected_params, response) in vec![
        (
            "nvim_win_get_buf",
            Some(vec![Value::Ext(1, vec![205, 3, 233])]),
            Value::Ext(0, vec![2]),
        ),
        (
            "nvim_buf_set_name",
            Some(vec![
                Value::Ext(0, vec![2]),
                Value::from("Vadre Program (1) - Terminal"),
            ]),
            Value::Nil,
        ),
    ] {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }

    for (expected_method, expected_params, response) in
        get_expected_buffer_set_vadre_option_items(2, "Terminal")?
    {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }

    // Create Logs buffer
    for (expected_method, expected_params, response) in vec![
        (
            "nvim_create_buf",
            Some(vec![Value::from(false), Value::from(false)]),
            Value::Ext(0, vec![4]),
        ),
        (
            "nvim_buf_set_name",
            Some(vec![
                Value::Ext(0, vec![4]),
                Value::from("Vadre Output (1) - Logs"),
            ]),
            Value::Nil,
        ),
    ] {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }

    for (expected_method, expected_params, response) in
        get_expected_buffer_set_vadre_option_items(4, "Logs")?
    {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }

    // Create CallStack buffer
    for (expected_method, expected_params, response) in vec![
        (
            "nvim_create_buf",
            Some(vec![Value::from(false), Value::from(false)]),
            Value::Ext(0, vec![5]),
        ),
        (
            "nvim_buf_set_name",
            Some(vec![
                Value::Ext(0, vec![5]),
                Value::from("Vadre Output (1) - CallStack"),
            ]),
            Value::Nil,
        ),
    ] {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }
    for (expected_method, expected_params, response) in
        get_expected_buffer_set_vadre_option_items(5, "CallStack")?
    {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }

    // Create Variables buffer
    for (expected_method, expected_params, response) in vec![
        (
            "nvim_create_buf",
            Some(vec![Value::from(false), Value::from(false)]),
            Value::Ext(0, vec![6]),
        ),
        (
            "nvim_buf_set_name",
            Some(vec![
                Value::Ext(0, vec![6]),
                Value::from("Vadre Output (1) - Variables"),
            ]),
            Value::Nil,
        ),
    ] {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }

    for (expected_method, expected_params, response) in
        get_expected_buffer_set_vadre_option_items(6, "Variables")?
    {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }

    // Create Breakpoints buffer
    for (expected_method, expected_params, response) in vec![
        (
            "nvim_create_buf",
            Some(vec![Value::from(false), Value::from(false)]),
            Value::Ext(0, vec![7]),
        ),
        (
            "nvim_buf_set_name",
            Some(vec![
                Value::Ext(0, vec![7]),
                Value::from("Vadre Output (1) - Breakpoints"),
            ]),
            Value::Nil,
        ),
    ] {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }

    for (expected_method, expected_params, response) in
        get_expected_buffer_set_vadre_option_items(7, "Breakpoints")?
    {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }

    // Log UI initialization complete line
    for (expected_method, expected_params, response) in get_expected_buffer_log_line_items()? {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }

    // Finishing off initializing UI
    for (expected_method, expected_params, response) in vec![
        (
            "nvim_set_var",
            Some(vec![
                Value::from("eventignore"),
                Value::from(vec![
                    Value::from(1u8),
                    Value::from("Key not found: eventignore"),
                ]),
            ]),
            Value::Nil,
        ),
        (
            "nvim_command",
            Some(vec![Value::from("doautocmd User VadreUICreated")]),
            Value::Nil,
        ),
    ] {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }

    // Log setup debugger lines
    for (expected_method, expected_params, response) in get_expected_buffer_log_line_items()? {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }
    for (expected_method, expected_params, response) in get_expected_buffer_log_line_items()? {
        rpc_expect_request_and_respond(stdin, stdout, expected_method, expected_params, response)
            .await?;
    }

    Ok(())
}

fn get_expected_buffer_set_vadre_option_items<'a>(
    buf_nr: u8,
    buffer_type: &'a str,
) -> Result<Vec<(&'a str, Option<Vec<Value>>, Value)>> {
    Ok(vec![
        (
            "nvim_buf_set_option",
            Some(vec![
                Value::Ext(0, vec![buf_nr]),
                Value::from("swapfile"),
                Value::from(false),
            ]),
            Value::Nil,
        ),
        (
            "nvim_buf_set_option",
            Some(vec![
                Value::Ext(0, vec![buf_nr]),
                Value::from("buftype"),
                Value::from("nofile"),
            ]),
            Value::Nil,
        ),
        (
            "nvim_buf_set_option",
            Some(vec![
                Value::Ext(0, vec![buf_nr]),
                Value::from("filetype"),
                Value::from(buffer_type),
            ]),
            Value::Nil,
        ),
        (
            "nvim_buf_set_option",
            Some(vec![
                Value::Ext(0, vec![buf_nr]),
                Value::from("buflisted"),
                Value::from(false),
            ]),
            Value::Nil,
        ),
        (
            "nvim_buf_set_option",
            Some(vec![
                Value::Ext(0, vec![buf_nr]),
                Value::from("modifiable"),
                Value::from(false),
            ]),
            Value::Nil,
        ),
    ])
}

fn get_expected_buffer_set_vadre_keymap_items<'a>(
    buf_nr: u8,
) -> Result<Vec<(&'a str, Option<Vec<Value>>, Value)>> {
    Ok(vec![
        (
            "nvim_buf_set_keymap",
            Some(vec![
                Value::Ext(0, vec![buf_nr]),
                Value::from("n"),
                Value::from("S"),
                Value::from(r#":<C-U>execute v:count1 . " VadreStepIn \"1\""<CR>"#),
                Value::Map(vec![
                    (Value::from("noremap"), Value::from(true)),
                    (Value::from("silent"), Value::from(true)),
                ]),
            ]),
            Value::Nil,
        ),
        (
            "nvim_buf_set_keymap",
            Some(vec![
                Value::Ext(0, vec![buf_nr]),
                Value::from("n"),
                Value::from("s"),
                Value::from(r#":<C-U>execute v:count1 . " VadreStepOver \"1\""<CR>"#),
                Value::Map(vec![
                    (Value::from("noremap"), Value::from(true)),
                    (Value::from("silent"), Value::from(true)),
                ]),
            ]),
            Value::Nil,
        ),
        (
            "nvim_buf_set_keymap",
            Some(vec![
                Value::Ext(0, vec![buf_nr]),
                Value::from("n"),
                Value::from("c"),
                Value::from(r#":VadreContinue 1<CR>"#),
                Value::Map(vec![
                    (Value::from("noremap"), Value::from(true)),
                    (Value::from("silent"), Value::from(true)),
                ]),
            ]),
            Value::Nil,
        ),
        (
            "nvim_buf_set_keymap",
            Some(vec![
                Value::Ext(0, vec![buf_nr]),
                Value::from("n"),
                Value::from("C"),
                Value::from(r#":VadreContinue 1<CR>"#),
                Value::Map(vec![
                    (Value::from("noremap"), Value::from(true)),
                    (Value::from("silent"), Value::from(true)),
                ]),
            ]),
            Value::Nil,
        ),
        (
            "nvim_buf_set_keymap",
            Some(vec![
                Value::Ext(0, vec![buf_nr]),
                Value::from("n"),
                Value::from("<localleader>l"),
                Value::from(r#":VadreOutputWindow 1 Logs<CR>"#),
                Value::Map(vec![
                    (Value::from("noremap"), Value::from(true)),
                    (Value::from("silent"), Value::from(true)),
                ]),
            ]),
            Value::Nil,
        ),
        (
            "nvim_buf_set_keymap",
            Some(vec![
                Value::Ext(0, vec![buf_nr]),
                Value::from("n"),
                Value::from("<localleader>s"),
                Value::from(r#":VadreOutputWindow 1 CallStack<CR>"#),
                Value::Map(vec![
                    (Value::from("noremap"), Value::from(true)),
                    (Value::from("silent"), Value::from(true)),
                ]),
            ]),
            Value::Nil,
        ),
        (
            "nvim_buf_set_keymap",
            Some(vec![
                Value::Ext(0, vec![buf_nr]),
                Value::from("n"),
                Value::from("<localleader>v"),
                Value::from(r#":VadreOutputWindow 1 Variables<CR>"#),
                Value::Map(vec![
                    (Value::from("noremap"), Value::from(true)),
                    (Value::from("silent"), Value::from(true)),
                ]),
            ]),
            Value::Nil,
        ),
        (
            "nvim_buf_set_keymap",
            Some(vec![
                Value::Ext(0, vec![buf_nr]),
                Value::from("n"),
                Value::from("<localleader>b"),
                Value::from(r#":VadreOutputWindow 1 Breakpoints<CR>"#),
                Value::Map(vec![
                    (Value::from("noremap"), Value::from(true)),
                    (Value::from("silent"), Value::from(true)),
                ]),
            ]),
            Value::Nil,
        ),
        // (
        //     "nvim_buf_set_keymap",
        //     Some(vec![
        //         Value::Ext(0, vec![buf_nr]),
        //         Value::from("n"),
        //         Value::from("<localleader>l"),
        //         Value::from(r#":VadreOutputWindow 1 Logs<CR>"#),
        //         Value::Map(vec![
        //             (Value::from("noremap"), Value::from(true)),
        //             (Value::from("silent"), Value::from(true)),
        //         ]),
        //     ]),
        //     Value::Nil,
        // ),
        (
            "nvim_buf_set_keymap",
            Some(vec![
                Value::Ext(0, vec![buf_nr]),
                Value::from("n"),
                Value::from(">"),
                Value::from(r#":VadreOutputWindow 1 Next<CR>"#),
                Value::Map(vec![
                    (Value::from("noremap"), Value::from(true)),
                    (Value::from("silent"), Value::from(true)),
                ]),
            ]),
            Value::Nil,
        ),
        (
            "nvim_buf_set_keymap",
            Some(vec![
                Value::Ext(0, vec![buf_nr]),
                Value::from("n"),
                Value::from("<"),
                Value::from(r#":VadreOutputWindow 1 Previous<CR>"#),
                Value::Map(vec![
                    (Value::from("noremap"), Value::from(true)),
                    (Value::from("silent"), Value::from(true)),
                ]),
            ]),
            Value::Nil,
        ),
    ])
}

fn get_expected_buffer_log_line_items<'a>() -> Result<Vec<(&'a str, Option<Vec<Value>>, Value)>> {
    Ok(vec![
        (
            "nvim_get_var",
            Some(vec![Value::from("vadre_log_level")]),
            Value::from(5u8),
        ),
        (
            "nvim_buf_line_count",
            Some(vec![Value::Ext(0, vec![4])]),
            Value::from(1u8),
        ),
        (
            "nvim_buf_line_count",
            Some(vec![Value::Ext(0, vec![4])]),
            Value::from(1u8),
        ),
        (
            "nvim_buf_set_option",
            Some(vec![
                Value::Ext(0, vec![4]),
                Value::from("modifiable"),
                Value::from(true),
            ]),
            Value::Nil,
        ),
        (
            "nvim_buf_get_lines",
            Some(vec![
                Value::Ext(0, vec![4]),
                Value::from(0u8),
                Value::from(1u8),
                Value::from(true),
            ]),
            Value::from(vec![Value::from("")]),
        ),
        ("nvim_buf_set_lines", None, Value::Nil),
        (
            "nvim_buf_set_option",
            Some(vec![
                Value::Ext(0, vec![4]),
                Value::from("modifiable"),
                Value::from(false),
            ]),
            Value::Nil,
        ),
    ])
}
async fn rpc_expect_request_and_respond(
    stdin: &mut ChildStdin,
    stdout: &mut ChildStdout,
    expected_method: &str,
    expected_params: Option<Vec<Value>>,
    response: Value,
) -> Result<()> {
    match read_message(stdout).await? {
        RpcMessage::RpcRequest {
            msgid,
            method,
            params,
        } => {
            assert_eq!(method, expected_method);
            if let Some(expected_params) = expected_params {
                assert_eq!(params, expected_params);
            }

            send_response(msgid, response, stdin).await?;
        }
        _ => unreachable!(),
    }

    Ok(())
}

async fn rpc_expect_response(
    stdout: &mut ChildStdout,
    expected_msgid: u64,
    expected_error: Value,
    expected_result: Value,
) -> Result<()> {
    match read_message(stdout).await? {
        RpcMessage::RpcResponse {
            msgid,
            error,
            result,
        } => {
            assert_eq!(msgid, expected_msgid);
            assert_eq!(error, expected_error);
            assert_eq!(result, expected_result);
        }
        _ => unreachable!(),
    }

    Ok(())
}

async fn read_message(stdout: &mut ChildStdout) -> Result<RpcMessage> {
    let mut cursor = Cursor::new(vec![]);
    let arr = read_from_stdout(stdout, &mut cursor).await?;

    let mut arr = arr.into_iter();
    match TryInto::<u64>::try_into(arr.next().unwrap()).unwrap() {
        0 => {
            // Request
            let msgid = TryInto::<u64>::try_into(arr.next().unwrap()).unwrap();

            let method = match arr.next() {
                Some(Value::String(s)) if s.is_str() => {
                    s.into_str().expect("Can remove using #230 of rmpv")
                }
                Some(_) => todo!(),
                None => todo!(),
            };
            let params: Vec<Value> = arr
                .next()
                .unwrap()
                .try_into()
                .map_err(|val| InvalidMessage::InvalidParams(val, method.clone()))?;
            let msg = RpcMessage::RpcRequest {
                msgid,
                method,
                params,
            };

            eprintln!("Received Msg: {:?}", msg);

            Ok(msg)
        }
        1 => {
            // Response

            let msgid = TryInto::<u64>::try_into(arr.next().unwrap()).unwrap();

            let error = arr.next().unwrap();
            let result = arr.next().unwrap();
            assert!(arr.next().is_none());
            let msg = RpcMessage::RpcResponse {
                msgid,
                error,
                result,
            };

            eprintln!("Received Msg: {:?}", msg);

            Ok(msg)
        }
        _ => unreachable!(),
    }
}

async fn read_from_stdout(
    stdout: &mut ChildStdout,
    cursor: &mut Cursor<Vec<u8>>,
) -> Result<Vec<Value>> {
    let mut buffer = vec![0; 1024];
    loop {
        let bytes_read = stdout.read(&mut buffer).await?;
        cursor.seek(SeekFrom::End(0))?;
        cursor.write_all(&buffer[..bytes_read]).await?;
        cursor.set_position(0);
        match read_value(cursor) {
            Ok(x) => {
                let ret = x.try_into().map_err(InvalidMessage::NotAnArray)?;
                eprintln!("Found value: {:?}", ret);
                return Ok(ret);
            }
            Err(_) => {}
        }
    }
}

async fn send_message(msgid: u64, msg: Value, params: Value, stdin: &mut ChildStdin) -> Result<()> {
    let msg = Value::from(vec![Value::from(0), Value::from(msgid), msg, params]);
    send(msg, stdin).await
}

async fn send_response(msgid: u64, msg: Value, stdin: &mut ChildStdin) -> Result<()> {
    let msg = Value::from(vec![Value::from(1), Value::from(msgid), Value::Nil, msg]);
    send(msg, stdin).await
}

async fn send(msg: Value, stdin: &mut ChildStdin) -> Result<()> {
    let mut v: Vec<u8> = vec![];
    write_value(&mut v, &msg)?;
    stdin.write(&v).await?;
    eprintln!("Sent Msg: {:?}", msg);
    Ok(())
}
