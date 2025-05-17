use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use futures::prelude::*;
use jack::RawMidi;
use midi_builder::MidiBuilder;
use nusb::{self, DeviceId, hotplug::HotplugEvent};
use tokio::sync::Mutex;

mod midi_builder;
mod model_config;
mod ring;
mod usb;

use model_config::{EndpointConfig, ModelConfig};
use ring::{MidiRecevier, MidiSender, create_midi_ring};
use usb::{ClaimedInterface, InteractiveInterface};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let mut jack = JackClient::new(ModelConfig::UltraNova.name())?;
    let mut devices = DeviceRegistry::new();
    let watcher = UsbWatch::new(devices.clone())?;
    watcher.spawn();

    for device_info in nusb::list_devices()? {
        if let Some(device) = NovationDevice::acquire(device_info)? {
            devices.add(device).await;
        }
    }

    loop {
        devices.process(&mut jack, Duration::from_millis(100)).await
    }
}

#[derive(Clone)]
struct DeviceRegistry {
    inner: Arc<Mutex<DeviceRegistryInner>>,
}

impl DeviceRegistry {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(DeviceRegistryInner::new())),
        }
    }

    async fn add(&mut self, device: NovationDevice) {
        let mut inner = self.inner.lock().await;
        inner.add(device);
    }

    async fn remove(&mut self, device_id: DeviceId) -> Option<NovationDevice> {
        let mut inner = self.inner.lock().await;
        inner.remove(device_id)
    }

    async fn process(&mut self, jack: &mut JackClient, timeout: Duration) {
        let mut inner = self.inner.lock().await;

        // timeout forces us to drop the mutex lock every once in while to add/remove hotplug devices
        let _ = tokio::time::timeout(timeout, inner.process(jack)).await;
    }
}

struct DeviceRegistryInner {
    devices: Vec<NovationDevice>,
}

impl DeviceRegistryInner {
    fn new() -> Self {
        Self {
            devices: Vec::new(),
        }
    }

    fn add(&mut self, device: NovationDevice) {
        self.devices.push(device);
        tracing::info!("novation device added");
    }

    fn remove(&mut self, device_id: DeviceId) -> Option<NovationDevice> {
        if let Some(pos) = self.devices.iter().position(|p| p.device_id() == device_id) {
            tracing::info!("novation device removed");
            Some(self.devices.remove(pos))
        } else {
            None
        }
    }

    async fn process(&mut self, jack: &mut JackClient) {
        if let Some(first) = self.devices.first_mut() {
            first.process(jack).await;
        } else {
            jack.drain().await;
        }
    }
}

struct UsbWatch {
    devices: DeviceRegistry,
    watcher: nusb::hotplug::HotplugWatch,
}

impl UsbWatch {
    fn new(devices: DeviceRegistry) -> Result<Self> {
        let watcher = nusb::watch_devices()?;
        Ok(Self { devices, watcher })
    }

    fn spawn(mut self) {
        let _ = tokio::spawn(async move {
            while let Some(event) = self.watcher.next().await {
                match event {
                    HotplugEvent::Connected(device_info) => {
                        if let Ok(Some(device)) = NovationDevice::acquire(device_info) {
                            self.devices.add(device).await;
                        }
                    }
                    HotplugEvent::Disconnected(device_id) => {
                        self.devices.remove(device_id).await;
                    }
                }
            }
        });
    }
}

struct NovationDevice {
    device_info: nusb::DeviceInfo,
    #[allow(unused)]
    device: nusb::Device,
    #[allow(unused)]
    device_interface: Option<ClaimedInterface>,
    midi_processor: Option<UsbMidiProcessor>,
    control_processor: Option<UsbControlProcessor>,
}

impl NovationDevice {
    fn acquire(device_info: nusb::DeviceInfo) -> Result<Option<Self>> {
        let Some(model_config) = model_config::ALL_MODELS
            .iter()
            .find(|model| model.matches_device_info(&device_info))
        else {
            return Ok(None);
        };

        let device = device_info.open()?;

        let mut device_interface = None;
        let mut midi_interface = None;
        let mut control_interface = None;

        for interface_config in model_config.interfaces() {
            let interface = device.claim_interface(interface_config.interface_id)?;
            match interface_config.endpoint {
                Some(EndpointConfig::Midi(endpoint_info)) => {
                    midi_interface = Some(InteractiveInterface::new(interface, endpoint_info));
                }
                Some(EndpointConfig::Control(endpoint_info)) => {
                    control_interface = Some(InteractiveInterface::new(interface, endpoint_info));
                }
                None => device_interface = Some(ClaimedInterface(interface)),
            }
        }

        let midi_processor = midi_interface.map(UsbMidiProcessor::new);
        let control_processor = control_interface.map(UsbControlProcessor::new);

        Ok(Some(NovationDevice {
            device_info,
            device,
            device_interface,
            midi_processor,
            control_processor,
        }))
    }

