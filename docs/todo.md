[] metrics_record 也要通过engine外部传入, engine负责基础指标的set，传给策略metricsMgr,让策略自己set特定的metric(需要基于字符串: metrics.gauge!)
移除框架层面的metric，用户自行维护， 反正 prometheus install是全局的