// Dediprog protocol command codes and types.

// =============================================================================
// Command codes (bRequest values for vendor control transfers)
// =============================================================================

pub const CMD_TRANSCEIVE: u8 = 0x01;
#[allow(dead_code)]
pub const CMD_POLL_STATUS_REG: u8 = 0x02;
pub const CMD_SET_VPP: u8 = 0x03;
pub const CMD_SET_TARGET: u8 = 0x04;
pub const CMD_READ_EEPROM: u8 = 0x05;
pub const CMD_SET_IO_LED: u8 = 0x07;
pub const CMD_READ_PROG_INFO: u8 = 0x08;
pub const CMD_SET_VCC: u8 = 0x09;
pub const CMD_SET_STANDALONE: u8 = 0x0A;
pub const CMD_SET_VOLTAGE: u8 = 0x0B;
pub const CMD_GET_BUTTON: u8 = 0x11;
pub const CMD_GET_UID: u8 = 0x12;
pub const CMD_SET_CS: u8 = 0x14;
pub const CMD_IO_MODE: u8 = 0x15;
pub const CMD_READ_FPGA_VERSION: u8 = 0x1C;
pub const CMD_SET_HOLD: u8 = 0x1D;
pub const CMD_READ: u8 = 0x20;
pub const CMD_WRITE: u8 = 0x30;
pub const CMD_SET_SPI_CLK: u8 = 0x61;
pub const CMD_CHECK_SOCKET: u8 = 0x62;

// =============================================================================
// SPI speed codes (wValue for CMD_SET_SPI_CLK)
// =============================================================================

#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[repr(u8)]
pub enum SpiSpeed {
    Mhz24 = 0,
    Mhz8 = 1,
    Mhz12 = 2,
    Mhz3 = 3,
    Mhz2_18 = 4,
    Mhz1_5 = 5,
    Khz750 = 6,
    Khz375 = 7,
}

impl SpiSpeed {
    pub fn from_code(code: u16) -> Self {
        match code {
            0 => SpiSpeed::Mhz24,
            1 => SpiSpeed::Mhz8,
            2 => SpiSpeed::Mhz12,
            3 => SpiSpeed::Mhz3,
            4 => SpiSpeed::Mhz2_18,
            5 => SpiSpeed::Mhz1_5,
            6 => SpiSpeed::Khz750,
            7 => SpiSpeed::Khz375,
            _ => SpiSpeed::Mhz12, // default
        }
    }

    pub fn frequency_hz(self) -> u32 {
        match self {
            SpiSpeed::Mhz24 => 24_000_000,
            SpiSpeed::Mhz8 => 8_000_000,
            SpiSpeed::Mhz12 => 12_000_000,
            SpiSpeed::Mhz3 => 3_000_000,
            SpiSpeed::Mhz2_18 => 2_180_000,
            SpiSpeed::Mhz1_5 => 1_500_000,
            SpiSpeed::Khz750 => 750_000,
            SpiSpeed::Khz375 => 375_000,
        }
    }
}

// =============================================================================
// SPI I/O lane modes (Dediprog CMD_IO_MODE wValue)
// =============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[repr(u8)]
pub enum IoMode {
    Single = 0,  // 1-1-1
    DualOut = 1, // 1-1-2
    DualIo = 2,  // 1-2-2
    QuadOut = 3, // 1-1-4
    QuadIo = 4,  // 1-4-4
    Qpi = 5,     // 4-4-4, not advertised by Dediprog iomode=quad
}

impl IoMode {
    pub fn from_dediprog_value(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::Single),
            1 => Some(Self::DualOut),
            2 => Some(Self::DualIo),
            3 => Some(Self::QuadOut),
            4 => Some(Self::QuadIo),
            5 => Some(Self::Qpi),
            _ => None,
        }
    }

    pub fn read_width(self) -> u8 {
        match self {
            Self::Single => 1,
            Self::DualOut | Self::DualIo => 2,
            Self::QuadOut | Self::QuadIo | Self::Qpi => 4,
        }
    }

    pub fn address_width(self) -> u8 {
        match self {
            Self::DualIo => 2,
            Self::QuadIo | Self::Qpi => 4,
            Self::Single | Self::DualOut | Self::QuadOut => 1,
        }
    }

    pub fn needs_mode_byte(self) -> bool {
        matches!(self, Self::DualIo | Self::QuadIo | Self::Qpi)
    }

    pub fn from_read_opcode(opcode: u8, requested: Self) -> Self {
        match opcode {
            0x3b | 0x3c => Self::DualOut,
            0xbb | 0xbc => Self::DualIo,
            0x6b | 0x6c => Self::QuadOut,
            0xeb | 0xec => Self::QuadIo,
            _ => requested,
        }
    }
}

// =============================================================================
// Read modes (byte 3 of read command packet)
// =============================================================================

#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[repr(u8)]
pub enum ReadMode {
    Std = 1,
    Fast = 2,
    AtmelFast = 3,
    Addr4bFast = 4,
    Addr4bFast0x0C = 5,
    Configurable = 9,
}

