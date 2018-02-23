use consts::{REKEY_TIMEOUT, REKEY_AFTER_TIME, REJECT_AFTER_TIME, REKEY_ATTEMPT_TIME,
             KEEPALIVE_TIMEOUT, STALE_SESSION_TIMEOUT, MAX_CONTENT_SIZE, TIMER_RESOLUTION};
use cookie;
use interface::{SharedPeer, SharedState, UtunPacket, config};
use peer::{Peer, SessionType};
use time::Timestamp;
use timer::{Timer, TimerMessage};

use std::io;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use byteorder::{ByteOrder, LittleEndian};
use failure::{Error, err_msg};
use futures::{Async, Future, Stream, Sink, Poll, unsync::mpsc, stream, future};
use socket2::{Socket, Domain, Type, Protocol};
use tokio_core::net::{UdpSocket, UdpCodec, UdpFramed};
use tokio_core::reactor::Handle;


pub type PeerServerMessage = (SocketAddr, Vec<u8>);
struct VecUdpCodec;
impl UdpCodec for VecUdpCodec {
    type In = PeerServerMessage;
    type Out = PeerServerMessage;

    fn decode(&mut self, src: &SocketAddr, buf: &[u8]) -> io::Result<Self::In> {
        let unmapped_ip = match src.ip() {
            IpAddr::V6(v6addr) => {
                if let Some(v4addr) = v6addr.to_ipv4() {
                    IpAddr::V4(v4addr)
                } else {
                    IpAddr::V6(v6addr)
                }
            }
            v4addr => v4addr
        };
        Ok((SocketAddr::new(unmapped_ip, src.port()), buf.to_vec()))
    }

    fn encode(&mut self, msg: Self::Out, buf: &mut Vec<u8>) -> SocketAddr {
        let (mut addr, mut data) = msg;
        buf.append(&mut data);
        let mapped_ip = match addr.ip() {
            IpAddr::V4(v4addr) => IpAddr::V6(v4addr.to_ipv6_mapped()),
            v6addr => v6addr
        };
        addr.set_ip(mapped_ip);
        addr
    }
}

struct Channel<T> {
    tx: mpsc::Sender<T>,
    rx: mpsc::Receiver<T>,
}

impl<T> From<(mpsc::Sender<T>, mpsc::Receiver<T>)> for Channel<T> {
    fn from(pair: (mpsc::Sender<T>, mpsc::Receiver<T>)) -> Self {
        Self {
            tx: pair.0,
            rx: pair.1,
        }
    }
}

pub struct PeerServer {
    handle       : Handle,
    shared_state : SharedState,
    ingress      : Option<stream::SplitStream<UdpFramed<VecUdpCodec>>>,
    egress_tx    : Option<mpsc::Sender<PeerServerMessage>>,
    port         : Option<u16>,
    outgoing     : Channel<UtunPacket>,
    config       : Channel<config::UpdateEvent>,
    timer        : Timer,
    tunnel_tx    : mpsc::Sender<Vec<u8>>,
    cookie       : cookie::Validator,
}

impl PeerServer {
    pub fn new(handle: Handle, shared_state: SharedState, tunnel_tx: mpsc::Sender<Vec<u8>>) -> Result<Self, Error> {
        Ok(PeerServer {
            shared_state, tunnel_tx,
            handle    : handle.clone(),
            timer     : Timer::new(handle),
            ingress   : None,
            egress_tx : None,
            port      : None,
            outgoing  : mpsc::channel(1024).into(),
            config    : mpsc::channel(1024).into(),
            cookie    : cookie::Validator::new(&[0u8; 32])
        })
    }

