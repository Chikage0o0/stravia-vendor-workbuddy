# WorkBuddy / CodeBuddy 官方 CLI 生图能力调查

调查日期：2026-10-18。范围：腾讯官方 npm 包 `@tencent-ai/codebuddy-code` 的发布元数据和实际发行代码；没有使用登录凭据，没有执行 CLI 或发起生图请求。

**历史范围说明（2026-11-09）：** 此文件记录的是 CodeBuddy CLI，不是 WorkBuddy 桌面安装包；其模型、endpoint、请求身份和行为不能直接外推到 WorkBuddy。下文对 README 和插件功能的描述是调查当时的快照，不代表当前实现。后续恢复 WorkBuddy 身份时没有重新执行本文件所述网络查询或生图。

## 结论

**官方 CLI 发行物中确实有独立的 AI 生图工具 `ImageGen`、图生图编辑分支和具体默认模型 ID，不只是图片输入识别。** README 所参考的 `2.161.2` 已包含该实现；调查时 npm `latest` 为 `2.168.2`，对应关键实现仍在。

可以源码确认的默认 ID 是文生图 `hunyuan-image-v3.0`、图生图 `hunyuan-image-v2.0-general-edit`。它们是客户端在没有其他配置时选择的 fallback，**不等于当前用户账号已经开通、配置接口一定下发、模型一定在线或桌面所有地区都可用**。

CLI 通过产品配置提供的 endpoint 调用 `POST /v2/images/generations` 或 `POST /v2/images/edits`。这是单独的图片服务调用，不是 chat-completions 中把图片当输入。代码具有 OpenAI 风格的字段和响应内容，但不能据此宣称存在可直接使用的公开 OpenAI `/v1/images/*` API。

## 一手来源与固定版本

