# Packet in, Data out and Data in, Packet out

## Basic operation

The headline summarizes what this library does. Give it l3 IP packets as input, 
and it will give l4 data as output. Simiarly give it l4 data as input, and it
will give l3 IP packets as output. The packets in are supplied by calling the
l3_rx() API, and the data that comes out is via callback l4_rx(). The l4 data
that needs to be trasmitted is supplied via the API l4_tx(), that data is then 
packet-ized and comes out as the l3_tx() callback.

The packets can be of any source or destination IP, there is no need for any 
route tables or anything of that sort. How/where the packet comes from and 
how/where it goes out of is not the concern of this library.

## Why callback for l4_rx() and l3_tx() ?

If l4_tx() data can be transmitted as IP packets, we ask the caller to provide
an "interface buffer" into which the packets are directly written into. This
avoids the library copying the packets into some intermediate buffer which then
then the caller copies into interface buffers -  for example if 
the interface driver is dpdk, the l3_tx_buffer() API can provide a dpdk buffer 
object directly. Whatever be the object type provided, it needs to be addressible
as a contigious slice. So calling l3_tx() inline from within l4_tx() allows 
the IP packets to be directly written into the interface buffers without any
intermediate copies etc..

If l3_rx() Rx packet ended up with L4 data in the TCP/UDP buffers, we 
can in theory provide an API to peek into those buffers, so that the caller can
get the data without any intermediate copies. But smoltcp does not allow peeking
into buffers directly, it only allows providing a callback to which the buffer
slice is provided, and hence the reason we also have l4_rx() as a callback

## House keeping

TCP sockets obviously need to schedule tasks in future like retransmits etc.. So
the caller has to arrange for some way to call the l4_poll() API at points
in future indicated by the return value of that API

## Pseudo code

Below we provide a pseudo code outline of what was described above, a concrete 
example is https://github.com/gopakumarce/l3l4kit/blob/main/examples/echo_server.rs
NOTE: The example works only on linux as of now, it can be made to work on macos also
with minor tweaks, thats a TODO

```
struct MyPacketBuf{}
struct MyFlowInfo{}
struct MyCallbacks {my_flows: HashMap<Flow, MyFlowInfo>}

impl MyCallBacks {
    fn add_flow_to_my_flows(&self, flow: Flow) {
        if !self.my_flows.contains_key(&flow) {
            self.my_flows.insert(flow, MyFlowInfo{})
        }
    }
    
    fn process_data(&self, flow: Flow, data: Option<&[u8]>) {
        if let Some(data) = data {
            // process the data
        } else {
            // flow is not going to receive any more data, maybe take 
            // the flow out of the my_flows hashtable ?
        }
    }
}

impl Callbacks<MyPacketBuf> for MyCallbacks {
    fn l3_tx(&self, pkt: MyPacketBuf) {
        // Do whatever you have to do to transmit the pkt
    }

    fn l3_tx_buffer() -> Option<MyPacketBuf> {
        // Return a new packet buffer or None if exhausted
    }

    fn l3_tx_buffer_mut<'a>(&self, pkt: &'a mut MyPacketBuf) -> &'a mut [u8] {
        // Return a contigious slice version of the pkt buffer
    }

    fn l4_rx(&self, flow: Flow, data: Opion<&[u8]>) {
        // You got data for a flow. The flow can be something you have seen
        // before or a new flow. You can maintain your own data structures to
        // store the flow and do whatever you want with it and with the data
        // received.
        add_flow_to_my_flows(flow);
        process_data(flow, data);
    }
}

main() {
    let callback = MyCallbacks::default();
    let l3l4 = L4L4Build::default().<options you want>.finalize();
    let next_house_keeping_time = l3l4.l4_poll();

    loop {
        // We might either get an Rx L3 packet or our application might have generated 
        // some L4 data to be transmitted, and/or we might have periodic house-keeping
        // work to do. How these three activities are interleaved / scheduled is upto
        // the caller, one way which we use in the examples/echo_server.rs is by
        // using async/await, but that absolutely need not be how its done
        let (pkt, time_for_house_keeping, l4_to_send) = get_l3_rx_or_l4_tx_with_timeout(next_house_keeping_time);
        if time_for_house_keeping {
            next_house_keeping_time = l3l4.l4_poll();
        }

        // This can trigger  MyCallbacks::l4_rx()
        flow = l3l4.l3_rx(&callback, pkt);

        // The l3 Rx packet above might have an ACK for this flow and hence we might 
        // have room to transmit. So try to send out data in pending queue
        if pending_l4_send = has_previous_pending_l4_tx(flow) {
            // This can trigger MyCallbacks::l3_tx()
            l3l4.l4_tx(&callback, pending_l4_send.flow, pending_l4_send.buffer);
        }

        // This can trigger MyCallbacks::l3_tx()
        // If the l4_to_send data is not fully transmitted, queue it to the pending queue
        l3l4.l4_tx(&callback, l4_to_send.flow, l4_to_send.buffer);
    }
}
```
