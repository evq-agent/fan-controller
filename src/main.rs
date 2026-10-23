//! ARCTIC Fan Controller clone — STM32F103C8T6 "Blue Pill" firmware.
//!
//! Presents over USB as an ARCTIC Fan Controller (VID 0x3904, PID 0xF001) so
//! the mainline Linux `arctic_fan` hwmon driver binds to it unpatched.
//!
//! - Channel 1 drives a physical 4-pin PC fan: 25 kHz PWM on PB0 (TIM3_CH3).
//! - Channel 2 drives a second physical fan: 25 kHz PWM on PB1 (TIM3_CH4).
//! - Channels 3-10 are mocks: writes are accepted and echoed, RPM reads 0.
//! - RPM for channels 1-2 is spoofed as a pure linear function of duty.
//! - Fail-safe: with no active USB host (unplugged, host suspend, or before
//!   the first host command), channels 1-2 run at 100 % duty.
//!
//! See SPEC.md for the full contract.

#![no_std]
#![no_main]

use cortex_m_rt::entry;
use stm32f1xx_hal::prelude::*;
use stm32f1xx_hal::rcc::Config;

mod protocol;
mod pwm;
mod usb;

use panic_halt as _;

/// SysTick tick rate (1 kHz) — drives the ~1 Hz status report.
const SYSTICK_HZ: u32 = 1_000;
/// SYSCLK is 72 MHz (HSE 8 MHz x PLL 9).
const SYSCLK_HZ: u32 = 72_000_000;

#[entry]
fn main() -> ! {
    let dp = stm32f1xx_hal::pac::Peripherals::take().unwrap();
    let mut cp = stm32f1xx_hal::pac::CorePeripherals::take().unwrap();

    // TIM3 peripheral clock (APB1ENR.TIM3EN); must run before freeze,
    // which does not touch APB1ENR.
    pwm::enable_clock(&dp.RCC);

    // Clocks: 8 MHz HSE crystal -> 72 MHz SYSCLK, 48 MHz USBCLK (PLL /1.5).
    let mut flash = dp.FLASH.constrain();
    let mut rcc = dp
        .RCC
        .freeze(Config::hse(8.MHz()).sysclk(72.MHz()), &mut flash.acr);
    assert!(
        rcc.clocks.usbclk_valid(),
        "USB clock must be exactly 48 MHz"
    );

    // 1 kHz SysTick; the interrupt only bumps a counter (see protocol::tick).
    let systick = &mut cp.SYST;
    systick.set_reload(SYSCLK_HZ / SYSTICK_HZ - 1);
    systick.set_clock_source(cortex_m::peripheral::syst::SystClkSource::Core);
    systick.enable_interrupt();
    systick.enable_counter();

    // Fan PWM on PB0/PB1 (TIM3_CH3/CH4), 25 kHz; both start at 100 %
    // (fail-safe).
    let mut gpiob = dp.GPIOB.split(&mut rcc);
    let mut pwm = pwm::init(
        dp.TIM3,
        gpiob.pb0,
        gpiob.pb1,
        &mut gpiob.crl,
        &rcc.clocks,
        protocol::FAILSAFE_DUTY,
    );

    // USB device with the ARCTIC identity and the raw-report HID interface.
    // The allocator owns the bus; `dev` and `hid` borrow it, so it must
    // outlive them (both live until the end of `main`).
    let gpioa = dp.GPIOA.split(&mut rcc);
    let alloc = usb::bus(dp.USB, gpioa);
    let (mut hid, mut dev) = usb::build(&alloc);

    let mut proto = protocol::Protocol::new();

    loop {
        dev.poll(&mut [&mut hid]);
        proto.service(&dev, &mut hid, &mut pwm);
    }
}

/// 1 kHz SysTick tick. Core exceptions use `#[cortex_m_rt::exception]`; the
/// PAC's `Interrupt` enum has no SysTick (it only lists NVIC peripherals).
#[cortex_m_rt::exception]
fn SysTick() {
    protocol::tick();
}
