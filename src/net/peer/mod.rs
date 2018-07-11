// Copyright 2017 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement.  This, along with the Licenses can be
// found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

pub use self::connect::{
    bootstrap, start_rendezvous_connect, BootstrapAcceptError, BootstrapAcceptor, BootstrapCache,
    BootstrapCacheError, BootstrapError, BootstrapRequest, ConnectError, ConnectHandshakeError,
    Demux, ExternalReachability, P2pConnectionInfo, PrivConnectionInfo, PubConnectionInfo,
    RendezvousConnectError, SingleConnectionError,
};
use std::fmt;

mod connect;
mod peer_message;

use priv_prelude::*;

#[cfg(not(test))]
pub const INACTIVITY_TIMEOUT_MS: u64 = 120_000;
#[cfg(not(test))]
const HEARTBEAT_PERIOD_MS: u64 = 20_000;

#[cfg(test)]
pub const INACTIVITY_TIMEOUT_MS: u64 = 900_000;
#[cfg(test)]
const HEARTBEAT_PERIOD_MS: u64 = 300_000;

/// A connection to a remote peer.
///
/// Use `Peer` to send and receive data asynchronously.
/// It implements [Stream and Sink](https://tokio.rs/docs/getting-started/streams-and-sinks/)
/// traits from futures crate.
/// In the background `Peer` keeps sending heartbeats to keep the connection alive and detect when
/// peers have disconnected.
// This wraps a `PaStream` and uses it to send `PeerMessage`s to peers.
//
// TODO: One problem with the implementation is that it takes serialized messages from the upper
// layer and then re-serialises them for no reason. This behaviour is inherited from the old crust
// (where `Peer` and `Socket` were the same type) but should really be fixed. The heartbeat could
// simply be encoded as a zero-byte message.
pub struct Peer {
    their_uid: PublicId,
    kind: CrustUser,
    stream: PaStream,
    last_send_time: Instant,
    send_heartbeat_timeout: Timeout,
    recv_heartbeat_timeout: Timeout,
}

impl fmt::Debug for Peer {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Peer")
            .field("id", &self.their_uid)
            .field("kind", &self.kind)
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
enum PeerMsg {
    HeartBeat,
    Data(BytesMut),
}

quick_error! {
    /// Peer related errors.
    #[derive(Debug)]
    pub enum PeerError {
        /// Peer was destroyed while still trying to do some actions on it.
        Destroyed {
            description("Socket has been destroyed")
        }
        /// Serialisation error
        Serialisation(e: SerialisationError) {
            description("serialisation error")
            display("serialisation error: {}", e)
            cause(e)
            from()
        }
        /// Peer socket related failure.
        Io(e: io::Error) {
            description("Io error on socket")
            display("Io error on socket: {}", e)
            cause(e)
            from()
        }
        /// Error reading from stream
        Read(e: PaStreamReadError) {
            description("error reading from stream")
            display("error reading from stream: {}", e)
            cause(e)
            from()
        }
        /// Error writing to stream
        Write(e: PaStreamWriteError) {
            description("error writing to stream")
            display("error writing to stream: {}", e)
            cause(e)
            from()
        }
        /// Peer was irresponsive.
        InactivityTimeout {
            description("connection to peer timed out")
            display("connection to peer timed out after {}s", INACTIVITY_TIMEOUT_MS / 1000)
        }
        /// Failure to encrypt message.
        Encrypt(e: EncryptionError) {
            description("Error encrypting message to peer")
            display("Error encrypting message to peer: {}", e)
            cause(e)
        }
        /// Failure to decrypt message.
        Decrypt(e: EncryptionError) {
            description("Error decrypting message from peer")
            display("Error decrypting message from peer: {}", e)
            cause(e)
        }
        /// Error deserializing message
        Deserialize(e: SerialisationError) {
            description("error deserializing message from remote peer")
            display("error deserializing message from remote peer: {}", e)
            cause(e)
        }
    }
}

/// Construct a `Peer` from a `PaStream` once we have completed the initial handshake.
pub fn from_handshaken_stream(
    handle: &Handle,
    their_uid: PublicId,
    stream: PaStream,
    kind: CrustUser,
) -> Peer {
    let now = Instant::now();
    Peer {
        their_uid,
        stream,
        kind,
        last_send_time: now,
        send_heartbeat_timeout: Timeout::new_at(
            now + Duration::from_millis(HEARTBEAT_PERIOD_MS),
            handle,
        ),
        recv_heartbeat_timeout: Timeout::new_at(
            now + Duration::from_millis(INACTIVITY_TIMEOUT_MS),
            handle,
        ),
    }
}

impl Peer {
    /// Return peer socket address.
    pub fn addr(&self) -> Result<PaAddr, PeerError> {
        Ok(self.stream.peer_addr()?)
    }

