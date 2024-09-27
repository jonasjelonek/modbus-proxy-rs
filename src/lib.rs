#[macro_use]
extern crate log;

use std::net::SocketAddr;

use futures::future::join_all;
use serde::Deserialize;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{self, Duration};

// Use Jemalloc only for musl-64 bits platforms
//#[cfg(all(target_env = "musl", target_pointer_width = "64"))]
//#[global_allocator]
//static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

type Frame = Vec<u8>;
type ReplySender = oneshot::Sender<Frame>;

#[derive(Debug)]
enum Message {
    Connection(SocketAddr),
    Disconnection(SocketAddr),
    Packet(Frame, ReplySender, SocketAddr),
}

type ChannelRx = mpsc::Receiver<Message>;
type ChannelTx = mpsc::Sender<Message>;

type Error = Box<dyn std::error::Error + Send + Sync + 'static>;
type Result<T> = std::result::Result<T, Error>;

type TcpReader = BufReader<tokio::net::tcp::OwnedReadHalf>;
type TcpWriter = BufWriter<tokio::net::tcp::OwnedWriteHalf>;

const TCP_READ_TIMEOUT: Duration = Duration::from_secs(5);

fn frame_size(frame: &[u8]) -> Result<usize> {
    Ok(u16::from_be_bytes(frame[4..6].try_into()?) as usize)
}

fn split_connection(stream: TcpStream) -> (TcpReader, TcpWriter) {
    let (reader, writer) = stream.into_split();
    (BufReader::new(reader), BufWriter::new(writer))
}

async fn create_connection(url: &str) -> Result<(TcpReader, TcpWriter)> {
    let stream = TcpStream::connect(url).await?;
    stream.set_nodelay(true)?;
    Ok(split_connection(stream))
}

async fn read_frame(stream: &mut TcpReader) -> Result<Frame> {
    let mut buf = vec![0u8; 6];
    // Read header
    stream.read_exact(&mut buf).await?;
    // calculate payload size
    let total_size = 6 + frame_size(&buf)?;
    buf.resize(total_size, 0);
    stream.read_exact(&mut buf[6..total_size]).await?;
    Ok(buf)
}

#[derive(Debug, Deserialize)]
struct Listen {
    bind: String,
}

#[derive(Debug, Deserialize)]
struct Modbus {
    url: String,
}

struct Device {
    url: String,
    stream: Option<(TcpReader, TcpWriter)>,
}

impl Device {
    pub fn new(url: &str) -> Device {
        Device {
            url: url.to_string(),
            stream: None,
        }
    }

    async fn connect(&mut self) -> Result<()> {
        match create_connection(&self.url).await {
            Ok(connection) => {
                info!("modbus connection to {} sucessfull", self.url);
                self.stream = Some(connection);
                Ok(())
            }
            Err(error) => {
                self.stream = None;
                info!("modbus connection to {} error: {} ", self.url, error);
                Err(error)
            }
        }
    }

    fn disconnect(&mut self) {
        self.stream = None;
    }

    fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    async fn raw_write_read(&mut self, frame: &Frame) -> Result<Frame> {
        let (reader, writer) = self.stream.as_mut().ok_or("no modbus connection")?;
        writer.write_all(&frame).await?;
        trace!("[raw_write_read]: wrote packet to stream");
        writer.flush().await?;
        read_frame(reader).await.inspect(|_| {
            trace!("[raw_write_read]: read response from stream");
        })
    }

    async fn write_read(&mut self, frame: &Frame) -> Result<Frame> {
        if self.is_connected() {
            let result = self.raw_write_read(&frame).await;
            match result {
                Ok(reply) => Ok(reply),
                Err(error) => {
                    warn!("modbus error: {}. Retrying...", error);
                    self.connect().await?;
                    self.raw_write_read(&frame).await
                }
            }
        } else {
            self.connect().await?;
            self.raw_write_read(&frame).await
        }
    }

