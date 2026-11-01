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

PORT = "/dev/ttyACM1"
BAUDRATE = 115200
TIMEOUT = 1.0

MQTT_BROKER = "10.0.0.201"
MQTT_PORT = 1883
MQTT_TOPIC = "factory/hil/nucleo-f446re"
OTEL_COLLECTOR_ENDPOINT = "10.0.0.202:4317"

HIL_METRICS = {
    "device_id": "stm32-nucleo-f446re",
    "tests_total": 0,
    "tests_passed": 0,
    "tests_failed": 0,
    "avg_latency_ms": 0.0,
    "max_latency_ms": 0.0,
    "vref_mv": 0,
    "mcu_temp_c": 0.0,
    "device_uptime_ms": 0,
    "failures": []
}

@pytest.fixture(scope="module")
def stm32_session():
    resource = Resource.create(attributes={
        "service.name": "stm32-hil-runner",
        "service.namespace": "iot-pipeline",
        "host.name": "raspi5"
    })
    provider = TracerProvider(resource=resource)
    otlp_exporter = OTLPSpanExporter(endpoint=OTEL_COLLECTOR_ENDPOINT, insecure=True)
    provider.add_span_processor(BatchSpanProcessor(otlp_exporter))
    trace.set_tracer_provider(provider)
    tracer = trace.get_tracer("stm32.hil.runner", "1.1.0")
    propagator = TraceContextTextMapPropagator()

    s = serial.Serial(PORT, BAUDRATE, timeout=TIMEOUT)
    time.sleep(0.1)
    s.reset_input_buffer()

    # テスト開始前に必ず NORMAL 状態へリセット
    s.write(b"RESET_FAULT\n")
    s.readline()
    s.write(b"LED 0\n")
    s.readline()

    span = tracer.start_span("run_functional_hil_suite")
    span.set_attribute("target.chip", "STM32F446RETx")
    span.set_attribute("target.transport", "USART2_DMA")

    yield s

    # 終了時も安全状態に戻す
    s.write(b"RESET_FAULT\n")
    s.readline()
    s.write(b"LED 0\n")
    s.readline()
    s.close()

    all_passed = (HIL_METRICS["tests_failed"] == 0)
    span.set_attribute("hil.all_passed", all_passed)
    span.set_attribute("hil.tests_total", HIL_METRICS["tests_total"])
    span.set_attribute("hil.tests_passed", HIL_METRICS["tests_passed"])
    span.set_attribute("hil.tests_failed", HIL_METRICS["tests_failed"])
    span.set_attribute("hil.avg_latency_ms", HIL_METRICS["avg_latency_ms"])
    span.set_attribute("hil.vref_mv", HIL_METRICS["vref_mv"])
    span.set_attribute("hil.mcu_temp_c", HIL_METRICS["mcu_temp_c"])

    carrier = {}
    propagator.inject(carrier=carrier)
    HIL_METRICS["traceparent"] = carrier.get("traceparent")
    HIL_METRICS["status"] = "PASSED" if all_passed else "FAILED"
    HIL_METRICS["timestamp"] = int(time.time() * 1000)

    client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2)
    try:
        client.connect(MQTT_BROKER, MQTT_PORT, 10)
        client.loop_start()
        payload_json = json.dumps(HIL_METRICS, ensure_ascii=False)
        client.publish(MQTT_TOPIC, payload_json, qos=1)
        time.sleep(0.2)
        client.loop_stop()
        client.disconnect()
        print(f"\n[MQTT] Published Functional HIL report to {MQTT_TOPIC}")
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

def record_result(passed: bool, name: str):
    HIL_METRICS["tests_total"] += 1
    if passed:
        HIL_METRICS["tests_passed"] += 1
    else:
        HIL_METRICS["tests_failed"] += 1
        HIL_METRICS["failures"].append(name)

# --- 通信・基本機能テスト ---

def test_01_ping_pong(stm32_session):
    resp, latency_ms = send_and_receive(stm32_session, "PING")
    passed = (resp == "PONG")
    record_result(passed, "PING_PONG")
    assert passed

def test_02_echo_integrity(stm32_session):
    payload = "TEST_PAYLOAD_12345_XYZ"
    resp, _ = send_and_receive(stm32_session, f"ECHO {payload}")
    passed = (resp == f"ECHO {payload}")
    record_result(passed, "ECHO_INTEGRITY")
    assert passed

