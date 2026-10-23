# Stravia Vendor WorkBuddy

把 **WorkBuddy / CodeBuddy（腾讯代码助手）** 账号接入 [Stravia](https://github.com/Stravia-AI/StraviaPlatform) 的 wasm 供应商插件 —— 你在 WorkBuddy 里能用的模型（DeepSeek、Kimi、GLM、混元……），经由 Stravia 统一网关以 **OpenAI 兼容接口**提供给任意客户端（Codex CLI、Claude Code、Cherry Studio、Open WebUI……）。

> ⚠️ 本插件不是腾讯 WorkBuddy 官方发布的插件，上游服务、产品名称及商标归各自权利人所有。上游接口属非公开协议，无稳定性承诺，仅供个人学习研究，请自担风险。

## 特性一览

- 🔐 **浏览器授权登录** — 打开授权链接完成 QQ / 微信 / 手机号登录，无需手动复制 token；令牌自动刷新与撤销
- 🌐 **双区域隔离** — 国内版（`www.workbuddy.cn`）与国际版（`www.workbuddy.ai`）两个 channel，凭据与流量严格限定在所选区域
- 🤖 **模型自动发现** — 从账号 `/v3/config` 的默认 agent 名单实时发现模型，按上游配置合并免费／付费线路；自定义模型可在宿主的可用模型页面手动添加
- **图片生成** — 国内、国际版均接入宿主 `media_image` 操作；按账号目录的图片标签发现模型，支持文生图及最多三张参考图
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

## 图片生成

在支持图片 Route 的 Stravia 宿主中使用 `media_image` 操作。两个 channel 均声明该能力，但模型是否可用仍由对应区域的账号配置和上游路由决定；不把普通聊天模型的 `supportsImages`（图片输入）误认为图片生成能力。

自动发现保留默认 agent 的聊天模型及顺序，再追加 `/v3/config.models` 中带 `text-to-image` 或 `image-to-image` 标签的模型，按 ID 去重。图片模型声明 `media_image` 和图片输出；带 `image-to-image` 标签时同时声明图片输入。聊天模型明确声明 `infer`，不会因新增插件能力而自动获得生图能力。国际客户端静态目录中的图片模型不会覆盖账号返回的真实目录。

### 请求与产物

- 文生图：所选区域的 `POST /v2/images/generations`；有参考图时改为 `POST /v2/images/edits`。使用 JSON 请求与响应，不走聊天 SSE。
- 请求使用宿主选定的上游模型，不设置生图默认模型或跨区域回退。Hunyuan 请求单张图片；OpenAI 风格模型另外请求 `response_format: "b64_json"`；Gemini 风格模型保留客户端的基础字段。
- 参考图由宿主先物化为字节，再编码为 `image` 数组中的 data URL；最多三张，每张非空且不超过 32 MiB。插件不获取用户提供的任意参考图 URL。
- 提示词不能为空。`aspect_ratio` 和 `resolution` 转为上游 `size: "宽x高"`，缺省为正方形 1K；1K 的长边为 1024，2K、4K 分别在下表尺寸上乘 2、4。模型若不接受某个尺寸，直接返回上游错误，不静默改尺寸或重试。

| `aspect_ratio` | 1K 的 `size` |
|---------------|-------------|
| `1:1` | `1024x1024` |
| `3:4` | `768x1024` |
| `4:3` | `1024x768` |
| `9:16` | `576x1024` |
| `16:9` | `1024x576` |

仅接受成功响应中的单张图片；空结果、多图、业务错误及损坏的 Base64 均报错。`b64_json` 解码为图片字节；URL 产物允许当前连接已批准的区域 HTTPS origin，以及静态声明的 COS 主机正则 `[a-z0-9-]+\.cos\.[a-z0-9-]+\.myqcloud\.com`。正则匹配完整规范化 DNS 主机名，不固定桶名或 COS 区域，也不扩大到任意 `myqcloud.com` 子域。域名、协议与端口的最终准入由宿主冻结的操作权限执行，不从响应动态扩权。

产物 GET 只发送 `Accept: image/*`，不转发 WorkBuddy 的 Authorization、账号、企业或会话头；URL 必须为 HTTPS／443，不能含嵌入式凭据或 fragment。下载要求图片 MIME，图片字节上限为 32 MiB；签名 URL 不进入返回产物。实际图片格式与像素内容由 Core 统一解码校验。重新导入时，支持正则的宿主会显示新增的主机正则、HTTPS 与有效端口 443，以及被移除的固定桶权限。COS 范围外的 CDN 仍会被拒绝，需要重新核实并更新声明，不开放任意响应 URL。

该声明使用 `host: {"regex":"..."}`，需要同时更新 SDK 和 Stravia 宿主。新宿主继续加载旧插件的字符串 host，并保留其精确匹配语义；旧宿主会在加载新对象时明确拒绝，不能只更新插件。插件不重复实现宿主的正则匹配器。

存在的 `input_tokens`、`output_tokens`、`total_tokens` 映射为 canonical token 用量；缺失 token 计量时不伪造零用量。上游原始图片计数和 `credit` 等计量保存在产物 `metadata.workbuddy_usage`，不把积分当作 token。图片生成可能消耗积分；`auto_paid_on_rate_limit` 只控制聊天线路，不作用于图片请求，图片请求没有自动付费切换或重试。

### 已验证的账号状态

2026-10-22 使用本机真实凭据和插件的完整配置请求身份头查询两区 `/v3/config`，均为 HTTP 200 / `code: 0`。本次复核使用修正后的国际版 `workbuddy-ai` 应用身份；国内 UA 保持不变。下表计数是上游原始 `models` 数组长度，不是按默认 agent 筛选后的同步模型数量：

| 区域 | 原始目录 | 生图模型 |
|------|----------|----------|
| 国内 | 62 条 | `hunyuan-image-alpha`：`text-to-image`；`hunyuan-image-alpha-edit`：`image-to-image` |
| 国际 | 28 条 | 当前账号没有返回生图标签模型 |

国内宿主实际同步记录中的两个 Alpha 模型均来自 `cn` 的 `account-config`，状态为 `present`。文生图模型声明文字输入、图片输出；Edit 模型另声明图片输入，与上游标签一致。它们不是插件对 `hunyuan-image-v3.0-art` 的重命名，也不是需要移除的静态占位模型。

**模型目录对 User-Agent 敏感，不能用简化身份的直连结果替代实际同步。** 同一国内账号、同一端点的单变量对照确认：`User-Agent: WorkBuddy/5.8.3` 返回 37 条目录和 `hunyuan-image-v3.0-art`；只把该字段改为插件实际使用的 `WorkBuddy/5.8.3 WorkBuddy/5.8.3 CLI/2.161.0`，就返回上述完整目录和两个 Alpha 模型。分别单独增加 `X-Requested-With`、Origin、Referer、Accept-Language、Accept 或 Content-Type，均不会切换到 Alpha 目录。独立诊断须保留完整身份头，不得手工简化 UA，也不应把不同身份返回的模型 ID 硬编码进发现逻辑。

早期已授权的国内单次出图使用了简化 UA 和 `hunyuan-image-v3.0-art`，得到 HTTP 200 / `code: 0`、1024×1024 PNG，上游报告 `credit: 5.71`；国际静态模型 `gpt-image-2.5-sunburst` 在该轮请求中返回 HTTP 400 / `14401`。这些历史结果只证明当时请求身份与模型对应的接口行为，**不能证明完整插件身份下两个 Alpha 模型已完成真实出图验证**。本轮目录纠正只执行配置 GET，没有再次发起生图或消耗生图积分；不承诺其他账号的权限、价格、模型尺寸或实时可用性。

已发布 v0.3.0 的 Wasm 另通过 36 项单元测试、11 项真实组件契约，以及独立程序的 7 个场景、8 次真实 loopback TCP 交换。回放使用脱敏后的真实目录和国内生成的 PNG、合成凭据，验证两区发现、URL 下载、Base64 文生图、参考图、尺寸映射、上游错误不重试及未批准 URL 拒绝；不把国际离线成功响应当作线上出图成功。组件契约同时检查下载不泄露凭据和跨区域凭据在出站前被拒绝。此次正则变更仅使用合成凭据、本地网络与隔离数据库验证，没有再次发起真实生图。

## 可选配置

| 字段 | 说明 |
|------|------|
| `auto_paid_on_rate_limit` | 布尔值，默认 `false`。开启即允许免费线路返回 `6004` 频控错误后，自动使用上游配置且允许切换的付费线路，可能消耗积分 |

连接设置不再提供“自定义模型 ID”输入框。需要额外模型时，在宿主的可用模型页面手动添加；自动同步仍按账号配置发现模型。已保存连接中的 `model_ids` 仍按原规则读取，本次不迁移或删除既有配置。若旧连接保存过该键，宿主会将其识别为未声明配置并阻止编辑保存，需从该连接的 `vendor_options` 中移除 `model_ids`，并保留其他选项；仅将其值设为空字符串不能移除该键。

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
- function 工具只保留 `name`、`description`、`parameters`、`strict`，其中 `strict` 在国内／国际版均统一为 `false`（包括覆盖显式 `true`）；工具调用只保留 ID、类型、函数名、arguments 和已知 Google 签名结构。
- `response_format` 只保留 `type`、`json_schema`；`json_schema` 只保留 `name`、`strict`、`schema`。
- **不递归过滤用户数据**：提示词、工具结果文本、工具 `arguments`、工具 JSON Schema / 输出 JSON Schema 内的属性名、默认值和自定义 schema 关键字保持原样。

例如，`metadata`、`service_tier`、`user`、`seed`、`n`、`logit_bias`、`logprobs`、`top_logprobs` 和未知字段不会上送。现有 `max_completion_tokens` → `max_tokens`、`reasoningEffort` → `reasoning_effort` 等归一化仍先执行；允许字段不表示每次必发，实际值仍受共享 codec、宿主目标思考控制及现有模型归一化约束。

请求头仅由插件的区域凭据、官方产品配置、固定 Windows/x64 兼容 profile 和宿主可信请求上下文生成，不转发客户端或 codec 自带的请求头。最终允许：

- 传输：`Content-Type`、`Accept`、`Authorization`、`User-Agent`、`X-Requested-With`；聊天 `Accept` 使用官方 SDK 的 `application/json`，请求体仍强制流式。插件不设置 `Accept-Encoding`、`Content-Encoding` 或 `Content-Length`，压缩相关头和实际长度由宿主 HTTP 传输层管理。
- SDK 标记：下文固定 profile 列出的八项 `X-Stainless-*` 字段；按完整名称放行，不开放前缀透传。
- 身份：`X-IDE-Name`、`X-IDE-Type`、`X-IDE-Version`、`X-Product`、`X-Domain`、`X-User-Id`、可选 `X-Enterprise-Id` / `X-Tenant-Id` / `X-Userinfo`。
- Agent：普通聊天采用官方缺省 `X-Agent-Purpose: conversation`、`X-Agent-Intent: craft`、`X-Agent-Type: main`；只用于聊天，不带入模型发现或额度请求。这是插件当前的普通聊天配置，不表示识别了 Stravia 内部的子 Agent、团队或辅助调用。
- 隐私标记：`X-Private-Data` 根据官方模型优化开关生成，不接受客户端请求头覆盖。
- 会话：`X-Request-ID`、`X-Root-Request-ID`、`X-Conversation-ID`、`X-Conversation-Message-ID`、`X-Conversation-Request-ID`；真实 ACP 上下文存在时允许 `acp-connection-id`。
- 追踪：`baggage`、`traceparent`、可选 `tracestate`、`b3`、`X-Trace-ID`、`X-B3-TraceId`、`X-B3-SpanId`、可选 `X-B3-ParentSpanId`、`X-B3-Sampled`。

头名称匹配不区分大小写。聊天不再发送 `Origin`、`Referer`、`X-CodeBuddy-Request`、`Accept-Language`、`X-Model-ID`、`X-Product-Version`。认证、模型发现和额度查询使用各自固定的请求体 / 请求头，不接收推理参数透传。

对照基准为官方 npm 发行包 [`@tencent-ai/codebuddy-code@2.161.2`](https://registry.npmjs.org/@tencent-ai/codebuddy-code/-/codebuddy-code-2.161.2.tgz)，依据是实际构造代码，而不是 OpenAI SDK 的全部可支持字段：

- [SDK bundle `9931.codebuddy.js`](https://unpkg.com/@tencent-ai/codebuddy-code@2.161.2/dist-server/9931.codebuddy.js)：`OpenAIChatCompletionsModel` 的请求对象、消息与 function 工具转换。
- [Provider bundle `3163.codebuddy.js`](https://unpkg.com/@tencent-ai/codebuddy-code@2.161.2/dist-server/3163.codebuddy.js) 与 [核心 bundle `codebuddy.js`](https://unpkg.com/@tencent-ai/codebuddy-code@2.161.2/dist-server/codebuddy.js)：会话 / OTel 注入、身份 / 产品 / User-Agent 拦截器及默认请求头；[传输 bundle `3043.codebuddy.js`](https://unpkg.com/@tencent-ai/codebuddy-code@2.161.2/dist-server/3043.codebuddy.js) 提供默认 `Accept`。
- [兼容规则 `1525.codebuddy.js`](https://unpkg.com/@tencent-ai/codebuddy-code@2.161.2/dist-server/1525.codebuddy.js)：已知思考格式、缓存控制、工具结果名称和 Google 签名结构。只据其中明确构造的字段放行，不开放任意兼容模板配置键。

官方版本变化不会自动放开新参数，须重新核对发行包并更新白名单。

工具严格模式另按国内 WorkBuddy、国际 WorkBuddy AI 的实际安装包所带 CLI 核对：两版 `BaseTool` 均默认 `strict:false`，SDK 工具构造与 OpenAI 转换保留并发送该值，并非省略字段。国际 DeepSeek 原始复杂工具集在 `strict:true` 时持续低缓存命中；真实 WASM 对照仅修改工具 `strict` 后，两区重复请求均恢复高命中。这不表示所有模型或所有严格工具都不能缓存，也不保证首次请求命中。工具参数 JSON Schema 与输出 `response_format.json_schema.strict` 保持原样；工具定义不再提供服务端 `strict:true` 的强约束，调用方仍须校验工具参数。

当前客户端身份按区域的官方稳定更新 API 与实际桌面发布包核实：国内 WorkBuddy `5.8.3`、国际 WorkBuddy AI `5.8.4`，两者内嵌 CLI 均为 `2.161.0`。产品版本同步用于 `X-IDE-Version` 和 User-Agent。来源：[国内更新 API](https://www.workbuddy.cn/v2/update?platform=workbuddy-darwin-arm64)、[国际更新 API](https://www.workbuddy.ai/v2/update?platform=workbuddy-darwin-arm64)。独立 npm CLI 的 latest 版本不是桌面应用的内嵌 CLI，不能直接替代其身份字段；上面的白名单引用仍保留其原始核对版本。

模型发现、聊天和图片请求使用完整 CLI UA。官方 `buildUserAgent()` 优先选择 `applicationName`，为空才使用 `productName`，因此国际版不能用产品展示名 `WorkBuddy AI` 替代应用名 `workbuddy-ai`：

- 国内：`WorkBuddy/5.8.3 WorkBuddy/5.8.3 CLI/2.161.0`。
- 国际：`WorkBuddy/5.8.4 workbuddy-ai/5.8.4 CLI/2.161.0`。

认证和额度接口继续使用各自的轻量产品 UA，不受本次修正影响。

### 聊天请求头生成规则

企业账号在现有凭据校验通过后，生成标准 Base64 编码的 `X-Userinfo`，内容为 `{"uin":"<uid>","owner_uin":"<enterprise_id>"}`。个人账号仅有 uid、没有其他真实身份来源时不生成该头；不从客户端头或任意 metadata 推断 `X-Auth-Method`、`X-Id-Source`。模型发现和额度请求不带聊天专用的 userinfo、隐私标记或 Agent 字段。

模型发现仅在官方配置 `productFeatures.DisableYuanbaoChannel` 是布尔值时，将其保存为模型 metadata 扩展 `workbuddy_disable_model_optimization`。聊天中该扩展严格为 `true` 时发送 `X-Private-Data: true`，缺失、`false` 或类型不正确时均发送官方缺省 `false`。免费与付费线路使用同一产品配置快照，`paidMetadata` 不能改变此开关。

当前宿主没有提供可信的子 Agent / 团队 / 辅助调用类型、原始父会话 ID 或上下文压缩状态，因此不生成相应的条件头，也不把传输亲和键当作父会话 ID。已核对的官方聊天链没有默认注入设备票据，故不增加 `X-Device-Token`。

### 固定 Windows x64 SDK profile

聊天请求按指定的 Windows x64 环境发送下列固定兼容头；认证、模型发现和额度请求不添加这一组，客户端请求头和 operation metadata 不能覆盖其值，也不放行任意 `X-Stainless-*` 字段。

| 请求头 | 固定值 |
|--------|--------|
| `X-Stainless-Lang` | `js` |
| `X-Stainless-Package-Version` | `6.25.0` |
| `X-Stainless-OS` | `Windows` |
| `X-Stainless-Arch` | `x64` |
| `X-Stainless-Runtime` | `node` |
| `X-Stainless-Runtime-Version` | `v22.21.1` |
| `X-Stainless-Retry-Count` | `0` |
| `X-Stainless-Timeout` | `600` |

SDK 版本与默认 timeout 取自上述官方 npm bundle；`DEFAULT_TIMEOUT=600000` 毫秒经秒级转换得到 timeout 头。免费→付费切换是新的模型请求，不属于 SDK 网络重试，两次均从零开始；本次不新增网络重试，也不改变宿主实际请求期限。

Node 版本选择已安装 WorkBuddy 所用 Electron `37.10.3` 对应的 Node `22.21.1`，来源为 [Electron 官方发布记录](https://releases.electronjs.org/release/v37.10.3)。这是固定兼容 profile，不是对 Wasm 宿主的运行时检测；现有静态证据也不足以证明所有官方聊天 CLI 路径都使用此 Node 版本。SDK 版本与 Node 版本分别来自 npm bundle 和 Electron 发布记录，不能混同于桌面产品版本或内嵌 CLI 版本。

### 请求上下文与 gzip

- `X-Product` 使用模型发现保存的官方 `deploymentType`，缺失时默认 `SaaS`，不接受客户端覆盖。`deploymentType` 和 `productFeatures.RequestBodyGzip` 分别保存到现有模型元数据扩展 `workbuddy_deployment_type`、`workbuddy_request_body_gzip`；免费／付费线路共用当前操作的产品配置快照。更新后需重新同步模型；手动模型 ID 未读取产品配置时使用产品默认值。
- `operation_metadata.root_request_id` 由宿主的用户运行 ID 产生，同一运行的内部重试与 Agent 子模型调用继承，不使用跨用户多轮的 Generation Chain root。插件将它确定性映射为 `X-Root-Request-ID`，输出 32 位小写、无横线的 UUIDv7 位格式。缺失、空串或非字符串时告警，退化为本次推理操作新建的 UUIDv7 root；不会把客户端新发起的 HTTP 请求自动归并为同一轮。
- `X-Conversation-ID` 只映射可信 `operation_metadata.session_affinity`，输出 36 字符带横线格式，不再原样发送宿主亲和键。缺失、空串或非字符串时省略会话头和 session baggage，不伪造会话关联；客户端 header 和 canonical session/request 参数不能覆盖该来源。
- 稳定映射契约固定为 `SHA-256(b"stravia-workbuddy-id-v1\0" || 源 ID 的完整 UTF-8 字节)`，取前 16 字节，再设置 UUID version 为 `7`、variant 为 RFC 4122，最后按字段添加或省略横线。输入不裁剪、不改大小写、不限可见 ASCII；时间、随机数、设备和插件实例均不参与映射。相同源 ID 跨重试、子调用和插件重载得到相同 UUID 内容，无需持久映射表。例如 `user-run-a` 的 root 固定为 `c845b199d22c77b88bf012ecdc201766`。命名域、摘要算法及位布局是稳定身份契约，不能随意修改。该摘要只匹配 UUIDv7 外形，不表示真实创建时间，不是官方确定性映射算法，也不用于转换认证密钥、签名或令牌。
- 宿主每个运行保存独立、未采样的 W3C 根传播上下文，经可信 `operation_metadata.traceparent` 传给插件；有效 `tracestate` 一同继承。插件为每次模型操作新建模型 span，内部免费→付费重试复用该 span，每个 HTTP 尝试另建子 span。B3 单头／多头表示模型 span，W3C 表示 HTTP span，采样位继承父上下文；WorkBuddy 的 B3 parent 扩展单独补充。宿主上下文缺失或无效时告警并新建推理级未采样 trace，不启动遥测导出，也不宣称完整复刻官方遥测。
- `X-Request-ID` 与 `X-Conversation-Message-ID` 每次 HTTP 尝试共用一枚新 UUIDv7，输出 32 位小写、无横线格式；`X-Conversation-Request-ID` 为同格式的新 UUIDv7，在同一次模型操作和内部免费→付费切换中稳定，新模型操作重新生成。当前 SDK 没有提供这两类字段的独立可信源 ID，因此不挪用 root 或 session ID 生成它们，也不把客户端的 `parameters.session.request_id` 当作宿主 ID。`baggage` 的 session 和 conversation-request 值与对应头完全一致，不使用 message ID。
- 上述默认形态来自 [WorkBuddy AI 5.8.4 客户端包](https://download.codebuddy.ai/workbuddy/saas/darwin-arm64/WorkBuddy-darwin-arm64-5.8.4.41366630-73952edb.zip)内嵌 CLI `2.161.0` 的 `SessionManager`、`generateConversationRequestId` 和 `buildConversationHeaders`。官方 session 校验并不强制 UUID，root 头也可继承非 UUID 字符串；conversation-request candidate 的客户端校验明确要求 32 位小写 UUIDv7 位格式。不能据此声称所有远端接口都强制 UUIDv7。认证、模型发现和额度路径保留既有请求 ID profile，trace/span 和真实 ACP 连接 ID 不参与这种转换。
- ACP 头只接受宿主从真实 ACP 连接取得的 `operation_metadata.acp_connection_id`。当前直连宿主没有 ACP 连接生产者，因此省略并记宿主诊断；不生成随机连接 ID，不挪用 OAuth 连接 ID、传输亲和键或云任务连接 ID，本次不新增 ACP 通道。
- 仅聊天 POST 通过 SDK 的 `GuestHost::http_start_with_compression(HttpRequest, HttpCompression)` 请求宿主压缩。Guest 始终传递原始 `application/json` 字节，不自行 gzip 或 Base64 编码；`HttpCompression.request` 默认选择 `Gzip`，官方元数据 `workbuddy_request_body_gzip` 明确为 `false` 时设为 `None`，不压缩请求。宿主负责 gzip、实际字节长度及相关传输头。响应 coding 列表保持为空（identity），本次不扩展上游响应压缩协商。认证、配置 GET 和额度 POST 不请求压缩。SDK / 宿主返回错误时按原错误处理，不回退或重新发送裸 JSON 请求。

跨重试／子调用的宿主根上下文需要同步构建 Stravia 宿主的 `RunObserver` 与 `model_turn::prepare_attempt` 修改；只导入新 Wasm 不能让旧宿主自动产生这些元数据。本次升级使用下述固定提交的 SDK 与 WIT `stravia:vendor@0.5.1` 新增 HTTP compression API；canonical format 仍为 `1`，不新增持久化 schema。必须先升级到支持该 API 的宿主，再重新构建并导入插件；旧宿主不能只靠重新导入插件获得新 API。

`x-device-token` 尚未实现。当前 `wasm32-wasip2` 不能直接加载官方客户端的 N-API `.node`；已核实的包装层仅尝试 Windows/macOS，不能据此承诺 Linux、CPU 架构或独立分发支持。推荐取得合法独立 SDK、WorkBuddy 认可的 channel 配置与平台支持后，在可信原生宿主的受限 HTTP 出口生成并注入，令牌不进入 Wasm 或声明为 non-secret 的 `operation_metadata`。腾讯公开 [SDK 平台资料](https://main.qcloudimg.com/raw/document/product/pdf/1628_111422_cn.pdf) 包含 Web/H5，但没有证明其票据兼容 WorkBuddy；[服务端风险 API](https://main.qcloudimg.com/raw/document/product/pdf/1628_79611_cn.pdf) 消费客户端票据，不能当作替代签发接口。本次没有执行设备 SDK 或发送设备信息。

### 新版客户端接口

| 区域 | 固定上游 origin | 授权 `platform` |
|------|---------------|----------------|
| 国内 | `https://www.workbuddy.cn` | `workbuddy` |
| 国际 | `https://www.workbuddy.ai` | `workbuddy-ai` |

认证、模型发现、聊天、生图账号 API 和积分查询均使用所选区域的固定 origin；图片产物下载另遵循上述精确 origin 限制：

- 授权状态：`POST /v2/plugin/auth/state?platform=...`；令牌轮询：`GET /v2/plugin/auth/token?state=...`。
- 令牌刷新：`POST /v2/plugin/auth/token/refresh`，两个区域均发送 `X-Auth-Refresh-Source: plugin`。
- 模型配置：`GET /v3/config`；聊天：`POST /v2/chat/completions`。
- 积分汇总：`POST /billing/meter/get-user-resource-summary`，请求体为 `{}`；对 `data.Packages` 中的 `CycleTotalCapacity`、`CycleRemainCapacity`、`CycleUsedCapacity` 做十进制精确求和。接口不分页；缺失或非法的周期额度字段会导致查询失败，不返回部分余额。请求发送桌面产品 User-Agent、`X-Request-ID`、账号与企业身份头，不再使用旧计费信封。

插件不再声明旧国内计费域，也不接受旧国内 base URL；配置响应中的 `data.endpoint` 不会改变固定目的地或扩大网络权限。升级后须重新导入插件，把已有国内连接的 base URL 改为 `https://www.workbuddy.cn`，并重新同步模型。

### 模型发现与线路选择

聊天模型发现遵循客户端的默认 agent 名单及顺序，不再用 `credits` 字段猜测可选模型，也不回退到全量后端目录；图片标签模型按上述规则追加。缺少有效名单时明确报错。免费／付费配对来自 `productFeaturesConfig.ModelRateLimitCap.lines`：默认列表只显示 `freeId`，名称优先使用配对的 `displayName`；不按名称合并无关模型。

自动付费开关的行为如下：

- 关闭时，不自动切换到配对的付费线路。这个开关不改变其他模型的正常计费，也不阻止用户显式指定付费 ID。
- 开启时，仅对明确的 `6004` 频控错误切换，且必须尚未向客户端输出任何增量。普通 HTTP 429、超时、其他错误和已经输出的响应不会触发付费重试。
- 每次插件推理最多切换一次，付费尝试失败后直接返回错误，不递归重试或切换其他模型。两次线路传输属于一次逻辑补全，宿主只收到一次 `UpstreamStarted`；宿主自身的重试和路由策略独立于此开关。
- 有未来重置时间时，在该频控窗口内复用付费线路，到期恢复免费线路；明确已到期的错误不触发新的付费尝试。没有可解析的重置时间时，仅当前请求允许切换，不自行假定窗口时长。
- 窗口按连接、区域、账号、企业及线路配对隔离。关闭开关后忽略窗口；认证待登录状态与窗口共存，吊销本地授权时一起清除。SDK 私有状态接口没有 CAS，因此并发认证／推理写入不保证原子合并。
- 显式 `model_ids` 或缺少配对元数据时，只有收到 `6004` 后才按需读取 `/v3/config` 查找有效配对；已有活动窗口时会先核对线上配对再使用付费线路，不根据模型名称或硬编码 ID 猜测。

更新插件后需重新同步模型。插件不会删除宿主保留的历史模型记录；旧记录可能仍在管理页面中显示。免费线路名称不承诺永久免费，实际价格、可用权限及扣费结果以上游账号为准。

## 自行构建

当前源码使用插件目录旁的完整 `StraviaPlatform/` checkout，五个 Stravia 依赖统一指向 `../StraviaPlatform/backend/crates/...`；不能只下载本插件源码。此未发布变更要求该 checkout 包含 SDK 的 `OriginHost` 和 Core 的主机正则权限实现，不能只按 crate 版本号判断兼容性。

发布工作流按同一目录关系检出到 `source/StraviaPlatform`；现有 `STRAVIA_PLATFORM_REF` 仓库变量可指定固定 revision。工作流默认的 [`20c1291c142d5c19ba267552a6bfbdbb1c947bc6`](https://github.com/Stravia-AI/StraviaPlatform/commit/20c1291c142d5c19ba267552a6bfbdbb1c947bc6) 是已发布 v0.3.0 的依赖基线，不含此新契约；发布正则版本前，必须先合入宿主变更，再将该变量设为包含变更的固定提交。覆盖变量后须核对 SDK、protocol-codec 和 vendor-runtime 契约，并通过构建与真实组件验证。Rust workspace crate 版本与 WIT ABI 包版本是两套版本号，本次不改变 `stravia:vendor@0.5.1`。

`assets/workbuddy.svg` 使用 [WorkBuddy 官方 SVG](https://download.codebuddy.ai/web/workbuddy/f5bce0c03cdc17fa28d25634fb48d2791c297da3/assets/logo.svg) 的原始 mascot 和双眼几何，去掉背景、渐变及模糊，编译进 Wasm 为透明单色图标。显示需要更新后的 Stravia 宿主并重新导入插件，不在运行时下载品牌资源。图标归属和使用限制见 [NOTICE](NOTICE)。

依赖：Rust `1.99.1`（`rust-toolchain.toml` 已锁定；固定上游提交声明的最低版本为 `1.99.0`）+ `wasm32-wasip2` target；可选安装 [Task](https://taskfile.dev)。若环境变量 `RUSTUP_TOOLCHAIN` 覆盖了项目工具链，须清除覆盖或在下列命令中使用 `cargo +1.99.1`。

```bash
# 仅构建组件
cargo build --locked --release --lib --target wasm32-wasip2
# 产物：target/wasm32-wasip2/release/stravia_vendor_workbuddy.wasm

# 或使用 Task：构建 + 打包 dist/（组件、许可证与 SHA256SUMS）
task dist

# 单元测试与组件契约验证（契约测试加载 release wasm，须先构建）
task test
```

## REA 逆向分析工具（开发用途）

项目根目录的 `.mcp.json` 为 Windows 下的 OMP 注册了 [REA](https://github.com/morluto/rea) MCP 服务，通过 `npx` 运行固定版本，不修改用户全局代理配置。需要符合 [REA 运行要求](https://github.com/morluto/rea/blob/main/docs/installation.md) 的 Node.js 与 npm；Rust 构建和 Wasm 插件不依赖此工具。

`.agents/skills/reverse-engineer-anything/` 包含该 npm 发行包配套的技能与参考资料。安装后重启当前项目的 OMP 会话，使其发现 MCP 服务和技能。CLI 也可直接使用：

```powershell
npx --yes --ignore-scripts rea-agents@6.7.0 --help
```

启动配置使用 `--ignore-scripts`，避免可选 Frida 依赖的原生安装脚本阻塞下载。Frida 原生绑定不在本次安装范围内；需要动态插桩时须单独完成安装。原生反编译还需自行配置 Ghidra 或 IDA 等分析引擎；注册 MCP 不代表所有分析后端均已就绪。

## 未发布变更

- 国内／国际 function 工具统一发送 `strict:false`，对齐官方 `BaseTool` 默认值；不修改参数 Schema、输出格式的严格模式或缓存用量统计。新旧真实 WASM 与两区真实凭据对照已验证重复请求的缓存恢复，上游原始 SSE、canonical、下游计数一致。
- 国际版完整 CLI UA 的应用段由产品展示名改为官方 `applicationName`；国内身份不变。两区真实账号的只读配置请求均成功，账号目录见[已验证的账号状态](#已验证的账号状态)。
- COS 图片下载权限由单一固定桶改为上述完整主机正则，支持不同桶和 COS 区域；HTTPS／443、默认拒绝及无凭据下载约束不变。
- 需要先升级宿主，再使用支持该契约的 SDK 构建、导入插件。新宿主保留旧插件加载及精确授权语义；实物 v0.2.2、已发布 v0.3.0 已在隔离实例验证加载、安装与数据库重开。
- 当前源码和本地构建与已发布 v0.3.0 的原始 artifact 不同；未重发或修改既有 release。

## v0.3.0 升级说明

- 国内、国际版均新增宿主 `media_image` 操作，支持文生图、最多三张参考图、宽高比与分辨率映射，以及 Base64／URL 图片产物。具体线规与边界见[图片生成](#图片生成)。
- 模型同步追加账号目录中带 `text-to-image` 或 `image-to-image` 标签的模型；普通视觉模型仍只声明聊天能力，不因支持图片输入而获得生图能力。升级后须重新同步模型。
- 新增精确的 HTTPS 出站 origin `ai-corpus-backup-1258344699.cos.ap-beijing.myqcloud.com`。重新导入时请核对该权限；产物下载不携带 WorkBuddy 凭据，其他域名及跨区域 URL 均拒绝，不使用通配域名。
- 国内真实账号已验证 `hunyuan-image-v3.0-art` 出图及 PNG 下载；当前实测国际账号未开放生图模型，官方静态模型返回路由不存在。国际接口支持不代表账号已经获得模型权限，插件不会硬编码或伪造可用模型。
- 图片操作可能消耗积分，不使用聊天的自动付费线路切换，也不自动重试。既有登录凭据、连接配置和 canonical format 不变；延续支持 HTTP compression API 的宿主要求，国内连接使用 `https://www.workbuddy.cn`。

## v0.2.3 升级说明

- **身份关联变更**：宿主 root/session ID 改为固定 SHA-256 派生的官方 UUIDv7 外形，分别输出 32/36 字符；模型操作与每次 HTTP 尝试的新 ID 改用真实 UUIDv7。首次切换会改变原先原样发送的上游身份，不能沿用旧会话身份；此后映射跨重载稳定。此改动对齐 ID 形态，不等于已解决原 debug 包中的低缓存命中率。
- root/session 仅使用可信宿主元数据；缺失或非法 root 退化为本次操作的新 UUIDv7，缺失或非法 session 则省略会话头与对应 baggage。客户端不能覆盖身份来源，免费→付费切换保留同一模型操作的关联。
- 连接设置移除“自定义模型 ID”输入框；额外模型改在宿主的可用模型页面添加。旧 `model_ids` 仍可读取，但编辑保存旧连接前须从 `vendor_options` 移除该键，不能只清空其值。
- 本次 ID 映射更新先确认旧组件原样发送 root 的回归契约失败，再通过 30 项单元测试和全部 10 项 Wasm 组件契约。独立程序加载本次国际版 Wasm，以测试用 TCP adapter 完成 10 个场景、11 次真实 loopback HTTP 交换，验证免费→付费、子调用与重载稳定性、不同会话隔离、UTF-8/CRLF/NUL 源 ID、空值／错误类型／缺失元数据，以及 header/baggage 关联。仅使用合成凭据、本地服务并显式关闭 gzip；这次冒烟没有运行生产 HTTP encoder，也没有调用真实上游或重新测量缓存命中率。

## v0.2.2 升级说明

- **升级要求**：须先升级到支持新版 HTTP compression API 的 Stravia 宿主，再重新导入组件；国内连接的 base URL 使用 `https://www.workbuddy.cn`，升级后重新同步模型。
- 认证、配置、聊天与积分查询切换为新版客户端接口和区域身份；保留区域凭据隔离，移除旧国内计费 origin，不提供旧地址回退。
- 额度查询改为单次资源包汇总，保留精确的小数积分，拒绝不完整数据；不再依赖旧资源列表分页或套餐名称。
- 补充轮次 root、B3 单头及 parent 扩展，按模型／HTTP 分层传播 W3C 上下文；ACP 按真实连接来源处理，缺失不伪造。
- 产品身份取官方配置或默认 `SaaS`；聊天通过宿主 HTTP compression API 请求 gzip，Guest 只传原始 JSON，尊重官方显式关闭；传输头与长度由宿主管理，错误不触发裸请求重发。
- 固定远端 Stravia dev 提交 `20c1291c142d5c19ba267552a6bfbdbb1c947bc6`，Rust workspace SDK crate 版本为 `0.5.2`、WIT ABI 为 `0.5.1`，构建工具链更新为 Rust `1.99.1`；canonical format 保持 `1`。须升级支持新 API 的宿主后重新构建并导入组件。
- 聊天按真实账号条件生成企业 `X-Userinfo`，按产品配置生成 `X-Private-Data`，并补齐普通聊天 Agent 缺省；这些字段不进入发现或额度请求。保留请求上下文与重试生命周期，不接受客户端伪造身份，也不虚构缺少来源的 Agent、父会话或上下文压缩状态。
- 聊天补齐上述八项固定 Windows/x64 SDK 兼容头；客户端和 operation metadata 无法覆盖，不放行未知 Stainless 字段或 helper/poll 头。免费与付费线路都以新的 SDK 请求计数，不改变宿主超时及压缩行为。
- 本轮请求头对齐使用上述固定 SDK 和 Rust `1.99.1` 完成 release Wasm 构建，通过 30 项单元测试与 10 项真实 Wasm 组件契约。独立 TCP 冒烟加载同一组件，4 个场景、5 次真实 loopback 交换验证了企业／个人身份、隐私开关缺失和错误类型、客户端及 metadata 伪造过滤、八项固定 SDK 头唯一且无额外字段、免费／付费计数均为零，以及 Agent 普通缺省和请求与 trace 生命周期。此次头部冒烟显式关闭 gzip，仅使用合成凭据与本地服务，未调用真实上游。
- SDK 迁移时另行验证的 loopback TCP 冒烟使用上游未修改的生产 HTTP 编码器：国内免费／付费请求均为单层 gzip，国际显式关闭后为原始 JSON；Unicode、产品快照、root/message 关联和单次 `UpstreamStarted` 均通过。宿主运行上下文的跨子执行根关联仍需同步构建宿主修改；设备令牌不包含原生 SDK 集成。

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
git tag -a v0.3.0 -m "发布 v0.3.0"
git push origin v0.3.0
```

tag 的 `v` 前缀版本号必须与 `Cargo.toml` 一致，否则 workflow 拒绝发布。Release 附件包含 `stravia-vendor-workbuddy-v<版本>.wasm`、`SHA256SUMS`、`LICENSE`、`NOTICE` 及第三方组件许可证。

## 项目结构

```
├── src/
│   ├── lib.rs          # VendorGuest 入口、区域定义与准入校验
│   ├── auth.rs         # 浏览器登录 Start/Poll/Refresh/Revoke
│   ├── models.rs       # 默认 agent 模型发现、线路配对与配置校验
│   ├── inference.rs    # OpenAI 兼容推理与 6004 付费线路切换
│   ├── media_generation.rs # 图片生成、参考图与无凭据产物下载
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
