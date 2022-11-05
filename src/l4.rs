use crate::packetq::PacketQ;
use crate::Callbacks;
use crate::Flow;
use crate::Timeouts;
use log::trace;
use smoltcp::iface::Interface;
use smoltcp::iface::SocketSet;
use smoltcp::socket::tcp;
use smoltcp::socket::tcp::State;
use smoltcp::socket::udp::{self, PacketMetadata};
use smoltcp::socket::Socket;
pub use smoltcp::wire::IpAddress;
use smoltcp::wire::{IpEndpoint, IpListenEndpoint};
use std::marker::PhantomData;
use std::time::Duration;
use std::time::Instant;

const TCP_MAX_SEGMENT_LIFE: Duration = Duration::from_secs(2 * 60);
// The time we wait for the initial three way handshake to complete, this might
// need to be configurable at some point
const TCP_ESTABLISH_WAIT: Duration = Duration::from_secs(15);

pub(crate) struct FlowHandle<'buf, T> {
    pub(crate) key: Flow,
    pub(crate) poll_at: Instant,
    pub(crate) socket: Option<Socket<'buf>>,
    pub(crate) last_rxtx: Instant,
    endpoint: Option<IpEndpoint>,
    mtu: usize,
    _phantom: PhantomData<T>,
}

impl<'buf, T> FlowHandle<'buf, T> {
    pub(crate) fn new(key: &Flow, tcp_bufsize: usize, mtu: usize) -> Option<Self> {
        let socket = if key.protocol == crate::TCP {
            let rxb = vec![0u8; tcp_bufsize];
            let txb = vec![0u8; tcp_bufsize];
            let rx = tcp::SocketBuffer::new(rxb);
            let tx = tcp::SocketBuffer::new(txb);
            let mut socket = tcp::Socket::new(rx, tx);
            socket.listen(key.dest_port).ok()?;
            Some(Socket::Tcp(socket))
        } else {
            // Smoltcp udp packet ring seems to work only if its at least 2*mtu
            let rxb = vec![0u8; 2 * mtu];
            let txb = vec![0u8; 2 * mtu];
            let rxm: Vec<PacketMetadata> = vec![PacketMetadata::EMPTY; 1];
            let txm: Vec<PacketMetadata> = vec![PacketMetadata::EMPTY; 1];
            let rx = udp::PacketBuffer::new(rxm, rxb);
            let tx = udp::PacketBuffer::new(txm, txb);
            let mut socket = udp::Socket::new(rx, tx);
            socket
                .bind(IpListenEndpoint {
                    addr: Some(key.dest_ip),
                    port: key.dest_port,
                })
                .ok()?;
            Some(Socket::Udp(socket))
        };
        Some(Self {
            key: *key,
            poll_at: Instant::now(),
            socket,
            last_rxtx: Instant::now(),
            endpoint: None,
            mtu,
            _phantom: PhantomData::default(),
        })
    }

