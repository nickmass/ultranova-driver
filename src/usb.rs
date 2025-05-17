use std::time::Duration;

use nusb::transfer::{Queue, RequestBuffer};

use crate::model_config::EndpointInfo;

const TRANSFER_SIZE: usize = 1024;
const TRANSFER_LIMIT: usize = 128;

pub struct ClaimedInterface(#[allow(unused)] pub nusb::Interface);

pub struct InteractiveInterface {
    #[allow(unused)]
    interface: nusb::Interface,
    usb_in: UsbIn,
    usb_out: UsbOut,
}

impl InteractiveInterface {
    pub fn new(interface: nusb::Interface, endpoint_info: EndpointInfo) -> Self {
        let usb_in = UsbIn::new(&interface, &endpoint_info);
        let usb_out = UsbOut::new(&interface, &endpoint_info);

        Self {
            interface,
            usb_in,
            usb_out,
        }
    }

    pub fn split(&mut self) -> (&mut UsbIn, &mut UsbOut) {
        (&mut self.usb_in, &mut self.usb_out)
    }

    pub fn has_err(&self) -> bool {
        self.usb_in.has_err() || self.usb_out.has_err()
    }
}

pub struct UsbIn {
    queue: Queue<RequestBuffer>,
    read_err: bool,
}

impl UsbIn {
    pub fn new(interface: &nusb::Interface, endpoint_info: &EndpointInfo) -> Self {
        let mut queue = interface.interrupt_in_queue(endpoint_info.input | 0x80);

        while queue.pending() < TRANSFER_LIMIT {
            queue.submit(RequestBuffer::new(TRANSFER_SIZE));
        }

        Self {
            queue,
            read_err: false,
        }
    }

    pub async fn recv(&mut self) -> UsbData<'_> {
        let completion = self.queue.next_complete().await;
        if let Err(err) = completion.status {
            tracing::error!("usb queue_in: {:?}", err);
            self.read_err = true;
        };

        UsbData {
            completion,
            queue: &mut self.queue,
        }
    }

    pub fn has_err(&self) -> bool {
        self.read_err
    }
}

pub struct UsbData<'a> {
    completion: nusb::transfer::Completion<Vec<u8>>,
    queue: &'a mut Queue<RequestBuffer>,
}

impl<'a> std::ops::Deref for UsbData<'a> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.completion.data
    }
}

impl<'a> Drop for UsbData<'a> {
    fn drop(&mut self) {
        let buf = std::mem::replace(&mut self.completion.data, Vec::new());
        self.queue.submit(RequestBuffer::reuse(buf, TRANSFER_SIZE));
    }
}

pub struct UsbOut {
    queue: Queue<Vec<u8>>,
    out_bufs: Vec<Vec<u8>>,
    write_err: bool,
}

impl UsbOut {
    pub fn new(interface: &nusb::Interface, endpoint_info: &EndpointInfo) -> Self {
        let queue_out = interface.interrupt_out_queue(endpoint_info.output & 0x7f);

        Self {
            queue: queue_out,
            out_bufs: Vec::new(),
            write_err: false,
        }
    }

    pub fn can_submit(&self) -> bool {
        self.queue.pending() < TRANSFER_LIMIT
    }

    pub fn submit(&mut self, bytes: &[u8]) {
        let mut buf = self
            .out_bufs
            .pop()
            .unwrap_or_else(|| Vec::with_capacity(TRANSFER_SIZE));
        buf.extend(bytes);
        self.queue.submit(buf);
    }

    pub async fn send<F: Future<Output = ()>>(&mut self, notify: F) {
        if self.queue.pending() > 0 {
            let completion = self.queue.next_complete().await;
            if let Err(err) = completion.status {
                tracing::error!("usb queue_out: {:?}", err);
                self.write_err = true;
            };

            if self.out_bufs.len() < TRANSFER_LIMIT {
                self.out_bufs.push(completion.data.reuse());
            }
        } else {
            let _ = tokio::time::timeout(Duration::from_millis(1), notify).await;
        }
    }

    pub fn has_err(&self) -> bool {
        self.write_err
    }
}
