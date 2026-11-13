# WorkBuddy / CodeBuddy 官方生图能力调查

调查日期：2026-10-18。仅检索、阅读官方公开产品文档和更新说明；未登录、未读取凭据、未实际生成图片或触发计费。所读文档页面标注构建日期为 2026-10-16。下列模型名称保留官方原文，不等同于已确认的 API model ID。

## 桌面发行物补充（2026-11-09）

[腾讯云 WorkBuddy 产品页](https://cloud.tencent.com/act/pro/workbuddy) 将 WorkBuddy 与 CodeBuddy 分别列为产品；[WorkBuddy 官方历史下载页](https://www.workbuddy.cn/docs/workbuddy/Download-History) 列出国内 SaaS Windows x64、macOS Apple 芯片和 Intel 安装包，未据此证明 Linux 桌面版。该页列 `v5.9.3` 发布于 2026-10-27，三个平台的文件名均带 `5.9.3.42398134-d4692421`。这是文件名中的发行标识，不可称为 SHA-256；该归档页面未列文件 SHA-256，以下 ZIP 校验来自另行核验的更新元数据和发行包。

后续静态发行包核验：国内 macOS ARM64 `5.9.3.42398134` ZIP 的 SHA-256 为 `55d73c63e44be58e31411254d10ad141faef9fd9eaf3cab67685d81f88e14835`，与[官方更新元数据](https://www.workbuddy.cn/v2/update?platform=workbuddy-darwin-arm64)一致；内嵌 CLI `2.165.0`、Electron `37.10.3`。国际 macOS `5.9.4.42593613` ZIP 的实测 SHA-256 为 `754926260c0ab867b5fe1050ab5cc01724a1330451617c8f339c679ff3bbc542`，与[官方元数据](https://www.workbuddy.ai/v2/update?platform=workbuddy-darwin-arm64)的 `6502dc77a0d931dc5b6c490dea20b5966bab0c5c64fea65938ecb15e4a5fbea6` 不匹配，原因未知，不能视为摘要验证通过。国内旧版 Linux x64 DEB `5.6.2.40046327` 的 SHA-256 `12149ad8c0cd748fd5062d8f62c7827d688d8095427ada99b1ae9e7019897523` 与[官方元数据](https://www.workbuddy.cn/v2/update?platform=workbuddy-linux-x64-deb)一致；其静态文件包含 Electron `37.10.3`、Node `22.22.2`，但没有证明运行时 Node 版本或当前网络行为。上述发行包的静态核验本身未捕获桌面 TLS/ClientHello。另一次隔离采集使用 Windows 官方 WorkBuddy `5.9.3` bundle 的 Electron Node，经 Wine、临时 CA 和合成 ACP 请求捕获同 socket ClientHello；不提供桌面 `CLIENT_INFO` 的新连接 JA3 `71dc8c533dd919ae9f4963224a4ba8fd`、JA4 `t13d181000_5d04281c6031_78e6aca7449b`。补齐桌面 `CLIENT_INFO` 并禁用服务端 ticket 的冷连接采集中，第 4 个请求带 21 个工具与 Agent purpose `conversation` / type `main`，为主推理，JA3 / JA4 与该新连接样本相同；第 2 个请求无工具，不能据此断定为主推理。补齐桌面 `CLIENT_INFO` 的另一次采集产生两个推理请求，UA 为 `WorkBuddy/5.9.3 WorkBuddy/5.9.3 CLI/2.165.0`，使用 gzip；PSK session resumption 的 JA3 为 `d76a5a80b4bb0c75ac45782b0b53da91`、JA4 为 `t13d181100_5d04281c6031_d5fe2c511efa`，与新连接样本不可混同。这不等于官方桌面 GUI 运行或真实上游成功。

两区 fresh 冷连接主推理再次采集均观察到 `Accept-Encoding: gzip, compress, deflate, br`，顺序位于 `Content-Length` 后、`Host` 前。插件声明该值并请求宿主 gzip/deflate/br 解码；Net 1.1 不支持 `compress`，服务端选用时明确失败，不代表完整支持该列表。国际桌面的 `productName` 按打包 resolver fallback 为 `WorkBuddy`，UA 的 `applicationName` 为 `workbuddy-ai`，不应以 CLI 展示名代替。

### 内嵌 HTTP 响应解码源码边界

国内 Windows `5.9.3` 和国际 Windows `5.9.4` 安装包的 `resources/app.asar.unpacked/cli/dist/codebuddy-lite-wb.mjs` 含相同 Axios HTTP 适配器实现；以下偏移是 UTF-8 解码后的零起算字符位置，不是字节偏移。约 `9654080` 构造 `gzip, compress, deflate`，Brotli 可用时加 `br`；约 `9665895` 的响应 `content-encoding` switch 将 `gzip`、`x-gzip`、`compress`、`x-compress` 都送入 `eZ.createUnzip(ta)`，`deflate` 先经 `new te` 再送入 `createUnzip`，`br` 则在可用时走 `createBrotliDecompress(ts)`。这些是适配器的静态分支，不代表所有客户端请求都必经该路径。

[Node.js v22 zlib 文档](https://nodejs.org/docs/latest-v22.x/api/zlib.html#compressing-http-requests-and-responses)将 `createUnzip()` 描述为处理 gzip 和 deflate；该 API 未声明支持 Unix compress/LZW。因此 `compress` / `x-compress` 名称被 switch 接受，不能证明真正 LZW 流可被解码；也不能由此声称插件 Net 1.1 支持它。插件目前对服务端选择的 `compress` 明确失败，gzip、deflate、br 交由宿主处理。本次没有向官方客户端发送压缩响应验证具体运行时错误或端到端行为。

国际 Windows 官方 `5.9.4` 安装包的更新 API 摘要为空，一次下载的本地 SHA-256 为 `6ff4be433d664ad14c88210b5b82ebb3674d7047aab8ae52459006c587be2ee0`，不能称为官方摘要匹配。`osslsigncode` 验证签名链、时间戳及签名者和 TSA CRL 成功；签名主体 Tencent Technology (Shenzhen) Company Limited，根 DigiCert Trusted Root G4。国内包同主体、中间和根，但叶证书及公钥不同，不是相同证书 pin；该国际包内 CLI 为 `2.165.0`。

仓库历史资料曾对照国内 WorkBuddy `5.8.3` 与国际 WorkBuddy AI `5.8.4`，均记载内嵌 CLI `2.161.0`；国际 [5.8.4 包](https://download.codebuddy.ai/workbuddy/saas/darwin-arm64/WorkBuddy-darwin-arm64-5.8.4.41366630-73952edb.zip) 的文件名标识为 `41366630-73952edb`。这是历史记录而非本次实测，文件 SHA-256 未确认。官方归档的新版本、历史桌面版本与独立 CodeBuddy npm CLI 不是同一版本轨迹；不从产品页面推断桌面请求线规、账号模型或授权兼容性。

## 结论

1. **有产品级 AI 生图能力，不只是图片输入。** 国内 WorkBuddy《设计创意（WorkBuddy × Ardot）》明确提供“AI 生图”素材处理选项，可根据场景自动生成图片；同时支持海报、Banner、Logo、PPT 等设计稿。注意：设计稿生成、画布修改和栅格图像生成不是同一个概念，本结论依据明确的“AI 生图”选项，而不是单凭“海报”营销描述。[设计创意官方教程](https://www.workbuddy.cn/docs/workbuddy/From-Beginner-to-Expert-Guide/Function-Description/Design-Idea)
2. **官方确实命名过生图模型，但证据属于 CodeBuddy Code 的更新说明，不是某个 WorkBuddy 账号当前模型清单。** v2.52.3 国内、国际公告均列出 **Gemini-3.0-Pro-Image、Gemini-3.1-Flash-Image、Gemini-2.5-Flash-Image**，明确支持文生图和图生图。不能据此断言这三款在所有地区、套餐或 WorkBuddy 桌面账号当前均可用，也不能将这些展示名直接用作调用 ID。[国内公告](https://www.codebuddy.cn/docs/cli/release-notes/v2.52.3) · [国际公告](https://www.workbuddy.ai/docs/cli/release-notes/v2.52.3)
3. **WorkBuddy 具备内置 ImageGen 路径，但启用有开关限制。** v2.152.0 中、英文公告明确说明恢复 WorkBuddy 精简产物中误被排除的 `ImageGen` / `VideoGen`，此前影响 craft / plan / expert 模式的生成能力；恢复后是否真正启用仍由“产品开关和环境变量”控制。这是 WorkBuddy 本身的直接证据，不必仅靠 CLI 能力推测桌面产品支持。[国内公告](https://www.codebuddy.cn/docs/cli/release-notes/v2.152.0) · [国际公告](https://www.workbuddy.ai/docs/cli/release-notes/v2.152.0)
4. **本次未找到 WorkBuddy 对外开放生图 HTTP API 的公开承诺。** 公开工具文档与 CLI 参数是应用内功能/编程工具接口，不自动等于供本仓库插件直接调用的 OpenAI Images API。自定义模型设置说明的是接入第三方模型、默认 `/chat/completions` 路径，不是 WorkBuddy 对外提供生图服务。[工具参考](https://www.codebuddy.cn/docs/cli/tools-reference) · [模型配置](https://www.workbuddy.cn/docs/workbuddy/From-Beginner-to-Expert-Guide/Function-Description/Model)

## 可核验的具体用法与限制

### 国内 WorkBuddy：设计创意 / Ardot

官方教程给出以下步骤：[完整教程](https://www.workbuddy.cn/docs/workbuddy/From-Beginner-to-Expert-Guide/Function-Description/Design-Idea)

1. 首页 → **新建任务** → **设计创意** Tab。
2. 首次使用完成 **Ardot 授权连接**。系统通过登录手机号关联 Ardot 身份；授权范围包含读取、编辑画布与云端同步。
3. 在输入框下方“素材处理”开关选择 **AI 生图**，而不是“色块占位”。
4. 用自然语言描述需求，例如官方示例“帮我设计一张产品发布会的宣传海报，主题是 XX”。可选择设计风格，后续通过对话调整设计稿。
5. 需要精修时跳转 Ardot 浏览器编辑器；教程说明 PNG/SVG/PDF 导出能力，PPT 类产物另有 PPTX/PDF 下载入口。

**边界：** 这是腾讯自研 Ardot 设计智能体平台的官方深度集成，不应称作独立生图基础模型。教程没有披露该模式“AI 生图”背后的模型名称、调用 ID、每张价格、完整地区或套餐矩阵。上传参考图修改画布不等于已证明此模式提供某种特定的底层图生图接口。[同上](https://www.workbuddy.cn/docs/workbuddy/From-Beginner-to-Expert-Guide/Function-Description/Design-Idea)

### 内置工具与图生图

当前《工具参考》开头说明是 CodeBuddy Code **内置工具**清单，其中 `ImageGen` 明确为“根据文本描述生成图片，支持文生图和图生图”，并标注“需要权限”。`Skill`、MCP 资源工具在表中另列，因此不能将这里的 ImageGen 误称为只能靠第三方 Skill/MCP 才能获得的能力。[工具参考](https://www.codebuddy.cn/docs/cli/tools-reference)

旧版 v2.48.0 将新增图生图工具命名为 `ImageEdit`，支持风格转换、添加元素、颜色修改，使用 `/model:image-to-image` 切换，默认模型由 `CODEBUDDY_IMAGE_TO_IMAGE_MODEL` 或 `imageToImageModel` 配置。当前工具文档已将文生图和图生图都归在 `ImageGen` 下，引用旧文档时需保留版本上下文。[v2.48.0 国际公告](https://www.workbuddy.ai/docs/cli/release-notes/v2.48.0)

### 地区、账号和权限

- 国内站与国际站均存在明确的 Gemini 生图模型公告和 WorkBuddy ImageGen 恢复公告；**这证明文档两地发布，不证明两地账号实际提供完全相同模型**。[国内 v2.52.3](https://www.codebuddy.cn/docs/cli/release-notes/v2.52.3) · [国际 v2.52.3](https://www.workbuddy.ai/docs/cli/release-notes/v2.52.3)
- 国际 WorkBuddy 模型页明确：内置模型列表可能随**产品版本、账号类型、服务可用性**变化，没有在该页列出生图具体型号。[国际模型配置](https://www.workbuddy.ai/docs/workbuddy/From-Beginner-to-Expert-Guide/Function-Description/Model)
- 国内 WorkBuddy 模型页明确：企业管理员禁用的模型会隐藏或置灰。此页出现的 **Hy4 preview、Hy3** 是混元模型，但其功能标记是“图片输入 / 思考模式”，**没有据此证明混元是 WorkBuddy 当前的生图后端**。[国内模型配置](https://www.workbuddy.cn/docs/workbuddy/From-Beginner-to-Expert-Guide/Function-Description/Model)
- `ImageGen` 工具需要权限，且 WorkBuddy 是否启用受产品开关/环境变量影响；没有找到完整的个人版/企业版/地区白名单或免费额度对照表，不作“所有账号免费可用”的承诺。[工具参考](https://www.codebuddy.cn/docs/cli/tools-reference) · [WorkBuddy 开关说明](https://www.codebuddy.cn/docs/cli/release-notes/v2.152.0)

## Skills、MCP 和 API 不能混为一谈

- **产品集成**：设计创意模式中的 Ardot 画布和 AI 生图素材选项，是有具体官方教程的产品体验。[设计创意](https://www.workbuddy.cn/docs/workbuddy/From-Beginner-to-Expert-Guide/Function-Description/Design-Idea)
- **内置工具**：`ImageGen` 是工具名，不是模型名；具体是否启用另有产品开关。[工具参考](https://www.codebuddy.cn/docs/cli/tools-reference) · [v2.152.0](https://www.codebuddy.cn/docs/cli/release-notes/v2.152.0)
- **Skill / Expert 案例**：国际 Explore 文档说明会自动装载案例关联的 Prompt、Skill、Expert。出现“生图案例”并不能单独证明使用官方模型，仍须追溯具体工具和服务。[Explore](https://www.workbuddy.ai/docs/workbuddy/From-Beginner-to-Expert-Guide/Function-Description/Explore)
- **自定义第三方模型**：WorkBuddy 模型设置是消费者连接外部模型，费用由用户向第三方支付；默认 `/chat/completions`。不能将此配置页视为 WorkBuddy 生图 API 文档。[国内模型配置](https://www.workbuddy.cn/docs/workbuddy/From-Beginner-to-Expert-Guide/Function-Description/Model)
- **公开 API 状态**：本次读到的官方页面没有给出 WorkBuddy 生图的公开 endpoint、鉴权规范、请求/响应 schema 或 SLA；结论是“未证实公开 API”，不是断言不存在内部接口，也不是断言腾讯云其他产品没有生图 API。

## 面向原问题的简答

**有。WorkBuddy 官方产品里有 AI 生图，CodeBuddy 的官方公告也确实列出过三款 Gemini 生图模型；但当前 WorkBuddy 账号到底开放哪款、设计创意模式具体用了谁、能否从现有插件当作生图 API 调用，不能仅凭这些文档确认。混元的图片输入标记不能算作生图证据。**
