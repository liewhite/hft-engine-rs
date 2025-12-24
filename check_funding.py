import requests
import time

def test_funding_rate():
    url = "https://fapi.binance.com/fapi/v1/premiumIndex"
    symbol = "PLAYUSDT"
    
    # 第一次查询
    r1 = requests.get(url, params={"symbol": symbol}).json()
    print(f"=== 第1次查询 ===")
    print(f"时间: {r1['time']}")
    print(f"lastFundingRate: {r1['lastFundingRate']}")
    print(f"nextFundingTime: {r1['nextFundingTime']}")
    
    # 等待10秒
    print("\n等待10秒...\n")
    time.sleep(10)
    
    # 第二次查询
    r2 = requests.get(url, params={"symbol": symbol}).json()
    print(f"=== 第2次查询 ===")
    print(f"时间: {r2['time']}")
    print(f"lastFundingRate: {r2['lastFundingRate']}")
    print(f"nextFundingTime: {r2['nextFundingTime']}")
    
    # 比较
    print("\n=== 结论 ===")
    if r1['lastFundingRate'] == r2['lastFundingRate']:
        print("lastFundingRate 未变化 → 是【已收取】的费率")
    else:
        print("lastFundingRate 有变化 → 是【预估】的费率")
        print(f"变化: {r1['lastFundingRate']} → {r2['lastFundingRate']}")

if __name__ == "__main__":
    test_funding_rate()