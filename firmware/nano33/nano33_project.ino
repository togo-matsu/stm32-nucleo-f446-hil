#include <Arduino.h>
#include <ArduinoBLE.h>
#include <Arduino_BMI270_BMM150.h>
#include <PDM.h>
#include <math.h>

const char* DEVICE_NAME     = "Nano33Sense-Rev2";
const char* SERVICE_UUID    = "19B10000-E8F2-537E-4F6C-D104768A1214";
const char* CHAR_TELEMETRY  = "19B10001-E8F2-537E-4F6C-D104768A1214";
const char* CHAR_CONFIG     = "19B10002-E8F2-537E-4F6C-D104768A1214";

BLEService sensorService(SERVICE_UUID);
BLECharacteristic telemetryChar(CHAR_TELEMETRY, BLERead | BLENotify, 20);
// Read と Write 両方を有効化
BLECharacteristic configChar(CHAR_CONFIG, BLERead | BLEWrite, 8);

// 動的ベースライン変数
float g_mu_accel = 1.0000f;
float g_mu_sound = 100.0f;

struct __attribute__((packed)) TelemetryPacket {
    uint32_t timestamp_ms;
    float    rms_accel;
    float    rms_sound;
    float    anomaly_score;
    uint8_t  is_anomaly;
    uint8_t  anomaly_type;
    uint8_t  reserved[2];
};

struct __attribute__((packed)) ConfigPacket {
    float mu_accel;
    float mu_sound;
};

#define SOUND_BUFFER_SIZE 256
short soundBuffer[SOUND_BUFFER_SIZE];
volatile int samplesRead = 0;

void onPDMdata() {
    int bytesAvailable = PDM.available();
    if (bytesAvailable > (int)sizeof(soundBuffer)) {
        bytesAvailable = sizeof(soundBuffer);
    }
    if (bytesAvailable > 0) {
        PDM.read(soundBuffer, bytesAvailable);
        samplesRead = bytesAvailable / sizeof(short);
    }
}

float calculateSoundRMS() {
    int count = samplesRead;
    if (count <= 0) return 0.0f;
    samplesRead = 0;

    long long sumSquares = 0;
    for (int i = 0; i < count; i++) {
        sumSquares += (long long)soundBuffer[i] * soundBuffer[i];
    }
    return sqrt((float)(sumSquares / count));
}

void setup() {
    Serial.begin(115200);
    pinMode(LED_BUILTIN, OUTPUT);

    if (!BLE.begin()) {
        while (1);
    }

    BLE.setDeviceName(DEVICE_NAME);
    BLE.setLocalName(DEVICE_NAME);
    BLE.setAdvertisedService(sensorService);
    sensorService.addCharacteristic(telemetryChar);
    sensorService.addCharacteristic(configChar);
    BLE.addService(sensorService);

    // 初期設定値をキャラクタリスティックにセット
    ConfigPacket init_cfg = { g_mu_accel, g_mu_sound };
    configChar.writeValue((uint8_t*)&init_cfg, sizeof(init_cfg));

    BLE.advertise();

    if (!IMU.begin()) {
        while (1);
    }

    PDM.setBufferSize(sizeof(soundBuffer));
    PDM.onReceive(onPDMdata);
    if (!PDM.begin(1, 16000)) {
        while (1);
    }
    PDM.setGain(20);
}

void loop() {
    BLEDevice central = BLE.central();

    if (central) {
        digitalWrite(LED_BUILTIN, HIGH);
        unsigned long lastSend = 0;

        while (central.connected()) {
            BLE.poll();

            // 1. パラメータ更新の受信処理
            if (configChar.written()) {
                if (configChar.valueLength() == sizeof(ConfigPacket)) {
                    ConfigPacket cfg;
                    memcpy(&cfg, configChar.value(), sizeof(ConfigPacket));
                    g_mu_accel = cfg.mu_accel;
                    g_mu_sound = cfg.mu_sound;
                    // Read 値も更新
                    configChar.writeValue((uint8_t*)&cfg, sizeof(cfg));
                }
            }

            // 2. センサー計測と動的判定
            unsigned long now = millis();
            if (now - lastSend >= 500) {
                lastSend = now;
                float x, y, z;
                if (IMU.accelerationAvailable()) {
                    IMU.readAcceleration(x, y, z);
                    float rms_accel = sqrt(x * x + y * y + z * z);
                    float rms_sound = calculateSoundRMS();

                    // 動的ベースラインからの逸脱判定 (Fast Path)
                    // 加速度: ベースラインから ±0.35G 以上の衝撃
                    bool accel_anom = (fabs(rms_accel - g_mu_accel) > 0.35f);
                    // 音圧: 現在の暗騒音ベースラインの 1.7 倍 かつ +40 以上の突発爆音
                    bool sound_anom = (rms_sound > (g_mu_sound * 1.7f) && (rms_sound - g_mu_sound) > 40.0f);

                    bool is_anom = accel_anom || sound_anom;
                    uint8_t anom_type = 0;
                    if (accel_anom && sound_anom) anom_type = 3;
                    else if (accel_anom) anom_type = 1;
                    else if (sound_anom) anom_type = 2;

                    float score = is_anom ? 1.0f : 0.05f;

                    TelemetryPacket packet;
                    packet.timestamp_ms  = now;
                    packet.rms_accel     = rms_accel;
                    packet.rms_sound     = rms_sound;
                    packet.anomaly_score = score;
                    packet.is_anomaly    = is_anom ? 1 : 0;
                    packet.anomaly_type  = anom_type;
                    packet.reserved[0]   = 0;
                    packet.reserved[1]   = 0;

                    telemetryChar.writeValue((uint8_t*)&packet, sizeof(packet));
                }
            }
        }

        digitalWrite(LED_BUILTIN, LOW);
        BLE.advertise();
    }
}
