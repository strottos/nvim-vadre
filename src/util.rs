use std::{env, net::TcpListener, path::PathBuf};

use anyhow::Result;

// A helper macro to check if a result errored and return early if so. Prevents having to write
// this kind of code all over the place:
// ```
// if let Err(e) = self.tcp_connect(port).await {
//     return Err(format!("Error creating TCP connection to process: {}", e).into());
// };
// ```
macro_rules! ret_err {
    ($e:expr) => {
        if let Err(e) = $e {
            bail!("Generic error: {}", e);
        }
    };
    ($e:expr, $msg:expr) => {
        if let Err(e) = $e {
            bail!("{}: {}", $msg, e);
        }
    };
}
pub(crate) use ret_err;

// A helper macro to check if a result errored and return early if so and logs the error to Vadre
// Log window.
macro_rules! log_err {
    ($e:expr, $logger:expr) => {
        if let Err(e) = $e {
            let msg = format!("Generic error: {}", e);
            tracing::error!("{}", msg);
            $logger
                .lock()
                .await
                .log_msg(VadreLogLevel::ERROR, &msg)
                .await
                .expect("logs should be logged");
        }
    };
    ($e:expr, $logger:expr, $msg:expr) => {
        if let Err(e) = $e {
            let msg = format!("{}: {}", $msg, e);
            tracing::error!("{}", msg);
            $logger
                .lock()
                .await
                .log_msg(VadreLogLevel::ERROR, &msg)
                .await
                .expect("logs should be logged");
        }
    };
}
pub(crate) use log_err;

// A helper macro to check if a result errored and return early if so and logs the error to Vadre
// Log window.
macro_rules! log_ret_err {
    ($e:expr, $logger:expr) => {
        if let Err(e) = $e {
            let msg = format!("Generic error: {}", e);
            ret_err!(
                $logger
                    .lock()
                    .await
                    .log_msg(VadreLogLevel::ERROR, &msg)
                    .await
            );
        }
    };
    ($e:expr, $logger:expr, $msg:expr) => {
        if let Err(e) = $e {
            let msg = format!("{}: {}", $msg, e);
            ret_err!(
                $logger
                    .lock()
                    .await
                    .log_msg(VadreLogLevel::ERROR, &msg)
                    .await
            );
        }
    };
}
pub(crate) use log_ret_err;

fn get_root_dir() -> Result<PathBuf> {
    let mut path = env::current_exe()?;
    path.pop();
    path.pop();
    path.pop();
    Ok(path)
}

pub fn get_debuggers_dir() -> Result<PathBuf> {
    let mut path = get_root_dir()?;
    path.push("debuggers");
    Ok(path)
}

/// Get an unused port on the local system and return it. This port
/// can subsequently be used.
pub fn get_unused_localhost_port() -> u16 {
    let listener = TcpListener::bind(format!("127.0.0.1:0")).unwrap();
    listener.local_addr().unwrap().port()
}

/// Translate the current machines os and arch to that required for downloading debugger adapters
pub fn get_os_and_cpu_architecture<'a>() -> (&'a str, &'a str) {
    #[cfg(target_os = "linux")]
    let os = "linux";
    #[cfg(target_os = "macos")]
    let os = "darwin";
    #[cfg(target_os = "windows")]
    let os = "windows";

    #[cfg(target_arch = "x86_64")]
    let arch = "x86_64";
    #[cfg(target_arch = "aarch64")]
    let arch = "aarch64";

    (os, arch)
}
