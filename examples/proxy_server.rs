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
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::ReadHalf;
use tokio::io::WriteHalf;
use tokio::net::TcpSocket;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_util::codec::Framed;
use tun::AsyncDevice;
use tun::TunPacket;
use tun::TunPacketCodec;

// RUN with RUST_LOG=trace cargo run --example proxy_server for full logs, including from smoltcp!

// Note1: the .cargo/config ensures that the cargo test runs as sudo
// sudo is required to create the tun interface

// Note2: We dont really have to use tokio/async for this example. If we could set the
// tun::Device interface read in new_pkt_or_external_data_or_poll() with some timeout, then we dont need async

// Once the example is run as above, it will create a tun interface with an ip address 1.2.3.4/24.
// So any IP 1.2.3.X will be routed via the tunnel to this proxy server. This example proxies
// anything that comes in and assumes its an http request to http://neverssl.com. So if you do
// a curl htpp://1.2.3.5 or open http://1.2.3.5 in a broswer, you will get the same result as
// curl http://neverssl.com or opening http://neverssl.com in a browser

const WAIT_FOREVER: Duration = Duration::from_secs(24 * 60 * 60);
const MTU: usize = 32 * 1024;

type MyL3L4<'a> = L3L4<'a, Vec<u8>>;

struct ExternalMsg {
    flow: Flow,
    data: Option<Vec<u8>>,
}

struct ExternalSock {
    writer: WriteHalf<TcpStream>,
    shutdown: mpsc::Sender<()>,
}
struct TestCallbacksInner {
    device: Framed<AsyncDevice, TunPacketCodec>,
    all_flows: HashMap<Flow, VecDeque<Vec<u8>>>,
    pkts: Vec<Vec<u8>>,
    pending_remove: Option<Vec<Flow>>,
}

// The l3l4 callbacks all take an &self, and hence using a RefCell to get mutability.
struct TestCallbacks {
    inner: RefCell<TestCallbacksInner>,
    all_external: HashMap<Flow, ExternalSock>,
    external_remote: mpsc::Sender<ExternalMsg>,
    external_pending: HashMap<Flow, VecDeque<Vec<u8>>>,
}

impl TestCallbacks {
    async fn remove_flow(&mut self, flow: &Flow) {
        let mut inner = self.inner.borrow_mut();
        inner.all_flows.remove(flow);
        if let Some(external) = self.all_external.get_mut(flow) {
            external.shutdown.send(()).await.ok();
        }
        self.all_external.remove(flow);
        self.external_pending.remove(flow);
    }

    fn get_pending_remove(&self) -> Vec<Flow> {
        let mut inner = self.inner.borrow_mut();
        let pending = inner.pending_remove.take().unwrap();
        inner.pending_remove = Some(vec![]);
        pending
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

    async fn data_to_external(&mut self, flow: &Flow, data: Vec<u8>) -> bool {
        let inner = self.inner.borrow();
        if inner.all_flows.contains_key(flow) && !self.all_external.contains_key(flow) {
            if let Ok(socket) = TcpSocket::new_v4() {
                if let Ok(mut host) = tokio::net::lookup_host("neverssl.com:80").await {
                    if let Some(host) = host.next() {
                        if let Ok(external) = socket.connect(host).await {
                            let (reader, writer) = tokio::io::split(external);
                            let (shutdown_send, shutdown_recv) = mpsc::channel(1);
                            let external = ExternalSock {
                                writer,
                                shutdown: shutdown_send,
                            };
                            self.all_external.insert(*flow, external);
                            tokio::spawn(external_read(
                                *flow,
                                shutdown_recv,
                                self.external_remote.clone(),
                                reader,
                            ));
                        }
                    }
                }
            }
        }
        if let Some(external) = self.all_external.get_mut(flow) {
            if external.writer.write_all(&data[..]).await.is_ok() {
                return true;
            } else {
                external.shutdown.send(()).await.ok();
            }
        } else {
            trace!("External Socket not found for flow {:?}", flow);
        }
        false
    }

    fn queue_external_data(&mut self, flow: &Flow, data: Vec<u8>) {
        let inner = self.inner.borrow();
        if inner.all_flows.contains_key(flow) && !self.external_pending.contains_key(flow) {
            self.external_pending.insert(*flow, VecDeque::new());
        }
        if let Some(vecd) = self.external_pending.get_mut(flow) {
            vecd.push_back(data);
        }
    }

    fn queue_retry_external_data(&mut self, flow: &Flow, data: Vec<u8>) {
        if let Some(vecd) = self.external_pending.get_mut(flow) {
            vecd.push_front(data);
        }
    }

    fn get_external_data(&mut self, flow: &Flow) -> Option<Vec<u8>> {
        if let Some(vecd) = self.external_pending.get_mut(flow) {
            vecd.pop_front()
        } else {
            None
        }
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

    // queue up received data, we will send them to external website in transmit_data_to_external()
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
            // We cannot do any async stuff here, so we queue up the flow to be closed
            inner.pending_remove.as_mut().unwrap().push(*flow);
        }
    }
}

