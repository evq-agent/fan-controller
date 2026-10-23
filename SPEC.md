# SPEC: ARCTIC Fan Controller clone — STM32F103 "Blue Pill" firmware (Rust)

Status: approved scope (v2 — supersedes the ATtiny85 + V-USB draft)
Target host driver: mainline `arctic_fan` (merged for Linux 7.2, `drivers/hwmon/arctic_fan_controller.c`)

## 1. Goal

Build firmware for an STM32F103C8T6 "Blue Pill" that presents over USB as an
**ARCTIC Fan Controller** (VID `0x3904`, PID `0xF001`) so the stock Linux
`arctic_fan` hwmon driver binds to it with no patches, exposing 10× `fanN_input`
(RPM) and 10× `pwmN_input` (duty 0–255 in sysfs).

- **Channel 1** drives a physical 4-pin PC fan: 25 kHz PWM on PB1 (TIM3_CH4).
- **Channels 2–10** are mocks: writes accepted and echoed, RPM always reported 0.
- **RPM for channel 1 is spoofed** as a pure linear function of duty — no
  tachometer counting.
- **Fail-safe:** with no active USB host (unplugged, host sleep/suspend, or
  before the first host command), channel 1 runs at **100 %** duty.

Deliverable is a **Rust `no_std` firmware** plus a Nix dev shell (`flake.nix` /
direnv). Hardware is assumed: a Blue Pill and a 4-pin fan powered from a PC fan
header (see §2). No PCB, wiring, or enclosure is in scope.

**Why v2 exists:** v1 targeted an ATtiny85 with V-USB. V-USB is low-speed USB,
whose interrupt endpoints are capped at 8 bytes per packet (`USB 2.0` §5.7.3);
Linux additionally clamps `wMaxPacketSize` to 8 for low-speed endpoints
(`drivers/usb/core/config.c`, `low_speed_maxpacket_maxes`). The ARCTIC protocol
needs 32-byte HID reports on interrupt endpoints, which a low-speed device
cannot provide. The Blue Pill's full-speed USB peripheral (64-byte interrupt
endpoints) removes that blocker; the host driver stays untouched.

## 2. Reference hardware & pin map (Blue Pill)

STM32F103C8T6: Cortex-M3 @ 72 MHz from the on-board 8 MHz HSE crystal, 64 KB
flash, 20 KB RAM, full-speed USB device peripheral, ST-Link/SWD header.

| Function                  | Pin              | Notes                                                                 |
|---------------------------|------------------|-----------------------------------------------------------------------|
| USB D-                    | PA11             | USB_FS DM; no external transceiver                                    |
| USB D+                    | PA12             | USB_FS DP; board pull-up present, leave as input except dev reset      |
| Fan PWM out (ch 1)        | PB1              | TIM3_CH4, 25 kHz, AF push-pull (see electrical note)                  |
| Fan motor power           | 12 V header      | **Not** from the board: fan's +12 V from a PC 4-pin header            |
| GND                       | shared           | Board GND tied to fan header GND                                      |
| Fan tach line             | unconnected      | RPM is spoofed; nothing wired                                         |
| Programming / debug       | SWDIO PA13, SWCLK PA14 | ST-Link V2, SWD, 3.3 V, GND                                    |
| Boot mode                 | BOOT0/BOOT1      | BOOT0=1 + reset selects system bootloader (DFU/UART fallback, §9)     |

Clocking:

- `sysclk = 72 MHz` from HSE 8 MHz (PLL ×9).
- `usbclk = 48 MHz` (PLL / 1.5). With `stm32f1xx-hal` 0.11,
  `rcc::Config::hse(8.MHz()).sysclk(72.MHz())` selects `UsbPre::Div1_5`
  automatically; assert `rcc.clocks.usbclk_valid()` at startup.
- TIM3 is clocked at 72 MHz (APB1 timer clock doubler), giving exact 25 kHz PWM.

Electrical notes (hardware, not firmware):

- The fan's PWM input is high-impedance and is typically pulled up to 5 V
  inside the fan. PB1 is 5 V-tolerant as a digital I/O. Firmware drives it as a
  normal AF push-pull output (3.3 V logic, accepted by most 4-pin fans). If a
  true 5 V high level is required, drive PB1 open-drain (the fan's pull-up sets
  the high level) or add a small N-MOSFET/level shifter.
- The Blue Pill must be powered from USB during operation. The fan must **not**
  be powered from the board; only PWM and GND are wired to the fan.
