use std::{
    env, io,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use reqwest::Url;
use tokio::sync::{Mutex, MutexGuard};

use crate::dap::Debugger;

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
pub fn get_unused_localhost_port() -> Result<u16> {
    let listener = TcpListener::bind(format!("127.0.0.1:0"))?;
    Ok(listener.local_addr()?.port())
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

pub(crate) fn merge_json(a: &mut serde_json::Value, b: serde_json::Value) {
    if let serde_json::Value::Object(a) = a {
        if let serde_json::Value::Object(b) = b {
            for (k, v) in b {
                if v.is_null() {
                    a.remove(&k);
                } else {
                    merge_json(a.entry(k).or_insert(serde_json::Value::Null), v);
                }
            }

            return;
        }
    }

    *a = b;
}

// We just make this synchronous because although it slows things down, it makes it much
// easier to do. If anyone wants to make this async and cool be my guest but it seems not
// easy.
#[tracing::instrument(skip(url))]
pub(crate) async fn download_extract_zip(full_path: &Path, url: Url) -> Result<()> {
    tracing::trace!("Downloading {} and unzipping to {:?}", url, full_path);
    let zip_contents = reqwest::get(url).await?.bytes().await?;

    let reader = io::Cursor::new(zip_contents);
    let mut zip = zip::ZipArchive::new(reader)?;

    zip.extract(full_path)?;

    Ok(())
}
