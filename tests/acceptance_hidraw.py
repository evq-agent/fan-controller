#!/usr/bin/env python3
"""Userspace acceptance test for the ARCTIC Fan Controller clone.

Talks raw HID over /dev/hidrawN (kernel usbhid bound; the arctic_fan hwmon
driver is NOT required). Exercises the SPEC.md contract byte-exactly:

  T1  identity          VID/PID/strings (3904:f001, ARCTIC / ARCTIC Fan Controller)
  T2  report descriptor 54 bytes, byte-exact vs SPEC.md §5
  T3  failsafe state    after (re-)enumeration, before any OUT: ch1+ch2 = 100 %, 2500 RPM
  T4  OUT -> ACK        ACK report (ID 0x02, status 0x00) arrives < 1 s
  T5  RPM spoof         ch1+ch2 RPM == duty * 25, PWM byte echoes duty
  T6  mock channels     ch3-10 echo duty, always report 0 RPM
  T7  stress            50 rapid writes, zero ACK timeouts

Hardware-only checks (PB0/PB1 frequency/duty, unplug behavior) are printed as
manual notes; the unplug/replug re-check is offered interactively (T8).

Usage:
    sudo python3 tests/acceptance_hidraw.py [dev] [--no-replug]

    dev          /dev/hidrawN to use (default: auto-detect 3904:f001)
    --no-replug  skip the interactive replug failsafe re-check
"""

import glob
import os
import select
import subprocess
import sys
import time

# --------------------------------------------------------------------------
# hidraw plumbing
# --------------------------------------------------------------------------

# The report descriptor is read from the kernel's enumeration-time cache
# (standard GET_DESCRIPTOR control transfer), exposed at
# /sys/class/hidraw/hidrawN/device/report_descriptor — that is the
# on-the-wire descriptor we want to verify. (The legacy HIDRAW_GET_REPORT
# ioctl was removed from the kernel; it issues a HID GET_REPORT *request*
# which this firmware STALLs by design, SPEC.md §10.)

# Byte-exact HID report descriptor, SPEC.md §5 (54 bytes).
SPEC_DESCRIPTOR = bytes(
    [
        0x05,
        0x01,  # Usage Page (Generic Desktop)
        0x09,
        0x01,  # Usage (Pointer)
        # Output report, ID 1: 31 bytes
        0x85,
        0x01,
        0x19,
        0x01,
        0x29,
        0x1F,
        0x15,
        0x00,
        0x26,
        0xFF,
        0x00,
        0x75,
        0x08,
        0x95,
        0x1F,
        0x81,
        0x02,
        # Input report, ID 1: 31 bytes (10 PWM + 20 RPM + 1 reserved)
        0x85,
        0x01,
        0x19,
        0x01,
        0x29,
        0x1F,
        0x15,
        0x00,
        0x26,
        0xFF,
        0x00,
        0x75,
        0x08,
        0x95,
        0x1F,
        0x81,
        0x02,
        # Input report, ID 2: 1 byte (ACK status)
        0x85,
        0x02,
        0x19,
        0x02,
        0x29,
        0x02,
        0x15,
        0x00,
        0x25,
        0x01,
        0x75,
        0x08,
        0x95,
        0x01,
        0x81,
        0x02,
    ]
)

REPORT_ID_DATA = 0x01
REPORT_ID_ACK = 0x02
ACK_TIMEOUT_S = 1.0  # driver's ARCTIC_ACK_TIMEOUT_MS


def find_device():
    """Auto-detect the hidraw node for 3904:f001 via sysfs."""
    for node in sorted(glob.glob("/dev/hidraw*")):
        dev = os.path.realpath(f"/sys/class/hidraw/{os.path.basename(node)}/device")
        d = dev
        for _ in range(3):  # HID device -> interface -> usb device
            vfile = os.path.join(d, "idVendor")
            if os.path.exists(vfile):
                vid = open(vfile).read().strip()
                pid = open(os.path.join(d, "idProduct")).read().strip()
                if vid == "3904" and pid == "f001":
                    return node
                break
            d = os.path.dirname(d)
    return None


def open_hidraw(dev):
    fd = os.open(dev, os.O_RDWR | os.O_NONBLOCK)
    return fd


def read_report(fd, timeout=2.0):
    """Return (report_id, payload) for the next IN report."""
    deadline = time.monotonic() + timeout
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"no HID report within {timeout:.1f} s")
        r, _, _ = select.select([fd], [], [], remaining)
        if not r:
            continue
        data = os.read(fd, 64)
        if not data:
            continue
        return data[0], data[1:]


