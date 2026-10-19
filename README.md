# Stravia Vendor WorkBuddy

把 **WorkBuddy / CodeBuddy（腾讯代码助手）** 账号接入 [Stravia](https://github.com/Stravia-AI/StraviaPlatform) 的 wasm 供应商插件 —— 你在 WorkBuddy 里能用的模型（DeepSeek、Kimi、GLM、混元……），经由 Stravia 统一网关以 **OpenAI 兼容接口**提供给任意客户端（Codex CLI、Claude Code、Cherry Studio、Open WebUI……）。

> ⚠️ 本插件不是腾讯 WorkBuddy 官方发布的插件，上游服务、产品名称及商标归各自权利人所有。上游接口属非公开协议，无稳定性承诺，仅供个人学习研究，请自担风险。

## 特性一览

- 🔐 **浏览器授权登录** — 打开授权链接完成 QQ / 微信 / 手机号登录，无需手动复制 token；令牌自动刷新与撤销
- 🌐 **双区域隔离** — 国内版（`www.workbuddy.cn`）与国际版（`www.workbuddy.ai`）两个 channel，凭据与流量严格限定在所选区域
- 🤖 **模型自动发现** — 从账号 `/v3/config` 的默认 agent 名单实时发现模型，按上游配置合并免费／付费线路；可选 `model_ids` 白名单只同步指定模型
- **可选自动付费线路** — 默认关闭；开启后可在免费线路返回 `6004` 频控错误、且尚未输出响应时自动切换付费线路并消耗积分
- 📊 **额度查询** — 从所选区域的新版计费汇总接口读取账号积分资源包余量
- 🧩 **wasm 沙箱组件** — 仅能访问插件声明的网络 origin，凭据由宿主持久化并跨连接隔离

## 快速开始

### 1. 安装 Stravia

参考 [StraviaPlatform](https://github.com/Stravia-AI/StraviaPlatform) —— 桌面应用或单一二进制，默认统一监听端口 `:23471`。

### 2. 安装插件

1. 从 [Releases](../../releases) 下载 `stravia-vendor-workbuddy-v<版本>.wasm`（可用同目录 `SHA256SUMS` 校验完整性）
2. 在 Stravia 中打开 **供应商插件（Vendor Plugins）**，导入本地 `.wasm` 包 —— 不依赖插件市场

### 3. 创建连接并登录

1. 新增供应商连接，选择 **WorkBuddy**
2. 选择 channel：
   - **国内版（中国大陆）**：国内 WorkBuddy 账号
   - **国际版**：国际版 WorkBuddy AI 账号
3. 发起浏览器授权：打开插件返回的授权链接，完成 WorkBuddy 登录；授权窗口有效期 10 分钟
4. 授权完成后连接就绪，模型列表自动从账号同步

### 4. 客户端接入

Stravia 统一入口照常使用，例如：

```
Base URL: http://127.0.0.1:23471/v1
API Key:  由 Stravia 访问控制配置决定
```

客户端按 OpenAI 兼容协议调用，Stravia 负责路由到本插件并转换为 WorkBuddy 上游协议。

## 可选配置

| 字段 | 说明 |
|------|------|
| `model_ids` | 自定义模型 ID，每行一个。填写后仅同步这些明确指定的 ID；留空时按账号 `/v3/config` 的默认 agent 名单发现并合并免费／付费线路。模型发现不检测额度或调用模型 |
| `auto_paid_on_rate_limit` | 布尔值，默认 `false`。开启即允许免费线路返回 `6004` 频控错误后，自动使用上游配置且允许切换的付费线路，可能消耗积分 |

### 推理请求参数白名单

聊天请求在共享 codec 编码、现有 WorkBuddy 线规归一化之后，再进行出站白名单过滤。国内版、国际版及自动付费线路的每次尝试使用同一规则。白名单外的字段和请求头静默丢弃，不产生警告或参数错误，也不会因官方 CLI 的自定义 `providerData` / header 透传配置而扩大范围。连接配置的校验规则不变。

允许的请求体顶层字段：

| 类别 | 字段 |
|------|------|
| 对话与工具 | `model`、`messages`、`tools`、`tool_choice`、`parallel_tool_calls` |
| 生成参数 | `temperature`、`top_p`、`frequency_penalty`、`presence_penalty`、`max_tokens` |
| 流与输出 | `stream`、`stream_options`、`response_format` |
| 存储与缓存 | `store`、`prompt_cache_retention` |
| 思考参数 | `reasoning_effort`、`reasoning_summary`、`reasoning`、`thinking`、`verbosity`、`enable_thinking`、`chat_template_kwargs` |

白名单同时限制协议结构中的嵌套字段：

- `stream_options` 只保留 `include_usage`。
- `thinking` 只保留 `type`、`clear_thinking`；`reasoning` 只保留 `effort`、`enabled`；`chat_template_kwargs` 只保留 `enable_thinking`、`preserve_thinking`。
- 消息只保留角色、内容、名称、工具调用及结果关联、推理文本、音频引用和已知扩展结构。内容块仅保留文本、拒绝文本、图片、音频、文件及缓存控制的协议字段；Google `thought_signature` 与 `extra_fields._meta` 数据保留。
- function 工具只保留 `name`、`description`、`parameters`、`strict`；工具调用只保留 ID、类型、函数名、arguments 和已知 Google 签名结构。
- `response_format` 只保留 `type`、`json_schema`；`json_schema` 只保留 `name`、`strict`、`schema`。
- **不递归过滤用户数据**：提示词、工具结果文本、工具 `arguments`、工具 JSON Schema / 输出 JSON Schema 内的属性名、默认值和自定义 schema 关键字保持原样。

例如，`metadata`、`service_tier`、`user`、`seed`、`n`、`logit_bias`、`logprobs`、`top_logprobs` 和未知字段不会上送。现有 `max_completion_tokens` → `max_tokens`、`reasoningEffort` → `reasoning_effort` 等归一化仍先执行；允许字段不表示每次必发，实际值仍受共享 codec、宿主目标思考控制及现有模型归一化约束。

请求头仅由插件的区域凭据、官方产品配置和宿主可信请求上下文生成，不转发客户端或 codec 自带的请求头。最终允许：

- 传输：`Content-Type`、可选 `Content-Encoding: gzip`、`Accept`、`Authorization`、`User-Agent`、`X-Requested-With`；聊天 `Accept` 使用官方 SDK 的 `application/json`，请求体仍强制流式。
- 身份：`X-Agent-Purpose`、`X-IDE-Name`、`X-IDE-Type`、`X-IDE-Version`、`X-Product`、`X-Domain`、`X-User-Id`、可选 `X-Enterprise-Id` / `X-Tenant-Id`。
- 会话：`X-Request-ID`、`X-Root-Request-ID`、`X-Conversation-ID`、`X-Conversation-Message-ID`、`X-Conversation-Request-ID`；真实 ACP 上下文存在时允许 `acp-connection-id`。
- 追踪：`baggage`、`traceparent`、可选 `tracestate`、`b3`、`X-Trace-ID`、`X-B3-TraceId`、`X-B3-SpanId`、可选 `X-B3-ParentSpanId`、`X-B3-Sampled`。

头名称匹配不区分大小写。聊天不再发送 `Origin`、`Referer`、`X-CodeBuddy-Request`、`Accept-Language`、`X-Model-ID`、`X-Product-Version`。认证、模型发现和额度查询使用各自固定的请求体 / 请求头，不接收推理参数透传。

对照基准为官方 npm 发行包 [`@tencent-ai/codebuddy-code@2.161.2`](https://registry.npmjs.org/@tencent-ai/codebuddy-code/-/codebuddy-code-2.161.2.tgz)，依据是实际构造代码，而不是 OpenAI SDK 的全部可支持字段：

- [SDK bundle `9931.codebuddy.js`](https://unpkg.com/@tencent-ai/codebuddy-code@2.161.2/dist-server/9931.codebuddy.js)：`OpenAIChatCompletionsModel` 的请求对象、消息与 function 工具转换。
- [Provider bundle `3163.codebuddy.js`](https://unpkg.com/@tencent-ai/codebuddy-code@2.161.2/dist-server/3163.codebuddy.js) 与 [核心 bundle `codebuddy.js`](https://unpkg.com/@tencent-ai/codebuddy-code@2.161.2/dist-server/codebuddy.js)：会话 / OTel 注入、身份 / 产品 / User-Agent 拦截器及默认请求头；[传输 bundle `3043.codebuddy.js`](https://unpkg.com/@tencent-ai/codebuddy-code@2.161.2/dist-server/3043.codebuddy.js) 提供默认 `Accept`。
- [兼容规则 `1525.codebuddy.js`](https://unpkg.com/@tencent-ai/codebuddy-code@2.161.2/dist-server/1525.codebuddy.js)：已知思考格式、缓存控制、工具结果名称和 Google 签名结构。只据其中明确构造的字段放行，不开放任意兼容模板配置键。

官方版本变化不会自动放开新参数，须重新核对发行包并更新白名单。

当前客户端身份按区域的官方稳定更新 API 与实际桌面发布包核实：国内 WorkBuddy `5.8.3`、国际 WorkBuddy AI `5.8.4`，两者内嵌 CLI 均为 `2.161.0`。产品版本同步用于 `X-IDE-Version` 和 User-Agent。来源：[国内更新 API](https://www.workbuddy.cn/v2/update?platform=workbuddy-darwin-arm64)、[国际更新 API](https://www.workbuddy.ai/v2/update?platform=workbuddy-darwin-arm64)。独立 npm CLI 的 latest 版本不是桌面应用的内嵌 CLI，不能直接替代其身份字段；上面的白名单引用仍保留其原始核对版本。

### 请求上下文与 gzip

- `X-Product` 使用模型发现保存的官方 `deploymentType`，缺失时默认 `SaaS`，不接受客户端覆盖。`deploymentType` 和 `productFeatures.RequestBodyGzip` 分别保存到现有模型元数据扩展 `workbuddy_deployment_type`、`workbuddy_request_body_gzip`；免费／付费线路共用当前操作的产品配置快照。更新后需重新同步模型；手动模型 ID 未读取产品配置时使用产品默认值。
- `operation_metadata.root_request_id` 由宿主的用户运行 ID 产生，同一运行的内部重试与 Agent 子模型调用继承，不使用跨用户多轮的 Generation Chain root。缺失或不安全时记录宿主告警，退化为本次推理操作的本地 root；不会把客户端新发起的 HTTP 请求自动归并为同一轮。
- 宿主每个运行保存独立、未采样的 W3C 根传播上下文，经可信 `operation_metadata.traceparent` 传给插件；有效 `tracestate` 一同继承。插件为每次模型操作新建模型 span，内部免费→付费重试复用该 span，每个 HTTP 尝试另建子 span。B3 单头／多头表示模型 span，W3C 表示 HTTP span，采样位继承父上下文；WorkBuddy 的 B3 parent 扩展单独补充。宿主上下文缺失或无效时告警并新建推理级未采样 trace，不启动遥测导出，也不宣称完整复刻官方遥测。
- `X-Request-ID` 与 `X-Conversation-Message-ID` 每次 HTTP 尝试共用一枚新 ID；`X-Conversation-Request-ID` 在同一次模型操作内稳定。`baggage` 使用真正的 conversation-request ID，而不是每次尝试的 message ID。`X-Conversation-ID` 仍只来自宿主会话亲和键。
- ACP 头只接受宿主从真实 ACP 连接取得的 `operation_metadata.acp_connection_id`。当前直连宿主没有 ACP 连接生产者，因此省略并记宿主诊断；不生成随机连接 ID，不挪用 OAuth 连接 ID、传输亲和键或云任务连接 ID，本次不新增 ACP 通道。
- 仅聊天 POST 的最终 JSON 字节使用纯 Rust `flate2` 压缩，以二进制发送并附带 `Content-Encoding: gzip`，不是 Base64。默认启用；官方配置明确为 `false` 时发送原 JSON。压缩失败记录宿主告警并回退原 JSON，不保留 gzip 头。认证、配置 GET 和额度 POST 不压缩，长度由宿主按实际字节计算。

跨重试／子调用的宿主根上下文需要同步构建本次 Stravia 宿主的 `RunObserver` 与 `model_turn::prepare_attempt` 修改；只导入新 Wasm 不能让旧宿主自动产生这些元数据。沿用现有 SDK `0.4.0` 的通用元数据与二进制 body 接口，不增加 WIT 字段或持久化 schema。

`x-device-token` 尚未实现。当前 `wasm32-wasip2` 不能直接加载官方客户端的 N-API `.node`；已核实的包装层仅尝试 Windows/macOS，不能据此承诺 Linux、CPU 架构或独立分发支持。推荐取得合法独立 SDK、WorkBuddy 认可的 channel 配置与平台支持后，在可信原生宿主的受限 HTTP 出口生成并注入，令牌不进入 Wasm 或声明为 non-secret 的 `operation_metadata`。腾讯公开 [SDK 平台资料](https://main.qcloudimg.com/raw/document/product/pdf/1628_111422_cn.pdf) 包含 Web/H5，但没有证明其票据兼容 WorkBuddy；[服务端风险 API](https://main.qcloudimg.com/raw/document/product/pdf/1628_79611_cn.pdf) 消费客户端票据，不能当作替代签发接口。本次没有执行设备 SDK 或发送设备信息。

### 新版客户端接口

| 区域 | 固定上游 origin | 授权 `platform` |
|------|---------------|----------------|
| 国内 | `https://www.workbuddy.cn` | `workbuddy` |
| 国际 | `https://www.workbuddy.ai` | `workbuddy-ai` |

认证、模型发现、聊天和积分查询均使用所选区域的固定 origin：

- 授权状态：`POST /v2/plugin/auth/state?platform=...`；令牌轮询：`GET /v2/plugin/auth/token?state=...`。
- 令牌刷新：`POST /v2/plugin/auth/token/refresh`，两个区域均发送 `X-Auth-Refresh-Source: plugin`。
- 模型配置：`GET /v3/config`；聊天：`POST /v2/chat/completions`。
- 积分汇总：`POST /billing/meter/get-user-resource-summary`，请求体为 `{}`；对 `data.Packages` 中的 `CycleTotalCapacity`、`CycleRemainCapacity`、`CycleUsedCapacity` 做十进制精确求和。接口不分页；缺失或非法的周期额度字段会导致查询失败，不返回部分余额。请求发送桌面产品 User-Agent、`X-Request-ID`、账号与企业身份头，不再使用旧计费信封。

插件不再声明旧国内计费域，也不接受旧国内 base URL；配置响应中的 `data.endpoint` 不会改变固定目的地或扩大网络权限。升级后须重新导入插件，把已有国内连接的 base URL 改为 `https://www.workbuddy.cn`，并重新同步模型。此次升级不新增生图接口。

### 模型发现与线路选择

自动发现现在遵循客户端的默认 agent 名单及顺序，不再用 `credits` 字段猜测可选模型，也不回退到全量后端目录。缺少有效名单时明确报错。免费／付费配对来自 `productFeaturesConfig.ModelRateLimitCap.lines`：默认列表只显示 `freeId`，名称优先使用配对的 `displayName`；不按名称合并无关模型。

自动付费开关的行为如下：

- 关闭时，不自动切换到配对的付费线路。这个开关不改变其他模型的正常计费，也不阻止用户显式指定付费 ID。
- 开启时，仅对明确的 `6004` 频控错误切换，且必须尚未向客户端输出任何增量。普通 HTTP 429、超时、其他错误和已经输出的响应不会触发付费重试。
- 每次插件推理最多切换一次，付费尝试失败后直接返回错误，不递归重试或切换其他模型。两次线路传输属于一次逻辑补全，宿主只收到一次 `UpstreamStarted`；宿主自身的重试和路由策略独立于此开关。
- 有未来重置时间时，在该频控窗口内复用付费线路，到期恢复免费线路；明确已到期的错误不触发新的付费尝试。没有可解析的重置时间时，仅当前请求允许切换，不自行假定窗口时长。
- 窗口按连接、区域、账号、企业及线路配对隔离。关闭开关后忽略窗口；认证待登录状态与窗口共存，吊销本地授权时一起清除。SDK 私有状态接口没有 CAS，因此并发认证／推理写入不保证原子合并。
- 显式 `model_ids` 或缺少配对元数据时，只有收到 `6004` 后才按需读取 `/v3/config` 查找有效配对；已有活动窗口时会先核对线上配对再使用付费线路，不根据模型名称或硬编码 ID 猜测。

更新插件后需重新同步模型。插件不会删除宿主保留的历史模型记录；旧记录可能仍在管理页面中显示。免费线路名称不承诺永久免费，实际价格、可用权限及扣费结果以上游账号为准。

## 自行构建

当前源码使用插件目录旁的完整 `StraviaPlatform/` checkout，五个 Stravia 依赖统一指向 `../StraviaPlatform/backend/crates/...`；不能只下载本插件源码。发布工作流按同一目录关系检出到 `source/StraviaPlatform`；`STRAVIA_PLATFORM_REF` 仓库变量可指定 revision，默认固定 dev 提交 `7fac0bc5af644ae13c389b219700a9c4359181af`，SDK 版本仍为 `0.4.0`。本地 checkout 应使用同一提交。覆盖该变量前须核对 SDK、protocol-codec 和 vendor-runtime 契约，并通过构建与真实组件验证。

`assets/workbuddy.svg` 使用 [WorkBuddy 官方 SVG](https://download.codebuddy.ai/web/workbuddy/f5bce0c03cdc17fa28d25634fb48d2791c297da3/assets/logo.svg) 的原始 mascot 和双眼几何，去掉背景、渐变及模糊，编译进 Wasm 为透明单色图标。显示需要更新后的 Stravia 宿主并重新导入插件，不在运行时下载品牌资源。图标归属和使用限制见 [NOTICE](NOTICE)。

依赖：Rust `1.99.0`（`rust-toolchain.toml` 已锁定，与上游最低版本一致）+ `wasm32-wasip2` target；可选安装 [Task](https://taskfile.dev)。若环境变量 `RUSTUP_TOOLCHAIN` 覆盖了项目工具链，须清除覆盖或在下列命令中使用 `cargo +1.99.0`。

```bash
# 仅构建组件
cargo build --locked --release --lib --target wasm32-wasip2
# 产物：target/wasm32-wasip2/release/stravia_vendor_workbuddy.wasm

# 或使用 Task：构建 + 打包 dist/（组件、许可证与 SHA256SUMS）
task dist

# 单元测试与组件契约验证（契约测试加载 release wasm，须先构建）
task test
```

## 未发布：请求传输与上下文对齐

- 补充轮次 root、B3 单头及 parent 扩展，按模型／HTTP 分层传播 W3C 上下文；ACP 按真实连接来源处理，缺失不伪造。
- 产品身份取官方配置或默认 `SaaS`；聊天请求启用真实 gzip 字节，尊重官方显式关闭。
- 固定 Stravia revision、SDK `0.4.0` 与 Rust `1.99.0` 构建通过；30 项单元测试、9 项真实 Wasm 契约通过。独立程序经实际 loopback TCP HTTP 完成国内 gzip 免费→付费重试与国际显式关闭 gzip，Node 解压并核对 Unicode、root／span 生命周期和产品头。使用合成凭据，没有调用真实上游或消耗账号积分。
- 宿主运行上下文跨子执行继承的定向回归通过；同一组件也在当前相邻 Stravia 的运行时完成上述独立 TCP 冒烟。需同步构建宿主修改才能获得跨模型调用的根关联。设备令牌仅完成方案调查，不包含原生 SDK 集成。

## v0.2.2 升级说明

- 认证、配置、聊天与积分查询切换为上述新版客户端接口和区域身份；保留区域凭据隔离，移除旧国内计费 origin，不提供旧地址回退。
- 额度查询改为单次资源包汇总，保留精确的小数积分，拒绝不完整数据；不再依赖旧资源列表分页或套餐名称。
- 使用项目固定的 Stravia revision 与 Rust `1.99.0` 构建，通过 34 项单元测试和 8 项 Wasm 组件契约。
- 独立程序加载新组件，使用真实国内凭据完成新版模型发现、积分查询与 `hy3` 推理，三个接口均返回 HTTP 200。聊天返回 `text/event-stream`，最终文本为 `OK`，正常结束并返回 usage；自动付费保持关闭，未执行生图。国际版已核对官方发行包并通过组件契约，未使用国际账号实测。
- SDK 仍为 `0.4.0`，本轮不迁移 Stravia SDK 或 canonical 请求契约。相邻 Stravia checkout 若不在项目指定 revision，应按上文构建基准准备独立源码，而不是直接更新依赖锁定。

## v0.2.1 升级说明

- 对齐上述 Stravia dev 提交的统一 canonical 请求控制，并将最低 Rust 与构建工具链同步升至 `1.99.0`。沿用 SDK 的 owned 请求、参数归一化与显式编码组件执行接口；canonical format 仍为 `1`，不新增旧字段透传或兼容分支。频道请求头声明、WebSocket reuse-age 参数、请求白名单、区域凭据隔离与既有付费线路授权策略保持不变。
- 分别更新国内与国际客户端产品及内嵌 CLI 身份，不混用区域发行版本。组件契约检查 `X-IDE-Version` 与 User-Agent 的产品版本一致性，不固定发行版本默认值。
- 组件契约覆盖宿主目标 effort 优先于入口 effort，并确认统一缓存参数 `prompt_cache_key` 仍不能绕过官方 CLI 白名单。Chat 目标关闭思考使用 `Effort { value: "none" }`，不使用其它目标协议的 `Disabled` 控制。
- 使用锁定 Rust 工具链通过 37 项单元测试与 8 项真实 Wasm 组件契约；独立离线程序加载组件，完成 CN / Intl 的 Responses canonical → 协议选择 → Chat SSE 推理，验证思考开启／关闭、目标优先级、JSON Schema、缓存白名单及 usage。本轮未调用真实账号服务；使用新版须重新构建并导入插件。

## v0.2.0 升级说明

- **不兼容变更**：推理请求体和请求头仅保留官方 CLI 白名单内的字段。白名单外的参数和自定义请求头不再透传；依赖这些字段的调用方须调整配置。提示词、工具参数和 JSON Schema 等用户数据保持原文。
- WorkBuddy SVG 图标已嵌入插件。显示图标需要支持 `ProviderDescriptor.icon_svg` 的新版 Stravia 宿主，并重新导入插件。
- 更新 Stravia SDK 和认证响应契约；源码构建需要上文指定的完整 Stravia checkout。

## 发布流程（维护者）

版本号以 `Cargo.toml` 的 `version` 为准。同步更新 `Cargo.lock` 中本插件的版本，运行 `task verify`，提交版本变更并推送后，打 tag 触发 GitHub Action 构建并发布 Release：

```bash
git tag -a v0.2.2 -m "发布 v0.2.2"
git push origin v0.2.2
```

tag 的 `v` 前缀版本号必须与 `Cargo.toml` 一致，否则 workflow 拒绝发布。Release 附件包含 `stravia-vendor-workbuddy-v<版本>.wasm`、`SHA256SUMS`、`LICENSE`、`NOTICE` 及第三方组件许可证。

## 项目结构

```
├── src/
│   ├── lib.rs          # VendorGuest 入口、区域定义与准入校验
│   ├── auth.rs         # 浏览器登录 Start/Poll/Refresh/Revoke
│   ├── models.rs       # 默认 agent 模型发现、线路配对与配置校验
│   ├── inference.rs    # OpenAI 兼容推理与 6004 付费线路切换
│   ├── state.rs        # 认证与付费窗口共享的私有状态读写
│   ├── allowance.rs    # 积分额度查询
│   └── profile.rs      # VendorDescriptor / ProviderDescriptor 声明
├── messages/           # zh-CN / en-US 界面文案
├── vendor/             # 构建期消息编译器与第三方许可证
└── tests/              # 组件契约测试（加载真实 wasm 验证）
```

## License

[MIT](LICENSE) · Copyright (c) 2026 Chikage0o0

第三方组件声明见 [NOTICE](NOTICE)。