    pub(crate) fn close(&mut self) {
        if let Socket::Tcp(sock) = self.socket.as_mut().unwrap() {
            sock.close();
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        if let Socket::Tcp(sock) = self.socket.as_ref().unwrap() {
            sock.is_open()
        } else {
            true
        }
    }

    pub(crate) fn is_listening(&self) -> bool {
        if let Socket::Tcp(sock) = self.socket.as_ref().unwrap() {
            sock.state() == State::Listen
        } else {
            false
        }
    }

    pub(crate) fn is_established(&self) -> bool {
        if let Socket::Tcp(sock) = self.socket.as_ref().unwrap() {
            sock.state() == State::Established
        } else {
            true
        }
    }

    pub(crate) fn read(&mut self, callbacks: &dyn Callbacks<T>) {
        let key = self.key;
        match self.socket.as_mut().unwrap() {
            Socket::Udp(sock) => match sock.recv() {
                Ok((data, endpoint)) => {
                    if self.endpoint.is_none() {
                        trace!("UDP flow {:?}, endpoint {:?}", self.key, endpoint);
                        self.endpoint = Some(endpoint);
                    }
                    self.last_rxtx = Instant::now();
                    callbacks.l4_rx(&key, Some(data));
                }
                Err(udp::RecvError::Exhausted) => {}
            },
            Socket::Tcp(sock) => {
                if sock.can_recv() {
                    let op = |x: &mut [u8]| -> (usize, usize) {
                        let len = if !x.is_empty() {
                            callbacks.l4_rx(&key, Some(x));
                            x.len()
                        } else {
                            0
                        };
                        (len, len)
                    };
                    let recv = sock.recv(op);
                    match recv {
                        Ok(_len) => self.last_rxtx = Instant::now(),
                        Err(tcp::RecvError::InvalidState) => {}
                        Err(tcp::RecvError::Finished) => {}
                    }
                }
            }
        }
    }

    // Write returns the BytesMut that is unwritten if any, if all the supplied
    // bytes were written succesfully, None is returned. Any errors returned are
    // fatal and qualify for closing/discarding the socket
    pub(crate) fn write(&mut self, data: &[u8]) -> Result<usize, smoltcp::Error> {
        match self.socket.as_mut().unwrap() {
            Socket::Udp(sock) => {
                if let Some(endpoint) = self.endpoint.as_ref() {
                    match sock.send_slice(data, *endpoint) {
                        Err(e) => match e {
                            udp::SendError::BufferFull => Ok(0),
                            udp::SendError::Unaddressable => Err(smoltcp::Error::Unaddressable),
                        },
                        Ok(()) => {
                            self.last_rxtx = Instant::now();
                            Ok(data.len())
                        }
                    }
                } else {
                    Err(smoltcp::Error::Unaddressable)
                }
            }
            Socket::Tcp(sock) => {
                if sock.can_send() {
                    match sock.send_slice(data) {
                        Err(tcp::SendError::InvalidState) => Err(smoltcp::Error::Illegal),
                        Ok(size) => {
                            if size > 0 {
                                self.last_rxtx = Instant::now();
                            }
                            Ok(size)
                        }
                    }
                } else {
                    Ok(0)
                }
            }
        }
    }

    pub(crate) fn poll(
        &mut self,
        iface: &mut Interface,
        time: Instant,
        rx: Option<&mut [u8]>,
        callbacks: &dyn Callbacks<T>,
    ) -> Option<Instant>
    where
        T: 'buf,
    {
        let has_rx = rx.is_some();
        trace!("poll called, has_rx {}", has_rx);
        let mut pktq = PacketQ::new(self.mtu, rx, callbacks);
        let mut sockets = SocketSet::new(vec![]);
        let handle = match self.socket.take().unwrap() {
            Socket::Udp(sock) => sockets.add(sock),
            Socket::Tcp(sock) => sockets.add(sock),
        };
        let smol_time: smoltcp::time::Instant = time.into();
        let mut poll_at = Some(smol_time);
        let ret = iface.poll(smol_time, &mut pktq, &mut sockets);
        if let Ok(changed) = ret {
            if !changed {
                poll_at = iface.poll_at(smol_time, &sockets);
            }
        } else {
            poll_at = iface.poll_at(smol_time, &sockets);
        }
        self.socket = Some(sockets.remove(handle));
        // Since we got an rx packet, see if there is any of that has been
        // terminated to tcp/udp and is available to the caller
        if has_rx {
            self.read(callbacks);
        }
        if let Some(poll_at) = poll_at {
            // smoltcp returns zero for "right now", not sure why they do that, they
            // could have just return it relative to the supplied time
            if poll_at == smoltcp::time::Instant::from_millis(0) {
                Some(time)
            } else {
                let elapsed = poll_at - smol_time;
                Some(time + elapsed.into())
            }
        } else {
            None
        }
    }

    pub(crate) fn read_closed(&self) -> bool {
        match &self.socket {
            Some(Socket::Tcp(sock)) => sock.state() == smoltcp::socket::tcp::State::CloseWait,
            _ => false,
        }
    }

    pub(crate) fn tcp_state(&self) -> smoltcp::socket::tcp::State {
        match &self.socket {
            Some(Socket::Tcp(sock)) => sock.state(),
            s => {
                trace!("Unexpected state call on socket {:?}", s);
                smoltcp::socket::tcp::State::Established
            }
        }
    }

    pub(crate) fn idle_timeout(&self, timeouts: &Timeouts) -> Duration {
        match &self.socket {
            Some(Socket::Tcp(sock)) => match sock.state() {
                State::Established => timeouts.tcp_idle_timeout,
                State::TimeWait => TCP_MAX_SEGMENT_LIFE,
                State::Listen | State::SynReceived | State::SynSent | State::Closed => {
                    // Well, closed doesnt belong here, but a closed socket is taken out
                    // immediately anyways, so its timeout doesnt matter
                    TCP_ESTABLISH_WAIT
                }
                State::CloseWait
                | State::FinWait1
                | State::FinWait2
                | State::LastAck
                | State::Closing => timeouts.tcp_halfopen_idle_timeout,
            },
            Some(Socket::Udp(_)) => timeouts.udp_idle_timeout,
            _ => timeouts.udp_idle_timeout,
        }
    }

    pub(crate) fn check_idle(&mut self, timeouts: &Timeouts) -> bool {
        match &mut self.socket {
            Some(Socket::Tcp(sock)) => {
                trace!(
                    "Tcp check idle called {:?} in state {:?}",
                    self.key,
                    sock.state()
                );
                if sock.state() == State::Established {
                    if self.last_rxtx.elapsed() > timeouts.tcp_idle_timeout {
                        sock.close();
                    }
                } else if self.last_rxtx.elapsed() > timeouts.tcp_halfopen_idle_timeout {
                    sock.abort();
                }
            }
            Some(Socket::Udp(_)) => {
                trace!("Udp check idle called {:?}", self.key);
                if self.last_rxtx.elapsed() > timeouts.udp_idle_timeout {
                    return true;
                }
            }
            _ => {}
        }
        false
    }
}
