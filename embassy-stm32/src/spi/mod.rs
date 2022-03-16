#![macro_use]

use core::marker::PhantomData;
use core::ptr;
use embassy::util::Unborrow;
use embassy_hal_common::unborrow;
use futures::future::join;

use self::sealed::WordSize;
use crate::dma::{slice_ptr_parts, NoDma, Transfer};
use crate::gpio::sealed::{AFType, Pin as _};
use crate::gpio::AnyPin;
use crate::pac::spi::Spi as Regs;
use crate::pac::spi::{regs, vals};
use crate::peripherals;
use crate::rcc::RccPeripheral;
use crate::time::Hertz;

pub use embedded_hal_02::spi::{Mode, Phase, Polarity, MODE_0, MODE_1, MODE_2, MODE_3};

#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Error {
    Framing,
    Crc,
    ModeFault,
    Overrun,
}

// TODO move upwards in the tree
#[derive(Copy, Clone)]
pub enum BitOrder {
    LsbFirst,
    MsbFirst,
}

#[non_exhaustive]
#[derive(Copy, Clone)]
pub struct Config {
    pub mode: Mode,
    pub bit_order: BitOrder,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: MODE_0,
            bit_order: BitOrder::MsbFirst,
        }
    }
}

impl Config {
    fn raw_phase(&self) -> vals::Cpha {
        match self.mode.phase {
            Phase::CaptureOnSecondTransition => vals::Cpha::SECONDEDGE,
            Phase::CaptureOnFirstTransition => vals::Cpha::FIRSTEDGE,
        }
    }

    fn raw_polarity(&self) -> vals::Cpol {
        match self.mode.polarity {
            Polarity::IdleHigh => vals::Cpol::IDLEHIGH,
            Polarity::IdleLow => vals::Cpol::IDLELOW,
        }
    }

    fn raw_byte_order(&self) -> vals::Lsbfirst {
        match self.bit_order {
            BitOrder::LsbFirst => vals::Lsbfirst::LSBFIRST,
            BitOrder::MsbFirst => vals::Lsbfirst::MSBFIRST,
        }
    }
}

pub struct Spi<'d, T: Instance, Tx, Rx> {
    sck: Option<AnyPin>,
    mosi: Option<AnyPin>,
    miso: Option<AnyPin>,
    txdma: Tx,
    rxdma: Rx,
    current_word_size: WordSize,
    phantom: PhantomData<&'d mut T>,
}

impl<'d, T: Instance, Tx, Rx> Spi<'d, T, Tx, Rx> {
    pub fn new<F>(
        peri: impl Unborrow<Target = T> + 'd,
        sck: impl Unborrow<Target = impl SckPin<T>> + 'd,
        mosi: impl Unborrow<Target = impl MosiPin<T>> + 'd,
        miso: impl Unborrow<Target = impl MisoPin<T>> + 'd,
        txdma: impl Unborrow<Target = Tx> + 'd,
        rxdma: impl Unborrow<Target = Rx> + 'd,
        freq: F,
        config: Config,
    ) -> Self
    where
        F: Into<Hertz>,
    {
        unborrow!(sck, mosi, miso);
        unsafe {
            sck.set_as_af(sck.af_num(), AFType::OutputPushPull);
            #[cfg(any(spi_v2, spi_v3))]
            sck.set_speed(crate::gpio::Speed::VeryHigh);
            mosi.set_as_af(mosi.af_num(), AFType::OutputPushPull);
            #[cfg(any(spi_v2, spi_v3))]
            mosi.set_speed(crate::gpio::Speed::VeryHigh);
            miso.set_as_af(miso.af_num(), AFType::Input);
            #[cfg(any(spi_v2, spi_v3))]
            miso.set_speed(crate::gpio::Speed::VeryHigh);
        }

        Self::new_inner(
            peri,
            Some(sck.degrade()),
            Some(mosi.degrade()),
            Some(miso.degrade()),
            txdma,
            rxdma,
            freq,
            config,
        )
    }