    fn device_id(&self) -> nusb::DeviceId {
        self.device_info.id()
    }

    async fn process(&mut self, jack: &mut JackClient) {
        let (jack, jack_midi, jack_control) = jack.split();
        let midi = async {
            if let Some(midi) = self.midi_processor.as_mut() {
                midi.process(jack, jack_midi).await
            } else {
                jack_midi.drain().await
            }
        };

        let control = async {
            if let Some(control) = self.control_processor.as_mut() {
                control.process(jack, jack_control).await
            } else {
                jack_control.drain().await
            }
        };

        tokio::select! {
            _ = midi => {},
            _ = control => {}
        }
    }
}

struct JackProcessor {
    midi: JackMidiProcessor,
    control: JackMidiProcessor,
}

impl jack::ProcessHandler for JackProcessor {
    fn process(
        &mut self,
        client: &jack::Client,
        process_scope: &jack::ProcessScope,
    ) -> jack::Control {
        let mut control = jack::Control::Continue;

        if let jack::Control::Quit = self.midi.process(client, process_scope) {
            control = jack::Control::Quit;
        }

        if let jack::Control::Quit = self.control.process(client, process_scope) {
            control = jack::Control::Quit;
        }

        control
    }
}

struct JackMidiProcessor {
    midi_in: jack::Port<jack::MidiIn>,
    midi_out: jack::Port<jack::MidiOut>,
    midi_rx: MidiRecevier,
    midi_tx: MidiSender,
    last_fame_time: u32,
    midi_write_err: bool,
    tx_buf_full: bool,
}

impl jack::ProcessHandler for JackMidiProcessor {
    fn process(&mut self, _: &jack::Client, process_scope: &jack::ProcessScope) -> jack::Control {
        let mut writer = self.midi_out.writer(process_scope);
        while let Some(mut msg) = self.midi_rx.recv() {
            msg.time = msg
                .time
                .saturating_sub(self.last_fame_time)
                .min(process_scope.n_frames());
            if let Err(e) = writer.write(&msg) {
                if !self.midi_write_err {
                    tracing::error!("jack midi write: {:?}", e);
                }
                self.midi_write_err = true;
            } else if self.midi_write_err {
                tracing::info!("jack midi write resolved");
                self.midi_write_err = false;
            }
        }

        for msg in self.midi_in.iter(process_scope) {
            if !self.midi_tx.send(msg) {
                if !self.tx_buf_full {
                    tracing::error!("jack midi_tx: buffer full");
                }
                self.tx_buf_full = true;
            } else if self.tx_buf_full {
                tracing::info!("jack midi_tx: buffer resolved");
                self.tx_buf_full = false;
            }
        }

        self.last_fame_time = process_scope.last_frame_time();
        jack::Control::Continue
    }
}

struct JackMidiConnection {
    midi_rx: MidiRecevier,
    midi_tx: MidiSender,
}

impl JackMidiConnection {
    fn new(jack: &jack::Client, port_name: &'static str) -> Result<(Self, JackMidiProcessor)> {
        let midi_in = jack.register_port(&format!("{port_name}_in"), jack::MidiIn::default())?;
        let midi_out = jack.register_port(&format!("{port_name}_out"), jack::MidiOut::default())?;

        let (midi_tx, jack_midi_rx) = create_midi_ring(1024 * 32);
        let (jack_midi_tx, midi_rx) = create_midi_ring(1024 * 32);

        let jack_processor = JackMidiProcessor {
            midi_in,
            midi_out,
            midi_rx: jack_midi_rx,
            midi_tx: jack_midi_tx,
            last_fame_time: 0,
            midi_write_err: false,
            tx_buf_full: false,
        };

        Ok((Self { midi_tx, midi_rx }, jack_processor))
    }

    async fn drain(&mut self) {
        loop {
            self.midi_rx.recv_async(|_| true).await
        }
    }

