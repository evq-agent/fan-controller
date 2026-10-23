//! Fan PWM output: TIM3_CH4 on PB1, 25 kHz, edge-aligned, active-high.
//!
//! Configured directly through the PAC registers. The HAL's timer module
//! cannot express a single-channel split: its `pins_impl!` macro fails to
//! generate the single-pin `Pins` impls (rustc silently drops impls
//! produced from a nested single-element macro repetition), so
//! `Timer::pwm()` is unusable with one channel, and a four-channel split
//! would force-claim PA6/PA7/PB0. The HAL still owns the system clocks
//! and the PB1 GPIO mode.
//!
//! Math: TIM3 is clocked at 72 MHz (APB1 timer clock = 2 x PCLK1).
//! Edge-aligned period = 72 MHz / 25 kHz = exactly 2880 ticks
//! (ARR = 2879). PWM mode 1: output active while CNT < CCR4, so
//! CCR4 = pct * 2880 / 100 gives an exact duty. 0 % -> always low,
//! 100 % -> CCR4 = 2880 > ARR -> always high.

use stm32f1xx_hal::gpio::{Cr, Pin};
use stm32f1xx_hal::pac::{RCC, TIM3};
use stm32f1xx_hal::rcc::Clocks;

/// APB1 timer clock (PCLK1 36 MHz x 2).
const TIM3_CLK_HZ: u32 = 72_000_000;
/// One 25.0 kHz period in TIM3 ticks.
const PWM_PERIOD_TICKS: u32 = TIM3_CLK_HZ / 25_000;

/// Channel-1 fan PWM (TIM3_CH4 -> PB1).
pub struct Pwm {
    tim3: TIM3,
}

impl Pwm {
    /// Set channel-1 duty to `pct` % (clamped to 0..=100).
    ///
    /// Single 16-bit CCR4 write: atomic w.r.t. the timer, no glitch.
    pub fn set_percent(&mut self, pct: u8) {
        let ccr = (pct.min(100) as u32) * PWM_PERIOD_TICKS / 100;
        self.tim3.ccr(3).write(|w| w.ccr().set(ccr as u16));
    }
}

/// Enable the TIM3 peripheral clock (APB1ENR.TIM3EN).
///
/// Must run before `Rcc::freeze`; `freeze` does not touch APB1ENR.
pub fn enable_clock(rcc: &RCC) {
    rcc.apb1enr().modify(|_, w| w.tim3en().set_bit());
}

/// Configure TIM3_CH4 as 25 kHz edge-aligned PWM on PB1, starting at
/// `initial_pct` % duty.
pub fn init(
    tim3: TIM3,
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

    // PB1 -> alternate function, push-pull. TIM3_CH4 -> PB1 is the
    // default map; no AFIO remap required.
    pb1.into_alternate_push_pull(crl);

    // Prescaler 0, period 2880 ticks.
    tim3.psc().write(|w| w.psc().set(0));
    tim3.arr()
        .write(|w| w.arr().set((PWM_PERIOD_TICKS - 1) as u16));
    // CH4: PWM mode 1 (OC4M = 0b110) with output preloading (OC4PE).
    // Both live in CCMR2_Output, not CR2.
    tim3.ccmr2_output()
        .modify(|_, w| w.oc4pe().set_bit().oc4m().pwm_mode1());
    // Channel 4 enabled, active-high polarity.
    tim3.ccer().write(|w| w.cc4e().set_bit().cc4p().clear_bit());
    // Initial duty before the counter starts, so the first edge is
    // already correct.
    let ccr = (initial_pct.min(100) as u32) * PWM_PERIOD_TICKS / 100;
    tim3.ccr(3).write(|w| w.ccr().set(ccr as u16));
    // Auto-reload preload + counter enable, last.
    tim3.cr1().write(|w| w.arpe().set_bit().cen().enabled());

    Pwm { tim3 }
}