    pub fn new_rxonly<F>(
        peri: impl Unborrow<Target = T> + 'd,
        sck: impl Unborrow<Target = impl SckPin<T>> + 'd,
        miso: impl Unborrow<Target = impl MisoPin<T>> + 'd,
        txdma: impl Unborrow<Target = Tx> + 'd, // TODO remove
        rxdma: impl Unborrow<Target = Rx> + 'd,
        freq: F,
        config: Config,
    ) -> Self
    where
        F: Into<Hertz>,
    {
        unborrow!(sck, miso);
        unsafe {
            sck.set_as_af(sck.af_num(), AFType::OutputPushPull);
            #[cfg(any(spi_v2, spi_v3))]
            sck.set_speed(crate::gpio::Speed::VeryHigh);
            miso.set_as_af(miso.af_num(), AFType::Input);
            #[cfg(any(spi_v2, spi_v3))]
            miso.set_speed(crate::gpio::Speed::VeryHigh);
        }

        Self::new_inner(
            peri,
            Some(sck.degrade()),
            None,
            Some(miso.degrade()),
            txdma,
            rxdma,
            freq,
            config,
        )
    }

    pub fn new_txonly<F>(
        peri: impl Unborrow<Target = T> + 'd,
        sck: impl Unborrow<Target = impl SckPin<T>> + 'd,
        mosi: impl Unborrow<Target = impl MosiPin<T>> + 'd,
        txdma: impl Unborrow<Target = Tx> + 'd,
        rxdma: impl Unborrow<Target = Rx> + 'd, // TODO remove
        freq: F,
        config: Config,
    ) -> Self
    where
        F: Into<Hertz>,
    {
        unborrow!(sck, mosi);
        unsafe {
            sck.set_as_af(sck.af_num(), AFType::OutputPushPull);
            #[cfg(any(spi_v2, spi_v3))]
            sck.set_speed(crate::gpio::Speed::VeryHigh);
            mosi.set_as_af(mosi.af_num(), AFType::OutputPushPull);
            #[cfg(any(spi_v2, spi_v3))]
            mosi.set_speed(crate::gpio::Speed::VeryHigh);
        }

        Self::new_inner(
            peri,
            Some(sck.degrade()),
            Some(mosi.degrade()),
            None,
            txdma,
            rxdma,
            freq,
            config,
        )
    }

    fn new_inner<F>(
        _peri: impl Unborrow<Target = T> + 'd,
        sck: Option<AnyPin>,
        mosi: Option<AnyPin>,
        miso: Option<AnyPin>,
        txdma: impl Unborrow<Target = Tx> + 'd,
        rxdma: impl Unborrow<Target = Rx> + 'd,
        freq: F,
        config: Config,
    ) -> Self
    where
        F: Into<Hertz>,
    {
        unborrow!(txdma, rxdma);

        let pclk = T::frequency();
        let br = compute_baud_rate(pclk, freq.into());

        let cpha = config.raw_phase();
        let cpol = config.raw_polarity();

        let lsbfirst = config.raw_byte_order();

        T::enable();
        T::reset();

        #[cfg(any(spi_v1, spi_f1))]
        unsafe {
            T::REGS.cr2().modify(|w| {
                w.set_ssoe(false);
            });
            T::REGS.cr1().modify(|w| {
                w.set_cpha(cpha);
                w.set_cpol(cpol);

                w.set_mstr(vals::Mstr::MASTER);
                w.set_br(br);
                w.set_spe(true);
                w.set_lsbfirst(lsbfirst);
                w.set_ssi(true);
                w.set_ssm(true);
                w.set_crcen(false);
                w.set_bidimode(vals::Bidimode::UNIDIRECTIONAL);
                if mosi.is_none() {
                    w.set_rxonly(vals::Rxonly::OUTPUTDISABLED);
                }
                w.set_dff(WordSize::EightBit.dff())
            });
        }
        #[cfg(spi_v2)]
        unsafe {
            T::REGS.cr2().modify(|w| {
                w.set_frxth(WordSize::EightBit.frxth());
                w.set_ds(WordSize::EightBit.ds());
                w.set_ssoe(false);
            });
            T::REGS.cr1().modify(|w| {
                w.set_cpha(cpha);
                w.set_cpol(cpol);

                w.set_mstr(vals::Mstr::MASTER);
                w.set_br(br);
                w.set_lsbfirst(lsbfirst);
                w.set_ssi(true);
                w.set_ssm(true);
                w.set_crcen(false);
                w.set_bidimode(vals::Bidimode::UNIDIRECTIONAL);
                w.set_spe(true);
            });
        }
        #[cfg(spi_v3)]
        unsafe {
            T::REGS.ifcr().write(|w| w.0 = 0xffff_ffff);
            T::REGS.cfg2().modify(|w| {
                //w.set_ssoe(true);
                w.set_ssoe(false);
                w.set_cpha(cpha);
                w.set_cpol(cpol);
                w.set_lsbfirst(lsbfirst);
                w.set_ssm(true);
                w.set_master(vals::Master::MASTER);
                w.set_comm(vals::Comm::FULLDUPLEX);
                w.set_ssom(vals::Ssom::ASSERTED);
                w.set_midi(0);
                w.set_mssi(0);
                w.set_afcntr(vals::Afcntr::CONTROLLED);
                w.set_ssiop(vals::Ssiop::ACTIVEHIGH);
            });
            T::REGS.cfg1().modify(|w| {
                w.set_crcen(false);
                w.set_mbr(br);
                w.set_dsize(WordSize::EightBit.dsize());
            });
            T::REGS.cr2().modify(|w| {
                w.set_tsize(0);
                w.set_tser(0);
            });
            T::REGS.cr1().modify(|w| {
                w.set_ssi(false);
                w.set_spe(true);
            });
        }

        Self {
            sck,
            mosi,
            miso,
            txdma,
            rxdma,
            current_word_size: WordSize::EightBit,
            phantom: PhantomData,
        }
    }

