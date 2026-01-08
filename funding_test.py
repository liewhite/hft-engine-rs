import requests
import time

# 1. 获取所有币种
# resp = requests.post("https://api.hyperliquid.xyz/info",
#                      json={"type": "meta"})
# coins = [asset["name"] for asset in resp.json()["universe"]]

# 2. 获取上一小时的时间戳
last_hour = int(time.time() * 1000) - 3600000

# 3. 遍历获取每个币种的历史资金费率
resp = requests.post(
    "https://api.hyperliquid.xyz/info",
    json={"type": "fundingHistory", "startTime": last_hour, "coin": "ETH"},
)
print(resp.content)
