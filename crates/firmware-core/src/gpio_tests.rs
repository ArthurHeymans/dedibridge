use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[derive(Clone)]
pub(crate) struct Pin(Arc<AtomicBool>);
impl Pin {
    pub(crate) fn new(high: bool) -> Self {
        Self(Arc::new(AtomicBool::new(high)))
    }
}
impl ErrorType for Pin {
    type Error = Infallible;
}
impl OutputPin for Pin {
    fn set_low(&mut self) -> Result<(), Infallible> {
        self.0.store(false, Ordering::SeqCst);
        Ok(())
    }
    fn set_high(&mut self) -> Result<(), Infallible> {
        self.0.store(true, Ordering::SeqCst);
        Ok(())
    }
}
impl InputPin for Pin {
    fn is_high(&mut self) -> Result<bool, Infallible> {
        Ok(self.0.load(Ordering::SeqCst))
    }
    fn is_low(&mut self) -> Result<bool, Infallible> {
        Ok(!self.0.load(Ordering::SeqCst))
    }
}
#[test]
fn open_drain_pulse_restores_state_and_inputs_cannot_be_outputs() {
    let reset = Pin::new(true);
    let monitor = reset.clone();
    let mut gpio = BoardGpio::new(reset, Pin::new(true), Pin::new(true), Pin::new(false));
    gpio.pulse_low(GPIO_RESET, 0);
    assert!(!monitor.0.load(Ordering::SeqCst));
    assert!(gpio.finish_pulse_if_due());
    assert!(monitor.0.load(Ordering::SeqCst));
    assert_eq!(gpio.state()[2], 0);
    gpio.set_direction(0xff, 0xff);
    gpio.set_output(0xff, 0);
    assert_eq!(gpio.state()[2], GPIO_RESET | GPIO_POWER);
    assert_eq!(gpio.state()[0] & GPIO_POWER_STATE, GPIO_POWER_STATE);
}