    /// Reconfigures it with the supplied config.
    pub fn reconfigure(&mut self, config: Config) {
        let cpha = config.raw_phase();
        let cpol = config.raw_polarity();

        let lsbfirst = config.raw_byte_order();

        #[cfg(any(spi_v1, spi_f1, spi_v2))]
        unsafe {
            T::REGS.cr1().modify(|w| {
                w.set_cpha(cpha);
                w.set_cpol(cpol);
                w.set_lsbfirst(lsbfirst);
            });
        }

        #[cfg(spi_v3)]
        unsafe {
            T::REGS.cfg2().modify(|w| {
                w.set_cpha(cpha);
                w.set_cpol(cpol);
                w.set_lsbfirst(lsbfirst);
            });
        }
    }

    pub fn get_current_config(&self) -> Config {
        #[cfg(any(spi_v1, spi_f1, spi_v2))]
        let cfg = unsafe { T::REGS.cr1().read() };
        #[cfg(spi_v3)]
        let cfg = unsafe { T::REGS.cfg2().read() };
        let polarity = if cfg.cpol() == vals::Cpol::IDLELOW {
            Polarity::IdleLow
        } else {
            Polarity::IdleHigh
        };
        let phase = if cfg.cpha() == vals::Cpha::FIRSTEDGE {
            Phase::CaptureOnFirstTransition
        } else {
            Phase::CaptureOnSecondTransition
        };

        let bit_order = if cfg.lsbfirst() == vals::Lsbfirst::LSBFIRST {
            BitOrder::LsbFirst
        } else {
            BitOrder::MsbFirst
        };

        Config {
            mode: Mode { polarity, phase },
            bit_order,
        }
    }

    fn set_word_size(&mut self, word_size: WordSize) {
        if self.current_word_size == word_size {
            return;
        }

        #[cfg(any(spi_v1, spi_f1))]
        unsafe {
            T::REGS.cr1().modify(|reg| {
                reg.set_spe(false);
                reg.set_dff(word_size.dff())
            });
            T::REGS.cr1().modify(|reg| {
                reg.set_spe(true);
            });
        }
        #[cfg(spi_v2)]
        unsafe {
            T::REGS.cr1().modify(|w| {
                w.set_spe(false);
            });
            T::REGS.cr2().modify(|w| {
                w.set_frxth(word_size.frxth());
                w.set_ds(word_size.ds());
            });
            T::REGS.cr1().modify(|w| {
                w.set_spe(true);
            });
        }
        #[cfg(spi_v3)]
        unsafe {
            T::REGS.cr1().modify(|w| {
                w.set_csusp(true);
            });
            while T::REGS.sr().read().eot() {}
            T::REGS.cr1().modify(|w| {
                w.set_spe(false);
            });
            T::REGS.cfg1().modify(|w| {
                w.set_dsize(word_size.dsize());
            });
            T::REGS.cr1().modify(|w| {
                w.set_csusp(false);
                w.set_spe(true);
            });
        }

        self.current_word_size = word_size;
    }

