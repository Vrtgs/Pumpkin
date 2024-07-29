use std::{
    collections::VecDeque,
    io::{self, Write},
    rc::Rc,
};

use crate::{
    protocol::{
        client::login::CLoginDisconnect,
        server::{
            handshake::SHandShake,
            login::{SEncryptionResponse, SLoginAcknowledged, SLoginPluginResponse, SLoginStart},
            status::{SPingRequest, SStatusRequest},
        },
        ClientPacket, RawPacket,
    },
    server::Server,
};

use crate::protocol::ConnectionState;
use anyhow::Context;
use mio::{event::Event, net::TcpStream, Registry};
use packet_decoder::PacketDecoder;
use packet_encoder::PacketEncoder;
use rsa::Pkcs1v15Encrypt;
use std::io::Read;

mod client_packet;
pub mod player;

mod packet_decoder;
mod packet_encoder;

use client_packet::ClientPacketProcessor;

pub const MAX_PACKET_SIZE: i32 = 2097152;

pub struct Client {
    pub name: Option<String>,
    pub uuid: Option<uuid::Uuid>,
    pub server: Rc<Server>,
    pub connection_state: ConnectionState,
    pub encrytion: bool,
    pub closed: bool,
    pub connection: TcpStream,
    enc: PacketEncoder,
    dec: PacketDecoder,
    pub client_packets_queue: VecDeque<RawPacket>,
}

impl Client {
    pub fn new(server: Rc<Server>, connection: TcpStream) -> Self {
        Self {
            name: None,
            uuid: None,
            server,
            connection_state: ConnectionState::HandShake,
            connection,
            enc: PacketEncoder::default(),
            dec: PacketDecoder::default(),
            encrytion: true,
            closed: false,
            client_packets_queue: VecDeque::new(),
        }
    }

    pub fn add_packet(&mut self, packet: RawPacket) {
        self.client_packets_queue.push_back(packet);
    }

    pub fn enable_encryption(&mut self, shared_secret: Vec<u8>) -> anyhow::Result<()> {
        self.encrytion = true;
        let shared_secret = self
            .server
            .private_key
            .decrypt(Pkcs1v15Encrypt, &shared_secret)
            .context("failed to decrypt shared secret")?;
        let crypt_key: [u8; 16] = shared_secret
            .as_slice()
            .try_into()
            .context("shared secret has the wrong length")?;
        self.dec.enable_encryption(&crypt_key);
        self.enc.enable_encryption(&crypt_key);
        Ok(())
    }

    pub fn send_packet<P: ClientPacket>(&mut self, packet: P) {
        dbg!("sending packet");
        self.enc.append_packet(packet).unwrap();
        self.connection.write_all(&self.enc.take()).unwrap();
    }

    pub fn procress_packets(&mut self) {
        let mut i = 0;
        while i < self.client_packets_queue.len() {
            let mut packet = self.client_packets_queue.remove(i).unwrap();
            self.handle_packet(&mut packet);
            i += 1;
        }
    }

    pub fn handle_packet(&mut self, packet: &mut RawPacket) {
        dbg!("Handling packet");
        let bytebuf = &mut packet.bytebuf;
        match self.connection_state {
            crate::protocol::ConnectionState::HandShake => match packet.id {
                SHandShake::PACKET_ID => self.handle_handshake(SHandShake::read(bytebuf)),
                _ => log::error!(
                    "Failed to handle packet id {} while in Handshake state",
                    packet.id
                ),
            },
            crate::protocol::ConnectionState::Status => match packet.id {
                SStatusRequest::PACKET_ID => {
                    self.handle_status_request(SStatusRequest::read(bytebuf))
                }
                SPingRequest::PACKET_ID => self.handle_ping_request(SPingRequest::read(bytebuf)),
                _ => log::error!(
                    "Failed to handle packet id {} while in Status state",
                    packet.id
                ),
            },
            crate::protocol::ConnectionState::Login => match packet.id {
                SLoginStart::PACKET_ID => self.handle_login_start(SLoginStart::read(bytebuf)),
                SEncryptionResponse::PACKET_ID => {
                    self.handle_encryption_response(SEncryptionResponse::read(bytebuf))
                }
                SLoginPluginResponse::PACKET_ID => {
                    self.handle_plugin_response(SLoginPluginResponse::read(bytebuf))
                }
                SLoginAcknowledged::PACKET_ID => {
                    self.handle_login_acknowledged(SLoginAcknowledged::read(bytebuf))
                }
                _ => log::error!(
                    "Failed to handle packet id {} while in Login state",
                    packet.id
                ),
            },
            _ => log::error!("Invalid Connection state {:?}", self.connection_state),
        }
    }

    /// Returns `true` if the connection is done.
    pub fn poll(&mut self, _registry: &Registry, event: &Event) -> anyhow::Result<()> {
        if event.is_readable() {
            let mut received_data = vec![0; 4096];
            let mut bytes_read = 0;
            // We can (maybe) read from the connection.
            loop {
                match self.connection.read(&mut received_data[bytes_read..]) {
                    Ok(0) => {
                        // Reading 0 bytes means the other side has closed the
                        // connection or is done writing, then so are we.
                        self.closed = true;
                        break;
                    }
                    Ok(n) => {
                        bytes_read += n;
                        if bytes_read == received_data.len() {
                            received_data.resize(received_data.len() + 1024, 0);
                        }
                    }
                    // Would block "errors" are the OS's way of saying that the
                    // connection is not actually ready to perform this I/O operation.
                    Err(ref err) if would_block(err) => break,
                    Err(ref err) if interrupted(err) => continue,
                    // Other errors we'll consider fatal.
                    Err(err) => return anyhow::bail!(err),
                }
            }

            if bytes_read != 0 {
                self.dec.reserve(4096);
                self.dec.queue_slice(&received_data[..bytes_read]);
                if let Some(packet) = self.dec.decode()? {
                    self.add_packet(packet);
                    self.procress_packets();
                }
                self.dec.clear();
            }
        }
        Ok(())
    }

    pub fn kick(&mut self, reason: String) {
        // Todo
        if self.connection_state == ConnectionState::Login {
            self.send_packet(CLoginDisconnect::new(reason));
        }
        self.close()
    }

    // Kick before when needed
    pub fn close(&mut self) {
        self.closed = true;
    }
}

fn would_block(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::WouldBlock
}

pub fn interrupted(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::Interrupted
}