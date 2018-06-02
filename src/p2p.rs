//
// Copyright 2018 Tamas Blummer
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
//!
//! # Dispatcher from network to node and back
//!
//! This module establishes network connections and routes messages between the P2P network and this node
//!

use bitcoin::network::constants::{magic, Network};
use bitcoin::network::encodable::{ConsensusDecodable, ConsensusEncodable};
use bitcoin::network::message::NetworkMessage;
use bitcoin::network::message::RawNetworkMessage;
use bitcoin::network::message_network::VersionMessage;
use bitcoin::network::serialize::{RawDecoder, RawEncoder};
use bitcoin::network::address::Address;
use bitcoin::util;
use error::SPVError;
use mio::*;
use mio::unix::UnixReady;
use mio::net::TcpStream;
use node::{Node, ProcessResult};
use rand::{Rng, StdRng};
use std::cmp::min;
use std::collections::{HashMap, VecDeque};
use std::collections::hash_map::Entry;
use std::fmt::{Display, Error, Formatter};
use std::io;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr};
use std::sync::{Arc, mpsc, RwLock, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const READ_BUFFER_SIZE:usize = 1024;
const EVENT_BUFFER_SIZE:usize = 10;

/// A peer's Id
/// used in log messages and as key to PeerMap
#[derive(Hash, Eq, PartialEq, Copy, Clone)]
pub struct PeerId {
    /// mio token used in networking
    pub token: Token
}

impl Display for PeerId {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        write!(f, "{}", self.token.0)?;
        Ok(())
    }
}

/// a map of peer id to peers
/// This map is shared between P2P and node
/// and is protected with an rw lock
/// Peers are mutex protected as sends
/// to them may be coming from different peers
pub type PeerMap = HashMap<PeerId, Mutex<Peer>>;

/// The P2P network layer
pub struct P2P {
    // network specific message prefix
    magic: u32,
    // This node's identifier on the network (random)
    nonce: u64,
    // height of the blockchain tree trunk
    height: u32,
    // This node's human readable type identification
    user_agent: String,
    // The collection of connected peers
    // access to this is shared with node and is rw lock protected
    peers: Arc<RwLock<PeerMap>>,
    // The poll object of the async IO layer (mio)
    // access to this is shared by P2P and Peer
    poll: Arc<Poll>,
    // next peer id
    // atomic only for interior mutability
    next_peer_id: AtomicUsize
}

impl P2P {
    /// create a new P2P network controller
    pub fn new(user_agent: String, network: Network, height: u32, peers: Arc<RwLock<PeerMap>>) -> P2P {
        let mut rng = StdRng::new().unwrap();
        P2P {
            magic: magic(network),
            nonce: rng.next_u64(),
            height,
            user_agent,
            peers,
            poll: Arc::new(Poll::new().unwrap()),
            next_peer_id: AtomicUsize::new(0)
        }
    }

    /// Add a peer
    pub fn add_peer (&self, addr: &SocketAddr) -> Result<PeerId, SPVError> {
        // new token, never re-using previously connected peer's id
        // so log messages are easier to follow
        let token = Token(self.next_peer_id.fetch_add(1, Ordering::Relaxed));
        let pid = PeerId{token};

        info!("initiating connect to {} peer={}", addr, pid);

        // create lock protected peer object
        let peer = Mutex::new(Peer::new(pid, self.poll.clone(), addr, self.nonce)?);

        // add peer object to peer map shared between P2P and node
        let mut peers = self.peers.write().unwrap();

        // send this node's version message to peer
        peer.lock().unwrap().send(&P2P::version(&self.user_agent, self.nonce, self.height, addr))?;

        // add to peer map
        peers.insert(pid, peer);

        trace!("added peer={}", pid);
        Ok(pid)
    }

    // compile this node's version message
    fn version (user_agent: &String, nonce: u64, height: u32, remote: &SocketAddr) -> NetworkMessage {
        // now in unix time
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;

        // build message
        NetworkMessage::Version(VersionMessage {
            version: 70001, // used only to be able to disable tx relay
            services: 0, // NODE_NONE this SPV implementation does not serve anything
            timestamp,
            receiver: Address::new(remote, 1),
            // TODO: sender is only dummy
            sender: Address::new(remote, 1),
            nonce: nonce,
            user_agent: user_agent.clone(),
            start_height: height as i32,
            relay: false,
        })
    }

