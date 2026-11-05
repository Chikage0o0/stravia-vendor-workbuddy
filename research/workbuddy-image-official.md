# WorkBuddy / CodeBuddy 官方生图能力调查

调查日期：2026-10-18。仅检索、阅读官方公开产品文档和更新说明；未登录、未读取凭据、未实际生成图片或触发计费。所读文档页面标注构建日期为 2026-10-16。下列模型名称保留官方原文，不等同于已确认的 API model ID。

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
