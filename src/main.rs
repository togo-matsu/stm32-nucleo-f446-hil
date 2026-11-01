#![no_std]
#![no_main]

use core::fmt::Write as FmtWrite;
use defmt::*;
use embassy_executor::Spawner;
use embassy_stm32::bind_interrupts;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::peripherals;
use embassy_stm32::usart::{self, Config, Uart};
use embassy_time::Timer;
use embedded_io_async::Write;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USART2 => usart::InterruptHandler<peripherals::USART2>;
});

// no_std 環境でスタック上に文字列フォーマットを行う軽量バッファライター
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

#[embassy_executor::task]
async fn monitor_task() {
    let mut seconds: u32 = 0;
    loop {
        Timer::after_secs(5).await;
        seconds += 5;
        info!("[SYS-MONITOR] Uptime: {}s | Background task alive", seconds);
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_stm32::init(Default::default());
    info!("=== [BOOT] STM32F446RE Real-Time Controller Active ===");

    // バックグラウンド監視タスクの起動
    spawner.spawn(monitor_task()).unwrap();

    // オンボード緑色 LED: PA5
    let mut led = Output::new(p.PA5, Level::Low, Speed::Low);
    let mut led_on = false;

    // USART2 初期化 (Nucleo ST-LINK 仮想COMポート直結: PA2=TX, PA3=RX)
    let mut config = Config::default();
    config.baudrate = 115200;

    let mut usart = Uart::new(
        p.USART2,
        p.PA3, // Rx
        p.PA2, // Tx
        Irqs,
        p.DMA1_CH6, // Tx DMA
        p.DMA1_CH5, // Rx DMA
        config,
    ).unwrap();

    info!("[UART] USART2 DMA Active at 115200 bps (Ready for commands)");

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

                info!("[UART RX] Command received: {}", raw_str);

                if raw_str == "PING" {
                    let _ = usart.write_all(b"PONG\r\n").await;
                } else if raw_str == "LED 1" || raw_str == "LED ON" {
                    led.set_high();
                    led_on = true;
                    let _ = usart.write_all(b"OK LED=1\r\n").await;
                } else if raw_str == "LED 0" || raw_str == "LED OFF" {
                    led.set_low();
                    led_on = false;
                    let _ = usart.write_all(b"OK LED=0\r\n").await;
                } else if raw_str == "STATUS" {
                    let mut out_buf = [0u8; 64];
                    let mut writer = BufferWriter::new(&mut out_buf);
                    let uptime_ms = embassy_time::Instant::now().as_millis();
                    let _ = core::write!(
                        writer,
                        "OK UPTIME={}ms LED={}\r\n",
                        uptime_ms,
                        if led_on { 1 } else { 0 }
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
                warn!("[UART RX] Read error: {:?}", defmt::Debug2Format(&e));
            }
        }
    }
}