    fn event_processor (&self, node: Arc<Node>, event: Event, pid: PeerId) -> Result<(), SPVError> {
        // check for error first
        if event.readiness().contains(Ready::hup()) {
            // disconnect on error
            if let Entry::Occupied(peer_entry) = self.peers.write().unwrap().entry(pid) {
                // get and lock the peer from the peer map entry
                peer_entry.get().lock().unwrap().stream.shutdown(Shutdown::Both).unwrap_or(());
                peer_entry.remove();
            }
            info!("left us peer={}", pid);
            node.disconnected(pid)?;
        } else {
            // check for ability to write before read, to get rid of data before buffering more read
            // token should only be registered for write if there is a need to write
            // to avoid superfluous wakeups from poll
            if event.readiness().contains(Ready::writable()) {
                trace!("writeable peer={}", pid);

                // figure peer's entry in the peer map, provided it is still connected, ignore event if not
                if let Some(peer) = self.peers.read().unwrap().get(&pid) {
                    // get and lock the peer from the peer map entry
                    let mut locked_peer = peer.lock().unwrap();
                    // get an outgoing message from the channel (if any)
                    while let Some(msg) = locked_peer.try_receive() {
                        // serialize the message
                        let mut buffer = Buffer::new();
                        let raw = RawNetworkMessage { magic: self.magic, payload: msg };
                        encode(&raw, &mut buffer)?;

                        // write to peer's socket
                        locked_peer.stream.write(buffer.into_vec().as_slice())?;
                        trace!("sent {} to peer={}", raw.command(), pid);
                    }
                    // de-register for write events if channel is empty
                    locked_peer.deregister()?;
                    // keep registered for read events
                    locked_peer.register_read()?;
                }
            }
            // is peer readable ?
            if event.readiness().contains(Ready::readable()) {
                trace!("readable peer={}", pid);
                // collect incoming messages here
                // incoming messages are collected here for processing after release
                // of the lock on the peer map.
                let mut incoming = Vec::new();
                // disconnect if set
                let mut disconnect = false;
                // new handshake if set
                let mut handshake = false;
                // read lock peer map and retrieve peer
                if let Some(peer) = self.peers.read().unwrap().get(&pid) {
                    // lock the peer from the peer
                    let mut locked_peer = peer.lock().unwrap();
                    // read buffer
                    let mut buffer = [0u8; READ_BUFFER_SIZE];
                    // read the peer's socket
                    while let Ok(len) = locked_peer.stream.read(&mut buffer) {
                        if disconnect || len == 0 {
                            break;
                        }
                        // accumulate in a buffer
                        locked_peer.buffer.write(&buffer[0..len])?;
                        // extract messages from the buffer
                        while let Some(msg) = decode(&mut locked_peer.buffer)? {
                            trace!("received {} peer={}", msg.command(), pid);
                            // process handshake first
                            match locked_peer.process_handshake(&msg)? {
                                HandShake::Disconnect => {
                                    trace!("disconnecting peer={}", pid);
                                    disconnect = true;
                                    break;
                                }
                                HandShake::Handshake => {
                                    handshake = true;
                                }
                                HandShake::InProgress => {},
                                HandShake::Process => {
                                    // queue messages to process outside of locked scope
                                    incoming.push(msg);
                                }
                            }
                        }
                    }
                }
                if disconnect {
                    if let Entry::Occupied(peer_entry) = self.peers.write().unwrap().entry(pid) {
                        // get and lock the peer from the peer map entry
                        peer_entry.get().lock().unwrap().stream.shutdown(Shutdown::Both)?;
                        peer_entry.remove();
                    }
                    info!("left us peer={}", pid);
                    node.disconnected(pid)?;
                }
                else {
                    if handshake {
                        info!("connected peer={}", pid);
                        node.connected (pid)?;
                    }
                    for msg in incoming {
                        trace!("processing {} for peer={}", msg.command(), pid);
                        match node.process (&msg.payload, pid)? {
                            ProcessResult::Ack | ProcessResult::Ignored => {},
                            ProcessResult::Disconnect => {
                                trace!("disconnecting peer={}", pid);
                                if let Some(peer) = self.peers.read().unwrap().get(&pid) {
                                    let locked_peer = peer.lock().unwrap();
                                    locked_peer.stream.shutdown(Shutdown::Both)?;
                                }
                                info!("disconnected peer={}", pid);
                                node.disconnected (pid)?;
                            },
                            ProcessResult::Height(new_height) => {
                                if let Some(peer) = self.peers.read().unwrap().get(&pid) {
                                    let mut locked_peer = peer.lock().unwrap();
                                    let mut nv = locked_peer.version.clone().unwrap();
                                    nv.start_height = new_height as i32;
                                    locked_peer.version = Some(nv);
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// run the message dispatcher loop
    /// this method does not return unless there is a serious networking error
    pub fn run(&self, node: Arc<Node>) -> Result<(), io::Error>{
        trace!("start mio event loop");
        loop {
            // events buffer
            let mut events = Events::with_capacity(EVENT_BUFFER_SIZE);

            // get the next batch of events
            self.poll.poll(&mut events, None)?;

            // iterate over events
            for event in events.iter() {
                // construct the id of the peer the event concerns
                let pid = PeerId { token: event.token() };
                if let Err(error) = self.event_processor(node.clone(), event, pid) {
                    warn!("error {} peer={}", error.to_string(), pid);
                    debug!("error {:?} peer={}", error, pid);
                }
            }
        }
    }
}

enum HandShake {
    Disconnect,
    InProgress,
    Handshake,
    Process
}

/// a peer
pub struct Peer {
    /// the peer's id for log messages
    pub pid: PeerId,
    poll: Arc<Poll>,
    stream: TcpStream,
    buffer: Buffer,
    got_verack: bool,
    nonce: u64,
    /// the version message the peer sent to us at connect
    pub version: Option<VersionMessage>,
    sender: mpsc::Sender<NetworkMessage>,
    receiver: mpsc::Receiver<NetworkMessage>
}

impl Peer {
    /// create a new peer
    pub fn new (pid: PeerId, poll: Arc<Poll>, addr: &SocketAddr, nonce: u64) -> Result<Peer, SPVError> {

        let stream = TcpStream::connect(addr)?;
        let (sender, receiver) = mpsc::channel();
        let peer = Peer{pid, poll: poll.clone(), stream, buffer: Buffer::new(),
            got_verack: false, nonce, version: None, sender, receiver};
        peer.register_read()?;
        Ok(peer)
    }

    fn register_read (&self) -> Result<(), SPVError> {
        trace!("register for mio read peer={}", self.pid);
        self.poll.register(&self.stream, self.pid.token, Ready::readable()|UnixReady::error(), PollOpt::edge())?;
        Ok(())
    }

    /// send a message to P2P network
    pub fn send (&self, msg: &NetworkMessage) -> Result<(), SPVError> {
        self.sender.send(msg.clone()).map_err(| _ | SPVError::Generic("can not send to peer queue".to_owned()))?;
        trace!("de-register mio events peer={}", self.pid);
        self.deregister()?;
        self.register_write()?;
        Ok(())
    }

    fn deregister (&self) -> Result<(), SPVError> {
        self.poll.deregister(&self.stream)?;
        Ok(())
    }

    fn register_write (&self) -> Result<(), SPVError> {
        trace!("register for mio write peer={}", self.pid);
        self.poll.register(&self.stream, self.pid.token, Ready::writable()|UnixReady::error(), PollOpt::edge())?;
        Ok(())
    }

    /// try to receive a message from node
    pub fn try_receive (&self) -> Option<NetworkMessage> {
        if let Ok (msg) = self.receiver.try_recv() {
            Some (msg)
        } else {
            None
        }
    }

    // process incoming messages
    // returns true after handshake
    fn process_handshake(&mut self, msg: &RawNetworkMessage) -> Result<HandShake, SPVError> {
        if !(self.version.is_some() && self.got_verack) {
            // before handshake complete
            match msg.payload {
                NetworkMessage::Version(ref version) => {
                    if self.version.is_some() {
                        return Ok(HandShake::Disconnect);
                    }

                    if version.nonce == self.nonce {
                        return Ok(HandShake::Disconnect);
                    } else {
                        // want to connect to full nodes upporting segwit
                        if version.services & 9 != 9 || version.version < 70013 {
                            return Ok(HandShake::Disconnect);
                        } else {
                            // acknowledge version message received
                            self.send(&NetworkMessage::Verack)?;
                            // all right, remember this peer
                            info!("Connected {} height: {} peer={}", version.user_agent, version.start_height, self.pid);
                            self.version = Some(version.clone());
                        }
                    }
                }
                NetworkMessage::Verack => {
                    if self.got_verack {
                        return Ok(HandShake::Disconnect);
                    }
                    trace!("got verack peer={}", self.pid);
                    self.got_verack = true;
                }
                _ => {
                    trace!("misbehaving peer={}", self.pid);
                    return Ok(HandShake::Disconnect);;
                }
            };
            if self.version.is_some() && self.got_verack {
                return Ok(HandShake::Handshake)
            }
            else {
                return Ok(HandShake::InProgress)
            }
        }
        Ok(HandShake::Process)
    }
}

struct Buffer {
    content: VecDeque<Vec<u8>>,
    pos: (usize, usize),
    checkpoint: (usize, usize)
}

impl Buffer {
    fn new () -> Buffer {
        Buffer{ content: VecDeque::new(), pos: (0, 0), checkpoint: (0, 0) }
    }

    fn checkpoint (&mut self) {
        self.checkpoint = self.pos;
    }

    fn rollback (&mut self) {
        self.pos = self.checkpoint;
    }

    fn commit (&mut self) {
        for _ in 0..self.pos.0 {
            self.content.pop_front();
        }
        self.pos.0 = 0;
    }

    fn into_vec (mut self) -> Vec<u8> {
        let mut merged = Vec::new();
        for v in self.content.drain(..) {
            merged.extend_from_slice(v.as_slice());
        }
        merged
    }
}

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        if buf.len() > 0 {
            if self.content.len () > 0 && self.content[self.pos.0].len() < READ_BUFFER_SIZE  {
                self.content[self.pos.0].extend_from_slice(buf);
            }
            else {
                self.content.push_back(buf.to_vec());
            }
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }
}

impl Read for Buffer {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        if self.content.len() == 0 {
            Ok(0)
        }
        else {
            let mut have = 0;
            while have < buf.len() {
                let current = &self.content[self.pos.0];
                let minlen = min(buf.len() - have, current.len() - self.pos.1);
                buf[have..have+minlen].copy_from_slice(&current[self.pos.1..self.pos.1 + minlen]);
                self.pos.1 += minlen;
                have += minlen;
                if self.pos.1 == current.len() {
                    if self.pos.0 < self.content.len() - 1 {
                        self.pos.0 += 1;
                        self.pos.1 = 0;
                    }
                    else {
                        break;
                    }
                }
            }
            Ok(have)
        }
    }
}

fn encode(item: &RawNetworkMessage, dst: &mut Buffer) -> Result<(), io::Error> {
    match item.consensus_encode(&mut RawEncoder::new(dst)) {
        Ok(_) => Ok(()),
        Err(e) => Err(io::Error::new(io::ErrorKind::WriteZero, e))
    }
}

fn decode(src: &mut Buffer) -> Result<Option<RawNetworkMessage>, io::Error> {
    src.checkpoint ();
    let mut raw = RawDecoder::new(src);
    let decode: Result<RawNetworkMessage, util::Error> =
        ConsensusDecodable::consensus_decode(&mut raw);
    let src = raw.into_inner();
    match decode {
        Ok(m) => {
            src.commit();
            Ok(Some(m))
        }
        Err(util::Error::ByteOrder(_)) => {
            src.rollback();
            Ok(None)
        },
        Err(e) => {
            trace!("invalid data in codec: {}", e);
            Err(io::Error::new(io::ErrorKind::InvalidData, e))
        }
    }
}
