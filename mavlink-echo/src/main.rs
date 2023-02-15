use std::net::SocketAddr;
use std::str::FromStr;
use std::io::{self, Write};
use std::fmt::Debug;

use structopt::StructOpt;
use anyhow::{anyhow, Result};
use serde::Serialize;

use mavlink_extras::mavconn::MavConn;

enum Addr {
    Serial {
        path: String,
        baud_rate: u32,
    },
    Udp(SocketAddr),
    Tcp(SocketAddr),
}

#[derive(StructOpt)]
#[structopt(name = "mavlink-echo", about = "A tool to echo mavlink messages")]
struct Args {
    addr: Addr,
    #[structopt(long)]
    show_header: bool,
    #[structopt(long, default_value = "16")]
    system_id: u8,
    #[structopt(long, default_value = "1")]
    component_id: u8,
    #[structopt(long)]
    json: bool,
    #[structopt(long)]
    minify: bool,
}

#[paw::main]
#[tokio::main(flavor = "current_thread")]
async fn main(args: Args) -> Result<()> {
    let conn = match args.addr {
        Addr::Tcp(addr) => MavConn::new_tcp(addr, args.system_id, args.component_id).await?,
        Addr::Udp(addr) => MavConn::new_udp(addr, args.system_id, args.component_id).await?,
        Addr::Serial { path, baud_rate } => MavConn::new_serial(&path, baud_rate, args.system_id, args.component_id).await?,
    };
    let mut stdout = std::io::stdout().lock();
    loop {
        if args.show_header {
            let msg = conn.recv_with_header().await?;
            echo_msg_to_writer(&mut stdout, msg, args.json, args.minify)?;
        } else {
            let msg = conn.recv().await?;
            echo_msg_to_writer(&mut stdout, msg, args.json, args.minify)?;
        }
    }
}

fn echo_msg_to_writer<W, M>(out: &mut W, msg: M, json: bool, minify: bool) -> io::Result<()>
where
    M: Debug + Serialize,
    W: Write,
{
    match (json, minify) {
        (false, false) => writeln!(out, "{msg:#?}")?,
        (false, true) => writeln!(out, "{msg:?}")?,
        (true, false) => {
            serde_json::to_writer_pretty(&mut *out, &msg)?;
            writeln!(out)?;
        },
        (true, true) => {
            serde_json::to_writer(&mut *out, &msg)?;
            writeln!(out)?;
        },
    }
    Ok(())
}

impl FromStr for Addr {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        if let Some(addr) = s.strip_prefix("tcp://") {
            Ok(Addr::Tcp(addr.parse()?))
        } else if let Some(addr) = s.strip_prefix("udp://") {
            Ok(Addr::Udp(addr.parse()?))
        } else {
            let (path, baud) = s.split_once(':')
                .ok_or_else(|| anyhow!("expected ':' in serial address"))?;
            Ok(Addr::Serial { path: path.to_string(), baud_rate: baud.parse()? })
        }
    }
}
