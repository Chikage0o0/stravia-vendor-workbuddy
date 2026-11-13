# Stravia Vendor WorkBuddy

此仓库提供非腾讯官方的 Stravia Wasm 供应商插件，将 WorkBuddy 国内和国际账号接入 Stravia。上游接口不是公开稳定协议，可能变化；账号权限、模型可用性和实际费用均以上游为准。使用前请确认上游条款，妥善保管凭据。

## 安装与使用

1. 先升级到支持插件市场 v2、兼容 Vendor `1.0.0`、Net `1.1.0` 和 State `1.1.0`，且支持组件声明并加载插件自有 egress shape 的 Stravia 宿主。旧宿主不支持 `plugin:workbuddy`，不会回退到旧内建 shape；升级后须重新安装插件。插件通过 Git tag `v0.7.0` 依赖 `StraviaPlatform` 的 SDK crate，构建时由 Cargo 自动拉取，无需本地检出宿主；使用 Rust `1.99.1` 和 `wasm32-wasip2` target。
2. 在供应商插件页面添加来源 `https://github.com/Chikage0o0/stravia-vendor-workbuddy`，通过独立渠道核对来源公钥的完整指纹后确认信任，再预览并安装 WorkBuddy、核对出站权限。也可从 [Releases](https://github.com/Chikage0o0/stravia-vendor-workbuddy/releases) 下载 `.wasm.zst` 本地导入；本地导入仍为 unverified，不会自动建立受信任的市场来源。当前版本为 `0.5.0`；`v0.4.0` 及更早 Release 不含新版目录与签名。
3. 创建 WorkBuddy 连接，选择国内或国际 channel，在浏览器完成授权并同步模型。国内账号使用 `https://www.workbuddy.cn`，国际账号使用 `https://www.workbuddy.ai`；凭据按区域隔离，配置响应不会扩大出站权限。若连接来自 CodeBuddy CLI 改版，应改回对应 WorkBuddy 区域地址并重新授权、同步模型；不要假定 CLI 凭据可用于桌面身份。
4. 在 Stravia 中启用所需模型，再以宿主提供的 OpenAI 兼容入口和访问凭据连接客户端。连接检查只查询受保护的账号资料，不生成内容；成功不代表聊天或生图模型已验证。

首次添加第三方来源是 TOFU：公钥与目录可能经过同一网络路径，必须通过独立渠道核对完整指纹。指纹是 Minisign 公钥 Base64 解码后二进制 payload 的 SHA-256，不是短 key ID 或公钥文本的哈希。目录签名只认证固定公钥对应的发布者，不代表腾讯官方发布、自动安全审查或插件安全；安装前仍须自行评估凭据和网络权限。

本来源目录签名公钥的完整 SHA-256 指纹为 `7c8ded592479ebb5a72abd6cc0f8c91dea80313eb789bea14131ba2bc49fe5c6`；首次信任前仍须通过独立渠道核对。

### 构建与发布插件目录

需要 Python 3.11+、Task、Minisign、Rust 和 `wasm32-wasip2`。SDK 依赖由 `Cargo.toml` 的 git tag 固定；共享工具 `stravia-tools plugin-catalog` 必须取自同一 Platform tag，或包含该工具且与 `Cargo.lock` 相容的 checkout。已有工具可通过绝对路径使用，不在本仓库复制目录生成实现。[v0.7.0 生成器](https://github.com/Stravia-AI/StraviaPlatform/blob/v0.7.0/backend/crates/stravia-devtools/src/plugin_catalog.rs) 读取真实 descriptor，校验插件身份，生成 level-19 单 frame Zstd 并解压回环；[目录契约](https://github.com/Stravia-AI/StraviaPlatform/blob/v0.7.0/backend/crates/stravia-core/src/plugin/distribution.rs) 定义压缩与原始组件的双摘要、公钥指纹及 32 KiB 图标上限。现有 WorkBuddy 图标已满足上限，无需修改。

本地示例（使用独立、加密的测试密钥；不要把密钥或密码放进仓库）：

```bash
export STRAVIA_TOOLS=/absolute/path/to/stravia-tools
export HOST_VERSION_REQ='>=0.7.0, <0.8.0' # 仅为示例，发布前由维护者评估兼容范围
export REPOSITORY_ID=1389031614 RELEASE_TAG=v0.5.0
export PUBLISHED_AT=2026-11-10T00:00:00Z
export PLUGIN_SIGNING_KEY_FILE=/secure/test-minisign.key
export PLUGIN_PUBLIC_KEY_FILE=/secure/test-minisign.pub
export PLUGIN_SIGNING_PASSWORD='<test password>'
task dist
```

`task dist` 构建组件，调用共享工具生成并验证压缩组件和 v2 目录，再用独立 Minisign 密钥签名、验签，核对压缩资产大小和 SHA-256。输出为 `dist/stravia-vendor-workbuddy-v0.5.0.wasm.zst`、`stravia-plugin-catalog.json`、`stravia-plugin-catalog.json.minisig`、`stravia-plugin-catalog.pub`、`LICENSE`、`NOTICE` 和 `rust_decimal.LICENSE`；不发布原始 `.wasm` 或 `SHA256SUMS`。验签成功后，脚本仅清理旧入口生成的 `dist/stravia-vendor-workbuddy-v*.wasm` 和 `dist/SHA256SUMS`，保留其他用户文件。Release tag 必须与 Cargo 版本一致，时间必须为 RFC 3339，宿主版本范围不得留空。

GitHub 发布前须配置 repository variables `HOST_VERSION_REQ`、`STRAVIA_PLUGIN_CATALOG_PUBLIC_KEY`（完整公钥文本），以及 secrets `STRAVIA_PLUGIN_CATALOG_SIGNING_KEY`（配对的加密私钥文本）、`STRAVIA_PLUGIN_CATALOG_SIGNING_PASSWORD`。目录密钥独立于 Desktop updater；生产密钥须在受控离线环境生成和保管，不得使用测试密钥。

Release workflow 由 `v*` tag 触发，也可在 Run workflow 中填写现有版本标签。它始终检出该标签，并按 `Cargo.toml` 固定的唯一 Platform tag 检出主仓库，仅用于构建共享目录工具；不需要 `STRAVIA_PLATFORM_REF`。工具构建前执行 `patch-crate` 和内建组件构建，与主仓库的准备流程一致。组件构建及现有单元、真实 Wasm 契约测试通过后生成签名资产，按明确的七文件清单创建 draft；`scripts/verify_release.py` 下载实际附件，逐字节比对并验签、核对摘要，成功后才公开。已有同 tag Release 不覆盖，失败保留 draft 供维护者检查处理；历史 Release 不补造目录或签名。构建和本地打包本身不发布、不推送。

## 模型、费用和状态

模型发现使用账号配置；支持的模型和权限随区域、账号及上游变化。聊天支持流式响应和工具调用。`auto_paid_on_rate_limit` 默认关闭；显式开启后，仅在免费线路返回 `6004` 且尚未输出时可切换一次付费线路，可能消耗积分。免费线路名称不保证永久免费。

`media_image` 支持文生图及参考图编辑，实际可用性取决于账号目录与上游权限；图片输入能力不等于生图能力。图片请求可能收费，不使用聊天自动付费切换，也不自动重试。图片产物支持 Base64 或受限 HTTPS URL；下载不携带账号授权头，并受宿主冻结的网络权限和大小限制。积分不转换为 token。

认证待登录状态保留在认证会话私有状态；付费频控窗口保存在 `workbuddy_paid_windows` provider State cell，以版本化 CAS 更新并按到期时间失效。宿主注销 OAuth 凭据不会调用插件撤销或清除该 cell。会话关联仅使用可信宿主上下文；插件使用声明于 `assets/workbuddy.json` 的 `plugin:workbuddy` egress shape，需配合支持插件自有 shape 的宿主。聊天 gzip 由宿主处理；插件提交原始 JSON，不因传输失败自动重发。两区冷连接主推理采集中的 `Accept-Encoding` 为 `gzip, compress, deflate, br`，位于 `Content-Length` 后、`Host` 前；插件声明该值并指定宿主解码 gzip、deflate、br。Net 1.1 不支持 `compress`：若服务端选择它将明确失败，不能声称完整支持该协商列表。两区内嵌 Axios HTTP 适配器虽将 `compress` / `x-compress` 分派到 `zlib.createUnzip`，Node 文档仅将该接口用于 gzip / deflate，并未提供 Unix compress/LZW 解码；因此源码分支本身也不能证明原生支持真正的 compress 流。该结论是静态源码分析，未以 compress 响应测试客户端。[Node.js zlib 文档](https://nodejs.org/docs/latest-v22.x/api/zlib.html#compressing-http-requests-and-responses)

### 区域、模型与额度

| 区域 | origin | 授权 platform | 客户端身份 |
| --- | --- | --- | --- |
| 国内 | `https://www.workbuddy.cn` | `workbuddy` | `WorkBuddy/5.9.3 WorkBuddy/5.9.3 CLI/2.165.0` |
| 国际 | `https://www.workbuddy.ai` | `workbuddy-ai` | `WorkBuddy/5.9.4 workbuddy-ai/5.9.4 CLI/2.165.0` |

这两组是插件固定的桌面身份，不保证始终为最新官方版本。国际桌面 `productName` 按打包 resolver 的 fallback 为 `WorkBuddy`；UA 的应用名则是 `workbuddy-ai`，不以 CLI 展示名替代。授权状态、轮询及刷新使用 `/v2/plugin/auth/` 接口；配置为 `GET /v3/config`，聊天为 `POST /v2/chat/completions`。区域不匹配的地址和凭据会被拒绝。自动发现以默认 agent 模型名单及顺序为基础，追加账号目录中带 `text-to-image` 或 `image-to-image` 标签的图片模型，按 ID 去重；免费／付费配对取自上游配置，不从模型名称推断。额外模型可在宿主可用模型页面手动添加；变更配置后重新同步模型。

额度查询以 `POST /billing/meter/get-user-resource-summary` 获取积分：存在有效 `TotalCredits` 时以它为权威余额，只有 `Packages` 完整且各项剩余之和与该余额相等时才同时展示总量和已用量，否则仅展示权威余额；缺少 `TotalCredits` 时按桌面客户端行为对 `Packages` 的周期总量、剩余量和已用量作十进制精确求和。资源包名不是订阅等级，不据此推断套餐或计费方式。参见[官方积分说明](https://www.workbuddy.cn/docs/workbuddy/Buddy-One/Credits)；个人中心与 5.9.3 桌面客户端的聚合行为属于客户端源码证据，并非公开 API 契约。附加账号展示读取国内 `/v2/plugin/account` 或国际 `/v2/plugin/accounts`，仅使用唯一匹配 uid 和企业的邮箱或昵称；资料不可用不丢弃已取得的额度。连接检查使用同一资料接口和唯一身份匹配，但资料错误会使检查失败；不刷新令牌、不查询额度或模型、不写状态。

开启自动付费后，只有明确的 `6004` 且未输出时才允许一次免费到付费的切换；普通 HTTP 429、超时、其他错误或已经输出不触发。存在未来重置时间时，窗口内可使用对应付费线路，到期恢复；没有可解析的重置时间时只允许当前请求切换。关闭开关时不使用窗口，显式选择付费模型仍可能收费。

### 图片与聊天请求约束

图片请求使用所选区域 `/v2/images/generations` 或 `/v2/images/edits`，最多三张非空参考图，每张不超过 32 MiB；提示词不能为空。缺省尺寸为 1024×1024，支持指定宽高比和 1K、2K、4K；上游不接受尺寸时直接报错。仅接受单张结果；图片 URL 必须为 HTTPS，且由宿主已批准的区域 origin 或声明的 COS 主机范围准入，下载不转发授权、账号或会话头。原始图片计量保存在 `metadata.workbuddy_usage`，不是 token 用量。

聊天对协议字段进行白名单过滤，未知字段和客户端自带请求头不透传；提示词、工具 arguments 和 JSON Schema 等用户数据不递归过滤。顶层允许 `model`、`messages`、`tools`、`tool_choice`、`parallel_tool_calls`、`temperature`、`top_p`、`frequency_penalty`、`presence_penalty`、`max_tokens`、`stream`、`stream_options`、`store`、`prompt_cache_retention`、`response_format`、`reasoning_effort`、`reasoning_summary`、`reasoning`、`thinking`、`verbosity`、`enable_thinking` 和 `chat_template_kwargs`；嵌套协议结构也有限制。function 工具的 `strict` 统一设为 `false`，调用方仍须校验参数；输出 JSON Schema 的 `strict` 不受此规则改变。

官方 CLI 2.165.0 的主 agent 请求处理器根据内部消息来源生成 `extra_vars.initiator`，不是信任客户端传入的值。本插件没有官方内部来源标记：末条为 user 时按普通直接用户请求生成 `{"initiator":"user"}`；末条为 tool 或 assistant 时不生成，且绝不透传客户端 `extra_vars`。这不能区分官方内部附加 user 消息或子 agent，因而不是全部官方场景的等价实现。产品网关请求若含字符串形式的 `reasoning_effort` 或 `reasoning.effort`，官方源码在缺省 summary 时补 `reasoning_summary: "auto"`；插件遵循该分支，但不把目录中的推理能力误认为官方 always-thinking 状态。以上为特定 bundle 的静态代码依据，不证明此差异是 Intl 400 的根因。

会话和 trace 头只来自插件生成值与可信宿主上下文，不接受客户端伪造；缺少 session 来源时不伪造关联。设备令牌没有实现，不能把固定身份或静态源码分析等同于完整桌面网络指纹。

官方 bundle 的模型客户端默认超时为 1200 秒，可由正整数 `CODEBUDDY_FIRST_TOKEN_TIMEOUT_MS` 覆盖；这与 HTTP `X-Stainless-Timeout` 是两件事：内嵌 SDK 仅在调用级选项显式提供有效 timeout 时生成该头，不能从客户端默认值推断固定请求头。两区现有冷连接采集未观察到该头，也不代表所有调用均不会发送。

## 官方来源与证据边界

- 腾讯云的 [WorkBuddy 产品页](https://cloud.tencent.com/act/pro/workbuddy) 将 WorkBuddy 描述为办公 AI Agent，并将 WorkBuddy 与 CodeBuddy 分别列为支持的工具；本插件目标是 **WorkBuddy 桌面客户端**，不是 CodeBuddy CLI。
- 官方 [WorkBuddy 历史版本下载](https://www.workbuddy.cn/docs/workbuddy/Download-History) 列出 Windows x64、macOS Apple 芯片及 Intel 安装包；其文件名后缀是发行标识，不应当作校验和。
- 国内 macOS ARM64 `5.9.3.42398134` ZIP 实测 SHA-256 `55d73c63e44be58e31411254d10ad141faef9fd9eaf3cab67685d81f88e14835`，与[官方更新信息](https://www.workbuddy.cn/v2/update?platform=workbuddy-darwin-arm64)一致；包内 CLI 为 `2.165.0`，Electron 为 `37.10.3`。这是静态包证据，不是运行时 TLS 捕获。
- 国际 macOS `5.9.4.42593613` ZIP 实测 SHA-256 `754926260c0ab867b5fe1050ab5cc01724a1330451617c8f339c679ff3bbc542`，与[官方更新信息](https://www.workbuddy.ai/v2/update?platform=workbuddy-darwin-arm64)中的 `6502dc77a0d931dc5b6c490dea20b5966bab0c5c64fea65938ecb15e4a5fbea6` 不匹配；原因未知，不能将该 macOS 包视为通过官方摘要验证；国际身份依据下述经签名验证的 Windows 包更新，而非此 macOS 包。
- 国际 Windows 官方 `5.9.4` 安装包的更新 API 摘要为空；一次下载得到本地 SHA-256 `6ff4be433d664ad14c88210b5b82ebb3674d7047aab8ae52459006c587be2ee0`，并非与官方摘要匹配。`osslsigncode` 验证签名链、时间戳以及签名者和时间戳颁发机构 CRL 成功，签名主体为 Tencent Technology (Shenzhen) Company Limited，根为 DigiCert Trusted Root G4。国内包同主体、中间和根证书，但叶证书及公钥不同，不能称为相同证书 pin。包内 CLI 为 `2.165.0`。
- 国内旧版 Linux x64 DEB `5.6.2.40046327` 实测 SHA-256 `12149ad8c0cd748fd5062d8f62c7827d688d8095427ada99b1ae9e7019897523`，与[官方更新信息](https://www.workbuddy.cn/v2/update?platform=workbuddy-linux-x64-deb)一致。包内静态文件有 Electron `37.10.3`、Node `22.22.2`；不证明运行时使用该 Node，也不证明最新版本网络行为。不能以 CodeBuddy TLS 形态代替 WorkBuddy 捕获。
- Windows 官方 WorkBuddy `5.9.3` bundle 的 Electron Node 经 Wine 在隔离网络和临时 CA 下，以合成 ACP 和主提示发起 `/v2/chat/completions`，捕获同 socket ClientHello 及脱敏字段。禁用服务端 TLS ticket 并提供桌面 `CLIENT_INFO` 的冷连接采集里，第 4 个请求包含 21 个工具、Agent purpose `conversation` / type `main`，是主推理；其 JA3 为 `71dc8c533dd919ae9f4963224a4ba8fd`，JA4 为 `t13d181000_5d04281c6031_78e6aca7449b`。第 2 个请求没有工具，不能据此断定为主推理。另一次补齐桌面 `CLIENT_INFO` 的合成提示产生两个推理请求，观察到 UA `WorkBuddy/5.9.3 WorkBuddy/5.9.3 CLI/2.165.0` 和 gzip；其 TLS 为 PSK session resumption，JA3 为 `d76a5a80b4bb0c75ac45782b0b53da91`、JA4 为 `t13d181100_5d04281c6031_d5fe2c511efa`，不能与新连接样本混同。这些是 bundle 代码的隔离采集，不是官方桌面 GUI 运行，也不证明真实上游请求成功或其他平台一致。
- 国际官方签名 Windows `5.9.4` bundle 在隔离环境、临时 CA 和合成提示下的冷连接采集中，第 4 个请求为 22 个工具、Agent type `main` 的推理；JA3 `71dc8c533dd919ae9f4963224a4ba8fd`、JA4 `t13d181000_5d04281c6031_78e6aca7449b`，与上述国内冷连接样本一致。观察到 UA `WorkBuddy/5.9.4 workbuddy-ai/5.9.4 CLI/2.165.0` 和 gzip；未请求生产上游，不证明桌面 GUI 或真实账号行为。
- 仓库历史记录曾对照国内 WorkBuddy `5.8.3`、国际 WorkBuddy AI `5.8.4` 及各自内嵌 CLI `2.161.0`；例如国际 [5.8.4 安装包](https://download.codebuddy.ai/workbuddy/saas/darwin-arm64/WorkBuddy-darwin-arm64-5.8.4.41366630-73952edb.zip) 文件名标识为 `41366630-73952edb`。这些是历史源码分析的版本身份，不是本次新采集或当前最新版本证明，文件哈希未在此确认。独立 npm CodeBuddy CLI 的版本和网络轨迹不能替代桌面客户端证据。
- [官方生图研究](research/workbuddy-image-official.md) 区分 WorkBuddy 产品功能与 CodeBuddy CLI 文档；[CLI 源码研究](research/workbuddy-image-cli.md) 仅是特定 CodeBuddy 发行物的历史证据，不证明 WorkBuddy 桌面请求或账号权限。这些研究和客户端隔离采集不使用生产凭据；插件真实调用与它们分开验证。

### 2026-11-09 插件真实调用

使用获授权来源中的既有凭据，经独立 Core、实际 Wasm 和正常 TLS 校验执行；来源数据库只读，不刷新令牌或修改真实到期时间。CN 连接成功发现 17 项模型、查询额度、返回推理文本，并使用真实发现确认的图片模型返回一个图片产物。两条 Intl 连接均成功发现 21 项模型并查询额度，但推理分别返回上游 429、400；没有把它们标为对齐成功，也没有重试限流响应。桌面身份、字段顺序和 TLS 有独立原生捕获证据，但上述结果仍不证明所有模型、账号或桌面 GUI 场景。

随后固定免费模型 `hy4free` 的验证发现：隔离运行器漏传正式 Model Turn 的 `session_affinity`，插件因此不发送 `X-Conversation-ID`。补齐真实 Generation Chain 的会话身份后，两条 Intl 连接均返回 HTTP 200 并完成文本解码；自动付费切换关闭，未刷新凭据或写回来源。这修正了验证器造成的失败，不能把此前 400 归因为传输库迁移，也不表示完整官方 Agent 流程已逐字节对齐。插件自有请求元数据的修正已通过 39 项单元测试和 16 项实际 Wasm 契约测试；使用修正后的组件仍须重新构建并导入，不会自动替换已安装版本。

## Windows / macOS / Linux 客户端采集

只有在获得明确授权并使用隔离测试环境时才采集。需要对应平台的真实 WorkBuddy 桌面应用、可交互桌面和一次性合成资料；Windows 选 x64，macOS 按 Apple 芯片或 Intel 选择官方安装包，Linux 使用已安装的 WorkBuddy 目录。官方历史下载归档列出 Windows 和 macOS；上述旧版 Linux DEB 另由官方更新元数据确认。记录产品版本、完整安装包文件名、平台架构及自行计算的 SHA-256，分别标明官方来源和本地观察。

共享宿主的 `capture-client` 是 loopback TLS/HTTP 观察器；需要独立隔离网络、使桌面应用实际连接观察器并在测试环境信任其临时 CA。不得拦截无关应用、使用生产凭据或提交 token、Cookie、私钥及未脱敏流量。结束后撤销临时信任并清理捕获资料。`scripts/vendor_alignment.py` 的 `--host-repo`（或 `STRAVIA_PLATFORM_DIR`）指定包含共享入口及实际 Core 的宿主仓库，默认使用相邻 `StraviaPlatform`。运行 `--help` 查看共享入口参数，需 Python 3，Windows 可用 `python scripts/vendor_alignment.py ...`。

`scripts/capture_workbuddy.py --client /path/to/WorkBuddy.app version` 可读取客户端版本（Windows 上可指向 `.exe`；也可在 Linux 等系统指向包含 `resources/app.asar` 的 Linux 安装目录或已解包 Windows 应用目录，静态读取包元数据）。旧版 Linux 包静态读取到 `5.6.2`，不证明客户端运行或采集成功。`capture` 子命令需要已编译的 `--capture-tool`、临时 `--ca`、`--output` 和 `--isolated-client`，可用 `--help` 查看参数。它只启动本机 loopback 观察器，**不会**设置或验证网络隔离、流量重定向、CA 信任或资料是否为合成；`--isolated-client` 只是操作者确认，不能强制这些保护。操作者须事先阻断外网、使用一次性合成资料、将真实桌面请求重定向至观察器并信任临时 CA，且在桌面客户端所在的同一操作系统采集。解包目录的静态版本读取不代表 Wine 采集方案已经验证。除上述 Wine 隔离 bundle 采集外，本次没有官方桌面 GUI 的 Windows/macOS 采集结果；静态资料和 Linux CLI 轨迹不能证明桌面 GUI 的 TLS、HTTP 或认证行为。

`scripts/capture_official_wine.py` 为 Linux 上的 Windows 实包 CLI 提供独立实验入口，不启动桌面 UI。其参数 `--region cn|intl` 选择国内或国际官方域名（默认 `cn`），脚本校验随包产品配置与区域一致并选择随包默认模型；`--application` 指向解包应用目录，`--exe` 和 `--entry` 是该目录内的相对路径，`--wine` 指向 Wine 程序，`--wineserver` 可覆盖同目录默认程序，`--output` 指定脱敏 JSON，`--temp-parent`、`--port` 和 `--timeout` 可选；`--cold-capture` 禁用服务端 TLS 会话票据，默认行为不变。依赖 `unshare` 用户、挂载及网络命名空间、`ip`、`mount`、`openssl` 和 Wine；不满足隔离条件时失败。入口仅使用临时 HOME、Wine 前缀及合成身份，从应用 ASAR 和随包 CLI 的 `package.json` 静态读取版本；桌面 `CLIENT_INFO_PRODUCT_NAME` 按桌面资源解析规则读取，未找到时回退 `WorkBuddy`，不将 `cli/product.json` 的展示名当成桌面资源。CLI 的区域域名、应用名及默认模型另从 `cli/product.json` 校验读取。在隔离网络中将官方域名映射到 loopback，使用匹配域名的临时 CA 和正常 TLS 校验，向本地模拟服务发送 ACP 合成提示；不转发真实上游。报告只保存脱敏的 TLS、请求头和结构信息，以及可识别的启动错误类别和提前退出码；头字段名按官方源码公开白名单保留，动态身份与凭据值仍脱敏。历史报告中 `<other>` 不可逆，不能据新版白名单推断实际字段；不会保存客户端原始输出。仅合成隔离环境排障时，可显式传入 `--diagnostic-log` 指向不存在的文件，脚本以 `0600` 独占创建、最多写入 256 KiB 过滤后的启动日志，不写入报告；敏感行、引用内容和路径会遮蔽，但过滤并非通用脱敏保证，日志应视作私密资料、人工检查后删除，不能提交。未观察到带合成提示的推理请求即标记失败。隔离和 Wine 行为仍取决于本机内核及运行环境，脚本并非通用恶意代码沙箱；只可用于可信官方实包，不使用生产账户或凭据。上述成功的隔离 Wine 采集是 bundle CLI 的证据，不能当作桌面 UI 的直接证据。


## 本地开发

推荐使用 Nix 与 devenv。`devenv.yaml`、`devenv.lock` 沿用相邻 `StraviaPlatform` 的输入和锁定版本；Rust 版本、开发组件及 Wasm target 统一由 `rust-toolchain.toml` 声明。环境提供 Rust、Clang（Linux 使用 Mold）、Task、Python 3、CMake、pkg-config、OpenSSL 和 Minisign，不启动数据库或前端服务。构建组件不依赖本地宿主源码；`scripts/vendor_alignment.py`、`verify-shape` 等流程仍须自行提供宿主 checkout 或匹配的工具。签名目录打包还须提供上文的 `STRAVIA_TOOLS`、宿主范围和独立签名密钥；devenv 不会自动检出或修改宿主。

```bash
devenv shell
# 可选：配置好 direnv 后执行 direnv allow，由 .envrc 自动加载环境
```

进入环境后执行下列命令；配置好上文的目录工具和签名环境后，也可执行 `devenv shell -- task dist`。不用 Nix 时，须自行安装 `rust-toolchain.toml` 指定的工具链及 Task、Python 3.11+、Minisign 等所需工具。

```bash
cargo build --locked --release --lib --target wasm32-wasip2
# target/wasm32-wasip2/release/stravia_vendor_workbuddy.wasm

# 按上文配置共享工具、宿主范围与独立测试密钥，生成签名目录
task dist
# 组件契约测试加载 release Wasm，须先构建
task test
# 显式指定与宿主同版本的已构建工具
STRAVIA_TOOLS=/absolute/path/to/stravia-tools task verify-shape
```

`verify-shape` 构建组件并通过通用工具在本地合成 ClientHello，与插件内 `tests/fixtures/workbuddy-windows-main.json` 的冷连接采集比较；不会连接上游。维护 shape 时同时保留 `assets/workbuddy.json` 和 fixture 的采集来源；重新采集必须显式提供 `--output`，检查脱敏后再更新 fixture。`task verify` 同时运行组件测试和该离线验证，因此同样需要匹配的 `STRAVIA_TOOLS`。

宿主工具的 `--help` 必须列出 `verify-egress-shape`；旧二进制即使位于当前源码的 `target/` 下也不代表兼容，需要从该宿主源码重新构建。`tests/test_response_code_map.py` 是附加的离线回归，须将 `CORE_RUNNER` 指向包含 `plugin::execution::alignment_tests::response_code_map_probe` 的已构建 Core 单元测试可执行文件，再运行 `python3 -m unittest discover -s tests -p test_response_code_map.py -v`；未设置运行器时这两项测试会跳过，不计作验证通过。

`task dist` 的资产与签名配置见上文；本地验签可执行 `minisign -V -p dist/stravia-plugin-catalog.pub -m dist/stravia-plugin-catalog.json`，首次信任仍须独立核对完整公钥指纹。`dist/`、`target/`、devenv/direnv 状态和 Python 缓存不提交，共享环境配置、锁文件、采集脚本和脱敏 fixture 需要提交。不要提交 `.env` 或真实凭据；可提交不含秘密的 `.env.example`。

升级宿主不会替换已安装的独立组件；须重新安装。历史 `v0.4.0` 插件附件的 SHA-256 为 `0c2d7982eaa9f7dee7e67541ff96150574515af5d7d2bd604f9dc449c23405bd`；它不是 WorkBuddy 安装包或 `v0.5.0` 的哈希。新版压缩与组件摘要以验签后的插件目录为准，历史 Release 的 `SHA256SUMS` 仅适用于其旧附件。

## 变更记录

### 0.5.0（2026-11-13）

- Stravia SDK、运行时与协议依赖固定到上游 `v0.7.0` Release tag，构建组件不再依赖本地宿主目录。
- 对齐 Stravia `v0.7.0` 的传输与状态契约，组件声明独立 WorkBuddy egress shape；更新国内、国际客户端身份与冷连接 fixture。
- 接入插件市场 v2：通过固定 Platform tag 的共享工具生成单 frame Zstd 与双摘要目录，使用独立 Minisign 密钥签名并分发公钥，不再发布原始 Wasm 与 `SHA256SUMS`。
- 发布工作流要求显式宿主兼容范围和签名配置；创建 draft 后下载校验精确资产、签名与摘要，成功才公开，已有 Release 不覆盖。支持按现有标签手动触发。
- 本地开发环境增加 Minisign；打包在验签成功后清理旧原始组件与校验清单，保留其他用户文件。

## License

[MIT](LICENSE) · Copyright (c) 2026 Chikage0o0。第三方组件声明见 [NOTICE](NOTICE)。
