//! Симулятор телефона: тот же клиент из linet-core, что и в Android-приложении.
//! Нужен, чтобы проверять демон без телефона.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use linet_core::client::{Client, ClientEvent};
use linet_core::files::{TransferState, Transfers};
use linet_core::pairing::Invite;
use linet_core::{DeviceKind, Hello};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
struct Args {
    /// Каталог состояния «телефона»
    #[arg(long, default_value = ".sim")]
    dir: PathBuf,
    #[arg(long, default_value = "linet-sim")]
    name: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Сопряжение по URI из `linetctl pair --uri`
    Pair { uri: String },
    /// Держать соединение и печатать события
    Run,
    /// Подключиться, пингануть ноутбук и выйти
    Ping,
    /// Подключиться, отправить файл и выйти
    Send { file: PathBuf },
    Unpair,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    let hello = Hello {
        name: args.name,
        kind: DeviceKind::Phone,
        proto: linet_core::PROTO_VERSION,
        caps: vec!["ping".into()],
    };
    let transfers = Transfers::new(
        args.dir.join("inbox"),
        Arc::new(|u| match u.state {
            TransferState::Progress => {}
            state => println!("передача {} ({}): {state:?}", u.name, if u.incoming { "приём" } else { "отправка" }),
        }),
    );
    let client = Arc::new(Client::new(&args.dir, hello, transfers)?);

    match args.cmd {
        Cmd::Pair { uri } => {
            let peer = client.pair(&Invite::parse(&uri)?).await?;
            println!("Сопряжено с {} ({})", peer.name, peer.id());
        }
        Cmd::Unpair => client.unpair()?,
        Cmd::Run | Cmd::Ping | Cmd::Send { .. } => {
            let once = !matches!(args.cmd, Cmd::Run);
            let (tx, mut rx) = mpsc::channel(16);
            let runner = client.clone();
            tokio::spawn(async move { runner.run(tx).await });
            while let Some(ev) = rx.recv().await {
                match ev {
                    ClientEvent::Connected(peer) => {
                        println!("подключено к {}", peer.name);
                        if let Cmd::Send { file } = &args.cmd {
                            let f = tokio::fs::File::open(file).await?;
                            let size = f.metadata().await?.len();
                            let name = file.file_name().unwrap().to_string_lossy();
                            client.send_file(f, &name, size, "application/octet-stream").await?;
                            return Ok(());
                        }
                        if once {
                            let rtt = client.session().unwrap().ping().await?;
                            println!("ping: {:.1} мс", rtt.as_secs_f64() * 1000.0);
                            return Ok(());
                        }
                    }
                    ClientEvent::Disconnected(reason) => {
                        println!("отключено: {reason}");
                        if once {
                            anyhow::bail!("не удалось подключиться");
                        }
                    }
                    ClientEvent::Unpaired => println!("ноутбук удалил сопряжение"),
                    ClientEvent::Message(env) => println!("сообщение: {:?}", env.body),
                }
            }
        }
    }
    Ok(())
}
