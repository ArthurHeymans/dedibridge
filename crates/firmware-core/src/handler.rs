use crate::{bulk::Shared, flash::Flash, gpio::LedControl, transport::Recovery};
use dedi_protocol::{identity::DeviceIdentity, sf600::*};
use embassy_usb::{
    Handler,
    control::{InResponse, OutResponse, Request, RequestType},
};

pub struct DediprogHandler<'d, F, L, R> {
    shared: &'d Shared<F, L, R>,
    identity: &'d DeviceIdentity,
    response: [u8; 16],
    response_len: usize,
    io_mode: IoMode,
}
impl<'d, F: Flash, L: LedControl, R: Recovery> DediprogHandler<'d, F, L, R> {
    pub fn new(shared: &'d Shared<F, L, R>, identity: &'d DeviceIdentity) -> Self {
        Self {
            shared,
            identity,
            response: [0; 16],
            response_len: 0,
            io_mode: IoMode::Single,
        }
    }
    fn read_setup(&self, data: &[u8]) -> bool {
        let Some(setup) = parse_read_setup(data) else {
            return false;
        };
        let mode = IoMode::from_read_opcode(setup.opcode, self.io_mode);
        if F::IO_MODES & (1 << mode as u8) == 0 {
            return false;
        }
        let mode_byte = mode.needs_mode_byte().then_some(0xff);
        let dummy_cycles = setup.dummy_cycles.saturating_sub(if mode_byte.is_some() {
            8 / mode.address_width()
        } else {
            0
        });
        if mode == IoMode::Single && !dummy_cycles.is_multiple_of(8) {
            return false;
        }
        self.shared.submit(BulkOperation::Read {
            address: setup.address,
            block_count: setup.block_count,
            opcode: setup.opcode,
            addr_len: setup.addr_len,
            io_mode: mode,
            mode_byte,
            dummy_cycles,
        })
    }
    fn write_setup(&self, data: &[u8]) -> bool {
        let Some((block_count, raw_mode, opcode, address)) = parse_rw_cmd_v2(data) else {
            return false;
        };
        let Some(mode) = WriteMode::from_byte(raw_mode) else {
            return false;
        };
        if matches!(mode, WriteMode::Aai2Byte) {
            return false;
        }
        self.shared.submit(BulkOperation::Write {
            address,
            block_count,
            opcode: if opcode == 0 { 2 } else { opcode },
            addr_len: if mode.uses_4byte_addr() { 4 } else { 3 },
        })
    }
}
impl<F: Flash, L: LedControl, R: Recovery> Handler for DediprogHandler<'_, F, L, R> {
    fn configured(&mut self, configured: bool) {
        if !configured {
            self.shared.cancel();
        }
    }
    fn reset(&mut self) {
        self.shared.cancel();
        self.response_len = 0;
        self.io_mode = IoMode::Single;
    }
    fn control_out(&mut self, req: Request, data: &[u8]) -> Option<OutResponse> {
        if req.request_type != RequestType::Vendor {
            return None;
        }
        let accepted = match req.request {
            CMD_TRANSCEIVE => {
                self.response_len = 0;
                if data.is_empty() || data.len() > 16 {
                    false
                } else {
                    let len = if req.value & 1 != 0 { 16 } else { 0 };
                    let result = self
                        .shared
                        .control(|flash| flash.transceive(data, &mut self.response[..len]));
                    if matches!(result, Some(Ok(()))) {
                        self.response_len = len;
                        true
                    } else {
                        false
                    }
                }
            }
            CMD_SET_IO_LED => {
                self.shared.set_leds(((req.value >> 8) as u8) ^ 7);
                true
            }
            CMD_SET_SPI_CLK => matches!(
                self.shared.control(
                    |flash| flash.set_frequency(SpiSpeed::from_code(req.value).frequency_hz())
                ),
                Some(Ok(()))
            ),
            CMD_SET_CS => self
                .shared
                .control(|flash| flash.select(req.value == 0))
                .is_some(),
            CMD_IO_MODE => match IoMode::from_dediprog_value(req.value) {
                Some(mode) if F::IO_MODES & (1 << mode as u8) != 0 => {
                    self.io_mode = mode;
                    true
                }
                _ => false,
            },
            CMD_READ => self.read_setup(data),
            CMD_WRITE => self.write_setup(data),
            CMD_SET_VPP | CMD_SET_TARGET | CMD_SET_VCC | CMD_SET_STANDALONE | CMD_SET_HOLD => true,
            _ => false,
        };
        Some(if accepted {
            OutResponse::Accepted
        } else {
            OutResponse::Rejected
        })
    }
    fn control_in<'a>(&'a mut self, req: Request, buf: &'a mut [u8]) -> Option<InResponse<'a>> {
        if req.request_type != RequestType::Vendor {
            return None;
        }
        let data: &[u8] = match req.request {
            CMD_TRANSCEIVE => &self.response[..self.response_len],
            CMD_READ_EEPROM => self.identity.eeprom(),
            CMD_READ_PROG_INFO => self.identity.device_string(),
            CMD_GET_UID => self.identity.unique_id(),
            CMD_SET_VOLTAGE => &[0x6f],
            CMD_GET_BUTTON => &[1],
            CMD_READ_FPGA_VERSION => &[0, 1],
            CMD_CHECK_SOCKET => &[0],
            _ => return Some(InResponse::Rejected),
        };
        let len = data.len().min(buf.len()).min(req.length.into());
        buf[..len].copy_from_slice(&data[..len]);
        Some(InResponse::Accepted(&buf[..len]))
    }
}
