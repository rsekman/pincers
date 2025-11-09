use std::io::{stdin, stdout, IsTerminal, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use clap::{Args, Parser as ArgParser, Subcommand};
use spdlog::prelude::*;
use tokio::{signal::ctrl_c, task::JoinSet};
use tokio_unix_ipc::channel_from_std;
use tokio_util::sync::CancellationToken;

use itertools::Itertools;

use pincers::clipboard::Clipboard;
use pincers::daemon::{
    socket_path, Daemon, RegisterCommand, Request, RequestType, Response, ResponseType,
};
use pincers::error::{Anyhow, Error};
use pincers::pincer::SeatPincerMap;
use pincers::register::{MimeType, Register, RegisterAddress, RegisterSummary, ADDRESS_HELP};
use pincers::seat::SeatSpecification;

#[derive(ArgParser)]
#[command(name = "pincers")]
#[command(version = "0.1")]
struct CliOptions {
    #[arg(long = "log-level", help = "Log level", default_value = "Info")]
    log_level: spdlog::Level,
    #[command(subcommand)]
    command: CliCommands,
}

#[derive(clap::ValueEnum, Copy, Clone, Default)]
enum OutputFormat {
    #[default]
    Plain,
    JSON,
    Rofi,
}

#[derive(Subcommand)]
enum CliCommands {
    /// Launch a pincer daemon
    Daemon {},

    /// Manipulate the daemon's register pointer
    Register(RegisterArgs),

    /// Yank from stdin into a register
    Yank {
        #[arg( help = ADDRESS_HELP)]
        address: Option<RegisterAddress>,
        #[arg(
            long = "mime-type",
            short = 't',
            help = "The MIME type of the content",
            default_value = "text/plain"
        )]
        mime: MimeType,
        #[arg(long = "base64", short = 'b', help = "Decode input as base64")]
        base64: bool,
    },
    /// Paste from a register to stdout
    Paste {
        #[arg(
            long = "mime-type",
            short = 't',
            help = "A MIME type to accept. Pass multiple times, in order of preference, to accept multiple types. `type/*` means all subtypes of `type`, `*/*` means accept any type.",
            default_values = ["text/plain; charset=utf-8", "text/plain", "text/*", "*/*"]
        )]
        mime: Vec<MimeType>,
        #[arg(help = ADDRESS_HELP)]
        address: Option<RegisterAddress>,
        #[arg(
            long = "print-binary",
            short = 'p',
            help = "Print binary data to stdout, even if it is a terminal"
        )]
        output_binary: bool,
        #[arg(long = "base64", short = 'b', help = "Print binary data as base64")]
        base64: bool,
    },

    ///Summarize contents of a register
    Show {
        #[arg(help = ADDRESS_HELP)]
        address: Option<RegisterAddress>,
    },

    /// List contents of all registers
    List {
        #[arg(
            long = "format",
            short = 'f',
            help = "Output format",
            default_value = "plain"
        )]
        format: OutputFormat,
    },
    // TODO: plain and json outputs
}

#[derive(Args)]
struct RegisterArgs {
    #[command(subcommand)]
    command: RegisterCommand,
}

async fn daemon() -> Result<(), Anyhow> {
    info!("Launching daemon");
    let pincers = SeatPincerMap::new();
    let pincers = Arc::new(Mutex::new(pincers));
    let token = CancellationToken::new();

    // Initializing in this order protects against messing up the clipboard in case the Daemon
    // fails to initialize.
    let mut d = Daemon::new(pincers.clone(), None).await?;
    let mut cb = Clipboard::new(pincers.clone())?;
    let tx = cb.get_tx();
    d.set_clipboard_tx(tx);

    // Three tasks in the JoinSet:
    // - the Clipboard interfacing with Wayland
    // - the Daemon handling IPC
    // - the signal handler waiting for Ctrl-C
    let mut tasks = JoinSet::new();
    let d_token = token.clone();
    let cb_token = token.clone();
    tasks.spawn(async move { d.listen(d_token).await });
    tasks.spawn(async move { cb.listen(cb_token).await });
    tasks.spawn(async move {
        match ctrl_c().await {
            Err(e) => warn!("Could not catch Ctrl-C: {e}"),
            Ok(_) => {
                info!("Received SIGINT, exiting")
            }
        };
        token.cancel();
        Ok(())
    });
    tasks.join_all().await;

    Ok(())
}

type ResponseHandler = dyn Fn(ResponseType) -> Result<(), Anyhow>;

use std::ops::Deref;
async fn send_request<H: Deref<Target = ResponseHandler>>(
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

fn handle_yank(rsp: ResponseType) -> Result<(), Anyhow> {
    if let ResponseType::Yank(addr, n) = rsp {
        info!("Yanked {n} bytes into {addr}");
        Ok(())
    } else {
        Err(Anyhow::msg(format!("Expected Yank response, got {rsp:?}")))
    }
}

fn handle_paste(rsp: ResponseType) -> Result<(), Anyhow> {
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

fn handle_list(rsp: ResponseType, format: OutputFormat) -> Result<(), Anyhow> {
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

fn handle_show(rsp: ResponseType) -> Result<(), Anyhow> {
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

fn make_yank_request(addr: Option<RegisterAddress>, mime: MimeType) -> Result<RequestType, Anyhow> {
    let mut stdin = stdin().lock();
    let mut buffer = Vec::new();
    stdin.read_to_end(&mut buffer)?;
    let mut r = Register::new();
    r.insert(mime, buffer);
    Ok(RequestType::Yank(addr, r))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Anyhow> {
    let args = CliOptions::parse();
    use CliCommands::*;
    spdlog::default_logger().set_level_filter(spdlog::LevelFilter::MoreSevereEqual(args.log_level));

    if let Daemon {} = args.command {
        daemon().await
    } else {
        let seat = SeatSpecification::Unspecified;
        let (request, handler): (RequestType, Box<ResponseHandler>) = match args.command {
            Paste {
                mime,
                address,
                output_binary: _,
                base64: _,
            } => (
                RequestType::Paste(address, mime),
                Box::new(|r| handle_paste(r)),
            ),
            Show { address } => (RequestType::Show(address), Box::new(handle_show)),
            List { format } => (
                RequestType::List(),
                Box::new(move |r| handle_list(r, format)),
            ),
            Register(RegisterArgs { command }) => (
                RequestType::Register(command),
                Box::new(|_| todo!("register")),
            ),
            Yank {
                address,
                mime,
                base64: _,
            } => (make_yank_request(address, mime)?, Box::new(handle_yank)),
            Daemon {} => unreachable!(),
        };
        send_request(Request { seat, request }, handler).await
    }
}
