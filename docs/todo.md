平仓和开仓阈值随资费差变化，如果资费日化很高，平仓条件就设置得相对严格， 如果资费不高了，就尽快平仓
开仓也是， 资费越大，开仓越快，资费越低，开仓越慢， 资费差低于阈值就不开仓了
exchange实现new cli id方法
ExchangeModule 是个多余的trait，直接使用ExchangeClient
