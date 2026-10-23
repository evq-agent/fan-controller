{
  description = "ARCTIC Fan Controller clone — STM32F103 (Blue Pill) Rust firmware";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    rust-overlay,
  }: let
    systems = [
      "x86_64-linux"
      "aarch64-linux"
    ];
    forAllSystems = nixpkgs.lib.genAttrs systems;
  in {
    devShells = forAllSystems (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [(import rust-overlay)];
        };

        # Bare-metal Rust toolchain with the Cortex-M3 target and the
        # components cargo-binutils / rust-analyzer need.
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          targets = ["thumbv7m-none-eabi"];
          extensions = [
            "rust-src"
            "llvm-tools"
            "rust-analyzer"
          ];
        };

        embeddedTools = with pkgs; [
          probe-rs-tools # primary flash/debug: `cargo run` / `probe-rs`
          dfu-util # fallback: STM32 system bootloader (BOOT0=1)
          stm32flash # fallback: UART bootloader on PA9/PA10
          cargo-binutils # `cargo size`, `cargo objcopy`
          usbutils # `lsusb`
          lm_sensors # `sensors` for the acceptance checks
        ];
      in {
        default = pkgs.mkShell {
          packages = [rustToolchain] ++ embeddedTools;
          shellHook = ''
            echo "ARCTIC fan-controller dev shell (STM32F103 / Blue Pill)"
            echo "  rustc:   $(rustc --version)"
            echo "  cargo:   $(cargo --version)"
            echo "  target:  thumbv7m-none-eabi"
            echo "  build:   cargo build --release"
            echo "  flash:   cargo run --release        # probe-rs via ST-Link (SWD)"
            echo "  dfu:     cargo objcopy --release -- -O binary fw.bin && dfu-util -a 0 -s 0x08000000:leave -D fw.bin"
            echo ""
            echo "NixOS note: if probe-rs/dfu-util cannot access the device as your user,"
            echo "add 'services.udev.packages = [ pkgs.probe-rs-tools ];' (and the STM32 DFU"
            echo "rules) to your system configuration."
          '';
        };
      }
    );
  };
}
