# Stravia Vendor WorkBuddy

把 **WorkBuddy / CodeBuddy（腾讯代码助手）** 账号接入 [Stravia](https://github.com/Stravia-AI/StraviaPlatform) 的 wasm 供应商插件 —— 你在 WorkBuddy 里能用的模型（DeepSeek、Kimi、GLM、混元……），经由 Stravia 统一网关以 **OpenAI 兼容接口**提供给任意客户端（Codex CLI、Claude Code、Cherry Studio、Open WebUI……）。

> ⚠️ 本插件不是腾讯 WorkBuddy 官方发布的插件，上游服务、产品名称及商标归各自权利人所有。上游接口属非公开协议，无稳定性承诺，仅供个人学习研究，请自担风险。

## 特性一览

- 🔐 **浏览器授权登录** — 打开授权链接完成 QQ / 微信 / 手机号登录，无需手动复制 token；令牌自动刷新与撤销
- 🌐 **双区域隔离** — 国内版（`copilot.tencent.com`）与国际版（`www.workbuddy.ai`）两个 channel，凭据与流量严格限定在所选区域
- 🤖 **模型自动发现** — 从账号 `/v3/config` 的默认 agent 名单实时发现模型，按上游配置合并免费／付费线路；可选 `model_ids` 白名单只同步指定模型
- **可选自动付费线路** — 默认关闭；开启后可在免费线路返回 `6004` 频控错误、且尚未输出响应时自动切换付费线路并消耗积分
- 📊 **额度查询** — 读取账号积分资源包余量（国内版走独立计费域 `codebuddy.cn`）
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

依赖：Rust `1.98.1`（`rust-toolchain.toml` 已锁定）+ `wasm32-wasip2` target；可选安装 [Task](https://taskfile.dev)。

```bash
# 仅构建组件
cargo build --locked --release --lib --target wasm32-wasip2
# 产物：target/wasm32-wasip2/release/stravia_vendor_workbuddy.wasm

# 或使用 Task：构建 + 打包 dist/（组件、许可证与 SHA256SUMS）
task dist

# 单元测试与组件契约验证（契约测试加载 release wasm，须先构建）
task test
```

## 发布流程（维护者）

版本号以 `Cargo.toml` 的 `version` 为准。打 tag 即触发 GitHub Action 构建并发布 Release：

```bash
git tag v0.1.2
git push origin v0.1.2
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