    fn split(&mut self) -> (&mut MidiSender, &mut MidiRecevier) {
        (&mut self.midi_tx, &mut self.midi_rx)
    }
}

struct JackClient {
    client: jack::AsyncClient<(), JackProcessor>,
    midi_connection: JackMidiConnection,
    control_connection: JackMidiConnection,
}

impl JackClient {
    fn new(name: &'static str) -> Result<Self> {
        let (jack, _jack_status) = jack::Client::new(name, jack::ClientOptions::empty())?;

        let (midi_connection, midi_processor) = JackMidiConnection::new(&jack, "midi")?;
        let (control_connection, control_processor) = JackMidiConnection::new(&jack, "control")?;

        let processor = JackProcessor {
            midi: midi_processor,
            control: control_processor,
        };

        let client = jack.activate_async((), processor)?;

        Ok(Self {
            client,
            midi_connection,
            control_connection,
        })
    }

    async fn drain(&mut self) {
        let (midi, control) = (&mut self.midi_connection, &mut self.control_connection);
        tokio::select! {
            _ = midi.drain() => {},
            _ = control.drain() => {},
        }
    }

    fn split(
        &mut self,
    ) -> (
        &jack::Client,
        &mut JackMidiConnection,
        &mut JackMidiConnection,
    ) {
        (
            self.client.as_client(),
            &mut self.midi_connection,
            &mut self.control_connection,
        )
    }
}

struct UsbMidiProcessor {
    interface: InteractiveInterface,
    midi_builder: MidiBuilder,
}

impl UsbMidiProcessor {
    fn new(interface: InteractiveInterface) -> Self {
        Self {
            interface,
            midi_builder: MidiBuilder::with_capacity(4096),
        }
    }

    async fn process(&mut self, jack: &jack::Client, jack_midi: &mut JackMidiConnection) {
        if self.interface.has_err() {
            let _: () = futures::future::pending().await;
            return;
        }

        let (midi_tx, midi_rx) = jack_midi.split();
        let (usb_in, usb_out) = self.interface.split();

        let read_in = async {
            while !usb_in.has_err() {
                let data = usb_in.recv().await;

                for &b in &data[..] {
                    if let Some(msg) = self.midi_builder.push(b) {
                        let time = jack.frame_time();
                        let midi = RawMidi { time, bytes: msg };

                        if let Ok(midi) = wmidi::MidiMessage::try_from(msg) {
                            tracing::debug!("midi_out: {:?}", midi);
                        }

                        if !midi_tx.send(midi) {
                            tracing::error!("usb midi_tx: buffer full");
                        }
                    }
                }
            }
        };

        let read_out = async {
            while !usb_out.has_err() {
                if usb_out.can_submit() {
                    if let Some(msg) = midi_rx.recv() {
                        if let Ok(midi) = wmidi::MidiMessage::try_from(msg.bytes) {
                            tracing::debug!("midi_in: {:?}", midi);
                        }

                        usb_out.submit(msg.bytes);
                    }
                }

                usb_out.send(midi_rx.notified()).await;
            }
        };

        tokio::select! {
            _ = read_in => {},
            _ = read_out => {},
        }
    }
}

struct UsbControlProcessor {
    interface: InteractiveInterface,
    midi_builder: MidiBuilder,
    automap_state: AutoMapState,
    automap_greeting: AutoMapGreeting,
}

impl UsbControlProcessor {
    fn new(interface: InteractiveInterface) -> Self {
        Self {
            interface,
            midi_builder: MidiBuilder::with_capacity(4096),
            automap_state: AutoMapState::Startup,
            automap_greeting: AutoMapGreeting::new(),
        }
    }