    pub fn rebind(&mut self) -> Result<(), Error> {
        let port    = self.shared_state.borrow().interface_info.listen_port.unwrap_or(0);
        let socket  = Socket::new(Domain::ipv6(), Type::dgram(), Some(Protocol::udp()))?;
        if self.port.is_some() && self.port.unwrap() == port {
            debug!("skipping rebind, since we're already listening on the correct port.");
            return Ok(())
        }
        socket.set_only_v6(false)?;
        socket.set_nonblocking(true)?;
        socket.bind(&SocketAddr::from((Ipv6Addr::unspecified(), port)).into())?;

        trace!("listening on {}", port);

        let socket = UdpSocket::from_socket(socket.into_udp_socket(), &self.handle)?;
        let (udp_sink, udp_stream) = socket.framed(VecUdpCodec{}).split();
        let (egress_tx, egress_rx) = mpsc::channel(1024);
        let udp_writethrough = udp_sink.sink_map_err(|_| ()).send_all(
            egress_rx.and_then(|(addr, packet)| {
                trace!("sending UDP packet to {:?}", &addr);
                future::ok((addr, packet))
            }).map_err(|_| { info!("udp sink error"); () }))
            .then(|_| Ok(()));

        self.handle.spawn(udp_writethrough);

        self.port      = Some(port);
        self.ingress   = Some(udp_stream);
        self.egress_tx = Some(egress_tx);
        Ok(())
    }

    pub fn tx(&self) -> mpsc::Sender<UtunPacket> {
        self.outgoing.tx.clone()
    }

    pub fn config_tx(&self) -> mpsc::Sender<config::UpdateEvent> {
        self.config.tx.clone()
    }

    fn send_to_peer(&self, payload: PeerServerMessage) -> Result<(), Error> {
        let tx = self.egress_tx.as_ref().ok_or_else(|| err_msg("no egress tx"))?.clone();
        self.handle.spawn(tx.send(payload).then(|_| Ok(())));
        Ok(())
    }

    fn send_to_tunnel(&self, packet: Vec<u8>) {
        self.handle.spawn(self.tunnel_tx.clone().send(packet).then(|_| Ok(())));
    }

    fn handle_ingress_packet(&mut self, addr: SocketAddr, packet: &[u8]) -> Result<(), Error> {
        trace!("got a UDP packet from {:?} of length {}, packet type {}", &addr, packet.len(), packet[0]);
        match packet[0] {
            1 => self.handle_ingress_handshake_init(addr, packet),
            2 => self.handle_ingress_handshake_resp(addr, packet),
            3 => self.handle_ingress_cookie_reply(addr, packet),
            4 => self.handle_ingress_transport(addr, packet),
            _ => bail!("unknown wireguard message type")
        }
    }

    fn handle_ingress_handshake_init(&mut self, addr: SocketAddr, packet: &[u8]) -> Result<(), Error> {
        ensure!(packet.len() == 148, "handshake init packet length is incorrect");
        let mut state = self.shared_state.borrow_mut();
        {
            let (mac_in, mac_out) = packet.split_at(116);
            self.cookie.verify_mac1(mac_in, &mac_out[..16])?;
        }

        debug!("got handshake initiation request (0x01)");

        let handshake = Peer::process_incoming_handshake(
            &state.interface_info.private_key.ok_or_else(|| err_msg("no private key!"))?,
            packet)?;

        let peer_ref = state.pubkey_map.get(handshake.their_pubkey())
            .ok_or_else(|| err_msg("unknown peer pubkey"))?.clone();

        let mut peer = peer_ref.borrow_mut();
        let (response, next_index) = peer.complete_incoming_handshake(addr, handshake)?;
        let _ = state.index_map.insert(next_index, peer_ref.clone());

        self.send_to_peer((addr, response))?;
        info!("sent handshake response, ratcheted session (index {}).", next_index);

        Ok(())
    }

