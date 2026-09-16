# OpenTelemetry 可观测性方案（草稿）

> 状态：草稿，未排期，不进 board。
> 起因：为 oh-my-claw 接入 OpenTelemetry，首要目标是 **LLM / Agent 链路的可观测性**。
> 本文档只沉淀现状梳理与方案方向，具体 span 树、配置项、实现计划留待正式设计阶段再定。

---

## 1. 目标

**首要目标：LLM / Agent 链路观测。**

回答这一类问题：

- 一次对话（run）里，模型往返了几轮？每轮的 TTFB（首字节延迟）是多少？
- 每轮消耗了多少 input / output token？token 主要花在文本、思考（reasoning）还是工具参数上？
- 工具调用链长什么样？哪个工具最慢、被调了几次？
- 为什么这次回答慢 / 为什么这次被 `max_tokens` 截断走了续写？

次要目标（暂缓，见 §7）：

- 系统健康观测（队列深度、写线程健康、心跳、panic 隔离）。
- 跨进程分布式链路（serve / http / tui 三进程自动串联）。
- 为 SaaS 多租户计量铺路。

---

## 2. 现状梳理

### 2.1 埋点基础设施已就位，但 span 结构是空的

项目全仓已用 `tracing` + `tracing-subscriber`：

- `oc-cli/src/main.rs`（`run_serve` / `run_http`）与 `oc-cli/src/cli_client.rs` 各 init 一次 subscriber；
- `oc-server`、`oc-llm`、`oc-tools` 均直接依赖 `tracing` 并打日志。

但当前：

| 指标 | 数量 |
|---|---|
| `#[instrument]` / `#[tracing::instrument]` | 0 |
| `span!` | 1 |
| `tracing::info/debug/error/warn` | 35 |

也就是说日志是现成的，**span 树是空的**——OTel 要的就是 span 树。

### 2.2 观测数据其实已经在代码里算出来了，只是没上报

`oc-server/src/run.rs` 的 `run_model_turn` 已经在局部变量里算好了大部分关键指标，只是散落未结构化上报：

| 数据 | 位置 |
|---|---|
| `ttfb_ms`（首字节延迟） | `run.rs` 局部变量 |
| `deltas`（delta 计数） | `run.rs` 局部变量 |
| `out_tokens`（输出 token） | `run.rs` 局部变量 |
| `tool_rounds`（工具轮数） | `run.rs` / `diag.rs` |
| `shape`（消息序列形状） | `run_model_turn` 内构造 |
| 空闲看门狗 / run 超时 | `run.rs` 分支里 |

### 2.3 相关 ID 已天然存在

`RunCtx`（`oc-server/src/run.rs:58`）已持有：

- `session_id` / `run_id` / `model` / `provider: Arc<dyn Provider>`（`provider.id()`）

这些天然可作 span 的关联 ID（correlation id）。

### 2.4 token 用量已归一

`oc-llm/src/types.rs` 的 `Usage { input_tokens, output_tokens }` 已由各 provider 归一，`Delta::Usage(u)` 已随流上报。

**一句话结论：缺的不是「收集什么」，而是「把已有的东西结构化成一个 span 树再导出」。**

---

## 3. 硬约束

### 3.1 `oc-core` 保持纯策略、无 IO

`oc-core` 是纯领域层（README：「不 spawn / 不连接 / 不读时钟 / 不 rand」，架构不变量 #1）。

OTel SDK 会开线程、发 HTTP、读时钟，**绝不能进 `oc-core`**。

因此埋点只落在 `oc-server` / `oc-llm` / `oc-tools`（本来就有 `tracing`），OTel SDK 只落在 `oc-cli` 边沿。

### 3.2 观测不能阻塞主会话回复

`oc-core` 架构不变量 #3 的精神同样适用于观测：**trace/metric 导出失败不能影响对话 run**。

具体含义：exporter 走批处理 + 后台 flush，collector 挂掉时丢数据而非丢回复（见 §6.2）。

### 3.3 不破坏 prompt 确定性

埋点不能改动 prompt 组装路径（`oc-core::prompt`）。span 字段只能读现成数据，不能为了观测往 prompt 里塞东西。

---

## 4. 候选路线

### 路线 A（推荐）：`tracing` 作唯一埋点 API + `tracing-opentelemetry` 桥

