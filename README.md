# ARCTIC Fan Controller clone

Firmware for an STM32F103C8T6 "Blue Pill" that presents over USB as an
**ARCTIC Fan Controller** (VID `0x3904`, PID `0xF001`) so the mainline Linux
`arctic_fan` hwmon driver (`drivers/hwmon/arctic_fan_controller.c`, merged for
Linux 7.2) binds to it unpatched and exposes 10× `fanN_input` (RPM) and
10× `pwmN_input` (duty) in sysfs.

- **Channel 1** drives a physical 4-pin PC fan: 25 kHz PWM on PB1 (TIM3_CH4).
- **Channels 2–10** are mocks: writes are accepted and echoed, RPM always 0.
- **RPM for channel 1 is spoofed** as a pure linear function of duty
  (`rpm = duty * 25`); nothing is measured.

The full design contract — USB report layout, state machine, PWM setup,
constants, and acceptance criteria — is in [`SPEC.md`](SPEC.md).

## Wiring

| Function        | Pin / source                                   |
|-----------------|------------------------------------------------|
| Fan PWM input   | PB1 (TIM3_CH4, 25 kHz, 3.3 V push-pull AF)     |
| Fan +12 V       | **Not from the board** — PC 4-pin fan header   |
| GND             | Board GND tied to fan header GND               |
| Fan tach line   | Unconnected (RPM is spoofed)                   |
| USB D- / D+     | PA11 / PA12 (on-board, nothing to wire)        |
| Programming     | SWD: SWDIO PA13, SWCLK PA14, GND (ST-Link V2 / Black Magic Probe) |

Electrical notes:

- The board must be powered from USB during operation; the fan is powered
  from the PC header, never from the board.
- PB1 is 5 V-tolerant as a digital I/O and drives 3.3 V logic, which most
  4-pin fans accept. If your fan insists on a 5 V high, rewire PB1 as
  open-drain and let the fan's internal pull-up set the level, or add a
  small N-MOSFET / level shifter.
- The Blue Pill must have a working 8 MHz HSE crystal (required for USB).
  Some cheap clones lack it.

## Fail-safe behavior

With no active USB host — before the first host command, on unplug, and
during host sleep/suspend — **channel 1 runs at 100 % duty** so a real fan
never stalls on a dead controller. After (re-)enumeration the device stays
at 100 % until the first `echo > pwmN` from the host. State lives in RAM
only; a power cycle always boots into the fail-safe state.

Expect the fan to spin at full speed while the controller is unpowered-by-a
host; that is intended.

## Building

The repo ships a Nix dev shell (`flake.nix` + direnv):

```sh
direnv allow        # or: nix develop
```

The shell provides the Rust toolchain with `thumbv7m-none-eabi`, `probe-rs`,
`dfu-util`, `stm32flash`, `cargo size`, `usbutils`, and `lm_sensors`.

```sh
cargo build --release
# artifact: target/thumbv7m-none-eabi/release/fan-controller (ELF)
cargo size --release
```

Release builds are ~10 KB of flash; even the (optimized) debug build fits in
the C8's 64 KB.

## Flashing

**Primary — SWD probe (ST-Link V2 or Black Magic Probe):**

```sh
probe-rs download --chip STM32F103C8 --protocol swd target/thumbv7m-none-eabi/release/fan-controller
```

`--protocol swd` is required: with a Black Magic Probe, probe-rs (≥ 0.32)
fails without it (*"The selected probe does not support the 'no protocol
specified' interface"*). It is harmless with an ST-Link. Add `--reset` if
you want to be explicit that the core is reset and left running after the
download.

**Fallback — DFU** (BOOT0 = 1 + reset puts the board in the system bootloader):

```sh
cargo objcopy --release -- -O binary fan-controller.bin
dfu-util -a 0 -s 0x08000000:leave -D fan-controller.bin
```

**Fallback — UART bootloader:** `stm32flash -w fan-controller.bin -v` on
PA9 (TX) / PA10 (RX) with BOOT0 = 1.

No fuses or option bytes are changed; the board stays in stock configuration.
If probe-rs/DFU cannot open the probe or DFU device as a normal user on
NixOS, add the packaged udev rules:

```nix
services.udev.packages = [ pkgs.probe-rs-tools ];
# plus the STM32 DFU (0483:df11) udev rules
```

## Verifying on the host (Linux ≥ 7.2)

```sh
lsusb                    # ID 3904:f001, strings ARCTIC / ARCTIC Fan Controller
dmesg | tail             # HID interface claimed, arctic_fan registered
sensors                  # 10 fans listed
echo 50 > /sys/class/hwmon/hwmon*/pwm1   # returns 0 within 1 s (ACK path)
cat  /sys/class/hwmon/hwmon*/fan1_input  # ~1250 at 50 % duty, 2500 at 100 %
```

`fan2_input`…`fan10_input` read 0 regardless of their pwm values; writes to
those channels change only the echoed duty bytes, not PB1. Unplugging USB
returns PB1 to 100 %; replugging stays at 100 % until the first `echo > pwm1`.

## Layout

| File               | Purpose                                            |
|--------------------|----------------------------------------------------|
| `src/main.rs`      | init, clock config, SysTick tick, main loop        |
| `src/usb.rs`       | ARCTIC identity, HID class + endpoint wiring       |
| `src/protocol.rs`  | report descriptor, state machine, status buffer    |
| `src/pwm.rs`       | TIM3_CH4 / PB1 setup + duty mapping                |
| `SPEC.md`          | full design contract & acceptance criteria         |
| `flake.nix` / `.envrc` | Nix dev shell (Rust + embedded tools)        |
