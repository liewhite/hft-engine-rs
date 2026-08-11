# 文档索引

**从 [architecture.md](./architecture.md) 开始**——品味、原则、全景、已知违反，以及改动前的检查清单。

## 长期文档（描述"应该怎样"）

| 文档 | 内容 |
|---|---|
| [architecture.md](./architecture.md) | 架构品味、原则、模块全景、已知违反清单、改动前检查清单 |
| [external-data-access.md](./external-data-access.md) | 引擎对外开放数据的两个面：流走 `Subscribe*`，快照走 `Get*`；持仓真值的宿主与外部消费者改造示例 |
| [backtest-design.md](./backtest-design.md) | 回测引擎设计：确定性、与实盘共享实现 |

## 阶段性文档（描述"当时做了什么"）

| 文档 | 内容 |
|---|---|
| [refactor-state-and-interfaces.md](./refactor-state-and-interfaces.md) | **进行中**：状态投影与接口收窄（R1–R5），针对 architecture.md §4 的违反清单 |
| [refactor-plan.md](./refactor-plan.md) | 已完成：Manager 瘦身、事件模型拆分、持仓基线退出总线（6 个阶段） |

## 审计记录（描述"查过什么，别重复查"）

| 文档 | 内容 |
|---|---|
| [false-zero-audit.md](./false-zero-audit.md) | "未知伪装成 0/空值"的全仓排查结果，附已核实的合法 0 清单 |
| [field-audit.md](./field-audit.md) | 字段级审计记录 |
| [todo.md](./todo.md) | 待办与设计备忘 |
| [implement.md](./implement.md) | 实现备忘 |
