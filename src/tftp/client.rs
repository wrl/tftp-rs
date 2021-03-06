//! A Trivial File Transfer (TFTP) protocol client implementation.
//!
//! This module contains the ability to read data from or write data to a remote TFTP server.

use std::convert::From;
use std::io;
use std::path::Path;
use std::net::SocketAddr;
use std::result;
use std::str;
use std::mem;

use packet::{Mode, RequestPacket, DataPacketOctet, AckPacket, ErrorPacket,
    EncodePacket, RawPacket, Opcode};
use decodedpacket::DecodedPacket;

use mio::udp::UdpSocket;
use mio::{Events, Poll, PollOpt, Event, Token, Ready};

static MAX_DATA_SIZE: usize = 512;

quick_error! {
    #[derive(Debug)]
    pub enum Error {
        Io(err: io::Error) {
            from()
            description("io error")
            display("I/O error: {}", err)
            cause(err)
        }
        Server(err: ErrorPacket<'static>) {
            from()
            description("server error")
            display("Server error: {}", err)
            cause(err)
        }
    }
}

type Result<T> = result::Result<T, Error>;

trait PacketSender {
    fn send_read_request(&self, path: &str, mode: Mode) -> Result<()>;
    fn send_ack(&mut self, block_id: u16) -> Result<Option<()>>;
}

trait PacketReceiver {
    fn receive_data(&mut self) -> Result<Option<DecodedPacket<DataPacketOctet<'static>>>>;
}

struct InternalClient {
    socket: UdpSocket,
    remote_addr: SocketAddr,
    buffer_data: Option<Vec<u8>>,
    buffer_ack: Vec<u8>,
}

impl InternalClient {
    fn new(socket: UdpSocket, remote_addr: SocketAddr) -> InternalClient {
        InternalClient {
            socket: socket,
            remote_addr: remote_addr,
            buffer_data: Some(vec![0; MAX_DATA_SIZE + 4]),
            buffer_ack: vec![0; MAX_DATA_SIZE + 4],
        }
    }

    fn put_buffer_data(&mut self, buf: Vec<u8>) {
        self.buffer_data = Some(buf);
    }
}

impl PacketSender for InternalClient {
    fn send_read_request(&self, path: &str, mode: Mode) -> Result<()> {
        let read_request = RequestPacket::read_request(path, mode);
        let encoded = read_request.encode();
        let buf = encoded.packet_buf();
        self.socket.send_to(&buf, &self.remote_addr).map(|_| ()).map_err(From::from)
    }

    fn send_ack(&mut self, block_id: u16) -> Result<Option<()>> {
        let buf = mem::replace(&mut self.buffer_ack, Vec::new());
        let ack = AckPacket::new(block_id);
        let encoded = ack.encode_using(buf);
        let result = {
            let buf = encoded.packet_buf();
            self.socket.send_to(&buf, &self.remote_addr).map(|opt| opt.map(|_| ())).map_err(From::from)
        };
        self.buffer_ack = encoded.get_buffer();
        result
    }
}

impl PacketReceiver for InternalClient {
    fn receive_data(&mut self) -> Result<Option<DecodedPacket<DataPacketOctet<'static>>>> {
        let mut buf = mem::replace(&mut self.buffer_data, None).unwrap_or(vec![0; MAX_DATA_SIZE + 4]);
        let result = try!(self.socket.recv_from(&mut buf));
        let p = result.map(|(n, from)| {
            self.remote_addr = from;
            RawPacket::new(buf, n)
        }).map(|packet| {
            match packet.opcode() {
                Some(Opcode::DATA) => {
                    DecodedPacket::decode(packet).unwrap()
                },
                _ => unimplemented!(),
            }
        });
        Ok(p)
    }
}

enum ClientStates<'a> {
    SendReadRequest(&'a Path, Mode),
    ReceivingData(u16),
    SendAck(DecodedPacket<DataPacketOctet<'static>>),
    Done,
}

impl<'a> ClientStates<'a> {
    fn is_done(&self) -> bool {
        match self {
            &ClientStates::Done => true,
            _ => false,
        }
    }
}

struct Client<'a> {
    poll: Poll,
    client: InternalClient,
    writer: &'a mut io::Write,
}

const CLIENT: Token = Token(0);

impl<'a> Client<'a> {
    fn new(poll: Poll, client: InternalClient, writer: &'a mut io::Write) -> Client<'a> {
        Client {
            poll: poll,
            client: client,
            writer: writer,
        }
    }
}

impl<'a> Client<'a> {
    fn get(&mut self, path: &Path, mode: Mode) -> Result<()> {
        let mut events = Events::with_capacity(1024);
        let mut current_state = ClientStates::SendReadRequest(path, mode);

        try!(self.poll.register(&self.client.socket, CLIENT, Ready::writable(), PollOpt::level()));

        loop {
            try!(self.poll.poll(&mut events, None));
            for event in events.iter() {
                match event.token() {
                    CLIENT => {
                        current_state = try!(self.handle_event(current_state, event));
                        if current_state.is_done() {
                            return Ok(())
                        }
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    fn handle_event<'b>(&mut self, current_state: ClientStates, event: Event) -> Result<ClientStates<'b>> {
        match current_state {
            ClientStates::SendReadRequest(path, mode) => {
                try!(self.client.send_read_request(path.to_str().unwrap(), mode));
                println!("Starting transfer ...");
                try!(self.poll.reregister(&self.client.socket, CLIENT, Ready::readable(), PollOpt::level()));
                Ok(ClientStates::ReceivingData(1))
            }
            ClientStates::ReceivingData(current_id) => {
                let data_packet = match try!(self.client.receive_data()) {
                    Some(data_packet) => data_packet,
                    None => return Ok(ClientStates::ReceivingData(current_id)),
                };
                if current_id == data_packet.block_id() {
                    self.handle_event(ClientStates::SendAck(data_packet), event)
                } else {
                    println!("Unexpected packet id: got={}, expected={}",
                             data_packet.block_id(), current_id);
                    Ok(ClientStates::ReceivingData(current_id))
                }
            }
            ClientStates::SendAck(data_packet) => {
                if try!(self.client.send_ack(data_packet.block_id())).is_none() {
                    try!(self.poll.reregister(&self.client.socket, CLIENT, Ready::writable(), PollOpt::level()));
                    println!("Could not send ack for packet id={}", data_packet.block_id());
                    Ok(ClientStates::SendAck(data_packet))
                } else {
                    try!(self.writer.write_all(data_packet.data()));
                    let data_len = data_packet.data().len();
                    let next_id = data_packet.block_id() + 1;
                    self.client.put_buffer_data(data_packet.into_inner());
                    if data_len < MAX_DATA_SIZE {
                        println!("Transfer complete");
                        Ok(ClientStates::Done)
                    } else {
                        if event.kind().is_writable() {
                            try!(self.poll.reregister(&self.client.socket, CLIENT, Ready::readable(), PollOpt::level()));
                        }
                        Ok(ClientStates::ReceivingData(next_id))
                    }
                }
            }
            _ => unreachable!()
        }
    }
}

pub fn get(path: &Path, mode: Mode, writer: &mut io::Write) {
    println!("starting ...");
    let remote_addr = "127.0.0.1:69".parse().unwrap();
    let any = str::FromStr::from_str("0.0.0.0:0").unwrap();
    let socket = UdpSocket::bind(&any).unwrap();
    let poll =  Poll::new().unwrap();
    let mut client = Client::new(poll, InternalClient::new(socket, remote_addr), writer);
    client.get(path, mode).unwrap();
}