def get_status(fd, timeout=2.0):
    """Next status report (ID 0x01) -> (pwm[10], rpm[10])."""
    while True:
        rid, payload = read_report(fd, timeout)
        if rid == REPORT_ID_DATA and len(payload) == 31:
            pwm = list(payload[0:10])
            rpm = [
                int.from_bytes(payload[10 + 2 * i : 12 + 2 * i], "little")
                for i in range(10)
            ]
            return pwm, rpm
        # else: an ACK report passed through; keep waiting for status


def write_duties(fd, duties):
    """Send one 32-byte OUT report: ID 0x01 + 10 duty bytes + 21 zeros."""
    assert len(duties) == 10
    buf = bytes([REPORT_ID_DATA] + [d & 0xFF for d in duties] + [0] * 21)
    written = os.write(fd, buf)
    assert written == 32, f"short write: {written}/32 bytes"


def wait_ack(fd, timeout=ACK_TIMEOUT_S):
    """Next ACK report (ID 0x02) -> status byte (0x00 OK / 0x01 error)."""
    while True:
        rid, payload = read_report(fd, timeout)
        if rid == REPORT_ID_ACK:
            if len(payload) != 1:
                return None
            return payload[0]


def get_report_descriptor(dev):
    """Kernel-cached HID report descriptor, from sysfs."""
    path = f"/sys/class/hidraw/{os.path.basename(dev)}/device/report_descriptor"
    with open(path, "rb") as f:
        return f.read()


# --------------------------------------------------------------------------
# test harness
# --------------------------------------------------------------------------

results = []


def check(name, cond, detail=""):
    results.append((name, bool(cond), detail))
    mark = "PASS" if cond else "FAIL"
    print(f"  [{mark}] {name}" + (f" — {detail}" if detail else ""))
    return cond