    pub async fn write<W: Word>(&mut self, data: &[W]) -> Result<(), Error>
    where
        Tx: TxDma<T>,
    {
        if data.len() == 0 {
            return Ok(());
        }

        self.set_word_size(W::WORDSIZE);
        unsafe {
            T::REGS.cr1().modify(|w| {
                w.set_spe(false);
            });
        }

        let tx_request = self.txdma.request();
        let tx_dst = T::REGS.tx_ptr();
        unsafe {
            self.txdma
                .start_write(tx_request, data, tx_dst, Default::default())
        }
        let tx_f = Transfer::new(&mut self.txdma);

        unsafe {
            set_txdmaen(T::REGS, true);
            T::REGS.cr1().modify(|w| {
                w.set_spe(true);
            });
            #[cfg(spi_v3)]
            T::REGS.cr1().modify(|w| {
                w.set_cstart(true);
            });
        }

        tx_f.await;

        finish_dma(T::REGS);

        Ok(())
    }

    pub async fn read<W: Word>(&mut self, data: &mut [W]) -> Result<(), Error>
    where
        Tx: TxDma<T>,
        Rx: RxDma<T>,
    {
        if data.len() == 0 {
            return Ok(());
        }

        self.set_word_size(W::WORDSIZE);
        unsafe {
            T::REGS.cr1().modify(|w| {
                w.set_spe(false);
            });
            set_rxdmaen(T::REGS, true);
        }

        // SPIv3 clears rxfifo on SPE=0
        #[cfg(not(spi_v3))]
        flush_rx_fifo(T::REGS);

        let clock_byte_count = data.len();

        let rx_request = self.rxdma.request();
        let rx_src = T::REGS.rx_ptr();
        unsafe {
            self.rxdma
                .start_read(rx_request, rx_src, data, Default::default())
        };
        let rx_f = Transfer::new(&mut self.rxdma);

        let tx_request = self.txdma.request();
        let tx_dst = T::REGS.tx_ptr();
        let clock_byte = 0x00u8;
        let tx_f = crate::dma::write_repeated(
            &mut self.txdma,
            tx_request,
            clock_byte,
            clock_byte_count,
            tx_dst,
        );

        unsafe {
            set_txdmaen(T::REGS, true);
            T::REGS.cr1().modify(|w| {
                w.set_spe(true);
            });
            #[cfg(spi_v3)]
            T::REGS.cr1().modify(|w| {
                w.set_cstart(true);
            });
        }

        join(tx_f, rx_f).await;

        finish_dma(T::REGS);

        Ok(())
    }

    async fn transfer_inner<W: Word>(
        &mut self,
        read: *mut [W],
        write: *const [W],
    ) -> Result<(), Error>
    where
        Tx: TxDma<T>,
        Rx: RxDma<T>,
    {
        let (_, rx_len) = slice_ptr_parts(read);
        let (_, tx_len) = slice_ptr_parts(write);
        assert_eq!(rx_len, tx_len);
        if rx_len == 0 {
            return Ok(());
        }

        self.set_word_size(W::WORDSIZE);
        unsafe {
            T::REGS.cr1().modify(|w| {
                w.set_spe(false);
            });
            set_rxdmaen(T::REGS, true);
        }

        // SPIv3 clears rxfifo on SPE=0
        #[cfg(not(spi_v3))]
        flush_rx_fifo(T::REGS);

        let rx_request = self.rxdma.request();
        let rx_src = T::REGS.rx_ptr();
        unsafe {
            self.rxdma
                .start_read(rx_request, rx_src, read, Default::default())
        };
        let rx_f = Transfer::new(&mut self.rxdma);

        let tx_request = self.txdma.request();
        let tx_dst = T::REGS.tx_ptr();
        unsafe {
            self.txdma
                .start_write(tx_request, write, tx_dst, Default::default())
        }
        let tx_f = Transfer::new(&mut self.txdma);

        unsafe {
            set_txdmaen(T::REGS, true);
            T::REGS.cr1().modify(|w| {
                w.set_spe(true);
            });
            #[cfg(spi_v3)]
            T::REGS.cr1().modify(|w| {
                w.set_cstart(true);
            });
        }

        join(tx_f, rx_f).await;

        finish_dma(T::REGS);

        Ok(())
    }

