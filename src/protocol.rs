//! ARCTIC Fan Controller protocol: report layout, state machine, status buffer.
//!
//! Contract authority: the mainline Linux `arctic_fan` hwmon driver
//! (`drivers/hwmon/arctic_fan_controller.c`). See SPEC.md section 3.
//!
//! - OUT report, host -> device, 32 bytes: ID 0x01 + 10 duties (0-100 %).
//! - IN status report, device -> host, 32 bytes: ID 0x01 + 10 duties
//!   (0-100 %) + 10 x LE16 RPM + 1 reserved.
//! - IN ACK report, device -> host, 2 bytes: ID 0x02 + status (0 = OK).
//!
//! Channels 1 and 2 drive physical fans (PB0 / PB1); their RPM is spoofed
//! as a pure linear function of duty. Channels 3-10 are mocks (0 RPM).

use core::sync::atomic::{AtomicU32, Ordering};
use usb_device::prelude::*;
use usbd_hid::hid_class::HIDClass;

use stm32f1xx_hal::usb::UsbBusType;

use crate::pwm::Pwm;

/// Number of fan channels (matches the host driver).
pub const NUM_CHANNELS: usize = 10;
/// Channels that drive physical fans (ch1 -> PB0, ch2 -> PB1).
pub const NUM_PHYSICAL_CHANNELS: usize = 2;
/// On-the-wire report size including the report-ID byte.
pub const REPORT_LEN: usize = 32;
/// Report ID of the 32-byte data reports (OUT duties / IN status).
pub const REPORT_ID_DATA: u8 = 0x01;
/// Report ID of the 2-byte ACK report.
pub const REPORT_ID_ACK: u8 = 0x02;
/// ACK status byte: OK.
pub const ACK_OK: u8 = 0x00;
/// Spoofed RPM at 100 % duty.
pub const RPM_MAX: u16 = 2500;
/// Fail-safe duty when no host is active (boot, unplug, suspend).
pub const FAILSAFE_DUTY: u8 = 100;
/// Status report cadence in 1 kHz ticks (1 s).
pub const STATUS_PERIOD_TICKS: u32 = 1_000;

/// HID report descriptor (54 bytes; payload sizes exclude the report-ID byte):
///
/// - Output, ID 0x01: 31 bytes (10 duties + 21 reserved)
/// - Input,  ID 0x01: 31 bytes (10 PWM + 20 RPM + 1 reserved)
/// - Input,  ID 0x02: 1 byte (ACK status)
pub static REPORT_DESCRIPTOR: [u8; 54] = [
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x01, // Usage (Pointer)
    // Output report, ID 1: 31 bytes
    0x85, 0x01, // Report ID (1)
    0x19, 0x01, // Usage Minimum (1)
    0x29, 0x1F, // Usage Maximum (31)
    0x15, 0x00, // Logical Minimum (0)
    0x26, 0xFF, 0x00, // Logical Maximum (255)
    0x75, 0x08, // Report Size (8)
    0x95, 0x1F, // Report Count (31)
    0x81, 0x02, // Output (Data, Array, Abs)
    // Input report, ID 1: 31 bytes (10 PWM + 20 RPM + 1 reserved)
    0x85, 0x01, // Report ID (1)
    0x19, 0x01, // Usage Minimum (1)
    0x29, 0x1F, // Usage Maximum (31)
    0x15, 0x00, // Logical Minimum (0)
    0x26, 0xFF, 0x00, // Logical Maximum (255)
    0x75, 0x08, // Report Size (8)
    0x95, 0x1F, // Report Count (31)
    0x81, 0x02, // Input (Data, Array, Abs)
    // Input report, ID 2: 1 byte (ACK status)
    0x85, 0x02, // Report ID (2)
    0x19, 0x02, // Usage Minimum (2)
    0x29, 0x02, // Usage Maximum (2)
    0x15, 0x00, // Logical Minimum (0)
    0x25, 0x01, // Logical Maximum (1)
    0x75, 0x08, // Report Size (8)
    0x95, 0x01, // Report Count (1)
    0x81, 0x02, // Input (Data, Array, Abs)
];

/// 1 kHz tick counter, bumped by the SYST interrupt.
static TICKS: AtomicU32 = AtomicU32::new(0);

/// Bump the tick counter (SYST interrupt body).
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

/// Relationship to the host.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HostState {
    /// No active host: channels 1-2 run at [FAILSAFE_DUTY].
    Failsafe,
    /// Host configured and commanding duties.
    HostControlled,
}

/// Protocol state: commanded duties, host state, status report buffer.
///
/// Lives in RAM only: a power cycle always boots into [HostState::Failsafe].
pub struct Protocol {
    state: HostState,
    /// Last commanded duty per channel, 0-100 % (0 at boot).
    duty: [u8; NUM_CHANNELS],
    /// 32-byte status (IN) report buffer.
    status: [u8; REPORT_LEN],
    /// Tick at which the last status report was pushed.
    last_status_tick: u32,
    /// Push a status report even before the 1 s cadence is due.
    status_pending: bool,
    /// ACK accepted but not yet sent: the single IN endpoint was busy.
    /// Retried with priority over the status report until it goes out.
    ack_pending: bool,
}

