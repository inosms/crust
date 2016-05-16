// Copyright 2016 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement, version 1.0.  This, along with the
// Licenses can be found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

use maidsafe_utilities::thread::RaiiThreadJoiner;
use mio::{EventLoop, NotifyError, Sender};
use net2;
use sodiumoxide::crypto::box_::{self, PublicKey, SecretKey};
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use core::{Core, CoreMessage, Context};
use connection_states::EstablishConnection;
use event::Event;
use error::Error;
use nat_traversal::{MappedTcpSocket, MappingContext, PrivRendezvousInfo, PubRendezvousInfo,
                    gen_rendezvous_info};
use peer_id::{self, PeerId};
use service_discovery::ServiceDiscovery;
use state::State;
use static_contact_info::StaticContactInfo;

/// The result of a `Service::prepare_contact_info` call.
#[derive(Debug)]
pub struct ConnectionInfoResult {
    /// The token that was passed to `prepare_connection_info`.
    pub result_token: u32,
    /// The new contact info, if successful.
    pub result: io::Result<OurConnectionInfo>,
}

/// Contact info generated by a call to `Service::prepare_contact_info`.
#[derive(Debug)]
pub struct OurConnectionInfo {
    id: PeerId,
    tcp_info: PubRendezvousInfo,
    priv_tcp_info: PrivRendezvousInfo,
    tcp_socket: Option<net2::TcpBuilder>,
    static_contact_info: StaticContactInfo,
}

impl OurConnectionInfo {
    /// Convert our connection info to theirs so that we can give it to peer.
    pub fn to_their_connection_info(&self) -> TheirConnectionInfo {
        TheirConnectionInfo {
            tcp_info: self.tcp_info.clone(),
            static_contact_info: self.static_contact_info.clone(),
            // tcp_addrs: self.tcp_addrs.clone(),
            id: self.id,
        }
    }
}


/// Contact info used to connect to another peer.
#[derive(Debug, RustcEncodable, RustcDecodable)]
pub struct TheirConnectionInfo {
    tcp_info: PubRendezvousInfo,
    static_contact_info: StaticContactInfo,
    id: PeerId,
}

impl TheirConnectionInfo {
    /// Returns the `PeerId` of the node that created this connection info.
    pub fn id(&self) -> PeerId {
        self.id
    }
}

/// A structure representing a connection manager.
pub struct Service {
    // This is the connection map -> PeerId <-> Context
    connection_map: Arc<Mutex<HashMap<PeerId, Context>>>,
    event_tx: ::CrustEventSender,
    mapping_context: Arc<MappingContext>,
    mio_tx: Sender<CoreMessage>,
    our_keys: (PublicKey, SecretKey),
    service_discovery: Arc<Mutex<Option<Context>>>,
    static_contact_info: Arc<Mutex<StaticContactInfo>>,
    _thread_joiner: RaiiThreadJoiner,
}

impl Service {
    /// Constructs a service.
    pub fn new(event_tx: ::CrustEventSender) -> Result<Self, Error> {
        let mut event_loop = try!(EventLoop::new());
        let mio_tx = event_loop.channel();
        let our_keys = box_::gen_keypair();
        // Form our initial contact info
        let static_contact_info = Arc::new(Mutex::new(StaticContactInfo {
            tcp_acceptors: Vec::new(),
            tcp_mapper_servers: Vec::new(),
        }));
        let mapping_context = try!(MappingContext::new()
                                       .result_log()
                                       .or_else(|e| {
                                           Err(io::Error::new(io::ErrorKind::Other,
                                                              format!("Failed to create \
                                                                       MappingContext: {}",
                                                                      e)))
                                       }));

        let joiner = RaiiThreadJoiner::new(thread!("Crust event loop", move || {
            let mut core = Core::new();
            event_loop.run(&mut core).expect("EventLoop failed to run");
        }));

        Ok(Service {
            connection_map: Arc::new(Mutex::new(HashMap::new())),
            event_tx: event_tx,
            mapping_context: Arc::new(mapping_context),
            mio_tx: mio_tx,
            our_keys: our_keys,
            service_discovery: Arc::new(Mutex::new(None)),
            static_contact_info: static_contact_info,
            _thread_joiner: joiner,
        })
    }

    /// Starts listening for beacon broadcasts.
    pub fn start_service_discovery(&mut self) {
        if self.service_discovery.lock().unwrap().is_some() {
            return;
        }
        let service_discovery = self.service_discovery.clone();
        let cloned_contact_info = self.static_contact_info.clone();
        let _ = self.post(move |core, event_loop| {
            if let Err(e) = ServiceDiscovery::new(core,
                                                  event_loop,
                                                  cloned_contact_info,
                                                  service_discovery,
                                                  5483) {
                println!("Could not start ServiceDiscovery: {:?}", e);
            }
        });
    }


    /// connect to peer
    pub fn connect(&mut self, peer_contact_info: SocketAddr) {
        let routing_tx = self.event_tx.clone();
        let connection_map = self.connection_map.clone();

        let _ = self.post(move |core, event_loop| {
            EstablishConnection::new(core,
                                     event_loop,
                                     connection_map,
                                     routing_tx,
                                     peer_contact_info);
        });
    }

