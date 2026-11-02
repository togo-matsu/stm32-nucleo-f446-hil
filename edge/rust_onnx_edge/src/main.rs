use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::Manager;
use futures::stream::StreamExt;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;
use serialport::SerialPort;
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

// OpenTelemetry 関連
use opentelemetry::global;
use opentelemetry::trace::{Span, TraceContextExt, Tracer};
use opentelemetry::{Context, KeyValue};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::TracerProvider;
use opentelemetry_sdk::Resource;

const TARGET_DEVICE_NAME: &str = "Nano33Sense-Rev2";
const TARGET_MAC_ADDR: &str = "48:91:4d:98:5a:fe";
const CHAR_UUID_STR: &str = "19b10001-e8f2-537e-4f6c-d104768a1214";

const SERIAL_PORT_PATH: &str = "/dev/ttyACM0";
const SERIAL_BAUD_RATE: u32 = 115_200;
const ONNX_MODEL_PATH: &str = "/home/you/onnx-workspace/anomaly_model.onnx";

// K8s OTel Collector VIP
const OTLP_ENDPOINT: &str = "http://10.0.0.202:4317";

fn send_stm32_cmd(port: &mut Box<dyn SerialPort>, cmd: &str) -> std::io::Result<String> {
    port.clear(serialport::ClearBuffer::Input)?;
    port.write_all(format!("{}\n", cmd).as_bytes())?;
    port.flush()?;

    let mut reader = BufReader::new(port);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    Ok(response.trim().to_string())
}

