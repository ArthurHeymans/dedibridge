#![no_std]
#![no_main]
mod backend;
mod fast_bulk_in;
mod spi_flash;
mod spi_flash_programs;
mod config {
    pub use dedi_core::config::*;
    pub const USB_MAX_PACKET_SIZE: u16 = 64;
}
mod protocol {
    pub use dedi_protocol::sf600::*;
}

use backend::*;
use dedi_core::{
    aux::AuxState,
    bulk::Shared,
    gpio::{BoardGpio, Leds},
    handler::DediprogHandler,
    transport::{GuardedEndpoint, UsbOut},
};
use dedi_protocol::{
    aux::DeviceInfo,
    identity::{DeviceIdentity, UNIQUE_ID_LEN},
};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_rp::{
    bind_interrupts,
    dma::{Channel, InterruptHandler as DmaIrq},
    flash::Blocking,
    gpio::{Flex, Input, Level, Output, OutputOpenDrain, Pull},
    peripherals::*,
    pio::{InterruptHandler as PioIrq, Pio},
    pio_programs::uart::*,
    usb::{Driver, InterruptHandler as UsbIrq},
};
use embassy_usb::Builder;
use panic_probe as _;
use spi_flash::SpiFlash;
use static_cell::StaticCell;

bind_interrupts!(pub(crate) struct Irqs {
    USBCTRL_IRQ => UsbIrq<USB>;
    PIO0_IRQ_0 => PioIrq<PIO0>;
    PIO1_IRQ_0 => PioIrq<PIO1>;
    DMA_IRQ_0 => DmaIrq<DMA_CH0>, DmaIrq<DMA_CH1>;
});
pub(crate) type UsbDriver = Driver<'static, USB>;
type FlashShared = Shared<SpiFlash<'static>, Leds<Output<'static>>, PicoRecovery>;
type Gpio = BoardGpio<OutputOpenDrain<'static>, Input<'static>>;
type Out = <UsbDriver as embassy_usb::driver::Driver<'static>>::EndpointOut;
type In = <UsbDriver as embassy_usb::driver::Driver<'static>>::EndpointIn;
static AUX: AuxState = AuxState::new(DeviceInfo::new(
    1,
    <SpiFlash<'static> as dedi_core::flash::Flash>::IO_MODES,
    3_000_000,
));

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let mut onboard =
        embassy_rp::flash::Flash::<_, Blocking, { 2 * 1024 * 1024 }>::new_blocking(p.FLASH);
    let mut uid = [0; UNIQUE_ID_LEN];
    onboard
        .blocking_unique_id(&mut uid)
        .expect("onboard flash ID");
    static IDENTITY: StaticCell<DeviceIdentity> = StaticCell::new();
    let identity = IDENTITY.init(DeviceIdentity::from_unique_id(uid));
    force_flash_bus_deselected_at_startup();
    let mut cs = Flex::new(p.PIN_7);
    cs.set_high();
    cs.set_as_output();
    let flash = SpiFlash::new(
        Pio::new(p.PIO0, Irqs),
        Channel::new(p.DMA_CH0, Irqs),
        Channel::new(p.DMA_CH1, Irqs),
        p.PIN_3,
        p.PIN_4,
        p.PIN_5,
        p.PIN_6,
        p.PIN_2,
        cs,
    );
    let leds = Leds::new(
        [
            Some(Output::new(p.PIN_25, Level::Low)),
            Some(Output::new(p.PIN_14, Level::Low)),
            Some(Output::new(p.PIN_15, Level::Low)),
        ],
        0,
    );
    static SHARED: StaticCell<FlashShared> = StaticCell::new();
    let shared = SHARED.init(Shared::new(flash, leds));
    let gpio = BoardGpio::new(
        OutputOpenDrain::new(p.PIN_8, Level::High),
        OutputOpenDrain::new(p.PIN_9, Level::High),
        Input::new(p.PIN_10, Pull::None),
        Input::new(p.PIN_11, Pull::None),
    );
    let mut pio = Pio::new(p.PIO1, Irqs);
    let tx_program = PioUartTxProgram::new(&mut pio.common);
    let rx_program = PioUartRxProgram::new(&mut pio.common);
    let tx = PicoTx(PioUartTx::new(
        115200,
        &mut pio.common,
        pio.sm0,
        p.PIN_0,
        &tx_program,
    ));
    let rx = PicoRx(PioUartRx::new(
        115200,
        &mut pio.common,
        pio.sm1,
        p.PIN_1,
        &rx_program,
    ));
    static CONFIG: StaticCell<[u8; 512]> = StaticCell::new();
    static BOS: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL: StaticCell<[u8; 128]> = StaticCell::new();
    let mut builder = Builder::new(
        Driver::new(p.USB, Irqs),
        dedi_core::usb::config(identity),
        CONFIG.init([0; 512]),
        BOS.init([0; 256]),
        MSOS.init([0; 256]),
        CONTROL.init([0; 128]),
    );
    static HANDLER: StaticCell<
        DediprogHandler<'static, SpiFlash<'static>, Leds<Output<'static>>, PicoRecovery>,
    > = StaticCell::new();
    builder.handler(HANDLER.init(DediprogHandler::new(shared, identity)));
    let endpoints = dedi_core::usb::interfaces(&mut builder, 64, true);
    spawner.spawn(usb_task(builder.build()).unwrap());
    spawner.spawn(bulk_task(shared, endpoints.flash_in, endpoints.flash_out).unwrap());
    spawner.spawn(aux_task(endpoints.aux_out, endpoints.aux_in).unwrap());
    spawner.spawn(rx_task(rx).unwrap());
    spawner.spawn(tx_task(tx).unwrap());
    spawner.spawn(gpio_task(gpio).unwrap());
}
fn force_flash_bus_deselected_at_startup() {
    use embassy_rp::pac;
    let mask = (1 << 5) | (1 << 6) | (1 << 7);
    pac::SIO.gpio_out(0).value_set().write_value(mask);
    pac::SIO.gpio_oe(0).value_set().write_value(mask);
    for pin in [5, 6, 7] {
        pac::PADS_BANK0.gpio(pin).modify(|w| {
            w.set_ie(true);
            w.set_od(false);
            w.set_pue(false);
            w.set_pde(false);
        });
        pac::IO_BANK0.gpio(pin).ctrl().write(|w| {
            w.set_funcsel(pac::io::vals::Gpio0ctrlFuncsel::SIO_0 as _);
            w.set_outover(pac::io::vals::Outover::NORMAL);
            w.set_oeover(pac::io::vals::Oeover::NORMAL);
        });
    }
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}
#[embassy_executor::task]
async fn usb_task(mut usb: embassy_usb::UsbDevice<'static, UsbDriver>) {
    usb.run().await;
}
#[embassy_executor::task]
async fn bulk_task(shared: &'static FlashShared, input: In, output: Out) {
    shared
        .run(PicoIn::new(input), UsbOut::<_, PicoRecovery>::new(output))
        .await;
}
#[embassy_executor::task]
async fn aux_task(output: Out, input: In) {
    AUX.run_usb(
        GuardedEndpoint::<_, PicoRecovery>::new(output),
        GuardedEndpoint::<_, PicoRecovery>::new(input),
    )
    .await;
}
#[embassy_executor::task]
async fn rx_task(rx: PicoRx) {
    AUX.run_rx(rx).await;
}
#[embassy_executor::task]
async fn tx_task(tx: PicoTx) {
    AUX.run_tx(tx).await;
}
#[embassy_executor::task]
async fn gpio_task(gpio: Gpio) {
    AUX.run_gpio(gpio).await;
}
