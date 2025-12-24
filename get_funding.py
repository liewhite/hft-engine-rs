import time
import requests

def init_interval_from_history(symbol: str) -> int:
    """从历史记录初始化间隔"""
    r = requests.get(
        "https://fapi.binance.com/fapi/v1/fundingRate",
        params={"symbol": symbol, "limit": 1}
    ).json()
    print(r)

    
    # if len(r) >= 2:
    #     return r[1]["fundingTime"] - r[0]["fundingTime"]
    # return 8 * 3600 * 1000  # 默认 8h

if __name__ == "__main__":
    interval = init_interval_from_history("PLAYUSDT")