    // TODO use the address to update endpoint if it changes i suppose
    fn handle_ingress_handshake_resp(&mut self, _addr: SocketAddr, packet: &[u8]) -> Result<(), Error> {
        ensure!(packet.len() == 92, "handshake resp packet length is incorrect");
        let mut state = self.shared_state.borrow_mut();
        {
            let (mac_in, mac_out) = packet.split_at(60);
            self.cookie.verify_mac1(mac_in, &mac_out[..16])?;
        }
        debug!("got handshake response (0x02)");

        let our_index = LittleEndian::read_u32(&packet[8..]);
        let peer_ref  = state.index_map.get(&our_index)
            .ok_or_else(|| format_err!("unknown our_index ({})", our_index))?
            .clone();
        let mut peer = peer_ref.borrow_mut();
        let dead_index = peer.process_incoming_handshake_response(packet)?;
        if let Some(index) = dead_index {
            let _ = state.index_map.remove(&index);
        }

        if peer.ready_for_transport() {
            if !peer.outgoing_queue.is_empty() {
                debug!("sending {} queued egress packets", peer.outgoing_queue.len());
                while let Some(packet) = peer.outgoing_queue.pop_front() {
                    self.send_to_peer(peer.handle_outgoing_transport(packet.payload())?)?;
                }
            } else {
                self.send_to_peer(peer.handle_outgoing_transport(&[])?)?;
            }
        } else {
            error!("peer not ready for transport after processing handshake response. this shouldn't happen.");
        }
        info!("handshake response received, current session now {}", our_index);

        self.timer.spawn_delayed(*KEEPALIVE_TIMEOUT,
                                 TimerMessage::PassiveKeepAlive(peer_ref.clone(), our_index));

        self.timer.spawn_delayed(*REJECT_AFTER_TIME,
                                 TimerMessage::Reject(peer_ref.clone(), our_index));

        match peer.info.keepalive {
            Some(keepalive) if keepalive > 0 => {
                self.timer.spawn_delayed(Duration::from_secs(u64::from(keepalive)),
                                         TimerMessage::PersistentKeepAlive(peer_ref.clone(), our_index));
            }, _ => {}
        }
        Ok(())
    }

    fn handle_ingress_cookie_reply(&mut self, _addr: SocketAddr, packet: &[u8]) -> Result<(), Error> {
        debug!("cookie len wheee {}", packet.len());
        let     state      = self.shared_state.borrow_mut();
        let     our_index  = LittleEndian::read_u32(&packet[4..]);
        let     peer_ref   = state.index_map.get(&our_index).ok_or_else(|| err_msg("unknown our_index"))?.clone();
        let mut peer       = peer_ref.borrow_mut();

        peer.consume_cookie_reply(&packet)
    }

    fn handle_ingress_transport(&mut self, addr: SocketAddr, packet: &[u8]) -> Result<(), Error> {
        let mut state      = self.shared_state.borrow_mut();
        let     our_index  = LittleEndian::read_u32(&packet[4..]);
        let     peer_ref   = state.index_map.get(&our_index).ok_or_else(|| err_msg("unknown our_index"))?.clone();
        let     raw_packet = {
            let mut peer = peer_ref.borrow_mut();
            let (raw_packet, transition) = peer.handle_incoming_transport(addr, packet)?;

            if let Some(possible_dead_index) = transition {
                if let Some(index) = possible_dead_index {
                    let _ = state.index_map.remove(&index);
                }

                let outgoing: Vec<UtunPacket> = peer.outgoing_queue.drain(..).collect();

                for packet in outgoing {
                    match peer.handle_outgoing_transport(packet.payload()) {
                        Ok(message) => self.send_to_peer(message)?,
                        Err(e) => warn!("failed to encrypt packet: {}", e)
                    }
                }
            }
            raw_packet
        };

        if raw_packet.is_empty() {
            debug!("received keepalive.");
            return Ok(()) // short-circuit on keep-alives
        }

        state.router.validate_source(&raw_packet, &peer_ref)?;
        trace!("received transport packet");
        self.send_to_tunnel(raw_packet);
        Ok(())
    }

    fn send_handshake_init(&mut self, peer_ref: &SharedPeer) -> Result<u32, Error> {
        let mut state       = self.shared_state.borrow_mut();
        let mut peer        = peer_ref.borrow_mut();
        let     private_key = &state.interface_info.private_key.ok_or_else(|| err_msg("no private key!"))?;

        let (endpoint, init_packet, new_index, dead_index) = peer.initiate_new_session(private_key)?;

        let _ = state.index_map.insert(new_index, peer_ref.clone());
        if let Some(index) = dead_index {
            trace!("removing abandoned 'next' session ({}) from index map", index);
            let _ = state.index_map.remove(&index);
        }

        self.send_to_peer((endpoint, init_packet))?;
        peer.last_sent_init = Timestamp::now();
        let when = *REKEY_TIMEOUT + *TIMER_RESOLUTION * 2;
        self.timer.spawn_delayed(when,
                                 TimerMessage::Rekey(peer_ref.clone(), new_index));
        Ok(new_index)
    }

