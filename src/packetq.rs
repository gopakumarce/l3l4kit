use crate::Callbacks;

use log::trace;
use smoltcp::phy::{self, Checksum, ChecksumCapabilities, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

pub(crate) struct PacketQ<'buf, 'ptr, T> {
    mtu: usize,
    rx: Option<&'buf mut [u8]>,
    callbacks: &'ptr dyn Callbacks<T>,
}

#[allow(clippy::new_without_default)]
impl<'buf, 'ptr, T> PacketQ<'buf, 'ptr, T> {
    pub fn new(mtu: usize, rx: Option<&'buf mut [u8]>, callbacks: &'ptr dyn Callbacks<T>) -> Self {
        PacketQ { mtu, rx, callbacks }
    }
}

impl<'buf, 'ptr, T: 'buf> Device for PacketQ<'buf, 'ptr, T>
where
    T: 'buf,
{
    type RxToken<'a> = RxToken<'buf> where Self: 'a;
    type TxToken<'a> = TxToken<'ptr, T> where Self: 'a;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut device = DeviceCapabilities::default();
        device.max_transmission_unit = self.mtu;
        device.medium = Medium::Ip;
        device.checksum = ChecksumCapabilities::ignored();
        device.checksum.ipv4 = Checksum::Tx;
        device.checksum.tcp = Checksum::Tx;
        device.checksum.udp = Checksum::Tx;
        device
    }

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if let Some(buffer) = self.rx.take() {
            trace!("Receive called, length {}", buffer.len());
            let rx = RxToken { buffer };
            let tx = TxToken {
                callbacks: self.callbacks,
            };
            Some((rx, tx))
        } else {
            trace!("Receive called, no buffer");
            None
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        trace!("Transmit called");
        Some(TxToken {
            callbacks: self.callbacks,
        })
    }
}

pub(crate) struct RxToken<'buf> {
    buffer: &'buf mut [u8],
}

impl<'buf> phy::RxToken for RxToken<'buf> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        trace!("rx token consumed");
        f(self.buffer)
    }
}

pub(crate) struct TxToken<'ptr, T> {
    callbacks: &'ptr dyn Callbacks<T>,
}

impl<'buf, 'ptr, T> phy::TxToken for TxToken<'ptr, T>
where
    T: 'buf,
{
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        if let Some(mut buffer) = self.callbacks.l3_tx_buffer(len) {
            let buf_mut = self.callbacks.l3_tx_buffer_mut(&mut buffer);
            let result = f(buf_mut);
            trace!("tx token consumed, length {}", buf_mut.len());
            self.callbacks.l3_tx(buffer);
            result
        } else {
            trace!("tx token failed, no buffers");
            f(&mut [])
        }
    }
}

pub(crate) struct Dummy {
    mtu: usize,
}

impl Dummy {
    pub fn new(mtu: usize) -> Self {
        Self { mtu }
    }
}

pub(crate) struct RxTokenDummy;
pub(crate) struct TxTokenDummy;

impl Device for Dummy {
    type RxToken<'a> = RxTokenDummy;
    type TxToken<'a> = TxTokenDummy;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut device = DeviceCapabilities::default();
        device.max_transmission_unit = self.mtu;
        device.medium = Medium::Ip;
        device.checksum = ChecksumCapabilities::ignored();
        device.checksum.ipv4 = Checksum::Tx;
        device.checksum.tcp = Checksum::Tx;
        device.checksum.udp = Checksum::Tx;
        device
    }
    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        None
    }
    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        None
    }
}

impl phy::TxToken for TxTokenDummy {
    fn consume<R, F>(self, _: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(&mut [])
    }
}

impl phy::RxToken for RxTokenDummy {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(&mut [])
    }
}
