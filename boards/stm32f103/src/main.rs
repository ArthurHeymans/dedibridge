#![no_std]
#![no_main]
mod backend;
use backend::*;
use dedi_core::{
    aux::AuxState,
    bulk::Shared,
    flash::SingleFlash,
    gpio::{BoardGpio, Leds},
    handler::DediprogHandler,
    transport::{GuardedEndpoint, UsbIn, UsbOut},
};
use dedi_protocol::{aux::DeviceInfo, identity::DeviceIdentity};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_stm32::{
    self as hal, bind_interrupts,
    gpio::{Input, Level, Output, OutputOpenDrain, Pull, Speed},
    peripherals::*,
    rcc::*,
    spi::{self, Spi},
    time::Hertz,
    usart::{self, UartTx},
    usb::{Driver, InterruptHandler},
};
use embassy_usb::Builder;
use panic_probe as _;
use static_cell::StaticCell;

bind_interrupts!(struct Irqs {
    USB_LP_CAN1_RX0 => InterruptHandler<USB>;
    USART2 => RxInterrupt;
    DMA1_CHANNEL4 => hal::dma::InterruptHandler<DMA1_CH4>;
    DMA1_CHANNEL5 => hal::dma::InterruptHandler<DMA1_CH5>;
    DMA1_CHANNEL7 => hal::dma::InterruptHandler<DMA1_CH7>;
});
type UsbDriver = Driver<'static, USB>;
type FlashShared = Shared<SingleFlash<FlashBus>, Leds<Output<'static>>, StmRecovery>;
type Gpio = BoardGpio<OutputOpenDrain<'static>, Input<'static>>;
type Out = <UsbDriver as embassy_usb::driver::Driver<'static>>::EndpointOut;
type In = <UsbDriver as embassy_usb::driver::Driver<'static>>::EndpointIn;
static AUX: AuxState = AuxState::new(DeviceInfo::new(
    3,
    <SingleFlash<FlashBus> as dedi_core::flash::Flash>::IO_MODES,
    UART_CLOCK_HZ / 16,
));

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut config = hal::Config::default();
    config.rcc.hse = Some(Hse {
        freq: Hertz(8_000_000),
        mode: HseMode::Oscillator,
    });
    config.rcc.pll = Some(Pll {
        src: PllSource::HSE,
        prediv: PllPreDiv::DIV1,
        mul: PllMul::MUL9,
    });
    config.rcc.sys = Sysclk::PLL1_P;
    config.rcc.apb1_pre = APBPrescaler::DIV2;
    let p = hal::init(config);
    let mut cfg = spi::Config::default();
    cfg.frequency = Hertz(24_000_000);
    let spi = Spi::new(
        p.SPI2, p.PB13, p.PB15, p.PB14, p.DMA1_CH5, p.DMA1_CH4, Irqs, cfg,
    );
    let flash = SingleFlash::new(FlashBus {
        spi,
        cs: Output::new(p.PB12, Level::High, Speed::Low),
    });
    let leds = Leds::new(
        [
            Some(Output::new(p.PC13, Level::High, Speed::Low)),
            None,
            None,
        ],
        1,
    );
    static SHARED: StaticCell<FlashShared> = StaticCell::new();
    let shared = SHARED.init(Shared::new(flash, leds));
    static IDENTITY: StaticCell<DeviceIdentity> = StaticCell::new();
    let identity = IDENTITY.init(DeviceIdentity::from_mcu_id(&hal::uid::uid()));
    let tx = UartTx::new(p.USART2, p.PA2, p.DMA1_CH7, Irqs, usart::Config::default()).unwrap();
    let rx = StmRx::new(Input::new(p.PA3, Pull::None));
    let gpio = BoardGpio::new(
        OutputOpenDrain::new(p.PB8, Level::High, Speed::Low),
        OutputOpenDrain::new(p.PB9, Level::High, Speed::Low),
        Input::new(p.PB10, Pull::None),
        Input::new(p.PB11, Pull::None),
    );
    // F103 has no software-controlled D+ pull-up. With a board's external 1.5k
    // pull-up, drive D+ low briefly before handing the pin to USB to re-enumerate.
    let mut dp_pin = p.PA12;
    let mut dp = Output::new(dp_pin.reborrow(), Level::Low, Speed::Low);
    embassy_time::Timer::after_millis(10).await;
    dp.set_high();
    drop(dp);
    let driver = Driver::new(p.USB, Irqs, dp_pin, p.PA11);
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
        DediprogHandler<'static, SingleFlash<FlashBus>, Leds<Output<'static>>, StmRecovery>,
    > = StaticCell::new();
    builder.handler(HANDLER.init(DediprogHandler::new(shared, identity)));
    let endpoints = dedi_core::usb::interfaces(&mut builder, 64, false);
    spawner.spawn(usb_task(builder.build()).unwrap());
    spawner.spawn(bulk_task(shared, endpoints.flash_in, endpoints.flash_out).unwrap());
    spawner.spawn(aux_task(endpoints.aux_out, endpoints.aux_in).unwrap());
    spawner.spawn(rx_task(rx).unwrap());
    spawner.spawn(tx_task(StmTx(tx)).unwrap());
    spawner.spawn(gpio_task(gpio).unwrap());
}
#[embassy_executor::task]
async fn usb_task(mut usb: embassy_usb::UsbDevice<'static, UsbDriver>) {
    usb.run().await;
}
#[embassy_executor::task]
async fn bulk_task(shared: &'static FlashShared, input: In, output: Out) {
    shared
        .run(
            UsbIn::<_, StmRecovery>::new(input, 64),
            UsbOut::<_, StmRecovery>::new(output),
        )
        .await;
}
#[embassy_executor::task]
async fn aux_task(output: Out, input: In) {
    AUX.run_usb(
        GuardedEndpoint::<_, StmRecovery>::new(output),
        GuardedEndpoint::<_, StmRecovery>::new(input),
    )
    .await;
}
#[embassy_executor::task]
async fn rx_task(rx: StmRx) {
    AUX.run_rx(rx).await;
}
#[embassy_executor::task]
async fn tx_task(tx: StmTx) {
    AUX.run_tx(tx).await;
}
#[embassy_executor::task]
async fn gpio_task(gpio: Gpio) {
    AUX.run_gpio(gpio).await;
}
