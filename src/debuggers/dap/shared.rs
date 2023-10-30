use std::{sync::Arc, time::Duration};

use super::protocol::{ProtocolMessageType, RequestArguments, ResponseResult};
use crate::{
    neovim::NeovimVadreWindow,
    util::{log_ret_err, ret_err},
    VadreLogLevel,
};

use anyhow::{bail, Result};
use tokio::{
    sync::{mpsc, oneshot, Mutex},
    time::timeout,
};

/// Actually send the request, should be the only function that does this, even the function
/// with the `&self` parameter still uses this in turn.
#[tracing::instrument(skip(request, debugger_sender_tx))]
pub(crate) async fn do_send_request(
    request: RequestArguments,
    debugger_sender_tx: mpsc::Sender<(
        ProtocolMessageType,
        Option<oneshot::Sender<ResponseResult>>,
    )>,
    sender: Option<oneshot::Sender<ResponseResult>>,
) -> Result<()> {
    let message = ProtocolMessageType::Request(request);

    debugger_sender_tx.send((message, sender)).await?;

    Ok(())
}

/// Actually send the request and await the response. Used in turn by the equivalent function
/// with the `&self` parameter.
#[tracing::instrument(skip(request, debugger_sender_tx))]
pub(crate) async fn do_send_request_and_await_response(
    request: RequestArguments,
    debugger_sender_tx: mpsc::Sender<(
        ProtocolMessageType,
        Option<oneshot::Sender<ResponseResult>>,
    )>,
) -> Result<ResponseResult> {
    let (sender, receiver) = oneshot::channel();

    do_send_request(request, debugger_sender_tx, Some(sender)).await?;

    // TODO: configurable timeout
    let response = match timeout(Duration::new(30, 0), receiver).await {
        Ok(resp) => resp?,
        Err(e) => bail!("Timed out waiting for a response: {}", e),
    };
    tracing::trace!("Response: {:?}", response);

    Ok(response)
}
