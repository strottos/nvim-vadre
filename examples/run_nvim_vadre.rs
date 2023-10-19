use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io::Cursor,
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

#[tokio::main]
async fn main() -> Result<()> {
    let run = escargot::CargoBuild::new()
        .bin("nvim-vadre")
        .current_release()
        .current_target()
        .manifest_path("Cargo.toml")
        .target_dir("./target/testing")
        .run()
        .unwrap();
    println!("artifact={}", run.path().display());

    let mut child = Command::new(run.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    rpc_request_and_response(
        &mut stdin,
        &mut stdout,
        0,
        "nvim_exec",
        vec!["highlight SignColumn".into(), true.into()],
        Value::from("SignColumn     xxx guifg=#938aa9 guibg=#2a2a37"),
    )
    .await?;

    rpc_request_and_response(
        &mut stdin,
        &mut stdout,
        1,
        "nvim_command",
        vec!["highlight VadreBreakpointHighlight guifg=#ff0000 ctermfg=red".into()],
        Value::Nil,
    )
    .await?;

    rpc_request_and_response(
        &mut stdin,
        &mut stdout,
        2,
        "nvim_command",
        vec!["highlight VadreDebugPointerHighlight guifg=#00ff00 ctermfg=green".into()],
        Value::Nil,
    )
    .await?;

    rpc_request_and_response(
        &mut stdin,
        &mut stdout,
        3,
        "nvim_command",
        vec!["sign define VadreBreakpoint text=() texthl=VadreBreakpointHighlight".into()],
        Value::Nil,
    )
    .await?;

    rpc_request_and_response(
        &mut stdin,
        &mut stdout,
        4,
        "nvim_command",
        vec!["sign define VadreDebugPointer text=-> texthl=VadreDebugPointerHighlight".into()],
        Value::Nil,
    )
    .await?;

    let params: Vec<Value> = vec![];
    send_message(
        1,
        Value::from("breakpoint"),
        Value::from(params),
        &mut stdin,
    )
    .await?;

    rpc_request_and_response(
        &mut stdin,
        &mut stdout,
        5,
        "nvim_get_current_win",
        vec![],
        Value::Ext(1, vec![205, 3, 232]),
    )
    .await?;

    rpc_request_and_response(
        &mut stdin,
        &mut stdout,
        6,
        "nvim_win_get_cursor",
        vec![Value::Ext(1, vec![205, 3, 232])],
        Value::from(vec![Value::from(50), Value::from(0)]),
    )
    .await?;

    rpc_request_and_response(
        &mut stdin,
        &mut stdout,
        7,
        "nvim_get_current_buf",
        vec![],
        Value::Ext(1, vec![1]),
    )
    .await?;

    rpc_request_and_response(
        &mut stdin,
        &mut stdout,
        8,
        "nvim_buf_get_name",
        vec![Value::Ext(1, vec![1])],
        Value::from("/home/strotter/code/strottos/nvim-vadre/src/main.rs"),
    )
    .await?;

    rpc_request_and_response(
        &mut stdin,
        &mut stdout,
        9,
        "nvim_exec",
        vec![Value::from("echo bufnr()"), Value::from(true)],
        Value::from("1"),
    )
    .await?;

    rpc_request_and_response(
        &mut stdin,
        &mut stdout,
        10,
        "nvim_exec",
        vec![
            Value::from("sign place 1157831 line=50 name=VadreBreakpoint buffer=1"),
            Value::from(false),
        ],
        Value::Nil,
    )
    .await?;

    Ok(())
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

async fn rpc_request_and_response(
    stdin: &mut ChildStdin,
    stdout: &mut ChildStdout,
    expected_msg_id: u64,
    expected_method: &str,
    expected_params: Vec<Value>,
    response: Value,
) -> Result<()> {
    match read_message(stdout).await? {
        RpcMessage::RpcRequest {
            msgid,
            method,
            params,
        } => {
            assert_eq!(msgid, expected_msg_id);
            assert_eq!(method, expected_method);
            assert_eq!(params, expected_params);

            send_response(msgid, response, stdin).await?;
        }
        _ => unreachable!(),
    }

    Ok(())
}

async fn read_message(stdout: &mut ChildStdout) -> Result<RpcMessage> {
    let mut buffer = vec![0; 1024];
    eprintln!("Reading");
    let bytes_read = stdout.read(&mut buffer).await?;
    eprintln!("Read: {} {:?}", bytes_read, &buffer[0..bytes_read]);
    let mut cursor = Cursor::new(&buffer[0..bytes_read]);
    let arr: Vec<Value> = match read_value(&mut cursor) {
        Ok(x) => x.try_into().map_err(InvalidMessage::NotAnArray)?,
        Err(e) => {
            eprintln!("Error: {:?}", e);
            let bytes_read = stdout.read(&mut buffer).await?;
            panic!("Read: {} {:?}", 3 + bytes_read, &buffer[0..bytes_read]);
        }
    };

    let mut arr = arr.into_iter();
    eprintln!("Received Msg: {:?}", arr);
    assert_eq!(TryInto::<u64>::try_into(arr.next().unwrap()).unwrap(), 0);
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

    Ok(msg)
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