impl ReadMode {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(ReadMode::Std),
            2 => Some(ReadMode::Fast),
            3 => Some(ReadMode::AtmelFast),
            4 => Some(ReadMode::Addr4bFast),
            5 => Some(ReadMode::Addr4bFast0x0C),
            9 => Some(ReadMode::Configurable),
            _ => None,
        }
    }

    /// Number of single-lane dummy bytes needed by legacy read modes.
    pub fn dummy_bytes(self) -> u8 {
        match self {
            ReadMode::Std => 0,
            ReadMode::Fast
            | ReadMode::AtmelFast
            | ReadMode::Addr4bFast
            | ReadMode::Addr4bFast0x0C => 1,
            ReadMode::Configurable => 0,
        }
    }

    /// Whether this mode uses 4-byte addressing.
    pub fn uses_4byte_addr(self) -> bool {
        matches!(self, ReadMode::Addr4bFast | ReadMode::Addr4bFast0x0C)
    }
}

// =============================================================================
// Write modes (byte 3 of write command packet)
// =============================================================================

#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[repr(u8)]
pub enum WriteMode {
    PageProgram = 1,
    Aai2Byte = 4,
    Addr4bPageProgram = 9,
    Addr4bPageProgram0x12 = 11,
}

impl WriteMode {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(WriteMode::PageProgram),
            4 => Some(WriteMode::Aai2Byte),
            9 => Some(WriteMode::Addr4bPageProgram),
            11 => Some(WriteMode::Addr4bPageProgram0x12),
            _ => None,
        }
    }

    pub fn uses_4byte_addr(self) -> bool {
        matches!(
            self,
            WriteMode::Addr4bPageProgram | WriteMode::Addr4bPageProgram0x12
        )
    }
}

// =============================================================================
// Bulk operation descriptors
// =============================================================================

#[derive(Clone, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum BulkOperation {
    Read {
        address: u32,
        block_count: u16,
        opcode: u8,
        addr_len: u8,
        io_mode: IoMode,
        mode_byte: Option<u8>,
        dummy_cycles: u8,
    },
    Write {
        address: u32,
        block_count: u16,
        opcode: u8,
        addr_len: u8,
        mode: WriteMode,
    },
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct ReadSetup {
    pub block_count: u16,
    pub mode_byte: u8,
    pub opcode: u8,
    pub address: u32,
    pub addr_len: u8,
    pub dummy_cycles: u8,
}

// =============================================================================
// Command packet parsing (Protocol V2/V3)
// =============================================================================

/// Parse a V2/V3 read/write command packet.
/// Returns (block_count, mode_byte, opcode, start_address).
pub fn parse_rw_cmd_v2(data: &[u8]) -> Option<(u16, u8, u8, u32)> {
    use zerocopy::byteorder::{LittleEndian, U16, U32};
    use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
    #[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
    #[repr(C)]
    struct Setup {
        blocks: U16<LittleEndian>,
        reserved0: u8,
        mode: u8,
        opcode: u8,
        reserved1: u8,
        address: U32<LittleEndian>,
    }
    let setup = Setup::read_from_bytes(data.get(..10)?).ok()?;
    if setup.blocks.get() == 0 {
        return None;
    }
    Some((
        setup.blocks.get(),
        setup.mode,
        setup.opcode,
        setup.address.get(),
    ))
}

pub fn parse_read_setup(data: &[u8]) -> Option<ReadSetup> {
    let (block_count, mode_byte, opcode, address) = parse_rw_cmd_v2(data)?;
    let read_mode = ReadMode::from_byte(mode_byte);

    let read_mode = read_mode?;
    if matches!(read_mode, ReadMode::Configurable) && data.len() < 12 {
        return None;
    }
    let read_mode = Some(read_mode);
    let opcode = if opcode != 0 { opcode } else { 0x03 };
    let (addr_len, dummy_cycles) =
        if matches!(read_mode, Some(ReadMode::Configurable)) && data.len() >= 12 {
            // Dediprog protocol V3 stores address length directly. Byte 11 is half
            // the actual dummy clock count, mirroring flashprog's prepare_rw_cmd_v3().
            let addr_len = match data[10] {
                4 => 4,
                _ => 3,
            };
            (addr_len, data[11].saturating_mul(2))
        } else {
            let addr_len = match read_mode {
                Some(mode) if mode.uses_4byte_addr() => 4,
                _ => match opcode {
                    0x13 | 0x0c | 0x3c | 0xbc | 0x6c | 0xec => 4,
                    _ => 3,
                },
            };

            let dummy_cycles = match opcode {
                0x03 | 0x13 => 0,
                0x0b | 0x0c => 8,
                0x3b | 0x3c => 8,
                0xbb | 0xbc => 4,
                0x6b | 0x6c => 8,
                0xeb | 0xec => 6,
                _ => read_mode.map(|m| m.dummy_bytes() * 8).unwrap_or(0),
            };
            (addr_len, dummy_cycles)
        };

    Some(ReadSetup {
        block_count,
        mode_byte,
        opcode,
        address,
        addr_len,
        dummy_cycles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configurable_read_validates_length_and_uses_dummy_clocks() {
        let setup = [1, 0, 0, 9, 0x0c, 0, 0x78, 0x56, 0x34, 0x12, 4, 4];
        let parsed = parse_read_setup(&setup).unwrap();
        assert_eq!(parsed.address, 0x12345678);
        assert_eq!((parsed.addr_len, parsed.dummy_cycles), (4, 8));
        assert!(parse_read_setup(&setup[..10]).is_none());
        assert!(parse_read_setup(&[0, 0, 0, 1, 3, 0, 0, 0, 0, 0]).is_none());
        assert_eq!(
            IoMode::from_read_opcode(0xeb, IoMode::Single),
            IoMode::QuadIo
        );
    }
}
