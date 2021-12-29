use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

struct Config {
    bind_address: String,
    modbus_address: String,
}

type Frame = Vec<u8>;
type ReplySender = oneshot::Sender<Frame>;

#[derive(Debug)]
enum Message {
    Connection,
    Disconnection,
    Packet((Frame, ReplySender)),
}

type ChannelRx = mpsc::Receiver<Message>;
type ChannelTx = mpsc::Sender<Message>;

type Error = Box<dyn std::error::Error + Send + Sync + 'static>;
type Result<T> = std::result::Result<T, Error>;

type TcpReader = BufReader<tokio::net::tcp::OwnedReadHalf>;
type TcpWriter = BufWriter<tokio::net::tcp::OwnedWriteHalf>;

struct Modbus {
    address: String,
    stream: Option<(TcpReader, TcpWriter)>,
}

impl Modbus {
    fn new(address: &str) -> Modbus {
        Modbus {
            address: address.to_string(),
            stream: None,
        }
    }

    async fn connect(&mut self) -> Result<()> {
        match create_connection(&self.address).await {
            Ok(connection) => {
                self.stream = Some(connection);
                Ok(())
            }
            Err(error) => {
                self.stream = None;
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
}

fn frame_size(frame: &[u8]) -> Result<usize> {
    Ok(u16::from_be_bytes(frame[4..6].try_into()?) as usize)
}

fn split_connection(stream: TcpStream) -> (TcpReader, TcpWriter) {
    let (reader, writer) = stream.into_split();
    (BufReader::new(reader), BufWriter::new(writer))
}

async fn create_connection(address: &str) -> Result<(TcpReader, TcpWriter)> {
    Ok(split_connection(TcpStream::connect(address).await?))
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

async fn client_task(client: TcpStream, channel: ChannelTx) -> Result<()> {
    channel.send(Message::Connection).await?;
    let (mut reader, mut writer) = split_connection(client);
    while let Ok(buf) = read_frame(&mut reader).await {
        let (tx, rx) = oneshot::channel();
        channel.send(Message::Packet((buf, tx))).await?;
        writer.write_all(&rx.await?).await?;
        writer.flush().await?;
    }
    channel.send(Message::Disconnection).await?;
    Ok(())
}

async fn modbus_write_read_raw(modbus: &mut Modbus, frame: &Frame) -> Result<Frame> {
    let (reader, writer) = modbus.stream.as_mut().ok_or("no modbus connection")?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    read_frame(reader).await
}

async fn modbus_write_read_frame(modbus: &mut Modbus, frame: &Frame) -> Result<Frame> {
    if modbus.is_connected() {
        let result = modbus_write_read_raw(modbus, &frame).await;
        match result {
            Ok(reply) => Ok(reply),
            Err(error) => {
                eprintln!("modbus error ({:?}). Retrying...", error);
                modbus.connect().await?;
                modbus_write_read_raw(modbus, &frame).await
            }
        }
    } else {
        modbus.connect().await?;
        modbus_write_read_raw(modbus, &frame).await
    }
}

async fn modbus_packet(modbus: &mut Modbus, packet: (Frame, ReplySender)) -> Result<()> {
    let (frame, channel) = packet;
    println!("modbus request: {:?}", frame);
    let reply = modbus_write_read_frame(modbus, &frame).await?;
    println!("modbus reply: {:?}", reply);
    channel.send(reply);
    Ok(())
}

async fn modbus_task(address: &str, channel: &mut ChannelRx) {
    let mut nb_clients = 0;
    let mut modbus = Modbus::new(address);

    while let Some(message) = channel.recv().await {
        match message {
            Message::Connection => {
                nb_clients += 1;
            }
            Message::Disconnection => {
                nb_clients -= 1;
                if nb_clients == 0 {
                    println!("disconnecting from modbus at {} (no clients)", address);
                    modbus.disconnect();
                }
            }
            Message::Packet(packet) => {
                if let Err(_) = modbus_packet(&mut modbus, packet).await {
                    modbus.disconnect();
                }
            }
        }
    }
}

async fn bridge_task(config: Config) {
    let listener = TcpListener::bind(config.bind_address).await.unwrap();
    let (tx, mut rx) = mpsc::channel::<Message>(32);

    tokio::spawn(async move {
        modbus_task(&config.modbus_address, &mut rx).await;
    });
    loop {
        let (client, _) = listener.accept().await.unwrap();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(err) = client_task(client, tx).await {
                eprintln!("Client error: {:?}", err);
            }
        });
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let config = Config {
        bind_address: "127.0.0.1:8080".to_string(),
        modbus_address: "127.0.0.1:5030".to_string(),
    };
    bridge_task(config).await
}
