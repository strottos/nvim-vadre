// Heavily based on the dap protocol from CodeLLDB:
// https://github.com/vadimcn/vscode-lldb/tree/master/adapter/crates/adapter-protocol
#![allow(non_camel_case_types)]

pub use super::schema::{
    Breakpoint, BreakpointEventBody, CancelArguments, Capabilities, CapabilitiesEventBody,
    ContinueArguments, ContinueResponseBody, ContinuedEventBody, DisconnectArguments,
    ExitedEventBody, InitializeRequest, InitializeRequestArguments, InitializeResponse,
    InvalidatedEventBody, LaunchRequestArguments, ModuleEventBody, NextArguments, OutputEventBody,
    PauseArguments, ProcessEventBody, RunInTerminalRequestArguments, RunInTerminalResponseBody,
    ScopesArguments, ScopesResponseBody, SetBreakpointsArguments, SetBreakpointsResponseBody,
    SetExceptionBreakpointsArguments, SetFunctionBreakpointsArguments, Source, SourceArguments,
    SourceBreakpoint, SourceResponseBody, StackTraceArguments, StackTraceResponseBody,
    StepInArguments, StoppedEventBody, TerminatedEventBody, ThreadEventBody, ThreadsResponseBody,
    VariablesArguments, VariablesResponseBody,
};

use std::{fmt::Write, io, str};

use bytes::BytesMut;
use serde_derive::{Deserialize, Serialize};
use tokio_util::codec;
use tracing::debug;

#[derive(Serialize, Deserialize, Debug, Copy, Clone)]
#[serde(untagged)]
pub enum Either<T1, T2> {
    First(T1),
    Second(T2),
}

impl<T1, T2> Either<T1, T2> {
    pub fn first(&self) -> &T1 {
        match &self {
            Either::First(x) => &x,
            Either::Second(_) => panic!("Called second when should have been first in Either"),
        }
    }

