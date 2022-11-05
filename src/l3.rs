use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    IpAddress, IpProtocol, Ipv4Packet, Ipv4Repr, Ipv6Packet, Ipv6Repr, TcpPacket, TcpRepr,
    UdpPacket, UdpRepr,
};

use crate::Flow;

pub(crate) fn parse_v4(rx: &[u8]) -> Option<Flow> {
    if let Ok(pkt) = Ipv4Packet::new_checked(rx) {
        let nocksum = ChecksumCapabilities::ignored();
        let pkt = Ipv4Packet::new_unchecked(pkt.into_inner());
        if let Ok(ipv4) = Ipv4Repr::parse(&pkt, &nocksum) {
            let source_ip = IpAddress::from(ipv4.src_addr);
            let dest_ip = IpAddress::from(ipv4.dst_addr);
            match pkt.next_header() {
                IpProtocol::Tcp => {
                    if let Ok(tpkt) = TcpPacket::new_checked(pkt.payload()) {
                        if let Ok(tcp) = TcpRepr::parse(&tpkt, &source_ip, &dest_ip, &nocksum) {
                            return Some(Flow {
                                source_ip,
                                dest_ip,
                                source_port: tcp.src_port,
                                dest_port: tcp.dst_port,
                                protocol: crate::TCP,
                            });
                        }
                    }
                }
                IpProtocol::Udp => {
                    if let Ok(upkt) = UdpPacket::new_checked(pkt.payload()) {
                        if let Ok(udp) = UdpRepr::parse(&upkt, &source_ip, &dest_ip, &nocksum) {
                            return Some(Flow {
                                source_ip,
                                dest_ip,
                                source_port: udp.src_port,
                                dest_port: udp.dst_port,
                                protocol: crate::UDP,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }
    None
}

pub(crate) fn parse_v6(rx: &[u8]) -> Option<Flow> {
    if let Ok(pkt) = Ipv6Packet::new_checked(rx) {
        let pkt = Ipv6Packet::new_unchecked(pkt.into_inner());
        let nocksum = ChecksumCapabilities::ignored();
        if let Ok(ipv6) = Ipv6Repr::parse(&pkt) {
            let source_ip = IpAddress::from(ipv6.src_addr);
            let dest_ip = IpAddress::from(ipv6.dst_addr);
            match pkt.next_header() {
                IpProtocol::Tcp => {
                    if let Ok(tpkt) = TcpPacket::new_checked(pkt.payload()) {
                        if let Ok(tcp) = TcpRepr::parse(&tpkt, &source_ip, &dest_ip, &nocksum) {
                            return Some(Flow {
                                source_ip,
                                dest_ip,
                                source_port: tcp.src_port,
                                dest_port: tcp.dst_port,
                                protocol: crate::TCP,
                            });
                        }
                    }
                }
                IpProtocol::Udp => {
                    if let Ok(upkt) = UdpPacket::new_checked(pkt.payload()) {
                        if let Ok(udp) = UdpRepr::parse(&upkt, &source_ip, &dest_ip, &nocksum) {
                            return Some(Flow {
                                source_ip,
                                dest_ip,
                                source_port: udp.src_port,
                                dest_port: udp.dst_port,
                                protocol: crate::UDP,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }
    None
}
