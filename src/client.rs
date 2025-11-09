use std::io::{stdout, IsTerminal, Write};
use std::ops::Deref;
use std::os::unix::net::UnixStream;
use tokio_unix_ipc::channel_from_std;

use itertools::Itertools;

use crate::daemon::{socket_path, Request, Response, ResponseType};
use crate::error::Anyhow;
use crate::register::{RegisterAddress, RegisterSummary};
use spdlog::prelude::*;

#[derive(clap::ValueEnum, Copy, Clone, Default)]
pub enum OutputFormat {
    #[default]
    Plain,
    JSON,
    Rofi,
}

pub type ResponseHandler = dyn Fn(ResponseType) -> Result<(), Anyhow>;

pub async fn send_request<H: Deref<Target = ResponseHandler>>(
    req: Request,
    handler: H,
) -> Result<(), Anyhow> {
    let sp = socket_path();
    let (tx, rx) = UnixStream::connect(&sp)
        .and_then(channel_from_std::<Request, Response>)
        .map_err(|e| {
            error!(
                "Could not connect to daemon at {}: {e}",
                sp.to_string_lossy()
            );
            e
        })?;
    debug!("Sending request: {req:?}");
    tx.send(req).await.map_err(|e| {
        error!("Could not transmit request: {e}");
        e
    })?;
    let rsp = rx
        .recv()
        .await
        .map_err(|e| {
            error!("Could not receive from daemon: {e}");
            e
        })?
        .map_err(|e| {
            error!("Error from daemon: {e}");
            Anyhow::msg(e)
        })?;
    handler(rsp).map_err(Anyhow::msg)
}

pub fn handle_yank(rsp: ResponseType) -> Result<(), Anyhow> {
    if let ResponseType::Yank(addr, n) = rsp {
        info!("Yanked {n} bytes into {addr}");
        Ok(())
    } else {
        Err(Anyhow::msg(format!("Expected Yank response, got {rsp:?}")))
    }
}

pub fn handle_paste(rsp: ResponseType) -> Result<(), Anyhow> {
    if let ResponseType::Paste(_, _, ref data) = rsp {
        let mut stdout = stdout();
        stdout
            .write_all(data)
            .map_err(|e| Anyhow::msg(format!("I/O error: {e}")))?;
        if stdout.is_terminal() {
            stdout
                .write("\n".as_bytes())
                .map_err(|e| Anyhow::msg(format!("I/O error: {e}")))?;
        }
        Ok(())
    } else {
        Err(Anyhow::msg(format!("Expected Paste response, got {rsp:?}")))
    }
}

fn list_plain<'a, R: IntoIterator<Item = (&'a RegisterAddress, &'a RegisterSummary)>>(
    regs: R,
) -> () {
    println!(
        "{}",
        regs.into_iter()
            .map(|(a, s)| format!("{a}: {s}"))
            .join("\n")
    );
}

fn list_rofi<'a, R: IntoIterator<Item = (&'a RegisterAddress, &'a RegisterSummary)>>(
    regs: R,
) -> () {
    println!(
        "{}",
        regs.into_iter()
            .map(|(a, s)| format!("{a:#}\0display\x1f{a}: {s}"))
            .join("\n")
    );
}

pub fn handle_list(rsp: ResponseType, format: OutputFormat) -> Result<(), Anyhow> {
    if let ResponseType::List(ref regs) = rsp {
        use OutputFormat::*;
        match format {
            Plain => list_plain(regs),
            JSON => todo!("JSON output"),
            Rofi => list_rofi(regs),
        }
        Ok(())
    } else {
        Err(Anyhow::msg(format!("Expected List response, got {rsp:?}")))
    }
}

pub fn handle_show(rsp: ResponseType) -> Result<(), Anyhow> {
    if let ResponseType::Show(_, ref reg) = rsp {
        println!(
            "{}",
            reg.into_iter()
                .map(|(m, d)| if m.ty() == "text" {
                    format!(
                        "{m}: {}",
                        std::str::from_utf8(d)
                            .unwrap()
                            .trim_end_matches(char::is_whitespace)
                    )
                } else {
                    format!("{m}: {}", humanize_bytes::humanize_bytes_binary!(d.len()))
                })
                .join("\n")
        );
        Ok(())
    } else {
        Err(Anyhow::msg(format!("Expected Show response, got {rsp:?}")))
    }
}
