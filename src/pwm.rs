//! Fan PWM outputs: TIM3_CH3 on PB0 (ch1) and TIM3_CH4 on PB1 (ch2),
//! 25 kHz, edge-aligned, active-high.
//!
//! Configured directly through the PAC registers. The HAL's timer module
//! cannot express a partial channel split: its `pins_impl!` macro fails to
//! generate the `Pins` impl for a pin subset (rustc silently drops impls
//! produced from a nested single-element macro repetition), so
//! `Timer::pwm()` is unusable with fewer than four channels, and a
//! four-channel split would force-claim PA6/PA7/PB4. The HAL still owns
//! the system clocks and the GPIO pin modes.
//!
//! Math: TIM3 is clocked at 72 MHz (APB1 timer clock = 2 x PCLK1).
//! Edge-aligned period = 72 MHz / 25 kHz = exactly 2880 ticks
//! (ARR = 2879). PWM mode 1: output active while CNT < CCRn, so
//! CCRn = pct * 2880 / 100 gives an exact duty. 0 % -> always low,
//! 100 % -> CCRn = 2880 > ARR -> always high.

use stm32f1xx_hal::gpio::{Cr, Pin};
use stm32f1xx_hal::pac::{RCC, TIM3};
use stm32f1xx_hal::rcc::Clocks;

/// APB1 timer clock (PCLK1 36 MHz x 2).
const TIM3_CLK_HZ: u32 = 72_000_000;
/// One 25.0 kHz period in TIM3 ticks.
const PWM_PERIOD_TICKS: u32 = TIM3_CLK_HZ / 25_000;

/// Fan PWM outputs (TIM3_CH3 -> PB0, TIM3_CH4 -> PB1).
pub struct Pwm {
    tim3: TIM3,
}

impl Pwm {
    /// Set channel-1 (PB0, TIM3_CH3) duty to `pct` % (clamped to 0..=100).
    ///
    /// Single 16-bit CCR3 write: atomic w.r.t. the timer, no glitch.
    pub fn set_percent_ch1(&mut self, pct: u8) {
        self.tim3.ccr(2).write(|w| w.ccr().set(Self::ccr_for(pct)));
    }

    /// Set channel-2 (PB1, TIM3_CH4) duty to `pct` % (clamped to 0..=100).
    pub fn set_percent_ch2(&mut self, pct: u8) {
        self.tim3.ccr(3).write(|w| w.ccr().set(Self::ccr_for(pct)));
    }

    /// Duty percent -> compare value, exact for all integer percents.
    fn ccr_for(pct: u8) -> u16 {
        ((pct.min(100) as u32) * PWM_PERIOD_TICKS / 100) as u16
    }
}

/// Enable the TIM3 peripheral clock (APB1ENR.TIM3EN).
///
/// Must run before `Rcc::freeze`; `freeze` does not touch APB1ENR.
pub fn enable_clock(rcc: &RCC) {
    rcc.apb1enr().modify(|_, w| w.tim3en().set_bit());
}

/// Configure TIM3_CH3/TIM3_CH4 as 25 kHz edge-aligned PWM on PB0/PB1,
/// starting both channels at `initial_pct` % duty.
pub fn init(
    tim3: TIM3,
    pb0: Pin<'B', 0>,
    pb1: Pin<'B', 1>,
    crl: &mut Cr<'B', false>,
    clocks: &Clocks,
    initial_pct: u8,
) -> Pwm {
    assert_eq!(
        clocks.pclk1_tim().raw(),
        TIM3_CLK_HZ,
        "TIM3 must be clocked at 72 MHz for exact 25 kHz"
    );

    // PB0 -> TIM3_CH3, PB1 -> TIM3_CH4: both default AFIO map;
    // no remap required.
    pb0.into_alternate_push_pull(crl);
    pb1.into_alternate_push_pull(crl);

    // Prescaler 0, period 2880 ticks.
    tim3.psc().write(|w| w.psc().set(0));
    tim3.arr()
        .write(|w| w.arr().set((PWM_PERIOD_TICKS - 1) as u16));
    // CH3 and CH4: PWM mode 1 (OCxM = 0b110) with output preloading
    // (OCxPE). Both live in CCMR2_Output, not CR2.
    tim3.ccmr2_output().modify(|_, w| {
        w.oc3pe()
            .set_bit()
            .oc3m()
            .pwm_mode1()
            .oc4pe()
            .set_bit()
            .oc4m()
            .pwm_mode1()
    });
    // Channels 3 and 4 enabled, active-high polarity.
    tim3.ccer().write(|w| {
        w.cc3e()
            .set_bit()
            .cc3p()
            .clear_bit()
            .cc4e()
            .set_bit()
            .cc4p()
            .clear_bit()
    });
    // Initial duty before the counter starts, so the first edge is
    // already correct.
    let ccr = Pwm::ccr_for(initial_pct);
    tim3.ccr(2).write(|w| w.ccr().set(ccr));
    tim3.ccr(3).write(|w| w.ccr().set(ccr));
    // Auto-reload preload + counter enable, last.
    tim3.cr1().write(|w| w.arpe().set_bit().cen().enabled());

    Pwm { tim3 }
}
