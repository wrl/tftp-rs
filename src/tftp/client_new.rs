//! A Trivial File Transfer (TFTP) protocol client implementation.
//!
//! This module contains the ability to read data from or write data to a remote TFTP server.

use std::convert::From;
use std::io;
use std::path::Path;
use std::net::SocketAddr;
use std::str::FromStr;
use std::result;
use std::error;
use std::fmt;
use std::str;

use packet::{Mode, RequestPacket, DataPacketOctet, AckPacket, ErrorPacket,
             EncodePacket, RawPacket, Opcode};

use mio::udp::UdpSocket;
use rotor::{EventSet, PollOpt, Loop, Config, Void};
use rotor::{Machine, Response, Scope, EarlyScope};

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
    fn send_ack(&self, block_id: u16) -> Result<Option<()>>;
}

trait PacketReceiver {
    fn receive_data(&mut self) -> Result<Option<DataPacketOctet<'static>>>;
}

struct InternalClient {
    socket: UdpSocket,
    remote_addr: SocketAddr,
}

impl InternalClient {
    fn new(socket: UdpSocket, remote_addr: SocketAddr) -> InternalClient {
        InternalClient { socket: socket, remote_addr: remote_addr }
    }
}

impl PacketSender for InternalClient {
    fn send_read_request(&self, path: &str, mode: Mode) -> Result<()> {
        let read_request = RequestPacket::read_request(path, mode);
        let encoded = read_request.encode();
        let buf = encoded.packet_buf();
        self.socket.send_to(&buf, &self.remote_addr).map(|_| ()).map_err(From::from)
    }

    fn send_ack(&self, block_id: u16) -> Result<Option<()>> {
        let ack = AckPacket::new(block_id);
        let encoded = ack.encode();
        let buf = encoded.packet_buf();
        self.socket.send_to(&buf, &self.remote_addr).map(|opt| opt.map(|_| ())).map_err(From::from)
    }
}

impl PacketReceiver for InternalClient {
    fn receive_data(&mut self) -> Result<Option<DataPacketOctet<'static>>> {
        let mut buf = vec![0; MAX_DATA_SIZE + 4];
        let result = try!(self.socket.recv_from(&mut buf));
        let p = result.map(|(n, from)| {
            self.remote_addr = from;
            RawPacket::new(buf, n)
        }).map(|packet| {
            match packet.opcode() {
                Some(Opcode::DATA) => {
                    packet.decode::<DataPacketOctet>().unwrap()
//                        .ok_or(io::Error::new(io::ErrorKind::Other, "todo")))
                },
                _ => unimplemented!(),
//                Some(Opcode::ERROR) => return Err(From::from(io::Error::new(io::ErrorKind::Other, "error"))),
//                _ => return Err(From::from(io::Error::new(io::ErrorKind::Other, "unexpected"))),
            }
        });
        Ok(p)
    }
}

macro_rules! mtry {
    ($s: ident, $e:expr) => (match $e {
        Ok(val) => val,
        Err(err) => {
            $s.error = Some(::std::convert::From::from(err));
            return Client::finish($s);
        },
    });
}

struct Context {
    error: Option<Error>,
}

struct ClientState<'a> {
    client: InternalClient,
    path: &'a Path,
    mode: Mode,
    writer: &'a mut io::Write,
}

enum Client<'a> {
    Idle(ClientState<'a>),
    ReceivingData(ClientState<'a>, u16),
    SendAck(ClientState<'a>, DataPacketOctet<'static>),
}

impl<'a> Client<'a> {
    fn new(scope: &mut EarlyScope, client_state: ClientState<'a>) -> Response<Client<'a>, Void> {
        scope.register(&client_state.client.socket, EventSet::writable(), PollOpt::level()).unwrap();
        Response::ok(Client::Idle(client_state))
    }

    fn finish(scope: &mut Scope<Context>) -> Response<Self, Void> {
        scope.shutdown_loop();
        Response::done()
    }
}

impl<'a> Machine for Client<'a> {
    type Context = Context;
    type Seed = Void;

    fn create(_: Void, scope: &mut Scope<Context>) -> Response<Self, Void>
    {
        println!("create");
        unreachable!();
    }

    fn ready(self, events: EventSet, scope: &mut Scope<Context>) -> Response<Self, Void>
    {
//        println!("ready: {:?}", events);
        match self {
            Client::Idle(state) => {
                mtry!(scope, state.client.send_read_request(state.path.to_str().unwrap(), Mode::Octet));
                println!("Starting transfer ...");
                mtry!(scope, scope.reregister(&state.client.socket, EventSet::readable(), PollOpt::level()));
                Response::ok(Client::ReceivingData(state, 1))
            }
            Client::ReceivingData(mut state, current_id) => {
                let data_packet = match mtry!(scope, state.client.receive_data()) {
                    Some(data_packet) => data_packet,
                    None => return Response::ok(Client::ReceivingData(state, current_id)),
                };
                if current_id == data_packet.block_id() {
                    Client::SendAck(state, data_packet).ready(events, scope)
                } else {
                    println!("Unexpected packet id: got={}, expected={}",
                             data_packet.block_id(), current_id);
                    Response::ok(Client::ReceivingData(state, current_id))
                }
            }
            Client::SendAck(state, data_packet) => {
                if mtry!(scope, state.client.send_ack(data_packet.block_id())).is_none() {
                    mtry!(scope, scope.reregister(&state.client.socket, EventSet::writable(), PollOpt::level()));
                    println!("Could not send ack for packet id={}", data_packet.block_id());
                    Response::ok(Client::SendAck(state, data_packet))
                } else {
                    mtry!(scope, state.writer.write_all(data_packet.data()));
                    if data_packet.data().len() < MAX_DATA_SIZE {
                        println!("Transfer complete");
                        Client::finish(scope)
                    } else {
                        if events.is_writable() {
                            mtry!(scope, scope.reregister(&state.client.socket, EventSet::readable(), PollOpt::level()));
                        }
                        Response::ok(Client::ReceivingData(state, data_packet.block_id() + 1))
                    }
                }
            }
        }
    }

    fn spawned(self, _scope: &mut Scope<Context>) -> Response<Self, Void>
    {
        println!("spawned");
        unreachable!();
    }

    fn timeout(self, _scope: &mut Scope<Context>) -> Response<Self, Void>
    {
        println!("timeout");
        unreachable!();
    }

    fn wakeup(self, _scope: &mut Scope<Context>) -> Response<Self, Void>
    {
        println!("wakeup");
        unreachable!();
    }
}

pub fn get(path: &Path, mode: Mode, writer: &mut io::Write) {
    println!("starting ...");
    let remote_addr = "127.0.0.1:69".parse().unwrap();
    let mut loop_creator = Loop::new(&Config::new()).unwrap();
    let any = str::FromStr::from_str("0.0.0.0:0").unwrap();
    let socket = UdpSocket::bound(&any).unwrap();
    let state = ClientState {
        client: InternalClient::new(socket, remote_addr),
        path: path,
        mode: mode,
        writer: writer,
    };
    loop_creator.add_machine_with(|scope| {
        Client::new(scope, state)
    }).unwrap();
    let context = Context {
        error: None,
    };
    loop_creator.run(context).unwrap();
}
