import time
import json
import pytest
import serial
import paho.mqtt.client as mqtt

from opentelemetry import trace
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor
from opentelemetry.sdk.resources import Resource
from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import OTLPSpanExporter
from opentelemetry.trace.propagation.tracecontext import TraceContextTextMapPropagator

# --- 接続先設定 ---
PORT = "/dev/ttyACM1"
BAUDRATE = 115200
TIMEOUT = 1.0

MQTT_BROKER = "10.0.0.201"
MQTT_PORT = 1883
MQTT_TOPIC = "factory/hil/nucleo-f446re"
OTEL_COLLECTOR_ENDPOINT = "10.0.0.202:4317"

# テストセッション全体のメトリクス収集用
HIL_METRICS = {
    "device_id": "stm32-nucleo-f446re",
    "tests_total": 0,
    "tests_passed": 0,
    "tests_failed": 0,
    "avg_latency_ms": 0.0,
    "max_latency_ms": 0.0,
    "device_uptime_ms": 0,
    "failures": []
}

@pytest.fixture(scope="module")
def stm32_session():
    """実機接続とOpenTelemetry/MQTTパブリッシュを統括するセッションFixture"""
    # 1. OpenTelemetry 初期化
    resource = Resource.create(attributes={
        "service.name": "stm32-hil-runner",
        "service.namespace": "iot-pipeline",
        "host.name": "raspi5"
    })
    provider = TracerProvider(resource=resource)
    otlp_exporter = OTLPSpanExporter(endpoint=OTEL_COLLECTOR_ENDPOINT, insecure=True)
    provider.add_span_processor(BatchSpanProcessor(otlp_exporter))
    trace.set_tracer_provider(provider)
    tracer = trace.get_tracer("stm32.hil.runner", "1.0.0")
    propagator = TraceContextTextMapPropagator()

    # 2. シリアルポート初期化
    s = serial.Serial(PORT, BAUDRATE, timeout=TIMEOUT)
    time.sleep(0.1)
    s.reset_input_buffer()

    # セッション開始（親スパンを生成）
    span = tracer.start_span("run_hil_test_suite")
    span.set_attribute("target.chip", "STM32F446RETx")
    span.set_attribute("target.transport", "USART2_DMA")

    yield s

    # 終了処理：マイコンを安全状態（LED消灯）にする
    s.write(b"LED 0\n")
    s.close()

    # テスト結果の集計と判定
    all_passed = (HIL_METRICS["tests_failed"] == 0)
    span.set_attribute("hil.all_passed", all_passed)
    span.set_attribute("hil.tests_total", HIL_METRICS["tests_total"])
    span.set_attribute("hil.tests_passed", HIL_METRICS["tests_passed"])
    span.set_attribute("hil.tests_failed", HIL_METRICS["tests_failed"])
    span.set_attribute("hil.avg_latency_ms", HIL_METRICS["avg_latency_ms"])
    span.set_attribute("hil.max_latency_ms", HIL_METRICS["max_latency_ms"])

    # W3C TraceContext を MQTT ペイロードへ注入
    carrier = {}
    propagator.inject(carrier=carrier)
    HIL_METRICS["traceparent"] = carrier.get("traceparent")
    HIL_METRICS["status"] = "PASSED" if all_passed else "FAILED"
    HIL_METRICS["timestamp"] = int(time.time() * 1000)

    # 3. MQTT へテストレポートを送信
    client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2)
    try:
        client.connect(MQTT_BROKER, MQTT_PORT, 10)
        client.loop_start()
        payload_json = json.dumps(HIL_METRICS, ensure_ascii=False)
        client.publish(MQTT_TOPIC, payload_json, qos=1)
        time.sleep(0.2)
        client.loop_stop()
        client.disconnect()
        print(f"\n[MQTT] Published HIL test report to {MQTT_TOPIC}")
    except Exception as e:
        print(f"\n[MQTT] Publish error: {e}")

    span.end()
    provider.shutdown()

def send_and_receive(ser, cmd_str: str) -> tuple[str, float]:
    ser.reset_input_buffer()
    cmd_bytes = f"{cmd_str}\n".encode("utf-8")
    
    start_time = time.perf_counter()
    ser.write(cmd_bytes)
    resp = ser.readline().decode("utf-8", errors="ignore").strip()
    elapsed_ms = (time.perf_counter() - start_time) * 1000.0
    return resp, elapsed_ms

# --- Test Cases ---

def test_01_ping_pong(stm32_session):
    HIL_METRICS["tests_total"] += 1
    resp, latency_ms = send_and_receive(stm32_session, "PING")
    if resp == "PONG":
        HIL_METRICS["tests_passed"] += 1
    else:
        HIL_METRICS["tests_failed"] += 1
        HIL_METRICS["failures"].append("PING_PONG")
    assert resp == "PONG"

def test_02_echo_integrity(stm32_session):
    HIL_METRICS["tests_total"] += 1
    payload = "TEST_PAYLOAD_12345_XYZ"
    resp, _ = send_and_receive(stm32_session, f"ECHO {payload}")
    if resp == f"ECHO {payload}":
        HIL_METRICS["tests_passed"] += 1
    else:
        HIL_METRICS["tests_failed"] += 1
        HIL_METRICS["failures"].append("ECHO_INTEGRITY")
    assert resp == f"ECHO {payload}"

def test_03_led_state_transition(stm32_session):
    HIL_METRICS["tests_total"] += 1
    resp_on, _ = send_and_receive(stm32_session, "LED 1")
    resp_stat_on, _ = send_and_receive(stm32_session, "STATUS")
    resp_off, _ = send_and_receive(stm32_session, "LED 0")
    resp_stat_off, _ = send_and_receive(stm32_session, "STATUS")

    success = (
        resp_on == "OK LED=1" and
        "LED=1" in resp_stat_on and
        resp_off == "OK LED=0" and
        "LED=0" in resp_stat_off
    )
    if success:
        HIL_METRICS["tests_passed"] += 1
        # UPTIME パース
        try:
            for part in resp_stat_off.split():
                if part.startswith("UPTIME="):
                    HIL_METRICS["device_uptime_ms"] = int(part.replace("UPTIME=", "").replace("ms", ""))
        except Exception:
            pass
    else:
        HIL_METRICS["tests_failed"] += 1
        HIL_METRICS["failures"].append("LED_STATE")

    assert success

def test_04_realtime_latency_benchmark(stm32_session):
    HIL_METRICS["tests_total"] += 1
    samples = 10
    latencies = []
    
    for _ in range(samples):
        _, lat = send_and_receive(stm32_session, "PING")
        latencies.append(lat)
        time.sleep(0.01)
        
    avg_lat = sum(latencies) / len(latencies)
    max_lat = max(latencies)
    HIL_METRICS["avg_latency_ms"] = round(avg_lat, 2)
    HIL_METRICS["max_latency_ms"] = round(max_lat, 2)

    passed = (max_lat < 15.0)
    if passed:
        HIL_METRICS["tests_passed"] += 1
    else:
        HIL_METRICS["tests_failed"] += 1
        HIL_METRICS["failures"].append("LATENCY_SPIKE")

    assert passed

def test_05_unknown_command_resilience(stm32_session):
    HIL_METRICS["tests_total"] += 1
    resp, _ = send_and_receive(stm32_session, "INVALID_CMD_XYZ")
    if resp == "ERR UNKNOWN_COMMAND":
        HIL_METRICS["tests_passed"] += 1
    else:
        HIL_METRICS["tests_failed"] += 1
        HIL_METRICS["failures"].append("COMMAND_RESILIENCE")
    assert resp == "ERR UNKNOWN_COMMAND"
