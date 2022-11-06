extern crate std;
use crate::L3L4Build;
use crate::{Callbacks, Flow, L3L4};
use core::cell::RefCell;
use futures::SinkExt;
use futures::StreamExt;
use log::trace;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::AtomicU16;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::vec;
use std::vec::Vec;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::net::UdpSocket;
use tokio::time::sleep;
use tokio_util::codec::Framed;
use tun::AsyncDevice;
use tun::TunPacket;
use tun::TunPacketCodec;

// NOTE: The tests dont work yet on macos, works on linux only at the moment

// RUN with RUST_LOG=trace cargo test for full logs, including from smoltcp!

// Note1: the .cargo/config ensures that the cargo test runs as sudo
// sudo is required to create the tun interface

// Note2: We dont really have to use tokio/async for this test. If we could set the
// tun::Device interface read in new_pkt_or_poll() with some timeout,
// then we dont need async

const WAIT_FOR_ONESEC: Duration = Duration::from_secs(1);
const TEST_PORT: AtomicU16 = AtomicU16::new(1234);
const MTU: usize = 1500;

type MyL3L4<'a> = L3L4<'a, Vec<u8>>;

struct TestCallbacksInner {
    device: Framed<AsyncDevice, TunPacketCodec>,
    data: VecDeque<Vec<u8>>,
    pkts: Vec<Vec<u8>>,
}

struct TestCallbacks {
    inner: RefCell<TestCallbacksInner>,
}

impl Callbacks<Vec<u8>> for TestCallbacks {
    fn l3_tx(&self, pkt: Vec<u8>) {
        trace!("Queue pkt to trasmit, length {}", pkt.len());
        self.inner.borrow_mut().pkts.push(pkt);
    }

    fn l3_tx_buffer(&self, size: usize) -> Option<Vec<u8>> {
        Some(vec![0u8; size])
    }

    fn l3_tx_buffer_mut<'tmp>(&self, buffer: &'tmp mut Vec<u8>) -> &'tmp mut [u8] {
        &mut buffer[..]
    }

    fn l4_rx(&self, flow: &Flow, data: Option<&[u8]>) {
        if let Some(data) = data {
            trace!("Queue data received, length {}", data.len());
            self.inner.borrow_mut().data.push_back(Vec::from(data));
        } else {
            trace!("Flow closed {:?}", flow);
        }
    }
}

async fn transmit_pkts(callbacks: &TestCallbacks) {
    let mut callbacks_mut = callbacks.inner.borrow_mut();
    // Transmit any packets that we have to transmit
    while let Some(pkt) = callbacks_mut.pkts.pop() {
        trace!("pkt tx size {}", pkt.len());
        if callbacks_mut
            .device
            .send(TunPacket::new(pkt))
            .await
            .is_err()
        {
            trace!("pkt tx fail");
        }
    }
}

// Simply loop back the l4 data
async fn transmit_data<'a>(l3l4: &mut MyL3L4<'a>, callbacks: &TestCallbacks, flow: &Flow) {
    loop {
        let mut callbacks_mut = callbacks.inner.borrow_mut();
        if let Some(mut data) = callbacks_mut.data.pop_front() {
            drop(callbacks_mut);
            trace!("Loopback data of len {}", data.len());
            match l3l4.l4_tx(callbacks, flow, &data[..]) {
                Some(size) => {
                    if size != data.len() {
                        trace!("Partial data written {} / {}", size, data.len());
                        // All the data could not be written, queue it back and try later
                        let mut callbacks_mut = callbacks.inner.borrow_mut();
                        callbacks_mut.data.push_back(data.drain(0..size).collect());
                        drop(callbacks_mut);
                    }
                }
                None => {
                    trace!("Socket error");
                    return;
                }
            }
        } else {
            break;
        }
    }
}