    fn handle_timer(&mut self, message: TimerMessage) -> Result<(), Error> {
        match message {
            TimerMessage::Rekey(peer_ref, our_index) => {
                {
                    let mut peer = peer_ref.borrow_mut();

                    match peer.find_session(our_index) {
                        Some((_, SessionType::Next)) => {
                            if peer.last_sent_init.elapsed() < *REKEY_TIMEOUT {
                                let wait = *REKEY_TIMEOUT - peer.last_sent_init.elapsed() + *TIMER_RESOLUTION * 2;
                                self.timer.spawn_delayed(wait,
                                                         TimerMessage::Rekey(peer_ref.clone(), our_index));
                                bail!("too soon since last init sent, waiting {:?} ({})", wait, our_index);
                            }
                            if peer.last_tun_queue.elapsed() > *REKEY_ATTEMPT_TIME {
                                peer.last_tun_queue = Timestamp::unset();
                                bail!("REKEY_ATTEMPT_TIME exceeded ({})", our_index);
                            }
                        },
                        Some((_, SessionType::Current)) => {
                            let since_last_handshake = peer.last_handshake.elapsed();
                            if since_last_handshake <= *REKEY_AFTER_TIME {
                                let wait = *REKEY_AFTER_TIME - since_last_handshake + *TIMER_RESOLUTION * 2;
                                self.timer.spawn_delayed(wait,
                                                         TimerMessage::Rekey(peer_ref.clone(), our_index));
                                bail!("recent last complete handshake - waiting {:?} ({})", wait, our_index);
                            }
                        },
                        _ => bail!("index is linked to a dead session, bailing.")
                    }
                }

                let new_index = self.send_handshake_init(&peer_ref)?;
                debug!("sent handshake init (Rekey timer) ({} -> {})", our_index, new_index);
            },
            TimerMessage::Reject(peer_ref, our_index) => {
                let mut peer  = peer_ref.borrow_mut();
                let mut state = self.shared_state.borrow_mut();

                debug!("rejection timeout for session {}, ejecting", our_index);

                match peer.find_session(our_index) {
                    Some((_, SessionType::Next))    => { peer.sessions.next = None; },
                    Some((_, SessionType::Current)) => { peer.sessions.current = None; },
                    Some((_, SessionType::Past))    => { peer.sessions.past = None; },
                    None                            => debug!("reject timeout for already-killed session")
                }
                let _ = state.index_map.remove(&our_index);
            },
            TimerMessage::PassiveKeepAlive(peer_ref, our_index) => {
                let mut peer = peer_ref.borrow_mut();
                {
                    let (session, session_type) = peer.find_session(our_index).ok_or_else(|| err_msg("missing session for timer"))?;
                    ensure!(session_type == SessionType::Current, "expired session for passive keepalive timer");

                    let since_last_recv = session.last_received.elapsed();
                    let since_last_send = session.last_sent.elapsed();
                    if since_last_recv < *KEEPALIVE_TIMEOUT {
                        let wait = *KEEPALIVE_TIMEOUT - since_last_recv + *TIMER_RESOLUTION;
                        self.timer.spawn_delayed(wait, TimerMessage::PassiveKeepAlive(peer_ref.clone(), our_index));
                        bail!("passive keepalive tick (waiting ~{}s due to last recv time)", wait.as_secs());
                    } else if since_last_send < *KEEPALIVE_TIMEOUT {
                        let wait = *KEEPALIVE_TIMEOUT - since_last_send + *TIMER_RESOLUTION;
                        self.timer.spawn_delayed(wait, TimerMessage::PassiveKeepAlive(peer_ref.clone(), our_index));
                        bail!("passive keepalive tick (waiting ~{}s due to last send time)", wait.as_secs());
                    } else if session.keepalive_sent {
                        self.timer.spawn_delayed(*KEEPALIVE_TIMEOUT, TimerMessage::PassiveKeepAlive(peer_ref.clone(), our_index));
                        bail!("passive keepalive already sent (waiting ~{}s to see if session survives)", KEEPALIVE_TIMEOUT.as_secs());
                    } else {
                        session.keepalive_sent = true;
                    }
                }

                self.send_to_peer(peer.handle_outgoing_transport(&[])?)?;
                debug!("sent passive keepalive packet ({})", our_index);

                self.timer.spawn_delayed(*KEEPALIVE_TIMEOUT, TimerMessage::PassiveKeepAlive(peer_ref.clone(), our_index));
            },
            TimerMessage::PersistentKeepAlive(peer_ref, our_index) => {
                let mut peer = peer_ref.borrow_mut();
                {
                    let (_, session_type) = peer.find_session(our_index).ok_or_else(|| err_msg("missing session for timer"))?;
                    ensure!(session_type == SessionType::Current, "expired session for persistent keepalive timer");
                }

                self.send_to_peer(peer.handle_outgoing_transport(&[])?)?;
                debug!("sent persistent keepalive packet ({})", our_index);

                if let Some(keepalive) = peer.info.keepalive {
                    self.timer.spawn_delayed(Duration::from_secs(u64::from(keepalive)),
                                             TimerMessage::PersistentKeepAlive(peer_ref.clone(), our_index));

                }
            }
        }
        Ok(())
    }

