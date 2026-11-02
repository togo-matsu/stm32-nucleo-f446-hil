import asyncio
import struct
import numpy as np
import onnx
from onnx import helper, TensorProto
from bleak import BleakClient, BleakScanner

DEVICE_NAME = "Nano33Sense-Rev2"
CHAR_TELEMETRY = "19B10001-E8F2-537E-4F6C-D104768A1214"
CHAR_CONFIG    = "19B10002-E8F2-537E-4F6C-D104768A1214"
EXPORT_PATH    = "/home/you/onnx-workspace/anomaly_model.onnx"
SAMPLE_COUNT_TARGET = 10

samples = []

def notification_handler(sender, data: bytearray):
    if len(data) >= 20:
        _, rms_accel, rms_sound = struct.unpack('<Iff', data[0:12])
        samples.append([rms_accel, rms_sound])
        print(f"  [Sample {len(samples)}/{SAMPLE_COUNT_TARGET}] Accel: {rms_accel:.3f}G, Sound: {rms_sound:.1f}")

async def run_adaptation():
    print(f"=== [1/3] Scanning for '{DEVICE_NAME}' ===")
    device = None
    for attempt in range(1, 4):
        device = await BleakScanner.find_device_by_name(DEVICE_NAME, timeout=5.0)
        if device:
            break
        print(f"  [Attempt {attempt}/3] Searching...")
        await asyncio.sleep(1.5)

    if not device:
        print(f"[ERROR] Could not find '{DEVICE_NAME}'.")
        return

    print(f"Found {device.name}. Connecting for baseline sampling...")
    async with BleakClient(device, timeout=15.0) as client:
        # 1. サンプリング
        await client.start_notify(CHAR_TELEMETRY, notification_handler)
        while len(samples) < SAMPLE_COUNT_TARGET:
            await asyncio.sleep(0.2)
        await client.stop_notify(CHAR_TELEMETRY)

        # 2. 統計値の算出
        data_arr = np.array(samples)
        mu = np.mean(data_arr, axis=0)
        std = np.std(data_arr, axis=0)
        std[0] = max(float(std[0]), 0.08)
        std[1] = max(float(std[1]), 15.0)

        mu_accel, mu_sound = float(mu[0]), float(mu[1])
        std_accel, std_sound = float(std[0]), float(std[1])

        print(f"\n=== [2/3] Updating Nano 33 Hardware Baseline via BLE ===")
        print(f"Writing -> Mu Accel: {mu_accel:.4f}G, Mu Sound: {mu_sound:.2f}")
        config_payload = struct.pack('<ff', mu_accel, mu_sound)
        
        # response=True で確実に送信
        await client.write_gatt_char(CHAR_CONFIG, config_payload, response=True)
        await asyncio.sleep(0.5)

        # 書き込み結果の検証 (Read back)
        read_val = await client.read_gatt_char(CHAR_CONFIG)
        r_accel, r_sound = struct.unpack('<ff', read_val)
        print(f"[VERIFY] Nano 33 Confirmed Baseline -> Accel: {r_accel:.4f}G, Sound: {r_sound:.2f}")

        # 3. ONNX モデル生成
        print(f"\n=== [3/3] Exporting Dynamic ONNX Model ===")
        export_onnx(mu_accel, mu_sound, std_accel, std_sound)

def export_onnx(mu_accel, mu_sound, std_accel, std_sound):
    X = helper.make_tensor_value_info('sensor_input', TensorProto.FLOAT, [1, 3])
    Y = helper.make_tensor_value_info('anomaly_prob', TensorProto.FLOAT, [1, 1])

    mean_vec = np.array([mu_accel, mu_sound, 0.0], dtype=np.float32)
    inv_scale_vec = np.array([1.0 / (std_accel * 3.0), 1.0 / (std_sound * 3.0), 1.0], dtype=np.float32)
    W_vec = np.array([[2.0], [2.0], [0.5]], dtype=np.float32)
    B_vec = np.array([-3.0], dtype=np.float32)

    T_mean = helper.make_tensor('Mean', TensorProto.FLOAT, [1, 3], mean_vec)
    T_inv_scale = helper.make_tensor('InvScale', TensorProto.FLOAT, [1, 3], inv_scale_vec)
    T_W = helper.make_tensor('W', TensorProto.FLOAT, [3, 1], W_vec.flatten())
    T_B = helper.make_tensor('B', TensorProto.FLOAT, [1], B_vec.flatten())

    sub_node = helper.make_node('Sub', ['sensor_input', 'Mean'], ['diff'])
    abs_node = helper.make_node('Abs', ['diff'], ['abs_diff'])
    norm_node = helper.make_node('Mul', ['abs_diff', 'InvScale'], ['norm_features'])
    gemm_node = helper.make_node('Gemm', ['norm_features', 'W', 'B'], ['linear_out'])
    sig_node = helper.make_node('Sigmoid', ['linear_out'], ['anomaly_prob'])

    graph = helper.make_graph(
        [sub_node, abs_node, norm_node, gemm_node, sig_node],
        'dynamic_adaptive_anomaly_model',
        [X], [Y],
        [T_mean, T_inv_scale, T_W, T_B]
    )

    model = helper.make_model(graph, opset_imports=[helper.make_opsetid('', 15)])
    model.ir_version = 8
    onnx.save(model, EXPORT_PATH)
    print(f"[SUCCESS] Exported new model to {EXPORT_PATH}\n")

if __name__ == "__main__":
    asyncio.run(run_adaptation())