// OpenTelemetry Tracer 初期化
fn init_tracer() -> Result<(), Box<dyn std::error::Error>> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(OTLP_ENDPOINT)
        .build()?;

    let resource = Resource::new(vec![
        KeyValue::new("service.name", "edge-ai-controller"),
        KeyValue::new("host.name", "raspberrypi-5"),
        KeyValue::new("device.id", "nano33-sense-rev2"),
    ]);

    let provider = TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(resource)
        .build();

    global::set_tracer_provider(provider);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== [INIT] Edge AI Hybrid Anomaly Controller Active ===");

    // 1. OpenTelemetry 初期化
    init_tracer()?;
    let tracer = global::tracer("edge-ai-controller");
    println!("[OTEL] Tracer initialized -> Exporting to {}", OTLP_ENDPOINT);

    // 2. STM32 Nucleo 初期化
    println!("[SERIAL] Connecting to STM32 on {}...", SERIAL_PORT_PATH);
    let mut raw_port = serialport::new(SERIAL_PORT_PATH, SERIAL_BAUD_RATE)
        .timeout(Duration::from_millis(1500))
        .open()?;
    std::thread::sleep(Duration::from_millis(200));

    send_stm32_cmd(&mut raw_port, "RESET_FAULT")?;
    let r_on = send_stm32_cmd(&mut raw_port, "LED 1")?;
    println!("[STM32] Normal Operation Started -> Actuator: {}", r_on);
    let stm32_port = Arc::new(Mutex::new(raw_port));

    // 3. ONNX Runtime 初期化
    println!("[AI] Initializing ONNX Runtime Session from {}...", ONNX_MODEL_PATH);
    let session = Session::builder()?
        .with_optimization_level(GraphOptimizationLevel::Level3)?
        .with_intra_threads(4)?
        .commit_from_file(ONNX_MODEL_PATH)?;
    let onnx_session = Arc::new(session);

    // 4. BLE Central 初期化 & Nano 33 探索
    println!("[BLE] Scanning for {} (MAC: {})...", TARGET_DEVICE_NAME, TARGET_MAC_ADDR);
    let manager = Manager::new().await?;
    let adapters = manager.adapters().await?;
    let central = adapters.into_iter().next().ok_or("No Bluetooth adapter found")?;

    central.start_scan(ScanFilter::default()).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    let peripherals = central.peripherals().await?;
    let mut target_peripheral = None;

    for p in peripherals {
        if let Ok(Some(props)) = p.properties().await {
            let name_match = props.local_name.as_deref() == Some(TARGET_DEVICE_NAME);
            let mac_match = props.address.to_string().to_lowercase() == TARGET_MAC_ADDR;
            if name_match || mac_match {
                target_peripheral = Some(p);
                break;
            }
        }
    }

    let peripheral = target_peripheral.ok_or_else(|| {
        format!("Device '{}' not found. Check if Nano 33 is powered on.", TARGET_DEVICE_NAME)
    })?;

    central.stop_scan().await?;
    println!("[BLE] Connecting to Nano 33 Sense...");
    peripheral.connect().await?;
    peripheral.discover_services().await?;

    let char_uuid = Uuid::parse_str(CHAR_UUID_STR)?;
    let chars = peripheral.characteristics();
    let telemetry_char = chars
        .iter()
        .find(|c| c.uuid == char_uuid)
        .ok_or("Telemetry characteristic not found")?;

    println!("[BLE] Connected! Subscribing to telemetry notifications...");
    peripheral.subscribe(telemetry_char).await?;
    let mut notification_stream = peripheral.notifications().await?;

    println!("\n\x1b[32;1m=== [STREAMING READY] OpenTelemetry Spans Active ===\x1b[0m\n");

    let mut is_locked_out = false;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\n[SHUTDOWN] Flushing OpenTelemetry traces...");
                let _ = peripheral.disconnect().await;
                global::shutdown_tracer_provider();
                println!("[SHUTDOWN] Completed. Exiting."); std::process::exit(0);
                break;
            }
            Some(notif) = notification_stream.next() => {
                if notif.uuid != char_uuid || notif.value.len() < 20 {
                    continue;
                }

                let data = notif.value;

                // --- Root Span: telemetry_cycle 開始 ---
                let mut root_span = tracer.start("telemetry_cycle");
                let rms_accel = f32::from_le_bytes(data[4..8].try_into().unwrap());
                let rms_sound = f32::from_le_bytes(data[8..12].try_into().unwrap());
                let nano_score = f32::from_le_bytes(data[12..16].try_into().unwrap());
                let nano_is_anom = data[16] != 0;
                let nano_type = data[17];

                root_span.set_attribute(KeyValue::new("sensor.rms_accel", rms_accel as f64));
                root_span.set_attribute(KeyValue::new("sensor.rms_sound", rms_sound as f64));
                root_span.set_attribute(KeyValue::new("nano33.is_anomaly", nano_is_anom));
                root_span.set_attribute(KeyValue::new("nano33.score", nano_score as f64));
                root_span.set_attribute(KeyValue::new("nano33.type", nano_type as i64));

                let parent_cx = Context::new().with_remote_span_context(root_span.span_context().clone());

                // --- Child Span: onnx_inference ---
                let mut infer_span = tracer.start_with_context("onnx_inference", &parent_cx);
                let raw_features = [rms_accel, rms_sound, 0.0_f32];
                let input_tensor = Tensor::from_array(([1usize, 3usize], raw_features.to_vec()))?;

                let infer_start = Instant::now();
                let outputs = onnx_session.run(ort::inputs!["sensor_input" => input_tensor]?)?;
                let infer_time = infer_start.elapsed();

                let (_, output_slice) = outputs["anomaly_prob"].try_extract_raw_tensor::<f32>()?;
                let onnx_prob = output_slice[0];
                let onnx_is_anom = onnx_prob >= 0.80;

                infer_span.set_attribute(KeyValue::new("onnx.prob", onnx_prob as f64));
                infer_span.set_attribute(KeyValue::new("onnx.latency_us", infer_time.as_micros() as i64));
                infer_span.set_attribute(KeyValue::new("onnx.is_anomaly", onnx_is_anom));
                infer_span.end();

                let is_anomaly = onnx_is_anom || nano_is_anom;

                if is_anomaly && !is_locked_out {
                    println!("\n\x1b[31;1m[!ALERT!] Anomaly -> Level1: {}, Level2: {:.4}\x1b[0m", nano_is_anom, onnx_prob);

                    // --- Child Span: stm32_estop_cutoff ---
                    let mut estop_span = tracer.start_with_context("stm32_estop_cutoff", &parent_cx);
                    let mut port = stm32_port.lock().unwrap();

                    let estop_start = Instant::now();
                    let estop_resp = send_stm32_cmd(&mut port, "ESTOP")?;
                    let estop_lat = estop_start.elapsed();

                    estop_span.set_attribute(KeyValue::new("stm32.latency_ms", estop_lat.as_secs_f64() * 1000.0));
                    estop_span.set_attribute(KeyValue::new("stm32.response", estop_resp.clone()));
                    estop_span.end();

                    println!("\x1b[33m[SAFETY ACTION] Cutoff: '{}' (Latency: {:.2?})\x1b[0m", estop_resp, estop_lat);

                    let reject_resp = send_stm32_cmd(&mut port, "LED 1")?;
                    assert_eq!(reject_resp, "ERR INTERLOCK_ACTIVE");
                    println!("\x1b[32m[PASS] Hardware Interlock verified.\x1b[0m\n");

                    root_span.set_attribute(KeyValue::new("event.action", "ESTOP_TRIGGERED"));
                    is_locked_out = true;
                } else if !is_locked_out {
                    root_span.set_attribute(KeyValue::new("event.action", "NORMAL_STREAM"));
                    println!(
                        "[STREAM] Accel: {:>5.2}G | Sound: {:>5.1} | ONNX Prob: {:>6.4} | Status: NORMAL",
                        rms_accel, rms_sound, onnx_prob
                    );
                }

                root_span.end();
            }
        }
    }

    Ok(())
}
