pub struct MidiBuilder {
    buffer: Vec<u8>,
    length: MidiLength,
    done: bool,
    reset: bool,
    oversized: bool,
}

impl MidiBuilder {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(capacity.max(2)),
            length: MidiLength::SysEx,
            done: true,
            reset: true,
            oversized: false,
        }
    }

    pub fn push(&mut self, byte: u8) -> Option<&[u8]> {
        if self.done || self.reset {
            self.buffer.clear();

            // all midi messages start with a high bit, if we don't get one ignore the byte until we can resync with
            // the next message
            if byte & 0x80 == 0 {
                tracing::warn!("unexpected midi status byte: 0x{:2x}", byte);
                return None;
            }

            self.length = MidiLength::new(byte);
            self.done = false;
            self.reset = false;
        }

        if self.buffer.len() < self.buffer.capacity() {
            self.buffer.push(byte);
        } else {
            self.oversized = true;
        }

        if self.length.was_last_byte(self.buffer.len(), byte) {
            self.done = true;
            if self.oversized {
                tracing::error!("midi_builder: dropped oversized message");
                None
            } else {
                Some(&self.buffer)
            }
        } else {
            None
        }
    }

    pub fn reset(&mut self) {
        self.reset = true;
    }

    pub fn message(&self) -> Option<&[u8]> {
        if self.done && !self.reset {
            Some(&self.buffer)
        } else {
            None
        }
    }
}

enum MidiLength {
    Fixed(usize),
    SysEx,
}

impl MidiLength {
    fn new(first_byte: u8) -> Self {
        match (first_byte, first_byte & 0xf0) {
            (0xf0, _) => MidiLength::SysEx,
            (0xf2, _) | (_, 0x80 | 0x90 | 0xa0 | 0xb0 | 0xe0) => MidiLength::Fixed(3),
            (0xf1 | 0xf3, _) | (_, 0xc0 | 0xd0) => MidiLength::Fixed(2),
            (_, 0xf0) => MidiLength::Fixed(1),
            _ => {
                tracing::warn!("unexpected midi status byte: 0x{:2x}", first_byte);
                MidiLength::Fixed(1)
            }
        }
    }

    fn was_last_byte(&self, len: usize, byte: u8) -> bool {
        match self {
            MidiLength::Fixed(n) => len >= *n,
            MidiLength::SysEx => byte == 0xf7,
        }
    }
}