impl Protocol {
    /// Boot state: FAILSAFE, no duties commanded yet.
    pub fn new() -> Self {
        let mut proto = Self {
            state: HostState::Failsafe,
            duty: [0; NUM_CHANNELS],
            status: [0; REPORT_LEN],
            last_status_tick: 0,
            status_pending: false,
            ack_pending: false,
        };
        proto.refresh_status();
        proto
    }

    /// Effective duty of physical channel i (0-based; i <
    /// [NUM_PHYSICAL_CHANNELS]): [FAILSAFE_DUTY] in FAILSAFE, else the
    /// commanded value.
    fn effective_duty(&self, i: usize) -> u8 {
        match self.state {
            HostState::Failsafe => FAILSAFE_DUTY,
            HostState::HostControlled => self.duty[i],
        }
    }

    /// One main-loop iteration: fail-safe check, OUT reports, status reports.
    ///
    /// All work is microsecond-scale and non-blocking.
    pub fn service(
        &mut self,
        dev: &UsbDevice<'_, UsbBusType>,
        hid: &mut HIDClass<'_, UsbBusType>,
        pwm: &mut Pwm,
    ) {
        // Fail-safe: any non-configured bus state (before the first host
        // configuration, host suspend, or cable removal) drives channels 1-2
        // at 100 %. Takes effect immediately.
        if dev.state() != UsbDeviceState::Configured && self.state != HostState::Failsafe {
            self.state = HostState::Failsafe;
            pwm.set_percent_ch1(FAILSAFE_DUTY);
            pwm.set_percent_ch2(FAILSAFE_DUTY);
            self.refresh_status();
        }

        // Host -> device OUT report: one `pull_raw_output` is one complete
        // 32-byte packet (Linux `hid_hw_output_report`), no reassembly.
        let mut buf = [0u8; REPORT_LEN];
        if let Ok(len) = hid.pull_raw_output(&mut buf) {
            if len == REPORT_LEN && buf[0] == REPORT_ID_DATA {
                self.duty.copy_from_slice(&buf[1..=NUM_CHANNELS]);
                if dev.state() == UsbDeviceState::Configured {
                    self.state = HostState::HostControlled;
                    // Channels 1-2 are the only channels that touch hardware.
                    pwm.set_percent_ch1(self.duty[0]);
                    pwm.set_percent_ch2(self.duty[1]);
                }
                // Queue the ACK; it goes out with priority on the single
                // IN endpoint (the driver allows 1 s, we deliver in ~us).
                self.ack_pending = true;
                self.refresh_status();
                self.status_pending = true;
            }
            // Anything else (wrong length / report ID): ignore. No ACK, so
            // the driver's write times out with -ETIMEDOUT: the correct
            // failure mode.
        }

        // ACK has priority on the single IN endpoint: retry every iteration
        // until it is out (the driver's ARCTIC_ACK_TIMEOUT_MS is 1 s).
        // While an ACK is pending the status report must not take the
        // endpoint in front of it; `status_pending` stays set so it goes
        // out right after.
        if self.ack_pending && hid.push_raw_input(&[REPORT_ID_ACK, ACK_OK]).is_ok() {
            self.ack_pending = false;
        }

        // Periodic (~1 Hz) and post-OUT status report. If the IN endpoint is
        // busy the push returns WouldBlock and is retried next iteration.
        let now = TICKS.load(Ordering::Relaxed);
        let due = now.wrapping_sub(self.last_status_tick) >= STATUS_PERIOD_TICKS;
        if !self.ack_pending
            && (due || self.status_pending)
            && hid.push_raw_input(&self.status).is_ok()
        {
            self.last_status_tick = now;
            self.status_pending = false;
        }
    }

    /// Rebuild the 32-byte status buffer from the current state.
    fn refresh_status(&mut self) {
        self.status[0] = REPORT_ID_DATA;
        self.status[REPORT_LEN - 1] = 0;
        for i in 0..NUM_CHANNELS {
            let physical = i < NUM_PHYSICAL_CHANNELS;
            let duty = if physical {
                self.effective_duty(i)
            } else {
                self.duty[i]
            };
            self.status[1 + i] = duty;
            // RPM spoof: pure linear duty -> RPM, integer math.
            // ch1-2: 0 % -> 0, 50 % -> 1250, 100 % -> 2500; mocks: always 0.
            let rpm = if physical {
                (self.effective_duty(i) as u16) * (RPM_MAX / 100)
            } else {
                0
            };
            self.status[11 + 2 * i] = (rpm & 0xFF) as u8;
            self.status[11 + 2 * i + 1] = (rpm >> 8) as u8;
        }
    }
}
