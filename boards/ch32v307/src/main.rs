#![no_std]
#![no_main]
mod backend;
use backend::*;
use ch32_hal::{
    self as hal, bind_interrupts,
    gpio::{Input, Level, Output, OutputOpenDrain, Pull, Speed},
    peripherals::*,
    spi::{self, Spi},
    time::Hertz,
    usart::{self, Uart},
    usb::EndpointDataBuffer512,
    usbhs::{Driver, InterruptHandler, WakeupInterruptHandler},
};
use dedi_core::{
    aux::AuxState,
    bulk::Shared,
    flash::SingleFlash,
    gpio::{BoardGpio, Leds},
    handler::DediprogHandler,
    transport::{UsbIn, UsbOut},
};
use dedi_protocol::{aux::DeviceInfo, identity::DeviceIdentity};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_usb::Builder;
use panic_halt as _;
use static_cell::StaticCell;

bind_interrupts!(struct Irqs {
    USBHS => InterruptHandler<USBHS>;
    USBHS_WKUP => WakeupInterruptHandler<USBHS>;
    USART2 => usart::InterruptHandler<USART2>;
});
const EP_BUFFERS: usize = 5;
type UsbDriver = Driver<'static, USBHS, EP_BUFFERS, 512>;
type FlashShared = Shared<SingleFlash<FlashBus>, Leds<Output<'static>>, ChRecovery>;
type Gpio = BoardGpio<OutputOpenDrain<'static>, Input<'static>>;
type Out = <UsbDriver as embassy_usb::driver::Driver<'static>>::EndpointOut;
type In = <UsbDriver as embassy_usb::driver::Driver<'static>>::EndpointIn;
static AUX: AuxState = AuxState::new(DeviceInfo::new(
    2,
    <SingleFlash<FlashBus> as dedi_core::flash::Flash>::IO_MODES,
    3_000_000,
));

#[embassy_executor::main(entry = "qingke_rt::entry")]
async fn main(spawner: Spawner) -> ! {
    let p = hal::init(hal::Config {
        rcc: hal::rcc::Config::SYSCLK_FREQ_144MHZ_HSE,
        ..Default::default()
    });
    let mut cfg = spi::Config::default();
    cfg.frequency = Hertz::hz(24_000_000);
    let spi = Spi::new(p.SPI2, p.PB13, p.PB15, p.PB14, p.DMA1_CH5, p.DMA1_CH4, cfg);
    let flash = SingleFlash::new(FlashBus {
        spi,
        cs: Output::new(p.PB12, Level::High, Speed::Low),
    });
    let leds = Leds::new(
        [
            Some(Output::new(p.PC0, Level::Low, Speed::Low)),
            Some(Output::new(p.PC1, Level::Low, Speed::Low)),
            Some(Output::new(p.PC2, Level::Low, Speed::Low)),
        ],
        0,
    );
    static SHARED: StaticCell<FlashShared> = StaticCell::new();
    let shared = SHARED.init(Shared::new(flash, leds));
    static IDENTITY: StaticCell<DeviceIdentity> = StaticCell::new();
    let identity = IDENTITY.init(DeviceIdentity::from_mcu_id(&hal::signature::unique_id()));
    let uart = Uart::new(
        p.USART2,
        p.PA3,
        p.PA2,
        Irqs,
        p.DMA1_CH7,
        p.DMA1_CH6,
        usart::Config::default(),
    )
    .unwrap();
    let (tx, rx) = uart.split();
    let gpio = BoardGpio::new(
        OutputOpenDrain::new(p.PB8, Level::High, Speed::Low),
        OutputOpenDrain::new(p.PB9, Level::High, Speed::Low),
        Input::new(p.PB10, Pull::None),
        Input::new(p.PB11, Pull::None),
    );
    static EP_BUFFER: StaticCell<[EndpointDataBuffer512; EP_BUFFERS]> = StaticCell::new();
    let driver = Driver::new(
        p.USBHS,
        Irqs,
        p.PB7,
        p.PB6,
        EP_BUFFER.init(core::array::from_fn(|_| EndpointDataBuffer512::default())),
    );
    static CONFIG: StaticCell<[u8; 512]> = StaticCell::new();
    static BOS: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL: StaticCell<[u8; 128]> = StaticCell::new();
    let mut builder = Builder::new(
        driver,
        dedi_core::usb::config(identity),
        CONFIG.init([0; 512]),
        BOS.init([0; 256]),
        MSOS.init([0; 256]),
        CONTROL.init([0; 128]),
    );
    static HANDLER: StaticCell<
        DediprogHandler<'static, SingleFlash<FlashBus>, Leds<Output<'static>>, ChRecovery>,
    > = StaticCell::new();
    builder.handler(HANDLER.init(DediprogHandler::new(shared, identity)));
    let endpoints = dedi_core::usb::interfaces(&mut builder, 512, false);
    spawner.spawn(usb_task(builder.build()).unwrap());
    spawner.spawn(bulk_task(shared, endpoints.flash_in, endpoints.flash_out).unwrap());
    spawner.spawn(aux_task(endpoints.aux_out, endpoints.aux_in).unwrap());
    spawner.spawn(rx_task(ChRx(rx)).unwrap());
    spawner.spawn(tx_task(ChTx(tx)).unwrap());
    spawner.spawn(gpio_task(gpio).unwrap());
    loop {
        embassy_time::Timer::after_secs(3600).await;
    }
}
#[embassy_executor::task]
async fn usb_task(mut usb: embassy_usb::UsbDevice<'static, UsbDriver>) {
    usb.run().await;
}
#[embassy_executor::task]
async fn bulk_task(shared: &'static FlashShared, input: In, output: Out) {
    shared
        .run(
            UsbIn::<_, ChRecovery>::new(input, 512),
            UsbOut::<_, ChRecovery>::new(output),
        )
        .await;
}
#[embassy_executor::task]
async fn aux_task(output: Out, input: In) {
    AUX.run_usb(output, input).await;
}
#[embassy_executor::task]
async fn rx_task(rx: ChRx) {
    AUX.run_rx(rx).await;
}
#[embassy_executor::task]
async fn tx_task(tx: ChTx) {
    AUX.run_tx(tx).await;
}
#[embassy_executor::task]
async fn gpio_task(gpio: Gpio) {
    AUX.run_gpio(gpio).await;
}
