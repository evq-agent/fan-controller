#!/usr/bin/env python3
"""Manual control tool for the ARCTIC Fan Controller clone (via hidraw).

Talks the raw HID protocol (SPEC.md §3) to /dev/hidrawN; no arctic_fan
driver required.

Usage:
    sudo python3 tools/fanctl.py status            # one-shot table of all channels
    sudo python3 tools/fanctl.py set <ch> <duty>   # set channel 1-10 duty (0-100 %)
    sudo python3 tools/fanctl.py all <duty>        # set every channel to <duty>
    sudo python3 tools/fanctl.py zero              # all channels to 0
    sudo python3 tools/fanctl.py watch             # stream status ~1 Hz (Ctrl-C to stop)
    sudo python3 tools/fanctl.py -d /dev/hidraw1 ...   # explicit device

Notes:
  - The OUT report carries all 10 duties; `set` does read-modify-write so
    the other channels keep their current values.
  - ch1 and ch2 drive real fans (PB0 / PB1); ch3-10 are mocks (echo only, RPM 0).
  - ch1-2 RPM is spoofed: rpm = duty * 25.
"""

import glob
import os
import select
import sys
import time

REPORT_ID_DATA = 0x01
REPORT_ID_ACK = 0x02
ACK_TIMEOUT_S = 1.0


def find_device():
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
    return os.open(dev, os.O_RDWR | os.O_NONBLOCK)


def read_report(fd, timeout=2.0):
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


def write_duties(fd, duties, timeout=ACK_TIMEOUT_S):
    """Send OUT report and wait for the ACK. Returns ACK latency (s)."""
    assert len(duties) == 10
    buf = bytes([REPORT_ID_DATA] + [d & 0xFF for d in duties] + [0] * 21)
    t0 = time.monotonic()
    os.write(fd, buf)
    while True:
        rid, payload = read_report(fd, timeout)
        if rid == REPORT_ID_ACK and len(payload) == 1:
            if payload[0] != 0x00:
                raise RuntimeError(f"device returned error status {payload[0]:#04x}")
            return time.monotonic() - t0


def print_status(pwm, rpm, stamp=False):
    ts = time.strftime("%H:%M:%S") + " " if stamp else ""
    print(f"{ts}{'ch':>3} {'duty %':>7} {'RPM':>6}")
    for i in range(10):
        hw = {0: " (PB0)", 1: " (PB1)"}.get(i, "")
        print(f"{'':>4}{i + 1:>2} {pwm[i]:>8} {rpm[i]:>6}{hw}")


def main():
    args = sys.argv[1:]
    dev = None
    if args and args[0] == "-d":
        dev = args[1]
        args = args[2:]
    if not dev:
        dev = find_device()
    if not dev:
        print("error: no /dev/hidraw* device with ID 3904:f001 found", file=sys.stderr)
        return 2

    if not args:
        print(__doc__)
        return 2

    cmd = args[0]
    fd = open_hidraw(dev)
    try:
        if cmd == "status":
            print_status(*get_status(fd))
            return 0

        if cmd == "watch":
            print(f"watching {dev} (Ctrl-C to stop)")
            try:
                while True:
                    print_status(*get_status(fd), stamp=True)
                    time.sleep(0.5)
            except KeyboardInterrupt:
                return 0

        if cmd == "set":
            if len(args) != 3:
                print(
                    "usage: fanctl.py set <channel 1-10> <duty 0-100>", file=sys.stderr
                )
                return 2
            ch, duty = int(args[1]), int(args[2])
            if not 1 <= ch <= 10 or not 0 <= duty <= 100:
                print("channel must be 1-10, duty 0-100", file=sys.stderr)
                return 2
            pwm, _ = get_status(fd)  # read-modify-write
            pwm[ch - 1] = duty
            lat = write_duties(fd, pwm)
            print(f"set ch{ch} = {duty}% (ACK in {lat * 1000:.1f} ms)")
            print_status(*get_status(fd))
            return 0

        if cmd == "all":
            if len(args) != 2:
                print("usage: fanctl.py all <duty 0-100>", file=sys.stderr)
                return 2
            duty = int(args[1])
            if not 0 <= duty <= 100:
                print("duty must be 0-100", file=sys.stderr)
                return 2
            lat = write_duties(fd, [duty] * 10)
            print(f"all channels = {duty}% (ACK in {lat * 1000:.1f} ms)")
            print_status(*get_status(fd))
            return 0

        if cmd == "zero":
            lat = write_duties(fd, [0] * 10)
            print(f"all channels = 0% (ACK in {lat * 1000:.1f} ms)")
            print_status(*get_status(fd))
            return 0

        print(f"unknown command: {cmd}", file=sys.stderr)
        print(__doc__)
        return 2
    finally:
        os.close(fd)


if __name__ == "__main__":
    sys.exit(main())