- Stock board, no option-byte changes (§9).

## 3. Host driver contract (unchanged from v1; verified against mainline)

The driver is the sole protocol authority. All report layouts below are
byte-exact and do not change with the MCU. `raw_event` receives the report
**including** the report-ID byte.

**OUT report, host → device** (32 bytes on the wire):

| Byte(s) | Content                              |
|---------|--------------------------------------|
| 0       | Report ID `0x01`                     |
| 1–10    | Duty per channel 1–10, **0–100 %** (u8 each). The driver converts sysfs 0–255 to 0–100 via `DIV_ROUND_CLOSEST(d*100/255)` — the firmware works in 0–100 % throughout. |
| 11–31   | Unused (host sends zeros)            |

**IN status report, device → host** (32 bytes, pushed ~1 Hz; no GET_REPORT exists):

| Byte(s) | Content                                        |
|---------|------------------------------------------------|
| 0       | Report ID `0x01`                               |
| 1–10    | Current PWM per channel, 0–100 %               |
| 11–30   | RPM per channel, 10 × uint16 **little-endian**  |
| 31      | Unused (0)                                     |

**IN ACK report, device → host** (2 bytes, sent once per accepted OUT, must
arrive < 1 s — driver timeout `ARCTIC_ACK_TIMEOUT_MS = 1000`):

| Byte(s) | Content                                   |
|---------|-------------------------------------------|
| 0       | Report ID `0x02`                          |
| 1       | Status: `0x00` = OK, `0x01` = error       |

Driver behaviors the firmware must respect:

- `hid_parse()` must succeed → a valid, consistent HID report descriptor is mandatory.
- The driver ignores GET_REPORT / feature requests → firmware needs none.
- On device reset/suspend-resume the driver clears its cached duties to 0 →
  firmware boot state (fail-safe 100 %) must not depend on any host state.
- `pwmN_input` sysfs reads come from the **host's** cache, not from our status
  bytes; the status report's PWM bytes only need to be plausibly consistent.
- ACK latency matters: the driver allows 1 s and measured the real device at
  ≤ ~563 ms over 500 iterations. The firmware ACKs immediately after applying
  the OUT report, so this margin is not a concern.

With full-speed USB, all three reports are single USB packets:
`32 ≤ wMaxPacketSize`. No chunking/reassembly is needed (the v1 V-USB path
received 8-byte chunks and could never satisfy `len == 32`).

## 4. Stack, tooling & repo layout

Pure **Rust**, `#![no_std]`, `#![no_main]`; no heap, no floating point (integer
math only), static buffers. USB handled by the `usb-device` ecosystem; PWM by
the F1 HAL.

| Crate              | Version | Role                                                        |
|--------------------|---------|-------------------------------------------------------------|
| `usb-device`       | 0.3.2   | USB device stack                                            |
| `usbd-hid`         | 0.10.2  | HID class: raw report descriptor, raw input/output reports   |
| `stm32f1xx-hal`    | 0.11.0  | F1 HAL; features `["stm32f103", "medium"]` (pulls `stm32-usbd` 0.8, `stm32f1` 0.16) |
| `cortex-m` / `cortex-m-rt` | 0.7 | Core + startup/vector table (`stm32f1` default features enable `rt`) |
| `panic-halt`       | 1.0     | Panic handler (optionally `panic-probe` + `defmt` for debug) |

`usbd-hid` 0.10 behavior this spec relies on:

- `HIDClass::new(&usb_alloc, REPORT_DESCRIPTOR, poll_ms)` allocates **both** an
  interrupt IN and an interrupt OUT endpoint at 64-byte max packet size.
- `push_raw_input(&[u8])` sends a raw device→host report (report ID included).
- `pull_raw_output(&mut [u8])` reads a host→device report (report ID included).
- `SET_IDLE` / `SET_PROTOCOL` / `SET_REPORT` (control) are handled;
  `GET_IDLE` / `GET_REPORT` are rejected with STALL (see §10).

Repo layout (created during implementation):

```
Cargo.toml              # package fan-controller, deps per table above
Cargo.lock              # committed (binary crate)
build.rs                # copies memory.x into OUT_DIR; link search
memory.x                # FLASH 64K @ 0x08000000, RAM 20K @ 0x20000000
.cargo/config.toml      # target thumbv7m-none-eabi; runner probe-rs
flake.nix               # devShell: Rust + embedded tools
.envrc                  # direnv: use flake
.gitignore
src/main.rs             # init, main loop, state machine, SysTick tick
src/usb.rs              # report descriptor, HID class wiring, OUT/IN helpers
src/protocol.rs         # constants, status buffer, RPM spoof
src/pwm.rs              # TIM3_CH4 setup + duty mapping
README.md               # wiring, flashing, fail-safe behavior
SPEC.md                 # this file
```