- **埋点**：在 `run::drive`（run span）、`run_model_turn`（模型往返 span）、工具执行（tool span）加 `#[tracing::instrument]`；把 §2.2 的局部变量用 `Span::current().record()` 塞进 span 字段。
- **导出**：加 `opentelemetry-otlp`（HTTP/protobuf，复用已有 reqwest，不加 tonic）+ `tracing-opentelemetry` layer，接进 `oc-cli/main.rs` 现有 subscriber init。
- 日志与 span 统一成一套 API；现有 `tracing::debug!` 自动变成 span event。

**优点**：改动最小、无第二套埋点体系；span 字段天然对齐 GenAI 语义约定；热路径 crate 只依赖已有的 `tracing`，OTel SDK 只在 `oc-cli` 边沿。
**缺点**：span 语义受 tracing 桥映射限制（span status / event 没有原生 OTel 那么细），极端定制略绕。

### 路线 B：直接用 `opentelemetry` SDK API

不经过 tracing 桥，用 `Tracer::start()` / `span.set_attribute()` 直接建 span。

**优点**：对 GenAI 语义约定、span link、status code 控制最精确。
**缺点**：引入**两套并行埋点体系**（日志走 tracing、span 走 OTel），每加一个观测点都要决定放哪边；代码量最大。

### 路线 C：自研极简 span 抽象 + 可插拔 exporter

自己定义 `Trace`/`Span` 类型 + trait，再手写 OTLP 序列化。

**优点**：依赖最轻、完全可控。
**缺点**：重造轮子；失去与 Collector / Jaeger / Grafana 生态的即插即用；将来想用 GenAI 语义约定、采样、metrics 都得自己补。**不建议**。

---

## 5. 推荐结论

**路线 A**：项目已在 `tracing` 上投了资，A 是生态里 tracing → OTel 的标准桥，能零成本把 §2.2 已算好的指标变成 span；同时天然满足 §3.1「oc-core 保持纯净」——埋点只在 oc-server/oc-llm，OTel SDK 只在 oc-cli 边沿。

---

## 6. 待展开的设计点（正式设计阶段再细化）

### 6.1 span 树怎么切

暂定三层，从大到小：

```
run (session_id, run_id, model, provider)
├── model_turn × N (ttfb_ms, deltas, input/output tokens, tool_rounds, shape)
│   └── tool_exec × M (tool name, duration, status)
└── (后续) memory_search / dreaming / cron 触发 各一层
```

字段尽量对齐 OpenTelemetry 的 GenAI 语义约定（`gen_ai.*`、`gen_ai.usage.input_tokens` 等）。

### 6.2 collector 挂了不阻塞主回复

- exporter 走 `BatchSpanProcessor`（后台线程 + 定时 flush），与 run 的执行路径解耦；
- 导出失败只 `tracing::warn!`，不传播到 run；
- 配置项 `enabled = false` 时完全不加载 OTel SDK（零依赖、零开销）。

### 6.3 配置开关

沿用 `config.example.toml` / `docs/reference/config.md` 的既有风格，暂定新增：

```toml
[telemetry]
enabled = false
# OTLP HTTP 端点，如 http://127.0.0.1:4318/v1/traces
endpoint = "http://127.0.0.1:4318"
service_name = "oc"
```

### 6.4 metrics 要不要一起上

暂缓。首期聚焦 trace；系统健康类 metric（队列深度、写线程健康）依赖 span 之外的时间序列，单独再议。

---

## 7. 暂缓项（按优先级后置）

1. **系统健康观测**（queue depth / writer health / heartbeat / panic 隔离）→ 偏 metric，与 trace 不同信号。
2. **跨进程分布式链路**（serve / http / tui 三进程自动串联）→ 需在协议层透传 trace context（`oc-proto` 加字段），改动面大。
3. **SaaS 多租户计量**（按 tenant 维度统计用量/成本）→ 依赖 [saas-multi-tenant.md](saas-multi-tenant.md) 落地后才有意义。

---

## 8. 开放问题

- 是否需要支持 gRPC exporter（需加 `tonic`，二进制体积会涨），还是只做 HTTP/protobuf？
- 采样策略：全量采、还是按错误/慢 run 采样？
- `tracing-opentelemetry` 桥对 span status / error event 的映射是否够用，要不要在关键 span 上退回路线 B 的 OTel 原生 API？
