mod l3;
mod l4;
mod packetq;
#[cfg(test)]
mod test;

use crate::l4::FlowHandle;
use crate::packetq::Dummy;
use log::trace;
use managed::ManagedMap;
use priority_queue::PriorityQueue;
use smoltcp::iface::Interface;
use smoltcp::iface::InterfaceBuilder;
use smoltcp::iface::Route;
use smoltcp::iface::Routes;
pub use smoltcp::socket::tcp::State;
pub use smoltcp::wire::IpAddress;
use smoltcp::wire::IpCidr;
pub use smoltcp::wire::{Ipv4Address, Ipv6Address};
use std::cmp::Reverse;
use std::collections::hash_map::Entry;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::time::Duration;
use std::time::Instant;

pub const TCP: usize = 6;
pub const UDP: usize = 17;

/// This is the typical "five tuple" for a flow, a tcp or udp flow
/// is identified by this combination. Flow can be an IPV4 or IPv6
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct Flow {
    pub source_ip: IpAddress,
    pub source_port: u16,
    pub dest_ip: IpAddress,
    pub dest_port: u16,
    pub protocol: usize,
}

/// These are callbacks that can be called inline from within the calls to
/// the L3L4.xyz() APIs.
pub trait Callbacks<T> {
    /// The pkt is an output l3 packet, caller can do whatever is required
    /// to "send" this packet out of whatever is their interface etc..
    fn l3_tx(&self, pkt: T);

    /// The caller can decide the storage/buffer where the L3 tx packet will
    /// be written into. The only requirement from the buffer is that it needs to
    /// be available as an &mut [u8] for writing.
    fn l3_tx_buffer(&self, size: usize) -> Option<T>;

    /// Get an &mut [u8] from the L3 Tx packet buffer
    fn l3_tx_buffer_mut<'tmp>(&self, buffer: &'tmp mut T) -> &'tmp mut [u8];

    /// New  data is available for the flow. If the data is None, that means the
    /// flow has either been fully closed/terminated. Note that we are calling out
    /// specifically that None is provided only for "fully" closed flows,
    /// a half-open situation will not be notified to the caller. Half open flows
    /// simply stop providing any more data
    fn l4_rx(&self, flow: &Flow, data: Option<&[u8]>);
}

struct Timeouts {
    udp_idle_timeout: Duration,
    tcp_idle_timeout: Duration,
    tcp_halfopen_idle_timeout: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            udp_idle_timeout: Duration::from_secs(300),
            tcp_idle_timeout: Duration::from_secs(7200),
            tcp_halfopen_idle_timeout: Duration::from_secs(7200),
        }
    }
}

pub struct L3L4Build<T> {
    mtu: usize,
    tcp_bufsize: usize,
    timeouts: Timeouts,
    _phantom: PhantomData<T>,
}

impl<T> Default for L3L4Build<T> {
    fn default() -> Self {
        Self {
            mtu: 1500,
            tcp_bufsize: 64 * 1024,
            timeouts: Default::default(),
            _phantom: PhantomData::default(),
        }
    }
}

impl<'a, T> L3L4Build<T> {
    /// The MTU, i.e max size of transmit packets to be expected in
    /// Callbacks.l3_tx(). Default is 1500
    pub fn mtu(mut self, mtu: usize) -> Self {
        self.mtu = mtu;
        self
    }

    /// Maximum TCP data in bytes buffered in the Rx AND Tx directions - the same
    /// buffer size is used for both directions. Default is 64*1024 bytes
    pub fn tcp_buffer_size(mut self, tcp_bufsize: usize) -> Self {
        self.tcp_bufsize = tcp_bufsize;
        self
    }

    /// TCP idle timeout in seconds - if there is no Rx or Tx data (excluding ACKs etc..) in
    /// this period of time, the flow will get closed. Ddefault is 7200 seconds
    pub fn tcp_idle_timeout(mut self, tcp_idle_timeout: u64) -> Self {
        self.timeouts.tcp_idle_timeout = Duration::from_secs(tcp_idle_timeout);
        self
    }

    /// If the TCP session is in a half open state - i.e either the flow is not fully open yet OR
    /// the remote end alone is closed or local end alone is closed - and there is no further Rx
    /// or Tx data excluding ACKs, in this period of time (in seconds), the flow is removed from
    /// the system. Default is 7200 seconds
    pub fn tcp_halfopen_idle_timeout(mut self, tcp_halfopen_idle_timeout: u64) -> Self {
        self.timeouts.tcp_halfopen_idle_timeout = Duration::from_secs(tcp_halfopen_idle_timeout);
        self
    }

    /// If there is no UDP RX or TX in this period of time (in seconds), the UDP flow is removed
    /// from the system
    pub fn udp_idle_timeout(mut self, udp_idle_timeout: u64) -> Self {
        self.timeouts.udp_idle_timeout = Duration::from_secs(udp_idle_timeout);
        self
    }

    /// Create an L3L4 structure with all the parameters built so far
    pub fn finalize(self) -> L3L4<'a, T> {
        L3L4::new(self.mtu, self.tcp_bufsize, self.timeouts)
    }
}