Nix dev shell (already in this repo):

- `rust-overlay` stable toolchain with target `thumbv7m-none-eabi` and
  extensions `rust-src`, `llvm-tools`, `rust-analyzer`.
- `probe-rs-tools` (primary flashing/debug), `dfu-util` + `stm32flash`
  (fallbacks), `cargo-binutils` (`cargo size`), `usbutils`, `lm_sensors`.
- On NixOS, if probe-rs/DFU cannot open the probe/device as a normal user, add
  the packaged udev rules to the system configuration
  (`services.udev.packages = [ pkgs.probe-rs-tools ]`, plus the STM32 DFU rules).

`.cargo/config.toml` (target and runner):

```toml
[build]
target = "thumbv7m-none-eabi"

[target.thumbv7m-none-eabi]
runner = "probe-rs run --chip STM32F103C8"
rustflags = ["-C", "link-arg=-Tlink.x"]
```

## 5. USB descriptors

- **Device:** `bDeviceClass 0` (per-interface), VID `0x3904`, PID `0xF001`,
  `bcdDevice 0x0100` (`.device_release(0x0100)`). Strings: manufacturer
  `"ARCTIC"`, product `"ARCTIC Fan Controller"` (cosmetic fidelity to the real
  device). Built with `UsbDeviceBuilder::new(&usb_alloc, UsbVidPid(0x3904,
  0xF001))` + `StringDescriptors`.
- **Configuration:** 1 interface, class `0x03` (HID), subclass 0, protocol 0.
  `usbd-hid` emits the interface, HID class descriptor, and endpoint
  descriptors from the allocations below:

| Endpoint | Address | Type      | wMaxPacketSize | bInterval |
|----------|---------|-----------|----------------|-----------|
| EP1 IN   | `0x81`  | Interrupt | 64             | 10 ms     |
| EP1 OUT  | `0x01`  | Interrupt | 64             | 10 ms     |

  `wMaxPacketSize = 64` is `usbd-hid`'s allocation (`alloc.interrupt(64,
  poll_ms)`); it is legal for full-speed and the report sizes that matter are
  declared by the HID report descriptor. A 32-byte report is a short packet on
  a 64-byte endpoint and completes the host URB normally. Exact 32-byte
  endpoint descriptors are not required by the driver; they would require
  patching `usbd-hid` or writing a custom class.

- **HID report descriptor** (54 bytes; payload sizes exclude the report-ID
  byte). This is the v1 descriptor, unchanged — only the sizes are load-bearing:

  - Output, report ID `0x01`: 31 bytes (10 used: ch 1–10 duty 0–100, 21 reserved)
  - Input,  report ID `0x01`: 31 bytes (10 PWM + 20 RPM + 1 reserved)
  - Input,  report ID `0x02`: 1 byte (ACK status)

  On the wire: 32 / 32 / 2 including the ID byte. `usbd-hid` takes it as a
  `&'static [u8]` and generates `wDescriptorLength = 54` in the HID class
  descriptor.

```rust
pub static REPORT_DESCRIPTOR: [u8; 54] = [
    0x05, 0x01,             // Usage Page (Generic Desktop)
    0x09, 0x01,             // Usage (Pointer)

    // Output report, ID 1: 31 bytes
    0x85, 0x01,             // Report ID (1)
    0x19, 0x01,             // Usage Minimum (1)
    0x29, 0x1F,             // Usage Maximum (31)
    0x15, 0x00,             // Logical Minimum (0)
    0x26, 0xFF, 0x00,       // Logical Maximum (255)
    0x75, 0x08,             // Report Size (8)
    0x95, 0x1F,             // Report Count (31)
    0x81, 0x02,             // Output (Data, Array, Abs)

    // Input report, ID 1: 31 bytes (10 PWM + 20 RPM + 1 reserved)
    0x85, 0x01,             // Report ID (1)
    0x19, 0x01,             // Usage Minimum (1)
    0x29, 0x1F,             // Usage Maximum (31)
    0x15, 0x00,             // Logical Minimum (0)
    0x26, 0xFF, 0x00,       // Logical Maximum (255)
    0x75, 0x08,             // Report Size (8)
    0x95, 0x1F,             // Report Count (31)
    0x81, 0x02,             // Input (Data, Array, Abs)

    // Input report, ID 2: 1 byte (ACK status)
    0x85, 0x02,             // Report ID (2)
    0x19, 0x02,             // Usage Minimum (2)
    0x29, 0x02,             // Usage Maximum (2)
    0x15, 0x00,             // Logical Minimum (0)
    0x25, 0x01,             // Logical Maximum (1)
    0x75, 0x08,             // Report Size (8)
    0x95, 0x01,             // Report Count (1)
    0x81, 0x02,             // Input (Data, Array, Abs)
];
```