fn create_tun(name: usize, ipaddr: (u8, u8, u8, u8)) -> AsyncDevice {
    let mut config = tun::Configuration::default();
    config
        .address(ipaddr)
        .mtu(MTU as i32)
        .netmask((255, 255, 255, 0))
        .up();
    #[cfg(target_os = "linux")]
    {
        let name = format!("tun{}", name);
        config.name(name);
        config.platform(|config| {
            config.packet_information(false);
        });
        tun::create_as_async(&config).unwrap()
    }

    #[cfg(target_os = "macos")]
    {
        let route_name = format!("utun{}", name - 1);
        let name = format!("utun{}", name);
        let route = format!("{}.{}.{}.{}", ipaddr.0, ipaddr.1, ipaddr.2, ipaddr.3 + 1);
        config.name(name);
        config.destination((ipaddr.0, ipaddr.1, ipaddr.2, ipaddr.3 + 1));
        let dev = tun::create_as_async(&config).unwrap();
        let err = std::process::Command::new("route")
            .arg("add")
            .arg("-host")
            .arg(route)
            .arg("-interface")
            .arg(route_name)
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

async fn new_pkt_or_poll<'a>(
    l3l4: &mut MyL3L4<'a>,
    callbacks: &TestCallbacks,
) -> Option<TunPacket> {
    // If wakeup is None, that means just wakeup on the next Rx packet, i.e sleep
    // infinitely. But we have tests that depends on us waking up periodically and
    // checking for states etc.., hence we sleep just one second
    let wakeup = match l3l4.l4_poll(callbacks) {
        Some(wakeup) => wakeup,
        None => Instant::now() + WAIT_FOR_ONESEC,
    };
    trace!("Wakeup after {:?} ", wakeup - Instant::now());
    let mut callbacks_mut = callbacks.inner.borrow_mut();
    tokio::select! {
        p = callbacks_mut.device.next() => {
            return Some(p.unwrap().unwrap());
        }
        _ = sleep(wakeup - Instant::now()) => {
            return None;
        }
    }
}

async fn tcp_read_write(tcp: &mut TcpStream, nbytes: usize) {
    let mut data_buf = Vec::new();
    for i in 0..nbytes {
        data_buf.push((i % 256) as u8);
    }
    tcp.write_all(&data_buf[..]).await.unwrap();
    trace!("Wrote {} bytes", nbytes);
    let mut n = 0;
    while n != nbytes {
        n += tcp.read(&mut data_buf[n..]).await.unwrap();
        trace!("Read {} bytes", n);
    }
    for i in 0..n {
        if data_buf[i] != (i % 256) as u8 {
            panic!("Bad data {} at index {}", data_buf[i], i);
        }
    }
}

async fn tcp_client_read_write(test_port: u16, wait_idle: bool, ipaddr: &str) {
    loop {
        let tcp = TcpStream::connect(&format!("{}:{}", ipaddr, test_port)).await;
        if let Ok(mut tcp) = tcp {
            trace!("Testing 1 byte");
            tcp_read_write(&mut tcp, 1).await;
            trace!("Testing 1024 bytes");
            tcp_read_write(&mut tcp, 1024).await;
            trace!("Testing 64K bytes");
            tcp_read_write(&mut tcp, 64 * 1024).await;
            trace!("Testing 2*64K bytes");
            tcp_read_write(&mut tcp, 2 * 64 * 1024).await;
            if !wait_idle {
                tcp.shutdown().await.ok();
                // Now wait for the application to close the flow
                tokio::time::sleep(Duration::from_secs(std::u64::MAX)).await;
                break;
            } else {
                // Now wait for the application to close the flow
                tokio::time::sleep(Duration::from_secs(std::u64::MAX)).await;
            }
        } else {
            trace!("Waiting for tcp socket {:?}", tcp);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn echo_server<'a>(test_port: u16, l3l4: &mut MyL3L4<'a>, callbacks: &mut TestCallbacks) {
    let mut test_flow = None;
    loop {
        let pkt = new_pkt_or_poll(l3l4, callbacks).await;
        if let Some(pkt) = pkt {
            trace!("Got Rx pkt of len {}", pkt.get_bytes().len());
            // Tun returns a Bytes, and we need a mutable buffer to be passed
            // to l3_rx, and hence having to do this copy
            let mut buf = Vec::new();
            buf.extend_from_slice(&pkt.into_bytes());
            if let Some(flow) = l3l4.l3_rx(callbacks, &mut buf[..]) {
                if flow.dest_port != test_port || (test_flow.is_some() && test_flow != Some(flow)) {
                    // This is a simple UT, we just deal with one flow
                    l3l4.l4_local_close(&flow);
                } else if test_flow.is_none() {
                    test_flow = Some(flow);
                }
            }
        }
        if let Some(flow) = test_flow {
            transmit_data(l3l4, callbacks, &flow).await;
            if flow.protocol == crate::TCP {
                if l3l4.l4_is_remote_closed(&flow) {
                    trace!("Remote closed");
                    l3l4.l4_local_close(&flow);
                }
                if l3l4.l4_is_local_and_remote_closed(&flow) {
                    break;
                }
                trace!("Socket state {:?}", l3l4.l4_state(&flow));
            }
        }
        transmit_pkts(callbacks).await;
    }
}

async fn udp_read_write(udp: &mut UdpSocket, nbytes: usize) {
    let mut data_buf = Vec::new();
    for i in 0..nbytes {
        data_buf.push((i % 256) as u8);
    }
    udp.send(&data_buf[..]).await.unwrap();
    trace!("Wrote {} bytes", nbytes);
    let n = udp.recv(&mut data_buf[..]).await.unwrap();
    trace!("Read {} bytes", n);
    assert!(n == nbytes);
    for i in 0..n {
        if data_buf[i] != (i % 256) as u8 {
            panic!("Bad data {} at index {}", data_buf[i], i);
        }
    }
}

async fn udp_client_read_write(test_port: u16, wait_idle: bool, ipaddr: &str) {
    let mut udp = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    udp.connect(&format!("{}:{}", ipaddr, test_port))
        .await
        .unwrap();
    trace!("Testing 1 byte");
    udp_read_write(&mut udp, 1).await;
    trace!("Testing 1024 bytes");
    udp_read_write(&mut udp, 1024).await;
    while wait_idle {
        tokio::time::sleep(Duration::from_secs(std::u64::MAX)).await;
    }
}

#[tokio::test]
async fn test_tcp() {
    init_logging();
    let device = create_tun(90, (1, 2, 3, 4));
    let stream = device.into_framed();
    let mut l3l4 = L3L4Build::default()
        .mtu(MTU)
        .tcp_buffer_size(64 * 1024)
        .finalize();
    let mut callbacks = TestCallbacks {
        inner: RefCell::new(TestCallbacksInner {
            device: stream,
            data: VecDeque::new(),
            pkts: Vec::new(),
        }),
    };
    let test_port = TEST_PORT.fetch_add(1, Ordering::Relaxed);
    tokio::select! {
        _ = echo_server(test_port, &mut l3l4, &mut callbacks) => {},
        _ = tcp_client_read_write(test_port, false, "1.2.3.5") => {},
        _ = tokio::time::sleep(Duration::from_secs(30)) => {
            panic!("Tcp test failed");
        }
    }
    assert_eq!(l3l4.flows.len(), 0);
    assert_eq!(l3l4.polling.len(), 0);
}

#[tokio::test]
async fn test_tcp_idle_timeout() {
    init_logging();
    let device = create_tun(91, (1, 2, 4, 4));
    let stream = device.into_framed();
    let mut l3l4 = L3L4Build::default()
        .mtu(MTU)
        .tcp_buffer_size(64 * 1024)
        .tcp_halfopen_idle_timeout(5) // First idle timeout will kick in and make it half open
        .tcp_idle_timeout(2) // and then half open timer will kick in and terminate the flow
        .finalize();
    let mut callbacks = TestCallbacks {
        inner: RefCell::new(TestCallbacksInner {
            device: stream,
            data: VecDeque::new(),
            pkts: Vec::new(),
        }),
    };
    let test_port = TEST_PORT.fetch_add(1, Ordering::Relaxed);
    tokio::select! {
        _ = echo_server(test_port, &mut l3l4, &mut callbacks) => {},
        _ = tcp_client_read_write(test_port, true, "1.2.4.5") => {},
        _ = tokio::time::sleep(Duration::from_secs(30)) => {
            panic!("Tcp test failed");
        }
    }
    assert_eq!(l3l4.flows.len(), 0);
    assert_eq!(l3l4.polling.len(), 0);
}

#[tokio::test]
async fn test_udp() {
    init_logging();
    let device = create_tun(92, (1, 2, 5, 4));
    let stream = device.into_framed();
    let mut l3l4 = L3L4Build::default().mtu(MTU).finalize();
    let mut callbacks = TestCallbacks {
        inner: RefCell::new(TestCallbacksInner {
            device: stream,
            data: VecDeque::new(),
            pkts: Vec::new(),
        }),
    };
    let test_port = TEST_PORT.fetch_add(1, Ordering::Relaxed);
    tokio::select! {
        _ = echo_server(test_port, &mut l3l4, &mut callbacks) => {},
        _ = udp_client_read_write(test_port, false, "1.2.5.5") => {},
        _ = tokio::time::sleep(Duration::from_secs(10)) => {
            panic!("Udp test failed");
        }
    }
    assert_eq!(l3l4.flows.len(), 1);
    assert_eq!(l3l4.polling.len(), 1);
    let mut test_flow = None;
    for (flow, _) in l3l4.flows.iter() {
        test_flow = Some(*flow);
        break;
    }
    if let Some(flow) = test_flow {
        l3l4.l4_local_close(&flow);
    }
    assert_eq!(l3l4.flows.len(), 0);
    assert_eq!(l3l4.polling.len(), 0);
}

#[tokio::test]
async fn test_udp_idle_timeout() {
    init_logging();
    let device = create_tun(93, (1, 2, 6, 4));
    let stream = device.into_framed();
    let mut l3l4 = L3L4Build::default().mtu(MTU).udp_idle_timeout(3).finalize();
    let mut callbacks = TestCallbacks {
        inner: RefCell::new(TestCallbacksInner {
            device: stream,
            data: VecDeque::new(),
            pkts: Vec::new(),
        }),
    };
    let test_port = TEST_PORT.fetch_add(1, Ordering::Relaxed);
    tokio::select! {
        _ = echo_server(test_port, &mut l3l4, &mut callbacks) => {},
        _ = udp_client_read_write(test_port, true, "1.2.6.5") => {},
        _ = tokio::time::sleep(Duration::from_secs(6)) => {
        }
    }
    assert_eq!(l3l4.flows.len(), 0);
    assert_eq!(l3l4.polling.len(), 0);
}
