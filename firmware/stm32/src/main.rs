#![no_std]
#![no_main]

use core::fmt::Write as FmtWrite;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use defmt::*;
use embassy_executor::Spawner;
use embassy_stm32::adc::{Adc, SampleTime};
use embassy_stm32::bind_interrupts;
use embassy_stm32::exti::ExtiInput;
use embassy_stm32::gpio::{Level, Output, Pull, Speed};
use embassy_stm32::peripherals;
use embassy_stm32::usart::{self, Config, Uart};
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::Timer;
use embedded_io_async::Write;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USART2 => usart::InterruptHandler<peripherals::USART2>;
});

// --- システム全体のリアルタイム共有状態 ---
static IS_EMERGENCY: AtomicBool = AtomicBool::new(false);
static LED_STATE: AtomicBool = AtomicBool::new(false);
static ESTOP_COUNT: AtomicU32 = AtomicU32::new(0);
static FPGA_PULSE_COUNT: AtomicU32 = AtomicU32::new(0);

// LED 制御コマンドの定義
#[derive(Clone, Copy)]
enum LedMsg {
    Set(bool),
    Pulse,
    EmergencyOff,
}

// タスク間で安全に LED 動作を要求するための非同期チャンネル
static LED_CHANNEL: Channel<ThreadModeRawMutex, LedMsg, 8> = Channel::new();

struct BufferWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> BufferWriter<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }
    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl<'a> FmtWrite for BufferWriter<'a> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        if self.len + bytes.len() > self.buf.len() {
            return Err(core::fmt::Error);
        }
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        Ok(())
    }
}

// タスク1: オンボード青ボタン (PC13) によるハードウェア外部割り込み緊急停止
#[embassy_executor::task]
async fn button_estop_task(mut button: ExtiInput<'static>) {
    info!("[EXTI] User Button (PC13) E-Stop monitor active");
    loop {
        button.wait_for_falling_edge().await;
        IS_EMERGENCY.store(true, Ordering::SeqCst);
        let cnt = ESTOP_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = LED_CHANNEL.try_send(LedMsg::EmergencyOff);
        warn!("[EXTI] Hardware E-STOP triggered via PC13! (Count: {})", cnt);
        Timer::after_millis(50).await;
    }
}

// タスク2: FPGA (Tang Nano 9K Pin 31) からの外部割り込み検知 (D2 / PA10)
#[embassy_executor::task]
async fn fpga_trigger_task(mut trigger: ExtiInput<'static>) {
    info!("[EXTI] FPGA Trigger (PA10 / D2) monitor active");
    loop {
        trigger.wait_for_rising_edge().await;
        let cnt = FPGA_PULSE_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
        // LED 点滅タスクへパルス通知 (ノンブロッキング)
        let _ = LED_CHANNEL.try_send(LedMsg::Pulse);
        info!("[FPGA-EXTI] Hardware pulse detected on Pin D2 (PA10)! (Count: {})", cnt);
    }
}

// タスク3: 独立した LED (PA5 / LD2) 駆動タスク
#[embassy_executor::task]
async fn led_task(mut led: Output<'static>) {
    let mut manual_on = false;
    loop {
        match LED_CHANNEL.receive().await {
            LedMsg::Set(on) => {
                manual_on = on;
                if manual_on {
                    led.set_high();
                } else {
                    led.set_low();
                }
            }
            LedMsg::Pulse => {
                // 手動点灯中でなく、かつ緊急停止中でない場合のみパルス点滅
                if !manual_on && !IS_EMERGENCY.load(Ordering::Relaxed) {
                    led.set_high();
                    Timer::after_millis(60).await;
                    led.set_low();
                }
            }
            LedMsg::EmergencyOff => {
                manual_on = false;
                led.set_low();
            }
        }
    }
}

