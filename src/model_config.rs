pub const ALL_MODELS: &[ModelConfig] = &[ModelConfig::UltraNova, ModelConfig::MiniNova];

#[derive(Debug, Copy, Clone)]
pub enum ModelConfig {
    UltraNova,
    MiniNova,
}

impl ModelConfig {
    pub fn name(&self) -> &'static str {
        match self {
            ModelConfig::UltraNova => "ultranova",
            ModelConfig::MiniNova => "mininova",
        }
    }

    pub fn matches_device_info(&self, device_info: &nusb::DeviceInfo) -> bool {
        device_info.vendor_id() == self.vendor_id() && device_info.product_id() == self.product_id()
    }

    pub fn vendor_id(&self) -> u16 {
        0x1235
    }

    pub fn product_id(&self) -> u16 {
        match self {
            ModelConfig::UltraNova => 0x0011,
            ModelConfig::MiniNova => 0x001e,
        }
    }

    pub fn interfaces(&self) -> &[InterfaceConfig] {
        match self {
            ModelConfig::UltraNova => &[
                InterfaceConfig {
                    interface_id: 0,
                    endpoint: None,
                },
                InterfaceConfig {
                    interface_id: 1,
                    endpoint: Some(EndpointConfig::Midi(EndpointInfo {
                        input: 3,
                        output: 3,
                    })),
                },
                InterfaceConfig {
                    interface_id: 3,
                    endpoint: Some(EndpointConfig::Control(EndpointInfo {
                        input: 5,
                        output: 5,
                    })),
                },
            ],
            ModelConfig::MiniNova => &[InterfaceConfig {
                interface_id: 0,
                endpoint: Some(EndpointConfig::Midi(EndpointInfo {
                    input: 1,
                    output: 2,
                })),
            }],
        }
    }
}

pub struct InterfaceConfig {
    pub interface_id: u8,
    pub endpoint: Option<EndpointConfig>,
}

#[derive(Debug, Copy, Clone)]
pub enum EndpointConfig {
    Midi(EndpointInfo),
    Control(EndpointInfo),
}

#[derive(Debug, Copy, Clone)]
pub struct EndpointInfo {
    pub input: u8,
    pub output: u8,
}