    // Just this way to avoid a double-mutable-borrow while peeking.
    fn handle_egress_packet(&mut self, packet: UtunPacket) -> Result<(), Error> {
        ensure!(!packet.payload().is_empty() && packet.payload().len() <= MAX_CONTENT_SIZE, "egress packet outside of size bounds");

        let peer_ref = self.shared_state.borrow_mut().router.route_to_peer(packet.payload())
            .ok_or_else(|| err_msg("no route to peer"))?;

        let needs_handshake = {
            let mut peer = peer_ref.borrow_mut();
            peer.queue_egress(packet);

            if peer.ready_for_transport() {
                if peer.outgoing_queue.len() > 1 {
                    debug!("sending {} queued egress packets", peer.outgoing_queue.len());
                }

                while let Some(packet) = peer.outgoing_queue.pop_front() {
                    self.send_to_peer(peer.handle_outgoing_transport(packet.payload())?)?;
                }
            }
            peer.needs_new_handshake()
        };

        if needs_handshake {
            debug!("sending handshake init because peer needs it");
            self.send_handshake_init(&peer_ref)?;
        }
        Ok(())
    }
}

impl Future for PeerServer {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // Handle config events
        loop {
            use self::config::UpdateEvent::*;
            match self.config.rx.poll() {
                Ok(Async::Ready(Some(event))) => {
                    match event {
                        PrivateKey(_) => {
                            let pub_key = &self.shared_state.borrow().interface_info.pub_key.unwrap();
                            self.cookie = cookie::Validator::new(pub_key);
                            if self.egress_tx.is_none() {
                                self.rebind().unwrap();
                            }
                        },
                        ListenPort(_) => self.rebind().unwrap(),
                        _ => {}
                    }
                },
                Ok(Async::NotReady) => break,
                Ok(Async::Ready(None)) | Err(_) => return Err(()),
            }
        }

        // Handle pending state-changing timers
        loop {
            match self.timer.poll() {
                Ok(Async::Ready(Some(message))) => {
                    let _ = self.handle_timer(message).map_err(|e| debug!("TIMER: {}", e));
                },
                Ok(Async::NotReady) => break,
                Ok(Async::Ready(None)) | Err(_) => return Err(()),
            }
        }

        // Handle UDP packets from the outside world
        if self.ingress.is_some() {
            loop {
                match self.ingress.as_mut().unwrap().poll() {
                    Ok(Async::Ready(Some((addr, packet)))) => {
                        let _ = self.handle_ingress_packet(addr, &packet).map_err(|e| warn!("UDP ERR: {:?}", e));
                    },
                    Ok(Async::NotReady) => break,
                    Ok(Async::Ready(None)) | Err(_) => return Err(()),
                }
            }
        }

        // Handle packets coming from the local tunnel
        loop {
            match self.outgoing.rx.poll() {
                Ok(Async::Ready(Some(packet))) => {
                    let _ = self.handle_egress_packet(packet).map_err(|e| warn!("UDP ERR: {:?}", e));
                },
                Ok(Async::NotReady) => break,
                Ok(Async::Ready(None)) | Err(_) => return Err(()),
            }
        }

        Ok(Async::NotReady)
    }
}