def test_03_led_state_transition(stm32_session):
    resp_on, _ = send_and_receive(stm32_session, "LED 1")
    resp_stat_on, _ = send_and_receive(stm32_session, "STATUS")
    resp_off, _ = send_and_receive(stm32_session, "LED 0")
    resp_stat_off, _ = send_and_receive(stm32_session, "STATUS")

    passed = (
        resp_on == "OK LED=1" and
        "LED=1" in resp_stat_on and
        resp_off == "OK LED=0" and
        "LED=0" in resp_stat_off
    )
    if passed:
        for part in resp_stat_off.split():
            if part.startswith("UPTIME="):
                HIL_METRICS["device_uptime_ms"] = int(part.replace("UPTIME=", "").replace("ms", ""))
    record_result(passed, "LED_STATE")
    assert passed

def test_04_realtime_latency_benchmark(stm32_session):
    latencies = []
    for _ in range(10):
        _, lat = send_and_receive(stm32_session, "PING")
        latencies.append(lat)
        time.sleep(0.01)

    avg_lat = sum(latencies) / len(latencies)
    max_lat = max(latencies)
    HIL_METRICS["avg_latency_ms"] = round(avg_lat, 2)
    HIL_METRICS["max_latency_ms"] = round(max_lat, 2)

    passed = (max_lat < 15.0)
    record_result(passed, "LATENCY_BENCHMARK")
    assert passed

def test_05_unknown_command_resilience(stm32_session):
    resp, _ = send_and_receive(stm32_session, "INVALID_CMD_XYZ")
    passed = (resp == "ERR UNKNOWN_COMMAND")
    record_result(passed, "UNKNOWN_CMD")
    assert passed

# --- 新規：ハードウェア・制御機能 HIL テスト ---

def test_06_internal_adc_hardware_validation(stm32_session):
    """内蔵ADC1を駆動し、基準電圧(VREFINT)とMCU温度が物理仕様内にあるか検証"""
    resp, _ = send_and_receive(stm32_session, "READ_ADC")
    # 期待フォーマット: OK VREF_RAW=1502 VREF_MV=1210 TEMP_C=32.4
    kv = {}
    for token in resp.split():
        if "=" in token:
            k, v = token.split("=", 1)
            kv[k] = v

    vref_mv = int(kv.get("VREF_MV", 0))
    temp_c = float(kv.get("TEMP_C", 0.0))
    HIL_METRICS["vref_mv"] = vref_mv
    HIL_METRICS["mcu_temp_c"] = temp_c

    print(f"\n[ADC HIL] VREFINT: {vref_mv} mV (Expected ~1210mV) | MCU Temp: {temp_c} C")

    # STM32F446RE データシート上の VREFINT 許容範囲 (約1.15V〜1.27V) と温度範囲を検証
    passed = (1150 <= vref_mv <= 1270) and (10.0 <= temp_c <= 75.0)
    record_result(passed, "ADC_HARDWARE_VALIDATION")
    assert passed, f"ADC value out of physical spec: {resp}"

def test_07_safety_interlock_state_machine(stm32_session):
    """緊急停止(E-STOP)時のアクチュエータ強制遮断とインターロック拒否ロジックを検証"""
    # 1. 通常状態で LED を点灯させる
    r1, _ = send_and_receive(stm32_session, "LED 1")
    assert r1 == "OK LED=1"

    # 2. 緊急停止 (ESTOP) を発動 -> 即座に MODE=EMERGENCY かつ LED=0 に遮断されること
    r_estop, _ = send_and_receive(stm32_session, "ESTOP")
    s_estop, _ = send_and_receive(stm32_session, "STATUS")

    # 3. 緊急停止中に LED 点灯コマンドを送ってもインターロックにより拒否されること
    r_reject, _ = send_and_receive(stm32_session, "LED 1")

    # 4. フォルト解除 (RESET_FAULT) 後は再び NORMAL モードに戻り、LED 点灯が許可されること
    r_reset, _ = send_and_receive(stm32_session, "RESET_FAULT")
    r_recover, _ = send_and_receive(stm32_session, "LED 1")
    send_and_receive(stm32_session, "LED 0")

    passed = (
        r_estop == "OK MODE=EMERGENCY LED=0" and
        "MODE=EMERGENCY" in s_estop and
        "LED=0" in s_estop and
        r_reject == "ERR INTERLOCK_ACTIVE" and
        r_reset == "OK MODE=NORMAL" and
        r_recover == "OK LED=1"
    )
    print(f"\n[SAFETY HIL] E-Stop Cutoff -> Reject({r_reject}) -> Fault Reset -> Recovery Verified")
    record_result(passed, "SAFETY_INTERLOCK_FSM")
    assert passed