async fn external_read_sock(
    flow: Flow,
    send: mpsc::Sender<ExternalMsg>,
    mut reader: ReadHalf<TcpStream>,
) {
    let mut buf = [0u8; 4 * 1024];
    loop {
        if let Ok(n) = reader.read(&mut buf).await {
            if n == 0 {
                trace!("Zero read, socket is closed {:?}", flow);
                send.send(ExternalMsg { flow, data: None }).await.ok();
                break;
            }
            let data = buf[0..n].to_owned();
            if send
                .send(ExternalMsg {
                    flow,
                    data: Some(data),
                })
                .await
                .is_err()
            {
                trace!("send external data to flow failed {:?}", flow);
                break;
            } else {
                trace!("flow got external data of size {}", n);
            }
        } else {
            trace!("external socket closed {:?}", flow);
            send.send(ExternalMsg { flow, data: None }).await.ok();
            break;
        }
    }
}

async fn external_read(
    flow: Flow,
    mut shutdown: mpsc::Receiver<()>,
    send: mpsc::Sender<ExternalMsg>,
    reader: ReadHalf<TcpStream>,
) {
    tokio::select! {
        _ = external_read_sock(flow, send, reader) => {
        },
        _ = shutdown.recv() => {
            trace!("Flow external read shutdown {:?}", flow);
        }
    }
}

async fn write_external_data_to_flow<'a>(
    l3l4: &mut MyL3L4<'a>,
    callbacks: &mut TestCallbacks,
    flow: &Flow,
) -> bool {
    if let Some(mut data) = callbacks.get_external_data(flow) {
        match l3l4.l4_tx(callbacks, flow, &data[..]) {
            Some(size) => {
                if size != data.len() {
                    trace!("Partial data written {} / {}", size, data.len());
                    // All the data could not be written, queue it back and try later
                    callbacks.queue_retry_external_data(flow, data.drain(0..size).collect());
                    return false;
                }
            }
            None => {
                trace!("l4_tx: Flow closed {:?}", flow);
                callbacks.remove_flow(flow).await;
                return false;
            }
        }
        return true;
    }
    false
}

async fn write_pending_external_data_to_flow<'a>(
    l3l4: &mut MyL3L4<'a>,
    callbacks: &mut TestCallbacks,
    flow: &Flow,
) {
    while write_external_data_to_flow(l3l4, callbacks, flow).await {}
}

async fn process_external_data<'a>(
    l3l4: &mut MyL3L4<'a>,
    callbacks: &mut TestCallbacks,
    mut msg: ExternalMsg,
    external_local: &mut mpsc::Receiver<ExternalMsg>,
) {
    loop {
        if msg.data.is_none() {
            l3l4.l4_local_close(&msg.flow);
        } else {
            // There might be other data already pending for this flow, so just queue
            // stuff behind it
            callbacks.queue_external_data(&msg.flow, msg.data.take().unwrap());
        }
        write_external_data_to_flow(l3l4, callbacks, &msg.flow).await;
        // While at it, try if we can process all the external data without blocking
        if let Ok(external) = external_local.try_recv() {
            msg = external;
        } else {
            break;
        }
    }
}

