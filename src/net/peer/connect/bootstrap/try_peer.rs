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

use net::peer;
use net::peer::connect::handshake_message::{
    BootstrapDenyReason, BootstrapRequest, HandshakeMessage,
};
use priv_prelude::*;

quick_error! {
    /// Error returned when we fail to connect to some specific peer.
    #[derive(Debug)]
    pub enum TryPeerError {
        TimedOut {
            description("timed out trying to make the connection")
        }
        Connect(e: DirectConnectError) {
            description("IO error connecting to remote peer")
            display("IO error connecting to remote peer: {}", e)
            from(e)
        }
        Handshake(e: ConnectHandshakeError) {
            description("Error during peer handshake")
            display("Error during peer handshake: {}", e)
            from()
        }
    }
}

quick_error! {
    #[derive(Debug)]
    pub enum ConnectHandshakeError {
        BootstrapDenied(e: BootstrapDenyReason) {
            description("Bootstrap denied")
            display("Bootstrap denied. reason: {:?}", e)
            from(e)
        }
        Io(e: io::Error) {
            description("io error on the socket")
            display("io error on the socket: {}", e)
            cause(e)
            from()
        }
        InvalidResponse {
            description("invalid response from peer")
        }
        Read(e: PaStreamReadError) {
            description("error reading response from peer")
            display("error reading response from peer: {}", e)
            cause(e)
        }
        Write(e: PaStreamWriteError) {
            description("error writing to underlying stream")
            display("error writing to underlying stream: {}", e)
            cause(e)
        }
        Disconnected {
            description("Disconnected from peer")
        }
        TimedOut {
            description("timed out performing handshake")
        }
        Encrypt(e: EncryptionError) {
            description("Error encrypting request to peer")
            display("Error encrypting request to peer: {}", e)
            cause(e)
        }
        Decrypt(e: EncryptionError) {
            description("Error decrypting message from peer")
            display("Error decrypting message from peer: {}", e)
            cause(e)
        }
    }
}

/// Try to bootstrap to the given peer.
pub fn try_peer(
    handle: &Handle,
    addr: &PaAddr,
    config: &ConfigFile,
    request: BootstrapRequest,
    their_pk: PublicKeys,
) -> BoxFuture<Peer, TryPeerError> {
    let handle0 = handle.clone();
    let addr = *addr;
    PaStream::direct_connect(handle, &addr, their_pk.clone(), config)
        .map_err(TryPeerError::Connect)
        .with_timeout(Duration::from_secs(10), handle)
        .and_then(|res| res.ok_or(TryPeerError::TimedOut))
        .and_then(move |socket| {
            bootstrap_connect_handshake(&handle0, socket, request, their_pk)
                .map_err(TryPeerError::Handshake)
        })
        .into_boxed()
}

/// Construct a `Peer` by performing a bootstrap connection handshake on a socket.
pub fn bootstrap_connect_handshake(
    handle: &Handle,
    stream: PaStream,
    request: BootstrapRequest,
    their_pk: PublicKeys,
) -> BoxFuture<Peer, ConnectHandshakeError> {
    let handle0 = handle.clone();
    stream
        .send_serialized(HandshakeMessage::BootstrapRequest(request))
        .map_err(ConnectHandshakeError::Write)
        .and_then(move |stream| {
            stream
                .recv_serialized()
                .map_err(ConnectHandshakeError::Read)
                .and_then(move |(msg_opt, stream)| {
                    let msg = msg_opt.ok_or(ConnectHandshakeError::Disconnected)?;
                    match msg {
                        HandshakeMessage::BootstrapGranted => Ok(peer::from_handshaken_stream(
                            &handle0,
                            their_pk,
                            stream,
                            CrustUser::Node,
                        )),
                        HandshakeMessage::BootstrapDenied(reason) => {
                            Err(ConnectHandshakeError::BootstrapDenied(reason))
                        }
                        _ => Err(ConnectHandshakeError::InvalidResponse),
                    }
                })
        })
        .with_timeout(Duration::from_secs(9), handle)
        .and_then(|res| res.ok_or(ConnectHandshakeError::TimedOut))
        .into_boxed()
}