## 6. Runtime behavior

### 6.1 State machine

```
                 power-on / USB unplugged / host suspend
        +----------------------------------------------+
        |                                              v
 +-----------+   host configured + OUT report   +----------------+
 | FAILSAFE  | ------------------------------>  | HOST_CONTROLLED|
 | duty1=100 | <------------------------------  | duty1=commanded|
 +-----------+   (never back until host         +----------------+
                 has re-sent a command)*
```

\*After (re-)enumeration the device stays in FAILSAFE until the first OUT
report — the driver sends nothing at probe time, so a freshly attached host
sees 100 % until userland writes a duty.

- **Host active** = `usb_dev.state() == UsbDeviceState::Configured` (i.e. the
  host configured us and the bus is not suspended). `usb-device` reports
  `Suspend` after the bus goes idle, which covers both host sleep and cable
  removal. No D-line sampling or bus-idle timers are needed.
- Transition to FAILSAFE takes effect immediately: PWM compare register ← 100 %.
- State lives in RAM only; power cycle ⇒ FAILSAFE.

### 6.2 OUT report handling

In the main loop:

1. `hid.pull_raw_output(&mut buf)`:
   - `Ok(32)` with `buf[0] == 0x01` → accept.
   - anything else → ignore (no ACK; the driver's write times out with
     `-ETIMEDOUT` after 1 s — the correct failure mode).
   - `Err(WouldBlock)` → no report pending yet.
2. Store `duty[0..9] = buf[1..10]` (0–100 %).
3. If host active: apply `duty[0]` to TIM3, leave FAILSAFE.
4. Immediately `hid.push_raw_input(&[0x02, 0x00])` (ACK).
5. Refresh the status buffer and mark a status report pending so the updated
   values are delivered on the next IN poll (ACK has priority on the single IN
   endpoint).

Writes to channels 2–10 change only stored/echoed values; they never touch
hardware. Since Linux's `hid_hw_output_report` sends the whole 32-byte buffer
as one packet, one `pull_raw_output` call is one complete report; no
reassembly keeps state.

### 6.3 PWM generation (TIM3_CH4, PB1)

- 25.0 kHz, edge-aligned, duty 0–100 %.
- HAL setup (verified API shape for `stm32f1xx-hal` 0.11):

  ```rust
  let mut pwm = p.TIM3.pwm_hz::<Tim3NoRemap, _, _>(
      (gpiob.pb1.into_alternate_push_pull(&mut gpiob.crl),),
      &mut afio.mapr,
      25.kHz(),
      &mut rcc,
  );
  pwm.enable(Channel::C4);
  let max = pwm.get_max_duty();
  // duty percent -> compare value; 0 -> low, 100 -> high
  pwm.set_duty(Channel::C4, ((max as u32 * pct as u32 + 50) / 100) as u16);
  ```

- The HAL computes the prescaler/ARR; for 72 MHz this lands on ARR ≈ 2879
  (72 MHz / 2880 = 25.0 kHz). The exact register values are the HAL's job; the
  requirement is 25 kHz ±1 % and a monotonic 0–100 % duty mapping.
- Edge-aligned (not center-aligned) is fine: 4-pin fans only care about
  frequency and average duty.
- No PWM interrupt is needed.

### 6.4 Tachometer spoof

`rpm1 = duty1_effective * 25` (0 % → 0, 50 % → 1250, 100 % → 2500).
Channels 2–10 → 0 RPM always. Integer math only; no smoothing/spin-up lag.

### 6.5 Periodic status report (~1 Hz)

- A 32-byte status buffer is refreshed every 1 s:
  - bytes 1–10: effective duty per channel — ch 1 = current effective value
    (100 in FAILSAFE, else commanded); ch 2–10 = last commanded (0 at boot).
  - bytes 11–30: RPM per channel (LE16), per §6.4.
  - byte 0 = `0x01`, byte 31 = 0.