- [npm latest 元数据](https://registry.npmjs.org/@tencent-ai%2fcodebuddy-code/latest)：本次实读 `version: 2.168.2`，`homepage: https://cnb.cool/codebuddy/codebuddy-code`，发行包地址为 `codebuddy-code-2.168.2.tgz`。
- [2.161.2 固定元数据](https://registry.npmjs.org/@tencent-ai%2fcodebuddy-code/2.161.2)。
- [2.161.2 官方 registry tarball](https://registry.npmjs.org/@tencent-ai/codebuddy-code/-/codebuddy-code-2.161.2.tgz)。本次下载至 `/tmp`，SHA-1 为 `42e099a1aede57d3985edada30831550fc97ebec`，与 registry `dist.shasum` 一致。
- [2.161.2 固定 bundle](https://unpkg.com/@tencent-ai/codebuddy-code@2.161.2/dist-server/codebuddy.js)。本次确认其字节与上述官方 tarball 的 `package/dist-server/codebuddy.js` 一致；SHA-256 为 `2b807198583c3f1d8a3f88cfb9e8e2c506acc7d9291101e02defa84d1328aec6`。
- [2.168.2 固定 bundle](https://unpkg.com/@tencent-ai/codebuddy-code@2.168.2/dist-server/codebuddy.js)。本次直接读取该固定版本并对照关键实现，没有下载最新版本整个 tarball。

下文位置为 bundle 解码为 UTF-8 字符串后的 **0 起算字符偏移**，不是字节偏移。bundle 高度压缩，附搜索锚点比仅提供行号更容易复核。

## 工具、注册与开关

`2.161.2`：

| 证据位置 | 事实 / 精读片段 |
| --- | --- |
| 6031202 附近，搜索 `IMAGE_GEN="ImageGen"` | 正式工具名为 `ImageGen`。 |
| 3655800–3656850，搜索 `Generate images from text descriptions using AI models.` | 工具 constructor 设置 `this.name=rT.L3.IMAGE_GEN`；参数由图片生成 schema 扩展。 |
| 3656450 起，搜索 `this.isEnabled=async` | 若 `CODEBUDDY_IMAGE_GEN_ENABLED` 存在，仅值 `true` 或 `1` 启用，否则禁用；未设置时要求 `productFeatures.ImageGen`，并要求配置中至少有 `text-to-image` 或 `image-to-image` 标签模型。 |
| 3659250 附近，搜索 `o9=image_gen_tool_decorate` | 该实现通过 `Component(rs.NL)` 绑定至工具组件集合，不是仅文档字符串。 |
| 1872250 起，搜索 `/model:text-to-image` | 模型选择命令的显示由相同环境开关或 `productFeatures.ImageGen` 控制。注意命令显示条件不等于工具全部可用条件。 |
| 1956612–1958845，搜索 `getTextToImageModels` | `/model:text-to-image list` 列出配置中标签匹配的模型；按 ID 或名字选择；disabled 模型拒绝切换；成功后保存 `textToImageModel`。 |

生成工具的 schema 包括 `prompt`、`size`、`quality`、`style`、`background`、`footnote`、`revise`、`output_dir`，并扩展 `image1`、`image2`、`image3`、`input_fidelity`。没有输入图片时调用 `generateImage`；有图片时走 `editImage`，且再次要求产品配置包含 `image-to-image` 模型（3657100 起）。生成结果有 `type:"image_gen_tool_result"`、`status:"completed"` 和本地保存路径，默认目录是工作区 `generated-images`。这些是明确的图片输出实现。

相关类型声明也随官方 tarball 发布：`dist-server/types/node/tools/image-gen-tool.d.ts`、`image-edit-tool.d.ts`、`image-service.d.ts`、`command/model-text-to-image-command-executor.d.ts`。仅存在 `ImageEdit` 枚举或声明不应单独被当成另一个工具已启用的证明；本调查确认执行的图生图路径是 `ImageGen` 的图片输入分支。

## 模型来源和选择顺序

`2.161.2` 偏移 3360383 起的 `async resolveModel(t,i)`：

```js
c = s ? "textToImageModel" : "imageToImageModel"
h = s ? "hunyuan-image-v3.0" : "hunyuan-image-v2.0-general-edit"
S = t?.options?.[c]
```

源码优先级是：

1. 会话 `context.options.textToImageModel` / `imageToImageModel`；
2. 环境变量 `CODEBUDDY_TEXT_TO_IMAGE_MODEL` / `CODEBUDDY_IMAGE_TO_IMAGE_MODEL`；
3. 合并 settings 中对应设置；
4. 产品配置 `models` 中第一个带 `text-to-image` / `image-to-image` 标签的模型 ID；
5. 上述 Hunyuan fallback ID。

环境变量导出映射可在 3049920、3050119、3050157 附近定位；实际字符串集中在 3052994 附近。标签常量在 3912250 附近。

`buildBaseRequestBody`（3359050–3360000）按 ID 前缀区分 `hunyuan-`、`gemini-` 与其余 `openai` 分支。**存在 Gemini/OpenAI 分支只说明代码适配这些请求形状，不证明当前产品实际提供某一个 Gemini/OpenAI 生图模型。** 本次未获得账号级可用模型清单，不猜测其他模型 ID。

## 接口路径、鉴权与 OpenAI 兼容边界

`2.161.2` 偏移 3354500–3355800：

```js
p = `${s}/v2/images/generations` // generateImage
p = `${s}/v2/images/edits`       // editImage
method: "POST"
headers: { "Content-Type": "application/json", Authorization: `Bearer ${n.accessToken}`, ... }
```

`prepareRequest`（3358900–3359400）从运行时上下文 auth 或当前登录 session 取得 token；没有 token 抛出 `Authentication required. Please login first.`；endpoint 取 `(await productManager.waitConfiguration()).endpoint`，未配置时抛错。本调查没有读取或使用 token。

请求 body 基础字段为 `model`、`prompt`、`size`（默认 `1024x1024`）。非 Hunyuan/Gemini 分支增加 `response_format:"b64_json"`，可带 `quality`、`style`、`background` 等；Hunyuan 分支支持 `footnote`、`revise:{value:...}`。编辑接口也是 JSON，图片被转为 data URL 数组后置于 `image`，不是这里直接提交 OpenAI multipart 表单。

`processResponse`（3359900–3360383）要求业务包装 `code === 0`，再读取外层 `data` 中的 `data` 图片数组，并可读取 `usage.credit`。图片内容接受 `b64_json` 或 `url`（3358100 附近）。因此：

- **已证实**：客户端专用图片工具可向产品后端图片路径发送带模型 ID 的请求。
- **未证实**：这是面向第三方的公开开发者 API；普通 OpenAI API key 能用；可直接把 OpenAI SDK base URL 指向该站；存在 `/v1/images/generations`；能从本仓库的 chat 兼容端点直接生成图片。
- **不能等同**：源码日志中的 `openai` 分支与完整 OpenAI Images API 兼容承诺。业务包装、路径、鉴权和编辑 body 均需单独适配。

## 当前发行版对照

`2.168.2` 固定 bundle 仍包含：

- `ImageGen` 枚举：6913590 附近；
- 工具开关与标签条件：3714125–3714650；
- `/v2/images/generations`：3391624；`/v2/images/edits`：3392111；
- `prepareRequest` 登录和配置 endpoint：3397357；
- `buildBaseRequestBody` 三类 ID 前缀分支：3397764；
- `resolveModel` 相同优先级与两个 Hunyuan fallback：3398756 起，默认文生图 ID 在 3398976。

这证明旧参考版本的生图代码并非仅历史残留已被当前版删除；不构成服务可用性测试。

## 与本地插件、桌面、账号的边界

以下本地及匿名接口证据由主调查代理实读后提供，本文件作者未重复读取或重放请求：

- `src/lib.rs:17` 的协议仅 `openai-compatible/chat-completions/v1`；`src/inference.rs:25` 的上游仅 `/v2/chat/completions`。
- `src/models.rs:413–421` 的 `supportsImages` 映射为图片 **输入**，输出仍为 `text`；不能把这个字段当成生图输出支持。
- 主调查代理匿名读取 [国内配置](https://copilot.tencent.com/v3/config) 与 [国际配置](https://www.workbuddy.ai/v3/config)，均返回 `code:0`，但 `data.models:null`、`data.agent.agents:null`。这无法确认登录账号的模型清单，更不能推导“账号没有生图模型”。

所以，**当前本地插件未接通 images 调用路径，不等于腾讯客户端没有生图工具；CLI 有源码实现，也不单独证明某个桌面版本、地区或账号已经启用。** 此次没有执行实际生图，不判断额度、价格、分辨率上限、服务端当前成功率或账户权限。