    pub fn second(&self) -> &T2 {
        match &self {
            Either::First(_) => panic!("Called first when should have been second in Either"),
            Either::Second(x) => &x,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Empty {}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProtocolMessage {
    pub seq: Either<u32, String>,
    #[serde(flatten)]
    pub type_: ProtocolMessageType,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum ProtocolMessageType {
    #[serde(rename = "request")]
    Request(RequestArguments),
    #[serde(rename = "response")]
    Response(Response),
    #[serde(rename = "event")]
    Event(EventBody),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "command", content = "arguments")]
pub enum RequestArguments {
    initialize(InitializeRequestArguments),
    launch(Either<LaunchRequestArguments, serde_json::Value>),
    setBreakpoints(SetBreakpointsArguments),
    setFunctionBreakpoints(SetFunctionBreakpointsArguments),
    setExceptionBreakpoints(SetExceptionBreakpointsArguments),
    configurationDone(Option<Empty>),
    pause(PauseArguments),
    #[serde(rename = "continue")]
    continue_(ContinueArguments),
    next(NextArguments),
    stepIn(StepInArguments),
    threads(Option<Empty>),
    stackTrace(StackTraceArguments),
    scopes(ScopesArguments),
    source(SourceArguments),
    variables(VariablesArguments),
    disconnect(DisconnectArguments),
    // Reverse
    runInTerminal(RunInTerminalRequestArguments),
    #[serde(other)]
    unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Response {
    pub request_seq: Either<u32, String>,
    pub success: bool,
    #[serde(flatten)]
    pub result: ResponseResult,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum ResponseResult {
    Success {
        #[serde(flatten)]
        body: ResponseBody,
    },
    Error {
        command: String,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        show_user: Option<bool>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "command", content = "body")]
pub enum ResponseBody {
    initialize(Capabilities),
    launch(Option<Empty>),
    setBreakpoints(SetBreakpointsResponseBody),
    setFunctionBreakpoints(SetBreakpointsResponseBody),
    setExceptionBreakpoints(Option<Empty>),
    configurationDone(Option<Empty>),
    pause(Option<Empty>),
    #[serde(rename = "continue")]
    continue_(ContinueResponseBody),
    next(Option<Empty>),
    stepIn(Option<Empty>),
    threads(ThreadsResponseBody),
    stackTrace(StackTraceResponseBody),
    scopes(ScopesResponseBody),
    source(SourceResponseBody),
    variables(VariablesResponseBody),
    disconnect(Option<Empty>),
    // Reverse
    runInTerminal(RunInTerminalResponseBody),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "event", content = "body")]
pub enum EventBody {
    initialized(Option<Empty>),
    output(OutputEventBody),
    breakpoint(BreakpointEventBody),
    capabilities(CapabilitiesEventBody),
    continued(ContinuedEventBody),
    exited(ExitedEventBody),
    module(ModuleEventBody),
    terminated(TerminatedEventBody),
    thread(ThreadEventBody),
    invalidated(InvalidatedEventBody),
    stopped(StoppedEventBody),
    process(ProcessEventBody),
}

#[derive(Debug)]
pub struct DAPCodec {
    state: State,
    content_len: usize,
}

#[derive(Debug)]
enum State {
    ReadingHeaders,
    ReadingBody,
}

impl DAPCodec {
    pub fn new() -> DAPCodec {
        DAPCodec {
            state: State::ReadingHeaders,
            content_len: 0,
        }
    }
}

pub type DecoderResult = Result<ProtocolMessage, DecoderError>;

#[derive(Debug)]
pub enum DecoderError {
    SerdeError {
        error: serde_json::error::Error,
        value: serde_json::value::Value,
    },
}

impl codec::Decoder for DAPCodec {
    type Item = DecoderResult;
    type Error = io::Error;

    fn decode(&mut self, buffer: &mut BytesMut) -> Result<Option<DecoderResult>, Self::Error> {
        loop {
            match self.state {
                State::ReadingHeaders => match buffer.windows(2).position(|b| b == &[b'\r', b'\n'])
                {
                    None => return Ok(None),
                    Some(pos) => {
                        let line = buffer.split_to(pos + 2);
                        if line.len() == 2 {
                            self.state = State::ReadingBody;
                        } else if let Ok(line) = str::from_utf8(&line) {
                            if line.len() > 15 && line[..15].eq_ignore_ascii_case("content-length:")
                            {
                                if let Ok(content_len) = line[15..].trim().parse::<usize>() {
                                    self.content_len = content_len;
                                }
                            }
                        }
                    }
                },
                State::ReadingBody => {
                    if buffer.len() < self.content_len {
                        return Ok(None);
                    } else {
                        let message_bytes = buffer.split_to(self.content_len);
                        self.state = State::ReadingHeaders;
                        self.content_len = 0;

                        debug!("<-- {}", str::from_utf8(&message_bytes).unwrap());
                        match serde_json::from_slice(&message_bytes) {
                            Ok(message) => return Ok(Some(Ok(message))),
                            Err(error) => {
                                let value = match serde_json::from_slice(&message_bytes) {
                                    Ok(value) => value,
                                    Err(_) => serde_json::value::Value::Null,
                                };
                                return Ok(Some(Err(DecoderError::SerdeError { error, value })));
                            }
                        }
                    }
                }
            }
        }
    }
}

impl codec::Encoder<ProtocolMessage> for DAPCodec {
    type Error = io::Error;

    fn encode(
        &mut self,
        message: ProtocolMessage,
        buffer: &mut BytesMut,
    ) -> Result<(), Self::Error> {
        tracing::trace!("encoding {:?} {:?}", message, buffer);
        let message_bytes = serde_json::to_vec(&message).unwrap();
        debug!("--> {}", str::from_utf8(&message_bytes).unwrap());

        buffer.reserve(32 + message_bytes.len());
        write!(buffer, "Content-Length: {}\r\n\r\n", message_bytes.len()).unwrap();
        buffer.extend_from_slice(&message_bytes);

        Ok(())
    }
}

////////////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(test)]
    macro_rules! assert_matches(($e:expr, $p:pat) => { let e = $e; assert!(matches!(e, $p), "{:?} !~ {}", e, stringify!($p)) });

    fn parse(s: &[u8]) -> ProtocolMessage {
        serde_json::from_slice::<ProtocolMessage>(s).unwrap()
    }

    #[test]
    fn test_initialize() {
        let request = parse(br#"{"command":"initialize","arguments":{"clientID":"vscode","clientName":"Visual Studio Code","adapterID":"lldb","pathFormat":"path","linesStartAt1":true,"columnsStartAt1":true,"supportsVariableType":true,"supportsVariablePaging":true,"supportsRunInTerminalRequest":true,"locale":"en-us"},"type":"request","seq":1}"#);
        assert_matches!(
            request,
            ProtocolMessage {
                seq: Either::First(1),
                type_: ProtocolMessageType::Request(RequestArguments::initialize(..))
            }
        );

        let response = parse(br#"{"seq":2, "request_seq":1,"command":"initialize","body":{"supportsDelayedStackTraceLoading":true,"supportsEvaluateForHovers":true,"exceptionBreakpointFilters":[{"filter":"rust_panic","default":true,"label":"Rust: on panic"}],"supportsCompletionsRequest":true,"supportsConditionalBreakpoints":true,"supportsStepBack":false,"supportsConfigurationDoneRequest":true,"supportTerminateDebuggee":true,"supportsLogPoints":true,"supportsFunctionBreakpoints":true,"supportsHitConditionalBreakpoints":true,"supportsSetVariable":true},"type":"response","success":true}"#);
        assert_matches!(
            response,
            ProtocolMessage {
                seq: Either::First(2),
                type_: ProtocolMessageType::Response(Response {
                    result: ResponseResult::Success {
                        body: ResponseBody::initialize(..)
                    },
                    ..
                })
            }
        );
    }

    #[test]
    fn test_launch() {
        let request = parse(br#"{"type":"request","seq":2, "command":"launch","arguments":{"type":"lldb","request":"launch","name":"Debug tests in types_lib",
                        "program":"target/debug/types_lib-d6a67ab7ca515c6b",
                        "args":[],
                        "cwd":"/home/debuggee",
                        "initCommands":["platform shell echo 'init'"],
                        "env":{"TEST":"folder"},
                        "sourceMap":{"/checkout/src":"/home/user/nightly-x86_64-unknown-linux-gnu/lib/rustlib/src/rust/src"},
                        "debugServer":41025,
                        "_displaySettings":{"showDisassembly":"always","displayFormat":"auto","dereferencePointers":true,"toggleContainerSummary":false,"containerSummary":true},
                        "__sessionId":"81865613-a1ee-4a66-b449-a94165625fd2"}
                      }"#);
        assert_matches!(
            request,
            ProtocolMessage {
                seq: Either::First(2),
                type_: ProtocolMessageType::Request(RequestArguments::launch(..))
            }
        );

        let response =
            parse(br#"{"seq": 3, "request_seq":2,"command":"launch","body":null,"type":"response","success":true}"#);
        assert_matches!(
            response,
            ProtocolMessage {
                seq: Either::First(3),
                type_: ProtocolMessageType::Response(Response {
                    result: ResponseResult::Success {
                        body: ResponseBody::launch(..),
                    },
                    ..
                })
            }
        );
    }

    #[test]
    fn test_event() {
        let event = parse(br#"{"type":"event","event":"initialized","seq":0}"#);
        assert_matches!(
            event,
            ProtocolMessage {
                seq: Either::First(0),
                type_: ProtocolMessageType::Event(EventBody::initialized(..))
            }
        );

        let event = parse(br#"{"body":{"reason":"started","threadId":7537},"type":"event","event":"thread","seq":0}"#);
        assert_matches!(
            event,
            ProtocolMessage {
                seq: Either::First(0),
                type_: ProtocolMessageType::Event(EventBody::thread(..))
            }
        );
    }

    #[test]
    fn test_scopes() {
        let request = parse(
            br#"{"command":"scopes","arguments":{"frameId":1000},"type":"request","seq":12}"#,
        );
        assert_matches!(
            request,
            ProtocolMessage {
                seq: Either::First(12),
                type_: ProtocolMessageType::Request(RequestArguments::scopes(..)),
            }
        );

        let response = parse(br#"{"seq":34,"request_seq":12,"command":"scopes","body":{"scopes":[{"variablesReference":1001,"name":"Local","expensive":false},{"variablesReference":1002,"name":"Static","expensive":false},{"variablesReference":1003,"name":"Global","expensive":false},{"variablesReference":1004,"name":"Registers","expensive":false}]},"type":"response","success":true}"#);
        assert_matches!(
            response,
            ProtocolMessage {
                seq: Either::First(34),
                type_: ProtocolMessageType::Response(Response {
                    request_seq: Either::First(12),
                    success: true,
                    result: ResponseResult::Success {
                        body: ResponseBody::scopes(..),
                    },
                    ..
                })
            }
        );
    }

    #[test]
    fn test_configuration_done() {
        let request = parse(br#"{"type":"request", "seq":12, "command":"configurationDone"}"#);
        println!("{:?}", request);
        assert_matches!(
            request,
            ProtocolMessage {
                seq: Either::First(12),
                type_: ProtocolMessageType::Request(RequestArguments::configurationDone(None)),
            }
        );

        let request =
            parse(br#"{"type":"request", "seq":12, "command":"configurationDone", "arguments": {"foo": "bar"}}"#);
        println!("{:?}", request);
        assert_matches!(
            request,
            ProtocolMessage {
                seq: Either::First(12),
                type_: ProtocolMessageType::Request(RequestArguments::configurationDone(Some(_)))
            }
        );
    }

    // #[test]
    // fn test_disconnect() {
    //     let request =
    //         parse(br#"{"type":"request", "seq":12, "command":"disconnect", "arguments":{"terminateDebuggee":true} }"#);
    //     assert_matches!(
    //         request,
    //         ProtocolMessage {
    //             seq: 12,
    //             type_: ProtocolMessageType::Request(RequestArguments::disconnect(Some(..)))
    //         }
    //     );

    //     let request = parse(br#"{"type":"request", "seq":12, "command":"disconnect"}"#);
    //     assert_matches!(
    //         request,
    //         ProtocolMessage {
    //             seq: 12,
    //             type_: ProtocolMessageType::Request(RequestArguments::disconnect(None))
    //         }
    //     );
    // }

    #[test]
    fn test_unknown() {
        let request = parse(br#"{"type":"request", "seq":12, "command":"foobar"}"#);
        assert_matches!(
            request,
            ProtocolMessage {
                seq: Either::First(12),
                type_: ProtocolMessageType::Request(RequestArguments::unknown)
            }
        );
    }
}