- `hid.push_raw_input(&status)` when due; if the IN endpoint is busy the call
  returns `WouldBlock` and the push is retried on the next loop iteration.
- A fresh status is also pushed after each applied OUT (after the ACK), so a
  duty change is visible to the host on the next poll.
- The 1 s tick comes from SysTick at 1 kHz (or a free hardware timer); an
  interrupt only increments counters. The V-USB “no timer interrupt” constraint
  no longer applies.

### 6.6 Main loop

```rust
loop {
    usb_dev.poll(&mut [&mut hid]);   // service USB control/data events
    service_host_state(&usb_dev);    // !Configured/Suspend -> FAILSAFE
    service_out(&mut hid);           // pull_raw_output -> apply -> ACK
    service_status(&mut hid);        // 1 Hz + post-OUT pending
}
```

All work is microsecond-scale and non-blocking; no delays in the loop.

## 7. Constants

| Constant          | Value    | Notes                          |
|-------------------|----------|--------------------------------|
| `RPM_MAX`         | 2500     | spoofed RPM at 100 %           |
| `FAILSAFE_DUTY`   | 100      | % — no-host duty               |
| `STATUS_PERIOD`   | 1 s      | status report cadence          |
| `NUM_CHANNELS`    | 10       | matches driver                 |
| `SYSCLK`          | 72 MHz   | HSE 8 MHz × 9                  |
| `USBCLK`          | 48 MHz   | PLL / 1.5                      |
| `PWM_FREQ`        | 25 kHz   | TIM3_CH4 / PB1                 |
| `REPORT_LEN`      | 32       | on-the-wire size incl. report ID |
| Report IDs        | 0x01, 0x02 | data, ACK                    |
| VID / PID         | 0x3904 / 0xF001 | ARCTIC identity         |

## 8. Non-goals

- Real tachometer measurement, per-channel hardware, multi-board fan power.
- Firmware auto/manual curves, thermal sensors, battery operation.
- Windows/macOS drivers, ARCTIC's official host software beyond the hwmon
  driver contract.
- HID feature reports, GET_REPORT, or any use of `usbd-human-interface-device`'s
  higher-level device abstractions.
- Power circuitry, wiring, enclosure, PCB.
- ATtiny85/Digispark/V-USB support (superseded by §1 and §11).

## 9. Build, flash, acceptance

**Tooling:** `direnv allow` (or `nix develop`) loads the flake dev shell with the
Rust toolchain (`thumbv7m-none-eabi`) and flashing tools.

**Build:**

```
cargo build --release
# artifact: target/thumbv7m-none-eabi/release/fan-controller (ELF)
cargo size --release        # optional: flash/RAM usage
```

Debug builds may overflow the 64 KB flash; always flash/measure release builds.

**Flash:**

- **Primary — ST-Link V2 (SWD):**
  `cargo run --release` (the `.cargo/config.toml` runner uses
  `probe-rs run --chip STM32F103C8`), or
  `probe-rs download --chip STM32F103C8 target/thumbv7m-none-eabi/release/fan-controller`.
- **Fallback — DFU (BOOT0=1, reset):**
  `cargo objcopy --release -- -O binary fan-controller.bin`
  then `dfu-util -a 0 -s 0x08000000:leave -D fan-controller.bin`.
- **Fallback — UART bootloader:** `stm32flash` on PA9/PA10 with BOOT0=1.
- No fuses or option bytes are changed; stock board configuration.

**Acceptance** (host: Linux ≥ 7.2, stock kernel, no patches):

1. `lsusb` shows `3904:f001` (`ID 3904:f001`) with strings `ARCTIC` /
   `ARCTIC Fan Controller`.
2. `dmesg`: HID interface claimed, `arctic_fan` hwmon device registered; no
   `invalid maxpacket` notices and no control-transfer STALL messages.
3. `/sys/class/hwmon/hwmon*/name` = `arctic_fan`; exactly 10× `fanN_input`
   (0444) and 10× `pwmN_input` (0644).
4. `sensors` lists 10 fans.
5. `echo 50 > .../pwm1` returns 0 within 1 s (ACK path). PB1 scope shows
   ~25 kHz at ~50 % duty; multimeter average ≈ 1.65 V (3.3 V push-pull) or
   ≈ 2.5 V if driven open-drain with the fan's 5 V pull-up.