pub struct L3L4<'buf, T> {
    mtu: usize,
    tcp_bufsize: usize,
    iface: Interface<'buf>,
    flows: HashMap<Flow, FlowHandle<'buf, T>>,
    polling: PriorityQueue<Flow, Reverse<Instant>>,
    timeouts: Timeouts,
}

impl<'buf, T> L3L4<'buf, T> {
    /// Create an L3L4 kit
    /// mtu: The MTU of the interface
    /// tcp_bufsize - this is the size of the TCP rx and tx buffers.
    fn new(mtu: usize, tcp_bufsize: usize, timeouts: Timeouts) -> Self {
        // we use "any_ip" option on the interface, so the IP and routes etc..
        // dont matter, just add dummy IP and routes
        let dummy_v4 = Ipv4Address::new(1, 2, 3, 4);
        let dummy_v6 = Ipv6Address::new(1, 2, 3, 4, 5, 6, 7, 8);
        let mut routes = Routes::new(ManagedMap::Owned(BTreeMap::<IpCidr, Route>::new()));
        routes.add_default_ipv4_route(dummy_v4).ok();
        routes.add_default_ipv6_route(dummy_v6).ok();
        let ip_addrs = vec![
            IpCidr::new(IpAddress::Ipv4(dummy_v4), 24),
            IpCidr::new(IpAddress::Ipv6(dummy_v6), 64),
        ];
        // The finalize() takes a dummy device just to figure out the capabilities
        // of the device. The actual device is supplied during poll()
        let iface = InterfaceBuilder::new()
            .ip_addrs(ip_addrs)
            .routes(routes)
            .any_ip(true)
            .finalize(&mut Dummy::new(mtu));

        L3L4 {
            mtu,
            tcp_bufsize,
            iface,
            flows: HashMap::new(),
            polling: PriorityQueue::new(),
            timeouts,
        }
    }

    /// An L3 packet has been received. This API decodes the packet, creates a new
    /// flow if it is a new one, or matches against an existing one, and feeds the
    /// TCP/UDP state machine with the data if any in the packet. For a TCP packet,
    /// this API returns the associated Flow if the socket is in Established state.
    /// For UDP it returns the associated Flow for every packet. If the flow is TCP
    /// and the caller has any pending data to be transmitted, this is a good time
    /// to call l4_tx(), because an incoming TCP packet might have increased the TCP
    /// window and there might be more room to send etc.. The previous statement
    /// about calling l4_tx() applies only to TCP, in UDP there is no "pending" data,
    /// whatever is provided to l4_tx() is transmitted entirely.
    pub fn l3_rx(&mut self, callbacks: &dyn Callbacks<T>, pkt: &mut [u8]) -> Option<Flow>
    where
        T: 'buf,
    {
        if let Some(flow) = l3::parse_v4(pkt).map_or(l3::parse_v6(pkt), Some) {
            if let Entry::Vacant(e) = self.flows.entry(flow) {
                if let Some(handle) = FlowHandle::new(&flow, self.tcp_bufsize, self.mtu) {
                    e.insert(handle);
                    self.polling.push(flow, Reverse(Instant::now()));
                }
            }
            if let Some(handle) = self.flows.get_mut(&flow) {
                let poll_at = handle.poll(&mut self.iface, Instant::now(), Some(pkt), callbacks);
                // If the socket was established and this new packet made it closed, remove the flow
                // If the socket is created new (listening) and this packet did not move the state to
                // something else, that means this is some stray tcp packet, remove the flow
                if !handle.is_open() || handle.is_listening() {
                    self.l4_remove(&flow, Some(callbacks));
                    return None;
                }
                // An Rx packet can change TCP states etc.., check if that needs a faster polling
                if let Some(poll_at) = poll_at {
                    if poll_at < handle.poll_at {
                        handle.poll_at = poll_at;
                        self.polling.change_priority(&flow, Reverse(poll_at));
                    }
                }
                if handle.is_established() {
                    return Some(flow);
                }
            }
        }
        None
    }

