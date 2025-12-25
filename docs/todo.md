[] engine 还是应该通用， 初始化时传入exchange，symbols，marketDataTypes来确定订阅哪些数据
[] metrics_record 也要通过engine外部传入, engine负责基础指标的set，传给策略metricsMgr,让策略自己set特定的metric(需要基于字符串: metrics.gauge!)
[] symbol元数据，min size，size step，price step，contract_size
[] engine中处理元数据， 比如下单 0.1eth， binance 的contract_size =1，则下单qty=0.1， okx的contract_size=0.1，则下单qty=1
[] 订阅和 engine策略 完全解耦，engine和策略类型上解耦， 其实逻辑上还是绑定的，因为我们不能拿一个需要bbo的策略去对接一个没有bbo的engine