    pub async fn transfer<W: Word>(&mut self, read: &mut [W], write: &[W]) -> Result<(), Error>
    where
        Tx: TxDma<T>,
        Rx: RxDma<T>,
    {
        self.transfer_inner(read, write).await
    }

    pub async fn transfer_in_place<W: Word>(&mut self, data: &mut [W]) -> Result<(), Error>
    where
        Tx: TxDma<T>,
        Rx: RxDma<T>,
    {
        self.transfer_inner(data, data).await
    }

    pub fn blocking_write<W: Word>(&mut self, words: &[W]) -> Result<(), Error> {
        unsafe { T::REGS.cr1().modify(|w| w.set_spe(true)) }
        flush_rx_fifo(T::REGS);
        self.set_word_size(W::WORDSIZE);
        for word in words.iter() {
            let _ = transfer_word(T::REGS, *word)?;
        }
        Ok(())
    }

    pub fn blocking_read<W: Word>(&mut self, words: &mut [W]) -> Result<(), Error> {
        unsafe { T::REGS.cr1().modify(|w| w.set_spe(true)) }
        flush_rx_fifo(T::REGS);
        self.set_word_size(W::WORDSIZE);
        for word in words.iter_mut() {
            *word = transfer_word(T::REGS, W::default())?;
        }
        Ok(())
    }

    pub fn blocking_transfer_in_place<W: Word>(&mut self, words: &mut [W]) -> Result<(), Error> {
        unsafe { T::REGS.cr1().modify(|w| w.set_spe(true)) }
        flush_rx_fifo(T::REGS);
        self.set_word_size(W::WORDSIZE);
        for word in words.iter_mut() {
            *word = transfer_word(T::REGS, *word)?;
        }
        Ok(())
    }

    pub fn blocking_transfer<W: Word>(&mut self, read: &mut [W], write: &[W]) -> Result<(), Error> {
        unsafe { T::REGS.cr1().modify(|w| w.set_spe(true)) }
        flush_rx_fifo(T::REGS);
        self.set_word_size(W::WORDSIZE);
        let len = read.len().max(write.len());
        for i in 0..len {
            let wb = write.get(i).copied().unwrap_or_default();
            let rb = transfer_word(T::REGS, wb)?;
            if let Some(r) = read.get_mut(i) {
                *r = rb;
            }
        }
        Ok(())
    }
}

impl<'d, T: Instance, Tx, Rx> Drop for Spi<'d, T, Tx, Rx> {
    fn drop(&mut self) {
        unsafe {
            self.sck.as_ref().map(|x| x.set_as_disconnected());
            self.mosi.as_ref().map(|x| x.set_as_disconnected());
            self.miso.as_ref().map(|x| x.set_as_disconnected());
        }
    }
}

#[cfg(not(spi_v3))]
use vals::Br;
#[cfg(spi_v3)]
use vals::Mbr as Br;

fn compute_baud_rate(clocks: Hertz, freq: Hertz) -> Br {
    let val = match clocks.0 / freq.0 {
        0 => unreachable!(),
        1..=2 => 0b000,
        3..=5 => 0b001,
        6..=11 => 0b010,
        12..=23 => 0b011,
        24..=39 => 0b100,
        40..=95 => 0b101,
        96..=191 => 0b110,
        _ => 0b111,
    };

    Br(val)
}

trait RegsExt {
    fn tx_ptr<W>(&self) -> *mut W;
    fn rx_ptr<W>(&self) -> *mut W;
}