// The received tcp/udp data are queued up, send them to the external website
async fn transmit_data_to_external<'a>(
    l3l4: &mut MyL3L4<'a>,
    callbacks: &mut TestCallbacks,
    flow: &Flow,
) {
    while let Some(data) = callbacks.pop_data(flow) {
        trace!("Data to external of len {}", data.len());
        if !callbacks.data_to_external(&flow, data).await {
            trace!("external_tx: Flow closed {:?}", flow);
            l3l4.l4_local_close(flow);
            return;
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

// This API either gets a new l3 packet from tun interface or data from external website
// or does periodic house-keeping work thats needed, by calling the l4_poll() API.
async fn new_pkt_or_external_data_or_poll<'a>(
    l3l4: &mut MyL3L4<'a>,
    callbacks: &mut TestCallbacks,
    external_local: &mut mpsc::Receiver<ExternalMsg>,
) -> (Option<TunPacket>, Option<ExternalMsg>) {
    // If wakeup is None, that means just wakeup on the next Rx packet,  we
    // just sleep here for an arbitrarily large period of time
    let wakeup = match l3l4.l4_poll(callbacks) {
        Some(wakeup) => wakeup,
        None => Instant::now() + WAIT_FOREVER,
    };
    trace!("Wakeup after {:?} ", wakeup - Instant::now());
    tokio::select! {
        p = callbacks.receive_pkt() => {
            trace!("Got pkt");
            return (p, None);
        }
        _ = sleep(wakeup - Instant::now()) => {
            trace!("Got wakeup");
            return (None, None);
        }
        external_data = external_local.recv() => {
            if let Some(ext) = &external_data {
                if let Some(data) = &ext.data {
                    trace!("Got external data {}", data.len());
                }
            }
            return (None, external_data);
        }
    }
}

async fn proxy_server<'a>(
    l3l4: &mut MyL3L4<'a>,
    callbacks: &mut TestCallbacks,
    mut external_local: mpsc::Receiver<ExternalMsg>,
) {
    loop {
        let (pkt, external_data) =
            new_pkt_or_external_data_or_poll(l3l4, callbacks, &mut external_local).await;
        if let Some(pkt) = pkt {
            trace!("Got Rx pkt of len {}", pkt.get_bytes().len());
            // Tun returns a Bytes, and we need a mutable buffer to be passed
            // to l3_rx, and hence having to do this copy
            let mut buf = Vec::new();
            buf.extend_from_slice(&pkt.into_bytes());
            if let Some(flow) = l3l4.l3_rx(callbacks, &mut buf[..]) {
                // We might have got some new l4 data in the rx packet, try to send it out
                transmit_data_to_external(l3l4, callbacks, &flow).await;
                // The rx packet might also have acks which enables us to send some external
                // data back to the flow/socket
                write_pending_external_data_to_flow(l3l4, callbacks, &flow).await;
            }
        }
        if let Some(external_data) = external_data {
            process_external_data(l3l4, callbacks, external_data, &mut external_local).await;
        }
        // Note that transmit_data_to_external and process_external_data can generate l3_tx pkts,
        // so transmit_pkts() has to be the last thing attempted so that all packets
        // are sent out before we wait in new_pkt_or_external_data_or_poll()
        callbacks.transmit_pkts().await;
        // Remove any closed flows
        let mut pending = callbacks.get_pending_remove();
        while let Some(flow) = pending.pop() {
            callbacks.remove_flow(&flow).await;
        }
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
    let (external_remote, external_local) = mpsc::channel(10);
    let mut callbacks = TestCallbacks {
        inner: RefCell::new(TestCallbacksInner {
            device: stream,
            all_flows: HashMap::new(),
            pkts: Vec::new(),
            pending_remove: Some(vec![]),
        }),
        all_external: HashMap::new(),
        external_remote,
        external_pending: HashMap::new(),
    };
    proxy_server(&mut l3l4, &mut callbacks, external_local).await;
}
