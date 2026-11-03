#!/usr/bin/env bash
set -euo pipefail

OTA_URL="http://10.0.0.200/ota/tangnano/latest.fs"
OTA_SHA_URL="http://10.0.0.200/ota/tangnano/latest.fs.sha256"
WORK_DIR="/tmp/fpga_ota"
CURRENT_HASH_FILE="/var/lib/fpga_current_sha256"

mkdir -p "$WORK_DIR"
cd "$WORK_DIR"

echo "[OTA] Checking for new Tang Nano 9K bitstream from ${OTA_URL}..."

# 1. メタデータ (SHA256) 取得
if ! curl -sf "${OTA_SHA_URL}" -o remote.sha256; then
    echo "[OTA-SKIP] Storage Gateway endpoint not reachable or no SHA file found."
    exit 0
fi

REMOTE_HASH=$(awk '{print $1}' remote.sha256)
LOCAL_HASH=""
if [ -f "$CURRENT_HASH_FILE" ]; then
    LOCAL_HASH=$(cat "$CURRENT_HASH_FILE")
fi

# 2. ハッシュ値チェック
if [ "$REMOTE_HASH" == "$LOCAL_HASH" ]; then
    echo "[OTA] Tang Nano 9K is already running the latest bitstream (${LOCAL_HASH:0:8})."
    exit 0
fi

echo "[OTA] New bitstream detected (${REMOTE_HASH:0:8}). Downloading payload..."
curl -sSf "${OTA_URL}" -o latest.fs

# 3. チェックサム検証
echo "${REMOTE_HASH}  latest.fs" | sha256sum -c -

echo "[OTA-FLASH] Programming Tang Nano 9K on-chip Flash via openFPGALoader..."
openFPGALoader -b tangnano9k -f latest.fs

# 4. 適用済みハッシュの保存
echo "$REMOTE_HASH" | sudo tee "$CURRENT_HASH_FILE" > /dev/null
echo "[OTA-SUCCESS] FPGA remote firmware update completed successfully!"
