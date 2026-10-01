use dedi_protocol::{USB_PID, USB_VID, identity::DeviceIdentity};
use embassy_usb::{
    Builder,
    driver::{Direction, Driver, EndpointAddress},
};

pub fn config(identity: &DeviceIdentity) -> embassy_usb::Config<'_> {
    let mut config = embassy_usb::Config::new(USB_VID, USB_PID);
    config.manufacturer = Some("DediProg");
    config.product = Some("SF600");
    config.serial_number = Some(identity.usb_serial());
    config.max_power = 200;
    config.max_packet_size_0 = 64;
    config
}

pub struct Endpoints<'d, D: Driver<'d>> {
    pub flash_out: D::EndpointOut,
    pub flash_in: D::EndpointIn,
    pub aux_out: D::EndpointOut,
    pub aux_in: D::EndpointIn,
}

/// Protocol addresses are fixed; only RP2040 needs the EP15 DPRAM reservation.
pub fn interfaces<'d, D: Driver<'d>>(
    builder: &mut Builder<'d, D>,
    packet_size: u16,
    reserve_rp_double_buffer: bool,
) -> Endpoints<'d, D> {
    let address = |number, direction| Some(EndpointAddress::from_parts(number, direction));
    let mut function = builder.function(0xff, 0, 0);
    let mut interface = function.interface();
    let mut alt = interface.alt_setting(0xff, 0, 0, None);
    let flash_out = alt.endpoint_bulk_out(address(1, Direction::Out), packet_size);
    let flash_in = alt.endpoint_bulk_in(address(2, Direction::In), packet_size);
    if reserve_rp_double_buffer {
        alt.endpoint_interrupt_in(address(15, Direction::In), packet_size, 1);
    }
    drop(function);
    let mut function = builder.function(0xff, 0xd1, 1);
    let mut interface = function.interface();
    let mut alt = interface.alt_setting(0xff, 0xd1, 1, None);
    // High-speed bulk descriptors require 512-byte MPS. Auxiliary messages
    // remain <=64 bytes, so they are short packets on CH32 and one packet on FS.
    let aux_out = alt.endpoint_bulk_out(address(3, Direction::Out), packet_size);
    let aux_in = alt.endpoint_bulk_in(address(4, Direction::In), packet_size);
    Endpoints {
        flash_out,
        flash_in,
        aux_out,
        aux_in,
    }
}