def main():
    args = [a for a in sys.argv[1:]]
    do_replug = "--no-replug" not in args
    args = [a for a in args if a != "--no-replug"]
    dev = args[0] if args else find_device()
    if not dev:
        print("error: no /dev/hidraw* device with ID 3904:f001 found", file=sys.stderr)
        return 2
    print(f"device: {dev}\n")

    fd = open_hidraw(dev)

    # Settle: after a fresh bind/enumeration the host driver may still be
    # bringing up the interrupt URBs; wait for the first status report so
    # T3 tests a live device rather than a half-initialized one.
    try:
        get_status(fd, timeout=5.0)
    except TimeoutError:
        print(
            "error: no status report within 5 s; is the device attached?",
            file=sys.stderr,
        )
        return 2

    # T1: identity ----------------------------------------------------------
    print("T1 identity")
    out = subprocess.run(
        ["lsusb", "-v", "-d", "3904:f001"], capture_output=True, text=True
    ).stdout
    check("VID/PID 3904:f001", "3904" in out and "f001" in out)
    check("manufacturer 'ARCTIC'", "iManufacturer" in out and "ARCTIC" in out)
    check(
        "product 'ARCTIC Fan Controller'",
        "ARCTIC Fan Controller" in out,
    )

    # T2: report descriptor -------------------------------------------------
    print("T2 report descriptor")
    try:
        desc = get_report_descriptor(dev)
        check("length 54", len(desc) == 54, f"got {len(desc)}")
        check("byte-exact vs SPEC.md §5", desc == SPEC_DESCRIPTOR)
    except OSError as e:
        check("read sysfs report_descriptor", False, str(e))

    # T3: failsafe state (before any OUT in this session) --------------------
    print("T3 failsafe state (fresh enumeration, no OUT sent yet)")
    try:
        pwm, rpm = get_status(fd)
        check(
            "ch1 PWM = 100",
            pwm[0] == 100,
            f"got {pwm[0]} (replug USB and re-run if this fails)",
        )
        check("ch1 RPM = 2500", rpm[0] == 2500, f"got {rpm[0]}")
        check("ch2 PWM = 100", pwm[1] == 100, f"got {pwm[1]}")
        check("ch2 RPM = 2500", rpm[1] == 2500, f"got {rpm[1]}")
        check(
            "ch3-10 PWM = 0",
            all(d == 0 for d in pwm[2:]),
            f"got {pwm[2:]}",
        )
        check("ch3-10 RPM = 0", all(r == 0 for r in rpm[2:]), f"got {rpm[2:]}")
    except TimeoutError:
        check("status report received", False, "timeout")

    # T4: OUT -> ACK ----------------------------------------------------------
    print("T4 OUT -> ACK")
    duties = [0] * 10
    t0 = time.monotonic()
    write_duties(fd, duties)
    ack = wait_ack(fd)
    latency = time.monotonic() - t0
    check("ACK received (ID 0x02)", ack is not None)
    check("ACK status = 0x00 (OK)", ack == 0x00, f"got {ack}")
    check(
        f"ACK latency < {ACK_TIMEOUT_S:.0f} s",
        latency < ACK_TIMEOUT_S,
        f"{latency * 1000:.1f} ms",
    )

    # T5: linear RPM spoof ----------------------------------------------------
    print("T5 RPM spoof (ch1+ch2 RPM = duty * 25)")
    for duty in (0, 25, 50, 75, 100):
        duties = [duty, duty] + [0] * 8
        write_duties(fd, duties)
        ack = wait_ack(fd)
        pwm, rpm = get_status(fd)
        ok = (
            check(
                f"duty {duty:3d} -> ACK ok",
                ack == 0x00,
                f"ack={ack}",
            )
            and check(
                f"duty {duty:3d} -> PWM1 echo",
                pwm[0] == duty,
                f"got {pwm[0]}",
            )
            and check(
                f"duty {duty:3d} -> RPM1 = {duty * 25}",
                rpm[0] == duty * 25,
                f"got {rpm[0]}",
            )
            and check(
                f"duty {duty:3d} -> PWM2 echo",
                pwm[1] == duty,
                f"got {pwm[1]}",
            )
            and check(
                f"duty {duty:3d} -> RPM2 = {duty * 25}",
                rpm[1] == duty * 25,
                f"got {rpm[1]}",
            )
        )

    # T6: mock channels -------------------------------------------------------
    print("T6 mock channels 3-10")
    duties = [0, 0, 42, 0, 0, 0, 0, 0, 0, 0]
    write_duties(fd, duties)
    ack = wait_ack(fd)
    pwm, rpm = get_status(fd)
    check("write ch3=42 ACK ok", ack == 0x00, f"ack={ack}")
    check("ch3 PWM echoed", pwm[2] == 42, f"got {pwm[2]}")
    check("ch3 RPM stays 0", rpm[2] == 0, f"got {rpm[2]}")
    check(
        "ch1-2 untouched",
        pwm[0] == 0 and rpm[0] == 0 and pwm[1] == 0 and rpm[1] == 0,
        f"pwm={pwm[0]}/{pwm[1]} rpm={rpm[0]}/{rpm[1]}",
    )
    print(
        "  [note] PB0/PB1 hardware unchanged by ch3 writes — verify with a scope/multimeter"
    )

    # T7: stress ---------------------------------------------------------------
    print("T7 stress: 50 rapid ch1 writes")
    worst = 0.0
    timeouts = 0
    bad_ack = 0
    for i in range(50):
        duty = (i * 7) % 101
        duties = [duty] + [0] * 9
        t0 = time.monotonic()
        try:
            write_duties(fd, duties)
            ack = wait_ack(fd)
            lat = time.monotonic() - t0
            worst = max(worst, lat)
            if ack != 0x00:
                bad_ack += 1
        except TimeoutError:
            timeouts += 1
    check("zero ACK timeouts", timeouts == 0, f"{timeouts}/50 timed out")
    check("all ACKs status 0x00", bad_ack == 0, f"{bad_ack} bad")
    check(
        f"worst ACK latency < {ACK_TIMEOUT_S:.0f} s",
        worst < ACK_TIMEOUT_S,
        f"{worst * 1000:.1f} ms",
    )
    pwm, rpm = get_status(fd)
    check(
        "final state consistent",
        rpm[0] == pwm[0] * 25 and rpm[1] == pwm[1] * 25,
        f"pwm={pwm[0]}/{pwm[1]} rpm={rpm[0]}/{rpm[1]}",
    )

    # T8: interactive replug failsafe ------------------------------------------
    if do_replug:
        print("\nT8 replug failsafe (interactive)")
        input(
            "  Unplug the board's USB cable and plug it back in, then press Enter... "
        )
        time.sleep(0.5)  # let the host re-enumerate
        try:
            os.close(fd)
        except OSError:
            pass
        # wait for the node to come back
        deadline = time.monotonic() + 10
        while not os.path.exists(dev) and time.monotonic() < deadline:
            time.sleep(0.2)
        try:
            fd = open_hidraw(dev)
            pwm, rpm = get_status(fd)
            check(
                "after replug, before any OUT: ch1+ch2 = 100 % / 2500 RPM",
                pwm[0] == 100
                and rpm[0] == 2500
                and pwm[1] == 100
                and rpm[1] == 2500,
                f"pwm={pwm[0]}/{pwm[1]} rpm={rpm[0]}/{rpm[1]}",
            )
        except (OSError, TimeoutError) as e:
            check("device re-attached", False, str(e))

    # summary -------------------------------------------------------------------
    print()
    failed = [r for r in results if not r[1]]
    print(f"{'=' * 60}")
    print(f"{len(results) - len(failed)}/{len(results)} checks passed")
    if failed:
        print("failed:")
        for name, _, detail in failed:
            print(f"  - {name}" + (f" ({detail})" if detail else ""))
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