    async fn process(&mut self, jack: &jack::Client, jack_midi: &mut JackMidiConnection) {
        if self.interface.has_err() {
            let _: () = futures::future::pending().await;
            return;
        }

        let (midi_tx, midi_rx) = jack_midi.split();
        let (usb_in, usb_out) = self.interface.split();

        let read_in = async {
            while !usb_in.has_err() {
                let data = usb_in.recv().await;

                for &b in &data[..] {
                    if let Some(msg) = self.midi_builder.push(b) {
                        if let Ok(midi) = wmidi::MidiMessage::try_from(msg) {
                            tracing::debug!("control_out: {:?}", midi);
                        }

                        match self.automap_state.process(msg) {
                            AutoMapAction::Forward => {
                                let time = jack.frame_time();
                                let midi = RawMidi { time, bytes: msg };

                                if !midi_tx.send(midi) {
                                    tracing::error!("usb control_tx: buffer full");
                                }
                            }
                            AutoMapAction::SendGreeting => self.automap_greeting.send(),
                            AutoMapAction::Discard => {}
                        }
                    }
                }
            }
        };

        let read_out = async {
            while !usb_out.has_err() {
                if usb_out.can_submit() {
                    if let Some(msg) = self.automap_greeting.message() {
                        if let Ok(midi) = wmidi::MidiMessage::try_from(msg) {
                            tracing::debug!("control_in: {:?}", midi);
                        }
                        usb_out.submit(msg);
                    } else if let Some(msg) = midi_rx.recv() {
                        if let Ok(midi) = wmidi::MidiMessage::try_from(msg.bytes) {
                            tracing::debug!("control_in: {:?}", midi);
                        }
                        usb_out.submit(msg.bytes);
                    }
                }

                usb_out.send(midi_rx.notified()).await;
            }
        };

        tokio::select! {
            _ = read_in => {},
            _ = read_out => {},
        }
    }
}

enum AutoMapState {
    Startup,
    Wait,
    Listen,
}

impl AutoMapState {
    fn process(&mut self, msg: &[u8]) -> AutoMapAction {
        match self {
            AutoMapState::Startup if msg == AUTOMAP_OK => {
                *self = AutoMapState::Listen;
                AutoMapAction::Discard
            }
            AutoMapState::Startup if msg == AUTOMAP_OFF => {
                *self = AutoMapState::Wait;
                AutoMapAction::Discard
            }
            AutoMapState::Startup => AutoMapAction::Discard,
            AutoMapState::Wait if msg == AUTOMAP_OK => {
                *self = AutoMapState::Listen;
                AutoMapAction::SendGreeting
            }
            AutoMapState::Wait => AutoMapAction::Discard,
            AutoMapState::Listen if msg == AUTOMAP_OFF => {
                *self = AutoMapState::Wait;
                AutoMapAction::Discard
            }
            AutoMapState::Listen => AutoMapAction::Forward,
        }
    }
}

enum AutoMapAction {
    Forward,
    SendGreeting,
    Discard,
}

struct AutoMapGreeting {
    step: AtomicU8,
}

impl AutoMapGreeting {
    fn new() -> Self {
        Self {
            step: AtomicU8::new(AutoMapGreetingState::InitialOk as u8),
        }
    }

    fn send(&self) {
        self.step
            .store(AutoMapGreetingState::Ok as u8, Ordering::Relaxed);
    }

    fn message(&self) -> Option<&'static [u8]> {
        let state = self.step.load(Ordering::Relaxed).into();
        let next = match state {
            AutoMapGreetingState::None => return None,
            _ => state.next(),
        };

        let state: AutoMapGreetingState = self
            .step
            .compare_exchange(
                state as u8,
                next as u8,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .unwrap_or_default()
            .into();

        state.message()
    }
}

#[repr(u8)]
enum AutoMapGreetingState {
    InitialOk = 3,
    Ok = 2,
    Greet = 1,
    None = 0,
}

impl AutoMapGreetingState {
    fn next(&self) -> Self {
        match self {
            AutoMapGreetingState::InitialOk => AutoMapGreetingState::None,
            AutoMapGreetingState::Ok => AutoMapGreetingState::Greet,
            AutoMapGreetingState::Greet => AutoMapGreetingState::None,
            AutoMapGreetingState::None => AutoMapGreetingState::None,
        }
    }

    fn message(&self) -> Option<&'static [u8]> {
        match self {
            AutoMapGreetingState::InitialOk | AutoMapGreetingState::Ok => Some(AUTOMAP_OK),
            AutoMapGreetingState::Greet => Some(AUTOMAP_GREETING),
            AutoMapGreetingState::None => None,
        }
    }
}

impl From<u8> for AutoMapGreetingState {
    fn from(value: u8) -> Self {
        match value {
            3 => Self::InitialOk,
            2 => Self::Ok,
            1 => Self::Greet,
            _ => Self::None,
        }
    }
}

const AUTOMAP_OK: &[u8] = &[0xf0, 0x00, 0x01, 0xf7];
const AUTOMAP_OFF: &[u8] = &[0xf0, 0x00, 0x00, 0xf7];
const AUTOMAP_GREETING: &[u8] = b"\xf0\x02\x00\x1eConnected\xf7";
