use std::sync::Arc;

use jack::RawMidi;
use tokio::sync::Notify;

use crate::midi_builder::MidiBuilder;

pub fn create_midi_ring(size: usize) -> (MidiSender, MidiRecevier) {
    let (producer, consumer) = direct_ring_buffer::create_ring_buffer(size);
    let notify = Arc::new(tokio::sync::Notify::new());

    let producer = MidiSender {
        producer,
        notify: notify.clone(),
    };

    let consumer = MidiRecevier {
        consumer,
        builder: MidiBuilder::with_capacity(4096),
        time: [0; 4],
        notify,
    };

    (producer, consumer)
}

pub struct MidiSender {
    notify: Arc<Notify>,
    producer: direct_ring_buffer::Producer<u8>,
}

impl MidiSender {
    pub fn send<'a>(&mut self, midi: RawMidi<'a>) -> bool {
        let size = midi.bytes.len() + 4;
        if self.producer.available() < size {
            return false;
        }

        let time = midi.time.to_le_bytes();

        self.producer.write_slices(
            |ring, mut read_offset| {
                let mut write_offset = 0;
                while read_offset < 4 {
                    if ring.len() <= write_offset {
                        return ring.len();
                    }
                    ring[write_offset] = time[read_offset];
                    read_offset += 1;
                    write_offset += 1;
                }

                let remaining_space = ring.len() - write_offset;
                let read_offset = read_offset - 4;
                let read_end = read_offset + remaining_space;
                ring[write_offset..].copy_from_slice(&midi.bytes[read_offset..read_end]);

                ring.len()
            },
            Some(size),
        );

        self.notify.notify_one();

        true
    }
}

pub struct MidiRecevier {
    notify: Arc<Notify>,
    consumer: direct_ring_buffer::Consumer<u8>,
    builder: MidiBuilder,
    time: [u8; 4],
}

impl MidiRecevier {
    pub fn recv(&mut self) -> Option<RawMidi<'_>> {
        self.builder.reset();
        self.consumer.read_slices(
            |ring, mut offset| {
                let mut read_offset = 0;
                while offset < 4 {
                    if ring.len() <= offset {
                        return ring.len();
                    }
                    self.time[offset] = ring[read_offset];
                    offset += 1;
                    read_offset += 1;
                }

                for (idx, b) in ring.iter().enumerate().skip(read_offset) {
                    if self.builder.push(*b).is_some() {
                        return idx + 1;
                    }
                }
                ring.len()
            },
            None,
        );

        let bytes = self.builder.message()?;
        Some(RawMidi {
            time: u32::from_le_bytes(self.time),
            bytes,
        })
    }

    pub async fn notified(&self) {
        self.notify.notified().await;
    }

    pub async fn recv_async<F: FnMut(RawMidi<'_>) -> bool>(&mut self, mut f: F) {
        self.notified().await;
        while let Some(msg) = self.recv() {
            if !f(msg) {
                break;
            }
        }
    }
}

#[test]
fn midi_ring() {
    let (mut tx, mut rx) = create_midi_ring(127);
    let lengths = [3, 2, 64, 3, 1, 1, 1, 64, 3, 2, 112, 3, 64];

    let make_msg = |len| {
        let bytes = match len {
            0 => unreachable!(),
            1 => vec![0xf1],
            2 => vec![0xf3, 0x00],
            3 => vec![0xb0, 0x00, 0x00],
            _ => {
                let mut b = vec![0; len];
                b[0] = 0xf0;
                b[len - 1] = 0xf7;
                b
            }
        };

        RawMidi {
            time: 123456,
            bytes: bytes.leak(),
        }
    };

    for len in lengths {
        let msg = make_msg(len);
        assert!(tx.send(msg));

        let m = rx.recv();
        let m = m.unwrap();

        assert_eq!(m.time, 123456);
        assert_eq!(m.bytes.len(), len);

        let m = rx.recv();
        assert!(m.is_none());
    }
}
