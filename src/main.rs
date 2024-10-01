use clap::{arg, command, Parser};

use modbus_proxy_rs::Server;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Options {
    #[arg(
        short = 'c',
        long = "config",
        help = "configuration file (accepts YAML, TOML, JSON)"
    )]
    config_file: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::init();
    let args = Options::parse();
    if let Err(error) = Server::launch(&args.config_file).await {
        eprintln!("Configuration error: {}", error)
    }
}
