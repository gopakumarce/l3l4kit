use crate::Callbacks;

use log::trace;
use smoltcp::phy::{self, Checksum, ChecksumCapabilities, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;
use smoltcp::Result;

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

    fn receive(&mut self) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if let Some(buffer) = self.rx.take() {
            trace!("Receive called, length {}", buffer.len());
            let rx = RxToken { buffer };
            // The tx_token being None is fie as per comments in smoltcp Device receive().
            // The comments say that the tx_token in receive is used only if we expect an
            // output pkt (like icmp response) which is basically the same as the input pkt
            // For tcp/udp use case, we dont have anything like that. And the reason why
            // we cant just pass self.callbacks here is the same lifetime problems mentioned
            // in transmit() below.
            let tx = TxToken {
                callbacks: self.callbacks,
            };
            Some((rx, tx))
        } else {
            trace!("Receive called, no buffer");
            None
        }
    }

    fn transmit(&mut self) -> Option<Self::TxToken<'_>> {
        // The reason why the callbacks has to be "taken" from self is because otherwise
        // there will be multiple mutable pointers to callbacks - from self, and from
        // tx_token, and that obviously will cause all sorts of lifetime probles.
        // We can potentially do something fancy to fix all this, but fancy is not
        // needed because our usecase is really simple here - in one invocation to
        // interface.poll(), we expect exatly ONE output packet! So we just need ONE
        // token. If there are more packets to be sent, interface.poll() will/should be
        // called again
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
    fn consume<R, F>(self, _timestamp: Instant, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
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
    fn consume<R, F>(self, _timestamp: Instant, len: usize, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        if let Some(mut buffer) = self.callbacks.l3_tx_buffer(len) {
            let buf_mut = self.callbacks.l3_tx_buffer_mut(&mut buffer);
            let result = f(buf_mut);
            trace!("tx token consumed, length {}", buf_mut.len());
            self.callbacks.l3_tx(buffer);
            result
        } else {
            trace!("tx token failed, no buffers");
            Err(smoltcp::Error::Exhausted)
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
    fn receive(&mut self) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        None
    }
    fn transmit(&mut self) -> Option<Self::TxToken<'_>> {
        None
    }
}

impl phy::TxToken for TxTokenDummy {
    fn consume<R, F>(self, _timestamp: Instant, _: usize, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        f(&mut [])
    }
}

impl phy::RxToken for RxTokenDummy {
    fn consume<R, F>(self, _timestamp: Instant, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        f(&mut [])
    }
}