impl RegsExt for Regs {
    fn tx_ptr<W>(&self) -> *mut W {
        #[cfg(not(spi_v3))]
        let dr = self.dr();
        #[cfg(spi_v3)]
        let dr = self.txdr();
        dr.ptr() as *mut W
    }

    fn rx_ptr<W>(&self) -> *mut W {
        #[cfg(not(spi_v3))]
        let dr = self.dr();
        #[cfg(spi_v3)]
        let dr = self.rxdr();
        dr.ptr() as *mut W
    }
}

fn check_error_flags(sr: regs::Sr) -> Result<(), Error> {
    if sr.ovr() {
        return Err(Error::Overrun);
    }
    #[cfg(not(any(spi_f1, spi_v3)))]
    if sr.fre() {
        return Err(Error::Framing);
    }
    #[cfg(spi_v3)]
    if sr.tifre() {
        return Err(Error::Framing);
    }
    if sr.modf() {
        return Err(Error::ModeFault);
    }
    #[cfg(not(spi_v3))]
    if sr.crcerr() {
        return Err(Error::Crc);
    }
    #[cfg(spi_v3)]
    if sr.crce() {
        return Err(Error::Crc);
    }

    Ok(())
}

fn spin_until_tx_ready(regs: Regs) -> Result<(), Error> {
    loop {
        let sr = unsafe { regs.sr().read() };

        check_error_flags(sr)?;

        #[cfg(not(spi_v3))]
        if sr.txe() {
            return Ok(());
        }
        #[cfg(spi_v3)]
        if sr.txp() {
            return Ok(());
        }
    }
}

fn spin_until_rx_ready(regs: Regs) -> Result<(), Error> {
    loop {
        let sr = unsafe { regs.sr().read() };

        check_error_flags(sr)?;

        #[cfg(not(spi_v3))]
        if sr.rxne() {
            return Ok(());
        }
        #[cfg(spi_v3)]
        if sr.rxp() {
            return Ok(());
        }
    }
}

fn flush_rx_fifo(regs: Regs) {
    unsafe {
        #[cfg(not(spi_v3))]
        while regs.sr().read().rxne() {
            let _ = regs.dr().read();
        }
        #[cfg(spi_v3)]
        while regs.sr().read().rxp() {
            let _ = regs.rxdr().read();
        }
    }
}

fn set_txdmaen(regs: Regs, val: bool) {
    unsafe {
        #[cfg(not(spi_v3))]
        regs.cr2().modify(|reg| {
            reg.set_txdmaen(val);
        });
        #[cfg(spi_v3)]
        regs.cfg1().modify(|reg| {
            reg.set_txdmaen(val);
        });
    }
}

fn set_rxdmaen(regs: Regs, val: bool) {
    unsafe {
        #[cfg(not(spi_v3))]
        regs.cr2().modify(|reg| {
            reg.set_rxdmaen(val);
        });
        #[cfg(spi_v3)]
        regs.cfg1().modify(|reg| {
            reg.set_rxdmaen(val);
        });
    }
}

fn finish_dma(regs: Regs) {
    unsafe {
        #[cfg(spi_v2)]
        while regs.sr().read().ftlvl() > 0 {}

        #[cfg(spi_v3)]
        while !regs.sr().read().txc() {}
        #[cfg(not(spi_v3))]
        while regs.sr().read().bsy() {}

        regs.cr1().modify(|w| {
            w.set_spe(false);
        });

        #[cfg(not(spi_v3))]
        regs.cr2().modify(|reg| {
            reg.set_txdmaen(false);
            reg.set_rxdmaen(false);
        });
        #[cfg(spi_v3)]
        regs.cfg1().modify(|reg| {
            reg.set_txdmaen(false);
            reg.set_rxdmaen(false);
        });
    }
}

fn transfer_word<W: Word>(regs: Regs, tx_word: W) -> Result<W, Error> {
    spin_until_tx_ready(regs)?;

    unsafe {
        ptr::write_volatile(regs.tx_ptr(), tx_word);

        #[cfg(spi_v3)]
        regs.cr1().modify(|reg| reg.set_cstart(true));
    }

    spin_until_rx_ready(regs)?;

    let rx_word = unsafe { ptr::read_volatile(regs.rx_ptr()) };
    return Ok(rx_word);
}