    async fn handle_packet(&mut self, frame: Frame, channel: ReplySender) -> Result<()> {
        info!("modbus request {}: {} bytes", self.url, frame.len());
        debug!("modbus request {}: {:?}", self.url, &frame[..]);
        let reply = self.write_read(&frame).await?;
        info!("modbus reply {}: {} bytes", self.url, reply.len());
        debug!("modbus reply {}: {:?}", self.url, &reply[..]);
        channel
            .send(reply)
            .or_else(|error| Err(format!("error sending reply to client: {:?}", error).into()))
    }

    async fn run(&mut self, channel: &mut ChannelRx) {
        let mut nb_clients = 0;

        while let Some(message) = channel.recv().await {
            match message {
                Message::Connection(ip) => {
                    nb_clients += 1;
                    if !self.is_connected() {
                        if let Err(_) = self.connect().await {
                            error!("failed to connect to Modbus device");
                        }
                    }

                    info!("new client {} connection (active = {})", ip, nb_clients);
                }
                Message::Disconnection(ip) => {
                    nb_clients -= 1;
                    info!("client {} disconnection (active = {})", ip, nb_clients);
                    if nb_clients == 0 {
                        info!("client disconnecting from modbus at {} (no clients)", self.url);
                        self.disconnect();
                    }
                }
                Message::Packet(frame, channel, ip) => {
                    debug!("[Device::run]: handling packet from {}", ip);
                    if let Err(_) = self.handle_packet(frame, channel).await {
                        self.disconnect();
                    }
                }
            }
        }
    }

    async fn launch(url: &str, channel: &mut ChannelRx) {
        let mut modbus = Self::new(url);
        modbus.run(channel).await;
    }
}

#[derive(Debug, Deserialize)]
struct Bridge {
    listen: Listen,
    modbus: Modbus,
}

impl Bridge {
    pub async fn run(&mut self) {
        let listener = TcpListener::bind(&self.listen.bind).await.unwrap();
        let modbus_url = self.modbus.url.clone();
        let (tx, mut rx) = mpsc::channel::<Message>(32);
        tokio::spawn(async move {
            Device::launch(&modbus_url, &mut rx).await;
        });
        info!(
            "Ready to accept requests on {} to {}",
            &self.listen.bind, &self.modbus.url
        );
        loop {
            let (client, _) = listener.accept().await.unwrap();
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Err(err) = Self::handle_client(client, tx).await {
                    error!("Client error: {:?}", err);
                }
                trace!("End of client task");
            });
        }
    }

    async fn handle_client(client: TcpStream, channel: ChannelTx) -> Result<()> {
        let ip = client.peer_addr().unwrap();
        client.set_nodelay(true)?;
        channel.send(Message::Connection(ip)).await?;
        trace!("[handle_client]: handling client {}", ip);

        let result = Self::client_loop(client, &channel).await;

        trace!("[handle_client]: finished client {}", ip);
        channel.send(Message::Disconnection(ip)).await?;

        result
    }

    async fn client_loop(client: TcpStream, channel: &ChannelTx) -> Result<()> {
        let ip = client.peer_addr().unwrap();
        let (mut reader, mut writer) = split_connection(client);
        while let Ok(buf) = read_frame(&mut reader).await {
            trace!("[client_loop]: packet from {}", ip);
            let (tx, rx) = oneshot::channel();
            channel.send(Message::Packet(buf, tx, ip)).await?;
            trace!("[client_loop]: sent message to device handler");
            writer.write_all(&rx.await?).await?;
            trace!("[client_loop]: wrote back result");
            writer.flush().await?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
pub struct Server {
    devices: Vec<Bridge>,
}

impl Server {
    pub fn new(config_file: &str) -> std::result::Result<Self, config::ConfigError> {
        let settings = config::Config::builder()
            .add_source(config::File::with_name(config_file))
            .build()?;
        settings.try_deserialize()
    }

    pub async fn run(self) {
        let mut tasks = vec![];
        for mut bridge in self.devices {
            tasks.push(tokio::spawn(async move { bridge.run().await }));
        }
        join_all(tasks).await;
    }

    pub async fn launch(config_file: &str) -> std::result::Result<(), config::ConfigError> {
        Ok(Self::new(config_file)?.run().await)
    }
}
