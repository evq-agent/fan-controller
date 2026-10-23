//! USB device setup: ARCTIC identity, HID class with raw reports.
//!
//! Endpoints (allocated by `usbd-hid`, 64-byte max packet, full-speed legal):
//!
//! | Endpoint | Address | Type      | wMaxPacketSize | bInterval |
//! |----------|---------|-----------|----------------|-----------|
//! | EP1 IN   | `0x81`  | Interrupt | 64             | 10 ms     |
//! | EP1 OUT  | `0x01`  | Interrupt | 64             | 10 ms     |
//!
//! The report sizes that matter (32 / 32 / 2) are declared by the HID report
//! descriptor; a 32-byte report is a short packet on a 64-byte endpoint.
//!
//! `usb-device` 0.3 pattern: the allocator owns the bus; `UsbDevice` and the
//! `HIDClass` borrow it, so the allocator must outlive both (it stays a
//! local in `main`).

use usb_device::bus::UsbBusAllocator;
use usb_device::prelude::*;
use usbd_hid::hid_class::HIDClass;

use stm32f1xx_hal::gpio::gpioa;
use stm32f1xx_hal::pac;
use stm32f1xx_hal::usb::{Peripheral, UsbBus, UsbBusType};

use crate::protocol;

/// ARCTIC VID/PID so the stock `arctic_fan` driver binds.
pub const ARCTIC_VID_PID: UsbVidPid = UsbVidPid(0x3904, 0xF001);
/// Endpoint polling interval advertised to the host (ms).
pub const EP_POLL_MS: u8 = 10;

/// Create the USB bus on PA11 (D-) / PA12 (D+).
///
/// The pins stay floating inputs; the board's D+ pull-up handles
/// enumeration and `stm32-usbd` toggles it for device reset.
pub fn bus(usb: pac::USB, gpioa: gpioa::Parts) -> UsbBusAllocator<UsbBusType> {
    UsbBus::new(Peripheral {
        usb,
        pin_dm: gpioa.pa11,
        pin_dp: gpioa.pa12,
    })
}

/// Build the HID class (raw report descriptor) and the USB device
/// (ARCTIC identity) on the given bus allocator.
pub fn build<'a>(
    alloc: &'a UsbBusAllocator<UsbBusType>,
) -> (HIDClass<'a, UsbBusType>, UsbDevice<'a, UsbBusType>) {
    // HID interface with the raw report descriptor (IN + OUT endpoints).
    let hid = HIDClass::new(alloc, &protocol::REPORT_DESCRIPTOR, EP_POLL_MS);

    // Cosmetic fidelity to the real device.
    let strs = StringDescriptors::new(0x0409.into())
        .manufacturer("ARCTIC")
        .product("ARCTIC Fan Controller");

    let dev = UsbDeviceBuilder::new(alloc, ARCTIC_VID_PID)
        .device_class(0) // per-interface (HID)
        .device_release(0x0100)
        .strings(&[strs])
        .expect("valid string descriptors")
        .build();

    (hid, dev)
}