mod eh02 {
    use super::*;

    // Note: It is not possible to impl these traits generically in embedded-hal 0.2 due to a conflict with
    // some marker traits. For details, see https://github.com/rust-embedded/embedded-hal/pull/289
    macro_rules! impl_blocking {
        ($w:ident) => {
            impl<'d, T: Instance> embedded_hal_02::blocking::spi::Write<$w>
                for Spi<'d, T, NoDma, NoDma>
            {
                type Error = Error;

                fn write(&mut self, words: &[$w]) -> Result<(), Self::Error> {
                    self.blocking_write(words)
                }
            }

            impl<'d, T: Instance> embedded_hal_02::blocking::spi::Transfer<$w>
                for Spi<'d, T, NoDma, NoDma>
            {
                type Error = Error;

                fn transfer<'w>(&mut self, words: &'w mut [$w]) -> Result<&'w [$w], Self::Error> {
                    self.blocking_transfer_in_place(words)?;
                    Ok(words)
                }
            }
        };
    }

    impl_blocking!(u8);
    impl_blocking!(u16);
}

#[cfg(feature = "unstable-traits")]
mod eh1 {
    use super::*;

    impl<'d, T: Instance, Tx, Rx> embedded_hal_1::spi::ErrorType for Spi<'d, T, Tx, Rx> {
        type Error = Error;
    }

    impl<'d, T: Instance, Tx, Rx> embedded_hal_1::spi::blocking::SpiBusFlush for Spi<'d, T, Tx, Rx> {
        fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    impl<'d, T: Instance, W: Word> embedded_hal_1::spi::blocking::SpiBusRead<W>
        for Spi<'d, T, NoDma, NoDma>
    {
        fn read(&mut self, words: &mut [W]) -> Result<(), Self::Error> {
            self.blocking_read(words)
        }
    }

    impl<'d, T: Instance, W: Word> embedded_hal_1::spi::blocking::SpiBusWrite<W>
        for Spi<'d, T, NoDma, NoDma>
    {
        fn write(&mut self, words: &[W]) -> Result<(), Self::Error> {
            self.blocking_write(words)
        }
    }

    impl<'d, T: Instance, W: Word> embedded_hal_1::spi::blocking::SpiBus<W>
        for Spi<'d, T, NoDma, NoDma>
    {
        fn transfer(&mut self, read: &mut [W], write: &[W]) -> Result<(), Self::Error> {
            self.blocking_transfer(read, write)
        }

        fn transfer_in_place(&mut self, words: &mut [W]) -> Result<(), Self::Error> {
            self.blocking_transfer_in_place(words)
        }
    }

