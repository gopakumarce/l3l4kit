extern crate std;
use core::cell::RefCell;
use futures::SinkExt;
use futures::StreamExt;
use l3l4kit::L3L4Build;
use l3l4kit::{Callbacks, Flow, L3L4};
use log::trace;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Write;
use std::time::Duration;
use std::time::Instant;
use std::vec;
use std::vec::Vec;
use tokio::time::sleep;
use tokio_util::codec::Framed;
use tun::AsyncDevice;
use tun::TunPacket;
use tun::TunPacketCodec;

// RUN with RUST_LOG=trace cargo run --example echo_server for full logs, including from smoltcp!

// Note1: the .cargo/config ensures that the cargo test runs as sudo
// sudo is required to create the tun interface

// Note2: We dont really have to use tokio/async for this example. If we could set the
// tun::Device interface read in new_pkt_or_poll() with some timeout, then we dont need async

// Once the example is run as above, it will create a tun interface with an ip address 1.2.3.4/24.
// So any IP 1.2.3.X will be routed via the tunnel to this echo server. If you have netcat
// installed, you can open a terminal and say "nc 1.2.3.5 22" for example, and whatever you type
// will be echoed back - demonstrating tcp. If you want to try udp, you can say "nc 1.2.3.5 22 -u"
// and again whatever is typed will be echoed back.

const WAIT_FOREVER: Duration = Duration::from_secs(24 * 60 * 60);
const MTU: usize = 32 * 1024;

type MyL3L4<'a> = L3L4<'a, Vec<u8>>;

struct TestCallbacksInner {
    device: Framed<AsyncDevice, TunPacketCodec>,
    all_flows: HashMap<Flow, VecDeque<Vec<u8>>>,
    pkts: Vec<Vec<u8>>,
}

// The l3l4 callbacks all take an &self, and hence using a RefCell to get mutability.
struct TestCallbacks {
    inner: RefCell<TestCallbacksInner>,
}

impl TestCallbacks {
    fn remove_flow(&self, flow: &Flow) {
        let mut inner = self.inner.borrow_mut();
        inner.all_flows.remove(flow);
    }

    fn retry_data(&self, flow: &Flow, data: Vec<u8>) {
        let mut inner = self.inner.borrow_mut();
        if let Some(q) = inner.all_flows.get_mut(flow) {
            q.push_back(data);
        }
    }

    fn pop_data(&self, flow: &Flow) -> Option<Vec<u8>> {
        let mut inner = self.inner.borrow_mut();
        if let Some(q) = inner.all_flows.get_mut(flow) {
            q.pop_front()
        } else {
            None
        }
    }

    async fn transmit_pkts(&self) {
        let mut inner = self.inner.borrow_mut();
        while let Some(pkt) = inner.pkts.pop() {
            trace!("pkt tx size {}", pkt.len());
            if inner.device.send(TunPacket::new(pkt)).await.is_err() {
                trace!("pkt tx fail");
            }
        }
    }

    async fn receive_pkt(&self) -> Option<TunPacket> {
        let mut inner = self.inner.borrow_mut();
        inner.device.next().await?.ok()
    }
}

impl Callbacks<Vec<u8>> for TestCallbacks {
    // queue up packets to be transmitted, we will transmit them in transmit_pkts()
    fn l3_tx(&self, pkt: Vec<u8>) {
        trace!("Queue pkt to trasmit, length {}", pkt.len());
        let mut inner = self.inner.borrow_mut();
        inner.pkts.push(pkt);
    }

    // Just use Vec for buffer data. In a real use case with say a DPDK based interface,
    // this can be a dpdk buffer or some area of packet memory in the device hardware
    fn l3_tx_buffer(&self, size: usize) -> Option<Vec<u8>> {
        Some(vec![0u8; size])
    }

    fn l3_tx_buffer_mut<'tmp>(&self, buffer: &'tmp mut Vec<u8>) -> &'tmp mut [u8] {
        &mut buffer[..]
    }

    // queue up received data, we will echo them back in transmit_data()
    fn l4_rx(&self, flow: &Flow, data: Option<&[u8]>) {
        let mut inner = self.inner.borrow_mut();
        if let Some(data) = data {
            trace!("Queue data received, flow {:?} length {}", flow, data.len());
            if !inner.all_flows.contains_key(flow) {
                inner.all_flows.insert(*flow, VecDeque::new());
            }
            if let Some(q) = inner.all_flows.get_mut(flow) {
                q.push_back(Vec::from(data));
            }
        } else {
            trace!("l4_4x: Flow closed {:?}", flow);
            // Tempting to call self.remove_flow(flow) here instead, but remember
            // RefCell cant be mut-borrowed multiple times
            inner.all_flows.remove(flow);
        }
    }
}