// タスク4: 定期システム監視
#[embassy_executor::task]
async fn monitor_task() {
    let mut seconds: u32 = 0;
    loop {
        Timer::after_secs(5).await;
        seconds += 5;
        let mode = if IS_EMERGENCY.load(Ordering::Relaxed) { "EMERGENCY" } else { "NORMAL" };
        let fpga_cnt = FPGA_PULSE_COUNT.load(Ordering::Relaxed);
        info!("[SYS-MONITOR] Uptime: {}s | Mode: {} | FPGA Pulses: {}", seconds, mode, fpga_cnt);
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_stm32::init(Default::default());
    info!("=== [BOOT] STM32F446RE Functional HIL Controller ===");

    // オンボード緑色 LED (PA5) の初期化と専用タスク起動
    let led = Output::new(p.PA5, Level::Low, Speed::Low);
    spawner.spawn(led_task(led)).unwrap();

    // EXTI タスク・監視タスク起動
    let button = ExtiInput::new(p.PC13, p.EXTI13, Pull::Up);
    spawner.spawn(button_estop_task(button)).unwrap();

    let fpga_trigger = ExtiInput::new(p.PA10, p.EXTI10, Pull::Down);
    spawner.spawn(fpga_trigger_task(fpga_trigger)).unwrap();

    spawner.spawn(monitor_task()).unwrap();

    // 内蔵 ADC1 の初期化
    let mut adc = Adc::new(p.ADC1);
    adc.set_sample_time(SampleTime::CYCLES480);
    let mut vrefint_channel = adc.enable_vrefint();
    let mut temp_channel = adc.enable_temperature();
    Timer::after_millis(10).await;

    // USART2 DMA 初期化
    let mut config = Config::default();
    config.baudrate = 115200;
    let mut usart = Uart::new(
        p.USART2,
        p.PA3,
        p.PA2,
        Irqs,
        p.DMA1_CH6,
        p.DMA1_CH5,
        config,
    ).unwrap();

    info!("[READY] Functional HIL Firmware Ready on USART2");

    let mut rx_buf = [0u8; 128];
    loop {
        match usart.read_until_idle(&mut rx_buf).await {
            Ok(n) if n > 0 => {
                let raw_str = match core::str::from_utf8(&rx_buf[..n]) {
                    Ok(s) => s.trim(),
                    Err(_) => {
                        let _ = usart.write_all(b"ERR INVALID_UTF8\r\n").await;
                        continue;
                    }
                };

                if raw_str.is_empty() {
                    continue;
                }

                info!("[UART RX] Cmd: {}", raw_str);

                if raw_str == "PING" {
                    let _ = usart.write_all(b"PONG\r\n").await;

                } else if raw_str == "ESTOP" {
                    IS_EMERGENCY.store(true, Ordering::SeqCst);
                    ESTOP_COUNT.fetch_add(1, Ordering::SeqCst);
                    LED_STATE.store(false, Ordering::SeqCst);
                    let _ = LED_CHANNEL.send(LedMsg::EmergencyOff).await;
                    warn!("[SAFETY] E-STOP activated! Actuator locked.");
                    let _ = usart.write_all(b"OK MODE=EMERGENCY LED=0\r\n").await;

                } else if raw_str == "RESET_FAULT" {
                    IS_EMERGENCY.store(false, Ordering::SeqCst);
                    info!("[SAFETY] Fault cleared. System back to NORMAL.");
                    let _ = usart.write_all(b"OK MODE=NORMAL\r\n").await;

                } else if raw_str == "LED 1" || raw_str == "LED ON" {
                    if IS_EMERGENCY.load(Ordering::SeqCst) {
                        warn!("[SAFETY] Rejected LED ON due to active E-STOP interlock!");
                        let _ = usart.write_all(b"ERR INTERLOCK_ACTIVE\r\n").await;
                    } else {
                        LED_STATE.store(true, Ordering::SeqCst);
                        let _ = LED_CHANNEL.send(LedMsg::Set(true)).await;
                        let _ = usart.write_all(b"OK LED=1\r\n").await;
                    }

                } else if raw_str == "LED 0" || raw_str == "LED OFF" {
                    LED_STATE.store(false, Ordering::SeqCst);
                    let _ = LED_CHANNEL.send(LedMsg::Set(false)).await;
                    let _ = usart.write_all(b"OK LED=0\r\n").await;

                } else if raw_str == "READ_ADC" {
                    let vref_raw = adc.blocking_read(&mut vrefint_channel);
                    let temp_raw = adc.blocking_read(&mut temp_channel);

                    let vref_mv = (vref_raw as u32 * 3300) / 4095;
                    let vsense_mv = (temp_raw as i32 * 3300) / 4095;
                    let temp_c_x10 = ((vsense_mv - 760) * 100) / 25 + 250;

                    let mut out_buf = [0u8; 96];
                    let mut writer = BufferWriter::new(&mut out_buf);
                    let _ = core::write!(
                        writer,
                        "OK VREF_RAW={} VREF_MV={} TEMP_C={}.{}\r\n",
                        vref_raw,
                        vref_mv,
                        temp_c_x10 / 10,
                        (temp_c_x10 % 10).abs()
                    );
                    let _ = usart.write_all(writer.as_bytes()).await;

                } else if raw_str == "STATUS" {
                    let mut out_buf = [128u8; 128];
                    let mut writer = BufferWriter::new(&mut out_buf);
                    let uptime_ms = embassy_time::Instant::now().as_millis();
                    let mode_str = if IS_EMERGENCY.load(Ordering::SeqCst) { "EMERGENCY" } else { "NORMAL" };
                    let led_val = if LED_STATE.load(Ordering::SeqCst) { 1 } else { 0 };
                    let estop_cnt = ESTOP_COUNT.load(Ordering::SeqCst);
                    let fpga_cnt = FPGA_PULSE_COUNT.load(Ordering::SeqCst);

                    let _ = core::write!(
                        writer,
                        "OK MODE={} UPTIME={}ms LED={} ESTOP_CNT={} FPGA_CNT={}\r\n",
                        mode_str,
                        uptime_ms,
                        led_val,
                        estop_cnt,
                        fpga_cnt
                    );
                    let _ = usart.write_all(writer.as_bytes()).await;

                } else if let Some(echo_payload) = raw_str.strip_prefix("ECHO ") {
                    let mut out_buf = [0u8; 128];
                    let mut writer = BufferWriter::new(&mut out_buf);
                    let _ = core::write!(writer, "ECHO {}\r\n", echo_payload);
                    let _ = usart.write_all(writer.as_bytes()).await;

                } else {
                    let _ = usart.write_all(b"ERR UNKNOWN_COMMAND\r\n").await;
                }
            }
            Ok(_) => {}
            Err(e) => {
                warn!("[UART RX] Error: {:?}", defmt::Debug2Format(&e));
            }
        }
    }
}