    /// Return peer id.
    pub fn public_id(&self) -> &PublicId {
        &self.their_uid
    }

    /// Returns peer type.
    pub fn kind(&self) -> CrustUser {
        self.kind
    }

    /// Return peer IP address.
    pub fn ip(&self) -> Result<IpAddr, PeerError> {
        Ok(self.stream.peer_addr().map(|a| a.ip())?)
    }

    /// Gracefully shutdown the connection to the remote peer
    pub fn finalize(self) -> IoFuture<()> {
        self.stream.finalize()
    }

    #[cfg(test)]
    /// Consume the peer, return it's inner PaStream
    pub fn into_pa_stream(self) -> PaStream {
        self.stream
    }

    /// Poll heartbeat timer and send heartbeat if required.
    fn poll_heartbeat(&mut self) {
        let heartbeat_period = Duration::from_millis(HEARTBEAT_PERIOD_MS);
        let now = Instant::now();
        while let Async::Ready(..) = self.send_heartbeat_timeout.poll().void_unwrap() {
            self.send_heartbeat_timeout
                .reset(self.last_send_time + heartbeat_period);
            if now - self.last_send_time >= heartbeat_period {
                self.last_send_time = now;
                let msg = Bytes::from(unwrap!(serialisation::serialise(&PeerMsg::HeartBeat)));
                let _ = self.stream.start_send(msg);
            }
        }
    }
}

impl Stream for Peer {
    type Item = BytesMut;
    type Error = PeerError;

    fn poll(&mut self) -> Result<Async<Option<BytesMut>>, PeerError> {
        self.poll_heartbeat();
        loop {
            match self.stream.poll() {
                Err(e) => return Err(PeerError::from(e)),
                Ok(Async::NotReady) => break,
                Ok(Async::Ready(None)) => return Ok(Async::Ready(None)),
                Ok(Async::Ready(Some(msg))) => {
                    let instant = Instant::now() + Duration::from_millis(INACTIVITY_TIMEOUT_MS);
                    self.recv_heartbeat_timeout.reset(instant);
                    let msg: PeerMsg = match serialisation::deserialise(&msg) {
                        Ok(msg) => msg,
                        Err(e) => return Err(PeerError::Deserialize(e)),
                    };
                    match msg {
                        PeerMsg::Data(data) => {
                            return Ok(Async::Ready(Some(data)));
                        }
                        PeerMsg::HeartBeat => (),
                    }
                }
            }
        }

        if let Async::Ready(..) = self.recv_heartbeat_timeout.poll().void_unwrap() {
            return Err(PeerError::InactivityTimeout);
        }

        Ok(Async::NotReady)
    }
}

impl Sink for Peer {
    type SinkItem = Bytes;
    type SinkError = PeerError;

    fn start_send(&mut self, data: Bytes) -> Result<AsyncSink<Bytes>, PeerError> {
        let data = BytesMut::from(data);
        let peer_msg = PeerMsg::Data(data);
        let msg = Bytes::from(unwrap!(serialisation::serialise(&peer_msg)));
        let data = match peer_msg {
            PeerMsg::Data(data) => data,
            _ => unreachable!(),
        };
        match self.stream.start_send(msg)? {
            AsyncSink::Ready => {
                self.last_send_time = Instant::now();
                Ok(AsyncSink::Ready)
            }
            AsyncSink::NotReady(_msg) => Ok(AsyncSink::NotReady(data.freeze())),
        }
    }

    fn poll_complete(&mut self) -> Result<Async<()>, PeerError> {
        self.stream.poll_complete().map_err(PeerError::from)
    }
}