// The received tcp/udp data are queued up, echoe them by writing it back
// on the same flow it was received on
async fn transmit_data<'a>(l3l4: &mut MyL3L4<'a>, callbacks: &TestCallbacks, flow: &Flow) {
    loop {
        if let Some(mut data) = callbacks.pop_data(flow) {
            trace!("Loopback data of len {}", data.len());
            match l3l4.l4_tx(callbacks, &flow, &data[..]) {
                Some(size) => {
                    if size != data.len() {
                        trace!("Partial data written {} / {}", size, data.len());
                        // All the data could not be written, queue it back and try later
                        callbacks.retry_data(flow, data.drain(0..size).collect());
                    }
                }
                None => {
                    trace!("l4_tx: Flow closed {:?}", flow);
                    callbacks.remove_flow(&flow);
                    return;
                }
            }
        } else {
            break;
        }
    }
}

fn create_tun() -> AsyncDevice {
    let mut config = tun::Configuration::default();
    config
        .address((1, 2, 3, 4))
        .mtu(MTU as i32)
        .netmask((255, 255, 255, 0))
        .up();
    #[cfg(target_os = "linux")]
    {
        config.platform(|config| {
            config.packet_information(false);
        });
        tun::create_as_async(&config).unwrap()
    }
    #[cfg(target_os = "macos")]
    {
        config.name("utun100");

        let dev = tun::create_as_async(&config).unwrap();
        #[cfg(target_os = "macos")]
        let err = std::process::Command::new("route")
            .arg("add")
            .arg("-host")
            .arg("1.2.3.5")
            .arg("-interface")
            .arg("utun99")
            .spawn();
        trace!("Route add result {:?}", err);
        dev
    }
}

fn init_logging() {
    env_logger::Builder::new()
        .format(move |buf, record| {
            if !record.target().starts_with("tokio") {
                writeln!(buf, "{}:{}", record.target(), record.args())
            } else {
                Ok(())
            }
        })
        .parse_env("RUST_LOG")
        .try_init()
        .ok();
}

// This API either gets a new packet or does periodic house-keeping work
// thats needed, by calling the l4_poll() API.
async fn new_pkt_or_poll<'a>(
    l3l4: &mut MyL3L4<'a>,
    callbacks: &TestCallbacks,
) -> Option<TunPacket> {
    // If wakeup is None, that means just wakeup on the next Rx packet,  we
    // just sleep here for an arbitrarily large period of time
    let wakeup = match l3l4.l4_poll(callbacks) {
        Some(wakeup) => wakeup,
        None => Instant::now() + WAIT_FOREVER,
    };
    trace!("Wakeup after {:?} ", wakeup - Instant::now());
    tokio::select! {
        p = callbacks.receive_pkt() => {
            return p;
        }
        _ = sleep(wakeup - Instant::now()) => {
            return None;
        }
    }
}

async fn echo_server<'a>(l3l4: &mut MyL3L4<'a>, callbacks: &mut TestCallbacks) {
    loop {
        let pkt = new_pkt_or_poll(l3l4, callbacks).await;
        if let Some(pkt) = pkt {
            trace!("Got Rx pkt of len {}", pkt.get_bytes().len());
            // Tun returns a Bytes, and we need a mutable buffer to be passed
            // to l3_rx, and hence having to do this copy
            let mut buf = Vec::new();
            buf.extend_from_slice(&pkt.into_bytes());
            if let Some(flow) = l3l4.l3_rx(callbacks, &mut buf[..]) {
                // In this example we just echo back the data, so if we go an l3
                // pkt, there is chance we have data to transmit. In more realistic
                // use cases, transmit_data() can be called any time, and if that
                // ends up with not all the data being transmitted, THEN we need
                // to attempt re-transmitting this after we get an Rx packet for that
                // flow - reason being that an Rx TCP packet can come with ACKs that
                // allow more data to be transmitted
                transmit_data(l3l4, callbacks, &flow).await;
            }
        }
        // Note that transmit_data can generate l3_tx pkts, so transmit_pkts()
        // has to be the last thing attempted so that all packets are sent out
        // before we wait in new_pkt_or_poll()
        callbacks.transmit_pkts().await;
    }
}

#[tokio::main]
async fn main() {
    init_logging();
    let device = create_tun();
    let stream = device.into_framed();
    // The idle timeouts can be left to their defaults, its configured here
    // just as an example
    let mut l3l4 = L3L4Build::default()
        .mtu(MTU)
        .tcp_buffer_size(64 * 1024)
        .tcp_idle_timeout(300)
        .tcp_halfopen_idle_timeout(30)
        .udp_idle_timeout(30)
        .finalize();
    let mut callbacks = TestCallbacks {
        inner: RefCell::new(TestCallbacksInner {
            device: stream,
            all_flows: HashMap::new(),
            pkts: Vec::new(),
        }),
    };
    echo_server(&mut l3l4, &mut callbacks).await;
}