    /// Disconnect from the given peer and returns whether there was a connection at all.
    pub fn disconnect(&mut self, peer_id: PeerId) -> bool {
        let context = match self.connection_map.lock().unwrap().remove(&peer_id) {
            Some(context) => context,
            None => return false,
        };

        let _ = self.post(move |mut core, mut event_loop| {
            if let Some(state) = core.get_state(&context) {
                state.borrow_mut().terminate(&mut core, &mut event_loop);
            }
        });

        true
    }

    /// sending data to a peer(according to it's u64 peer_id)
    pub fn send(&mut self, peer_id: PeerId, data: Vec<u8>) {
        if data.len() > ::MAX_DATA_LEN as usize {
            let _ = self.event_tx.send(Event::WriteMsgSizeProhibitive(peer_id, data));
            return;
        }
        let context = self.connection_map.lock().unwrap().get(&peer_id).expect("Context not found")
                                                                       .clone();
        let mut data = Some(data);
        let _ = self.post(move |mut core, mut event_loop| {
            let state = core.get_state(&context).expect("State not found");
            state.borrow_mut().write(&mut core,
                                     &mut event_loop,
                                     data.take().expect("Logic Error"));
        });
    }

    /// Enable listening and responding to peers searching for us. This will allow others finding us
    /// by interrogating the network.
    pub fn set_service_discovery_listen(&self, listen: bool) {
        if let Some(handle) = *self.service_discovery.lock().unwrap() {
            let _ = self.post(move |core, _| {
                let state = core.get_state(&handle)
                                .expect("ServiceDiscovery not found")
                                .clone();
                let mut temp = state.borrow_mut();
                let service_discovery = match temp.as_any().downcast_mut::<ServiceDiscovery>() {
                    Some(b) => b,
                    None => panic!("&ServiceDiscovery isn't a ServiceDiscovery!"),
                };
                service_discovery.set_listen(listen);
            });
        }
    }

    /// Interrogate the network to find peers.
    pub fn seek_peers(&self) {
        if let Some(handle) = *self.service_discovery.lock().unwrap() {
            let _ = self.post(move |core, _| {
                let state = core.get_state(&handle).expect("State not found").clone();
                let mut temp = state.borrow_mut();
                let service_discovery = match temp.as_any().downcast_mut::<ServiceDiscovery>() {
                    Some(b) => b,
                    None => panic!("&ServiceDiscovery isn't a ServiceDiscovery!"),
                };
                service_discovery.seek_peers().unwrap();
            });
        }
    }

    /// Lookup a mapped udp socket based on result_token
    // TODO: immediate return in case of sender.send() returned with NotificationError
    pub fn prepare_connection_info(&mut self, result_token: u32) {
        // FIXME: If the listeners are directly addressable (direct full cone or upnp mapped etc.
        // then our conact info is our static liseners
        // for udp we can map another socket, but use same local port if accessable/mapped
        // otherwise do following
        let our_static_contact_info = self.static_contact_info.clone();
        let event_tx = self.event_tx.clone();
        let mapping_context = self.mapping_context.clone();
        let our_pub_key = self.our_keys.0.clone();
        if let Err(_) = self.post(move |_, _| {
            let (tcp_socket, (our_priv_tcp_info, our_pub_tcp_info)) =
                match MappedTcpSocket::new(&mapping_context).result_log() {
                    Ok(MappedTcpSocket { socket, endpoints }) => {
                        (Some(socket), gen_rendezvous_info(endpoints))
                    }
                    Err(err) => {
                        let _ =
                            event_tx.send(Event::ConnectionInfoPrepared(ConnectionInfoResult {
                                result_token: result_token,
                                result: Err(From::from(err)),
                            }));
                        return;
                    }
                };

            let event = Event::ConnectionInfoPrepared(ConnectionInfoResult {
                result_token: result_token,
                result: Ok(OurConnectionInfo {
                    id: peer_id::new_id(our_pub_key),
                    tcp_info: our_pub_tcp_info,
                    priv_tcp_info: our_priv_tcp_info,
                    tcp_socket: tcp_socket,
                    static_contact_info: unwrap_result!(our_static_contact_info.lock()).clone(),
                }),
            });
            let _ = event_tx.send(event);
        }) {
            let _ = self.event_tx.send(Event::ConnectionInfoPrepared(ConnectionInfoResult {
                result_token: result_token,
                result: Err(io::Error::new(io::ErrorKind::Other,
                                           format!("Failed to register task with mio eventloop"))),
            }));
        }
    }

    fn post<F>(&self, f: F) -> Result<(), NotifyError<CoreMessage>>
        where F: FnOnce(&mut Core, &mut EventLoop<Core>) + Send + 'static
    {
        self.mio_tx.send(CoreMessage::new(f))
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.post(|_, el| el.shutdown());
    }
}

#[cfg(test)]
mod test {
    // use super::*;
    // use event::Event;

    // use std::thread;
    // use std::sync::mpsc;
    // use std::time::Duration;
    // use std::sync::mpsc::Receiver;

    // use maidsafe_utilities::event_sender::{MaidSafeObserver, MaidSafeEventCategory};

    // fn _get_event_sender() -> (::CrustEventSender, Receiver<Event>) {
    //     let (category_tx, _) = mpsc::channel();
    //     let event_category = MaidSafeEventCategory::Crust;
    //     let (event_tx, event_rx) = mpsc::channel();

    //     (MaidSafeObserver::new(event_tx, event_category, category_tx), event_rx)
    // }
}
