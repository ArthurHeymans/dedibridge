#[cfg(test)]
#[path = "gpio_tests.rs"]
pub(crate) mod tests;

use core::convert::Infallible;
use dedi_protocol::aux::*;
use embassy_time::{Duration, Instant};
use embedded_hal::digital::{ErrorType, InputPin, OutputPin};

pub trait LedControl {
    fn set(&mut self, state: u8);
}

pub struct Leds<P> {
    pins: [Option<P>; 3],
    active_low: u8,
}
impl<P: OutputPin<Error = Infallible>> Leds<P> {
    pub fn new(pins: [Option<P>; 3], active_low: u8) -> Self {
        let mut leds = Self { pins, active_low };
        leds.set(0);
        leds
    }
}
impl<P: OutputPin<Error = Infallible>> LedControl for Leds<P> {
    fn set(&mut self, state: u8) {
        for (index, pin) in self.pins.iter_mut().enumerate() {
            if let Some(pin) = pin {
                pin.set_state(((state ^ self.active_low) & (1 << index) != 0).into())
                    .unwrap();
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Pulse {
    pins: u8,
    directions: u8,
    outputs: u8,
    deadline: Instant,
}

pub struct BoardGpio<O, I> {
    reset: O,
    power: O,
    power_state: I,
    aux_state: I,
    outputs: u8,
    directions: u8,
    pulse: Option<Pulse>,
}

pub trait BoardIo {
    fn state(&mut self) -> [u8; 4];
    fn set_direction(&mut self, mask: u8, directions: u8);
    fn set_output(&mut self, mask: u8, values: u8);
    fn pulse_low(&mut self, mask: u8, ms: u16);
    fn pulse_deadline(&self) -> Option<Instant>;
    fn finish_pulse_if_due(&mut self) -> bool;
}

impl<O: OutputPin + InputPin + ErrorType<Error = Infallible>, I: InputPin<Error = Infallible>>
    BoardGpio<O, I>
{
    pub fn new(reset: O, power: O, power_state: I, aux_state: I) -> Self {
        let mut gpio = Self {
            reset,
            power,
            power_state,
            aux_state,
            outputs: GPIO_RESET | GPIO_POWER,
            directions: 0,
            pulse: None,
        };
        gpio.apply();
        gpio
    }
    fn apply(&mut self) {
        let low = self.directions & !self.outputs;
        self.reset
            .set_state((low & GPIO_RESET == 0).into())
            .unwrap();
        self.power
            .set_state((low & GPIO_POWER == 0).into())
            .unwrap();
    }
    fn finish_pulse(&mut self) {
        if let Some(pulse) = self.pulse.take() {
            self.outputs = (self.outputs & !pulse.pins) | (pulse.outputs & pulse.pins);
            self.directions = (self.directions & !pulse.pins) | (pulse.directions & pulse.pins);
            self.apply();
        }
    }
}

impl<O: OutputPin + InputPin + ErrorType<Error = Infallible>, I: InputPin<Error = Infallible>>
    BoardIo for BoardGpio<O, I>
{
    fn state(&mut self) -> [u8; 4] {
        let inputs = (u8::from(self.reset.is_high().unwrap()) * GPIO_RESET)
            | (u8::from(self.power.is_high().unwrap()) * GPIO_POWER)
            | (u8::from(self.power_state.is_high().unwrap()) * GPIO_POWER_STATE)
            | (u8::from(self.aux_state.is_high().unwrap()) * GPIO_AUX);
        [
            inputs,
            self.outputs,
            self.directions,
            GPIO_COUNT | CAP_OPEN_DRAIN | CAP_PULSE,
        ]
    }
    fn set_direction(&mut self, mask: u8, directions: u8) {
        self.finish_pulse();
        let mask = mask & (GPIO_RESET | GPIO_POWER);
        self.directions = (self.directions & !mask) | (directions & mask);
        self.apply();
    }
    fn set_output(&mut self, mask: u8, values: u8) {
        self.finish_pulse();
        let mask = mask & (GPIO_RESET | GPIO_POWER);
        self.outputs = (self.outputs & !mask) | (values & mask);
        self.apply();
    }
    fn pulse_low(&mut self, mask: u8, ms: u16) {
        self.finish_pulse();
        let pins = mask & (GPIO_RESET | GPIO_POWER);
        self.pulse = Some(Pulse {
            pins,
            directions: self.directions,
            outputs: self.outputs,
            deadline: Instant::now() + Duration::from_millis(u64::from(ms)),
        });
        self.directions |= pins;
        self.outputs &= !pins;
        self.apply();
    }
    fn pulse_deadline(&self) -> Option<Instant> {
        self.pulse.map(|pulse| pulse.deadline)
    }
    fn finish_pulse_if_due(&mut self) -> bool {
        if self.pulse.is_some_and(|p| p.deadline <= Instant::now()) {
            self.finish_pulse();
            true
        } else {
            false
        }
    }
}
