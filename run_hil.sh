#!/bin/bash
set -e

# Rust / Cargo / probe-rs のパスを読み込む
source "$HOME/.cargo/env"

echo "=== [1/3] Building STM32 Release Firmware ==="
cargo build --release

echo "=== [2/3] Flashing to STM32F446RE via ST-LINK ==="
probe-rs download --chip STM32F446RETx --connect-under-reset target/thumbv7em-none-eabihf/release/nucleo_f446_app
probe-rs reset --chip STM32F446RETx --connect-under-reset

# マイコン起動待機
sleep 0.5

echo "=== [3/3] Running Pytest HIL Automation Suite ==="
source ~/iot-gateway/venv/bin/activate
pytest -v -s tests/test_hil.py

echo "=== [SUCCESS] Hardware-in-the-Loop Tests Completed ==="
