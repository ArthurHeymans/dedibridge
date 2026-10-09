use super::*;
use futures::{executor::block_on, poll};
use std::{cell::Cell, vec, vec::Vec};

struct AaiBus {
    commands: Vec<Vec<u8>>,
    data: Vec<u8>,
    next: Option<usize>,
    wel: bool,
    selected: bool,
    words: usize,
    busy_reads: usize,
    fail_word: Option<usize>,
    stuck: bool,
}
impl AaiBus {
    fn new() -> Self {
        Self {
            commands: Vec::new(),
            data: vec![0xff; 512],
            next: None,
            wel: false,
            selected: false,
            words: 0,
            busy_reads: 0,
            fail_word: None,
            stuck: false,
        }
    }
}
impl SingleSpi for AaiBus {
    fn set_frequency(&mut self, _: u32) -> Result<(), Error> {
        Ok(())
    }
    fn select(&mut self, selected: bool) {
        self.selected = selected;
    }
    fn blocking_write(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.commands.push(bytes.to_vec());
        match bytes[0] {
            0x05 => {}
            0x06 => {
                assert!(self.next.is_none());
                assert_eq!(self.busy_reads, 0);
                self.wel = true;
            }
            0xad => {
                assert!(self.wel);
                assert_eq!(self.busy_reads, 0);
                let (addr, word) = if let Some(next) = self.next {
                    assert_eq!(bytes.len(), 3);
                    (next, &bytes[1..])
                } else {
                    assert_eq!(bytes.len(), 6);
                    let addr = (usize::from(bytes[1]) << 16)
                        | (usize::from(bytes[2]) << 8)
                        | usize::from(bytes[3]);
                    assert!(addr.is_multiple_of(2));
                    (addr, &bytes[4..])
                };
                for (dest, src) in self.data[addr..addr + 2].iter_mut().zip(word) {
                    *dest &= *src;
                }
                self.next = Some(addr + 2);
                self.busy_reads = 2;
                let fail = self.fail_word == Some(self.words);
                self.words += 1;
                if fail {
                    return Err(Error::Hardware);
                }
            }
            0x04 => {
                if self.stuck && self.words > 0 {
                    return Err(Error::Hardware);
                }
                assert_eq!(self.busy_reads, 0, "WRDI sent while a word is busy");
                self.next = None;
                self.wel = false;
            }
            _ => panic!("unexpected SPI command {bytes:x?}"),
        }
        Ok(())
    }
    fn blocking_read(&mut self, bytes: &mut [u8]) -> Result<(), Error> {
        if bytes.is_empty() {
            return Ok(());
        }
        assert_eq!(self.commands.last().unwrap(), &[0x05]);
        let busy = self.busy_reads > 0 || (self.stuck && self.words > 0);
        bytes.fill(u8::from(busy));
        self.busy_reads = self.busy_reads.saturating_sub(1);
        Ok(())
    }
    async fn write(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.blocking_write(bytes)
    }
    async fn read(&mut self, bytes: &mut [u8]) -> Result<(), Error> {
        self.blocking_read(bytes)
    }
}

#[test]
fn aai_stream_programs_words_and_exits_after_success_or_uncertain_transfer() {
    for fail_word in [None, Some(0), Some(1)] {
        let mut bus = AaiBus::new();
        bus.fail_word = fail_word;
        let mut flash = SingleFlash::new(bus);
        let data: Vec<_> = (0..256).map(|i| i as u8).collect();
        let result = block_on(flash.write_aai(0x100, &data, || false));
        assert_eq!(
            result,
            if fail_word.is_some() {
                Err(Error::Hardware)
            } else {
                Ok(())
            }
        );
        assert_eq!(flash.bus.next, None);
        assert!(!flash.bus.wel);
        assert!(!flash.bus.selected);
        assert_eq!(flash.bus.commands.last().unwrap(), &[0x04]);
        assert_eq!(
            flash.bus.commands.iter().filter(|c| c[0] == 0x06).count(),
            1
        );
        assert_eq!(
            flash.bus.commands.iter().find(|c| c[0] == 0xad).unwrap(),
            &[0xad, 0, 1, 0, data[0], data[1]]
        );
        if fail_word.is_none() {
            assert_eq!(&flash.bus.data[256..], data);
            assert_eq!(flash.bus.words, 128);
        }
        assert!(flash.bus.data[..256].iter().all(|b| *b == 0xff));
    }
}

#[test]
fn cancellation_waits_for_the_pending_word_and_sends_wrdi() {
    let mut flash = SingleFlash::new(AaiBus::new());
    let cancelled = Cell::new(false);
    block_on(async {
        let operation = flash.write_aai(0, &[0x55; 256], || cancelled.get());
        let mut operation = core::pin::pin!(operation);
        assert!(poll!(operation.as_mut()).is_pending());
        cancelled.set(true);
        assert_eq!(operation.await, Err(Error::Cancelled));
    });
    assert_eq!(flash.bus.words, 1);
    assert_eq!(flash.bus.next, None);
    assert!(!flash.bus.selected);
    assert_eq!(flash.bus.commands.last().unwrap(), &[0x04]);
}

#[test]
fn aai_timeout_is_bounded_and_still_attempts_wrdi() {
    let mut bus = AaiBus::new();
    bus.stuck = true;
    let mut flash = SingleFlash::new(bus);
    let start = Instant::now();
    assert_eq!(
        block_on(flash.write_aai(0, &[0x55; 2], || false)),
        Err(Error::FlashBusyTimeout)
    );
    assert!(Instant::now() - start < Duration::from_secs(2));
    assert_eq!(flash.bus.commands.last().unwrap(), &[0x04]);
    assert!(!flash.bus.selected);
}

#[test]
fn invalid_aai_ranges_do_not_touch_spi() {
    for (addr, len) in [(1, 2), (0, 1), (0x1000000, 2), (0xfffffe, 4)] {
        let mut flash = SingleFlash::new(AaiBus::new());
        assert_eq!(
            block_on(flash.write_aai(addr, &vec![0; len], || false)),
            Err(Error::Unsupported)
        );
        assert!(flash.bus.commands.is_empty());
    }
}