    /// Write data. For Tcp, all the bytes might not be written, the bytes written
    /// is the return value. For Udp all the bytes will be written. If all the bytes
    /// were not written, the caller should retry the remaining after the next Rx
    /// packet arrives for that flow, the logic being that a next Rx packet for tcp
    /// might ACK more bytes and make room to transmit more data.
    /// For TCP, a return value of None indicates that either the flow has been
    /// fully closed/terminated. Note that a "half-closed" scenario will not result
    /// in a None output. If the caller has half closed the flow by calling l4_close()
    /// and still attempts to send data after that, the return value will be Option<size>
    /// with the size of the data provided. For UDP the write will always succeed,
    /// i.e. it will never return None
    pub fn l4_tx(&mut self, callbacks: &dyn Callbacks<T>, flow: &Flow, data: &[u8]) -> Option<usize>
    where
        T: 'buf,
    {
        if let Some(handle) = self.flows.get_mut(flow) {
            let ret = handle.write(data).ok();
            handle.poll(&mut self.iface, Instant::now(), None, callbacks);
            if !handle.is_open() {
                self.l4_remove(flow, None);
                return None;
            }
            ret
        } else {
            None
        }
    }

    /// The flows may need to perform periodic tasks like handling ageing/timeouts
    /// or handling tcp state machine retransmits etc.. The caller has to call this
    /// periodically. Each invocation of this API returns an Instant in future when
    /// this API has to be called again. Not calling this API at the time mentioned by
    /// the returned Instant can result in lower TCP performance for example.
    /// This API can return None if there are no flows in the system as of now. The next
    /// call to l3_rx() that returns a Flow tells the caller that there is at least
    /// one flow at that point, and the calls to l4_poll() has to start/re-start from
    /// that point onwards
    pub fn l4_poll(&mut self, callbacks: &dyn Callbacks<T>) -> Option<Instant>
    where
        T: 'buf,
    {
        let now = Instant::now();
        loop {
            let flow = if let Some((flow, _)) = self.polling.peek() {
                *flow
            } else {
                return None;
            };
            if let Some(handle) = self.flows.get_mut(&flow) {
                // If the earliest polling time is in future, then we are done
                // polling all the flows, return the future time hoping the caller
                // calls this again at that point in time
                if handle.poll_at > now {
                    trace!("polling return {:?} {:?}", flow, handle.poll_at - now);
                    return Some(handle.poll_at);
                }
                let poll_at = handle.poll(&mut self.iface, now, None, callbacks);
                let idle_remove = handle.check_idle(&self.timeouts);
                trace!(
                    "polling flow {:?} elapsed {:?} idle {}, open {}",
                    flow,
                    now - handle.poll_at,
                    idle_remove,
                    handle.is_open()
                );
                if !handle.is_open() || idle_remove {
                    self.l4_remove(&flow, Some(callbacks));
                    continue;
                }
                let idle_check = now + handle.idle_timeout(&self.timeouts);
                let poll_at = poll_at.map_or(idle_check, |t| std::cmp::min(t, idle_check));
                handle.poll_at = poll_at;
                self.polling.change_priority(&flow, Reverse(poll_at));
            } else {
                panic!(
                    "Polling exists for flow {:?} but handle does not exist",
                    flow
                );
            }
        }
    }

    /// Close the writer side flow. For tcp, this will initiate the FIN/FIN-ACK sequence.
    /// For tcp, the flow will hang on in a half-open/half-closed state till
    /// the other end also closes the connection OR the half-open timers kick in and
    /// remove the flow
    pub fn l4_local_close(&mut self, flow: &Flow) {
        if let Some(handle) = self.flows.get_mut(flow) {
            handle.close();
            if flow.protocol == UDP {
                self.l4_remove(flow, None);
            }
        }
    }

    /// This returns true if the TCP socket is completely closed on both remote and local ends.
    /// For UDP socket this will always return a false. If the flow does not exist, this
    /// API will return true.
    pub fn l4_is_local_and_remote_closed(&self, flow: &Flow) -> bool {
        if let Some(handle) = self.flows.get(flow) {
            !handle.is_open()
        } else {
            true
        }
    }

    /// Check if the remote end of the flow has closed the connection. This matters
    /// only to TCP where TCP can be in half-open state. If the flow is not found,
    /// the API returns true
    pub fn l4_is_remote_closed(&self, flow: &Flow) -> bool {
        if let Some(handle) = self.flows.get(flow) {
            handle.read_closed()
        } else {
            true
        }
    }

    /// This API gets the state of a TCP socket. If the flow is not found,
    /// it returns state as None. If invoked for UDP socket it returns
    /// state as Some(Established)
    pub fn l4_state(&self, flow: &Flow) -> Option<State> {
        self.flows.get(flow).map(|handle| handle.tcp_state())
    }

    fn l4_remove(&mut self, flow: &Flow, callbacks: Option<&dyn Callbacks<T>>) {
        self.flows.remove(flow);
        self.polling.remove(flow);
        if let Some(callbacks) = callbacks {
            callbacks.l4_rx(flow, None);
        }
    }
}