    impl embedded_hal_1::spi::Error for Error {
        fn kind(&self) -> embedded_hal_1::spi::ErrorKind {
            match *self {
                Self::Framing => embedded_hal_1::spi::ErrorKind::FrameFormat,
                Self::Crc => embedded_hal_1::spi::ErrorKind::Other,
                Self::ModeFault => embedded_hal_1::spi::ErrorKind::ModeFault,
                Self::Overrun => embedded_hal_1::spi::ErrorKind::Overrun,
            }
        }
    }
}

cfg_if::cfg_if! {
    if #[cfg(all(feature = "unstable-traits", feature = "nightly"))] {
        use core::future::Future;
        impl<'d, T: Instance, Tx, Rx> embedded_hal_async::spi::SpiBusFlush for Spi<'d, T, Tx, Rx> {
            type FlushFuture<'a> = impl Future<Output = Result<(), Self::Error>> + 'a where Self: 'a;

            fn flush<'a>(&'a mut self) -> Self::FlushFuture<'a> {
                async { Ok(()) }
            }
        }

        impl<'d, T: Instance, Tx: TxDma<T>, Rx, W: Word> embedded_hal_async::spi::SpiBusWrite<W>
            for Spi<'d, T, Tx, Rx>
        {
            type WriteFuture<'a> = impl Future<Output = Result<(), Self::Error>> + 'a where Self: 'a;

            fn write<'a>(&'a mut self, data: &'a [W]) -> Self::WriteFuture<'a> {
                self.write(data)
            }
        }

        impl<'d, T: Instance, Tx: TxDma<T>, Rx: RxDma<T>, W: Word> embedded_hal_async::spi::SpiBusRead<W>
            for Spi<'d, T, Tx, Rx>
        {
            type ReadFuture<'a> = impl Future<Output = Result<(), Self::Error>> + 'a where Self: 'a;

            fn read<'a>(&'a mut self, data: &'a mut [W]) -> Self::ReadFuture<'a> {
                self.read(data)
            }
        }

        impl<'d, T: Instance, Tx: TxDma<T>, Rx: RxDma<T>, W: Word> embedded_hal_async::spi::SpiBus<W>
            for Spi<'d, T, Tx, Rx>
        {
            type TransferFuture<'a> = impl Future<Output = Result<(), Self::Error>> + 'a where Self: 'a;

            fn transfer<'a>(&'a mut self, rx: &'a mut [W], tx: &'a [W]) -> Self::TransferFuture<'a> {
                self.transfer(rx, tx)
            }

            type TransferInPlaceFuture<'a> = impl Future<Output = Result<(), Self::Error>> + 'a where Self: 'a;

            fn transfer_in_place<'a>(
                &'a mut self,
                words: &'a mut [W],
            ) -> Self::TransferInPlaceFuture<'a> {
                self.transfer_in_place(words)
            }
        }
    }
}

pub(crate) mod sealed {
    use super::*;

    pub trait Instance {
        const REGS: Regs;
    }

    pub trait Word: Copy + 'static {
        const WORDSIZE: WordSize;
    }

    impl Word for u8 {
        const WORDSIZE: WordSize = WordSize::EightBit;
    }
    impl Word for u16 {
        const WORDSIZE: WordSize = WordSize::SixteenBit;
    }

    #[derive(Copy, Clone, PartialOrd, PartialEq)]
    pub enum WordSize {
        EightBit,
        SixteenBit,
    }

    impl WordSize {
        #[cfg(any(spi_v1, spi_f1))]
        pub fn dff(&self) -> vals::Dff {
            match self {
                WordSize::EightBit => vals::Dff::EIGHTBIT,
                WordSize::SixteenBit => vals::Dff::SIXTEENBIT,
            }
        }

        #[cfg(spi_v2)]
        pub fn ds(&self) -> vals::Ds {
            match self {
                WordSize::EightBit => vals::Ds::EIGHTBIT,
                WordSize::SixteenBit => vals::Ds::SIXTEENBIT,
            }
        }

        #[cfg(spi_v2)]
        pub fn frxth(&self) -> vals::Frxth {
            match self {
                WordSize::EightBit => vals::Frxth::QUARTER,
                WordSize::SixteenBit => vals::Frxth::HALF,
            }
        }

        #[cfg(spi_v3)]
        pub fn dsize(&self) -> u8 {
            match self {
                WordSize::EightBit => 0b0111,
                WordSize::SixteenBit => 0b1111,
            }
        }

        #[cfg(spi_v3)]
        pub fn _frxth(&self) -> vals::Fthlv {
            match self {
                WordSize::EightBit => vals::Fthlv::ONEFRAME,
                WordSize::SixteenBit => vals::Fthlv::ONEFRAME,
            }
        }
    }
}

pub trait Word: Copy + 'static + sealed::Word + Default + crate::dma::Word {}

impl Word for u8 {}
impl Word for u16 {}

pub trait Instance: sealed::Instance + RccPeripheral {}
pin_trait!(SckPin, Instance);
pin_trait!(MosiPin, Instance);
pin_trait!(MisoPin, Instance);
dma_trait!(RxDma, Instance);
dma_trait!(TxDma, Instance);

foreach_peripheral!(
    (spi, $inst:ident) => {
        impl sealed::Instance for peripherals::$inst {
            const REGS: Regs = crate::pac::$inst;
        }

        impl Instance for peripherals::$inst {}
    };
);