6. `cat fan1_input` tracks duty linearly: 0 → 0, 50 → ≈1250, 100 → 2500.
   `fan2_input` … `fan10_input` all read 0.
7. `echo 50 > .../pwm2` returns 0; PB1 unchanged; `fan2_input` stays 0.
8. Unplug USB: PB1 rises to 100 % fail-safe. Replug: stays 100 % until the
   first `echo > pwm1`. Same behavior across a host suspend/resume cycle
   (`systemctl suspend` or `rtcwake -m mem -s 5`).
9. Stress: 50 consecutive rapid `pwm1` writes — zero timeouts, no USB errors in
   `dmesg`.

## 10. Risks / notes

- **`usbd-hid` control behavior:** `GET_IDLE` and `GET_REPORT` are rejected with
  STALL; `SET_IDLE`, `SET_PROTOCOL` and `SET_REPORT` are accepted. Linux
  `usbhid` sends `SET_IDLE` in `usbhid_parse()` and has no `GET_IDLE` request
  path at all; `arctic_fan` never issues `GET_REPORT`. This should therefore be
  invisible. Verify `dmesg` during item 2; if STALL noise appears, patch
  `usbd-hid` or implement a small custom `UsbClass` (the report descriptor and
  endpoint plan in §5 stay the same).
- **Endpoint size:** `usbd-hid` allocates 64-byte endpoints. The driver's report
  sizes come from the HID descriptor (32/32/2); 32-byte packets are short
  packets and complete the host URBs. No functional impact. Exact 32-byte EP
  descriptors would require a custom class or crate patch.
- **Blue Pill clone variability:** the board must have a working 8 MHz HSE
  crystal (required for USB). Some clones lack it or have poor boards; some
  ST-Link clones need connect-under-reset. DFU/UART fallbacks are provided.
- **PWM level:** see §2. Prefer open-drain or a MOSFET if the fan insists on 5 V
  logic; always common ground.
- **100 % fail-safe on attach** means a fan on this channel runs full speed
  until the host sends a command — intended trade-off, documented in README.
- **RPM spoof is visible** to anyone cross-checking against a tach probe; that
  is the intended trade-off.
- **VID/PID:** `0x3904/0xF001` are ARCTIC's identifiers. Using them makes the
  stock driver bind and is fine for a personal interoperability experiment; it
  is not USB-IF-compliant for a redistributed product.

**Verification status (at spec writing time):** the host-driver contract (§3),
`usbd-hid`/`usb-device`/`stm32-usbd`/`stm32f1xx-hal` APIs and versions (§4–§5),
Linux `usbhid` request behavior, and the Nix toolchain were checked against
their sources. Not yet verifiable without the board or an implemented crate:
`hid_parse`/driver binding with this descriptor, the suspend/unplug fail-safe
path, PWM scope measurements, ACK latency, and the `.cargo`/`memory.x` build
itself. The product strings and the endpoint `bInterval` value are cosmetic
choices; the driver checks neither.

## 11. Decisions log

| Question | Decision |
|----------|----------|
| MCU / USB stack | STM32F103C8T6 (Blue Pill) + Rust `usb-device` + `usbd-hid`; full-speed USB is required because the protocol uses 32-byte interrupt reports and V-USB/low-speed cannot carry them |
| HID crate | `usbd-hid` (small, raw report descriptors and raw IN/OUT reports) over `usbd-human-interface-device` (higher-level standard-device abstractions, extra typestate and a 1 ms `tick()`; workable but unnecessary for a bespoke protocol) |
| Duty scale in reports | 0–100 %, matching mainline driver exactly |
| No-host duty | Fixed 100 % fail-safe (boot, unplug, suspend) |
| Host-activity detection | `UsbDeviceState::Configured` + suspend state from `usb-device`; no D-line sampling |
| RPM spoof shape | Pure linear duty→RPM, no spin-up lag |
| PWM | TIM3_CH4 / PB1, 25 kHz edge-aligned, HAL `pwm_hz` |
| Endpoints | IN+OUT interrupt, 64-byte max packet (`usbd-hid` default), bInterval 10 ms; report sizes 32/32/2 from descriptor |
| Flashing | ST-Link + `probe-rs` primary; DFU and UART bootloader fallbacks |
| Tooling | Nix flake (`rust-overlay`, target `thumbv7m-none-eabi`) + direnv; no rustup-managed toolchain |
| Verification | Acceptance §9 including suspend/resume and stress |
