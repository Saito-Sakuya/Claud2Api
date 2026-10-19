# API 文档（中文）

中文 · [English](API.md) · [文档索引](DOCS_ZH.md)

Base URL 为服务根地址，例如 `http://127.0.0.1:18081`，使用 Anthropic SDK 时不要再追加 `/v1`。
主要接口为 `POST /v1/messages`，支持 JSON 与 SSE 流式响应。

## 认证与令牌

发送 `x-api-key: <API令牌>` 或 `Authorization: Bearer <API令牌>`，两者同时提供时必须一致。
可使用服务端配置的网关密钥，或在控制台「令牌管理」中创建的令牌。重复、冲突或无效认证头会被拒绝。
账号凭证只在服务端配置；API 不接受客户端传入的 `X-Claude-Cookie`。配置账号不会关闭公开接口认证。

管理页面可新增、编辑、删除令牌，设置 1–512 的令牌并发数及有效期。完整令牌只在创建时显示一次，
请立即保存；列表不会再次显示明文。到期时间可选「永不过期」，界面默认 30 天。
删除或到期后，新请求返回 401，已经接入的请求可正常完成。
令牌并发名额在整个响应传输期间持有，完成或断开后释放；超出返回 429 和 `Retry-After: 1`。

令牌限制叠加在账号池、全局调度限制上，每个账号始终最多执行 1 个请求。
环境配置中的网关密钥继续有效，不受新增令牌的单独并发限制，仍受账号和全局限制。
API 令牌不能登录管理控制台；管理密钥、Bot 只读密钥也不能代替 API 令牌。

管理页面支持编辑已创建令牌的名称、并发数和有效期，保留原密钥及会话。降低上限时已有请求正常结束，新请求按新上限接入；编辑不再次显示完整密钥。

## 模型名称与模型列表

```sh
curl http://127.0.0.1:18081/v1/models \
  -H "x-api-key: $API_TOKEN"
```

返回采用 OpenAI 风格的列表，并非原生 Anthropic 模型列表的完整 schema：

```json
{"object":"list","data":[{"id":"claude-haiku-5-5","object":"model","created":0,"owned_by":"anthropic"}]}
```

默认未启用自定义模型配置，列表展示所选账号实际支持的原生模型名。模型权限由上游决定；
示例名称不保证每个账号可用。显式查询会刷新该账号的目录，生成请求通常使用最多 5 分钟的目录缓存。
目录失败时返回上游错误，不以猜测的静态列表替代。列表来自本次选中的一个账号，
不汇总整个号池；账号权限不同可能导致返回列表不同。

| 名称形式 | 例子 | 用途 |
|---|---|---|
| 原生名称 | `claude-haiku-5-5`、`claude-sonnet-5-5` | 直接调用账号实际支持的模型 |
| `c2a-` 模型名 | `c2a-claude-haiku-5-5`、`c2a-claude-sonnet-5-5` | 在需要区分参数兼容规则的中间服务中，标识本服务的模型配置 |
| 自定义 ID | `work-haiku` | 管理员启用模型配置后发布的下游别名 |

默认模式下，`c2a-claude-*` 只剥离一次 `c2a-`，调用相同原生模型，响应的 `model` 返回原生名称。
它不增加权限、额度或模型能力，不代表原生思考或真实缓存支持；是否影响客户端的预校验由客户端实现决定。
服务端无法改变请求到达前发生的客户端拒绝。`c2a-` 模型前缀与 `c2a_tok_` API 令牌前缀是两个概念。

管理员可在「模型配置」中启用自定义列表，逐项填写下游 ID、显示名称、真实上游模型并启停条目：

- 列表仅返回已启用且真实目标存在于本次账号目录中的条目；`display_name` 用于人类阅读，请求使用 `id`。
- 所有生成接口的 JSON / SSE 中，带有 `model` 的响应对象返回选定的下游 ID。
  显式配置的 `c2a-claude-*` ID 也会原样返回。
- 先匹配精确配置的 ID，明确停用的条目直接拒绝；没有精确条目时，`c2a-claude-*` 可匹配已发布的原生 `claude-*` 条目，反向匹配不自动发生。
- `c2a-` 保留给 `claude-*` 模型名；其他别名使用其他前缀。真实上游目标填写原生名称，不支持链式映射。
- 未发布或停用的 ID 返回 400；真实权限仍会校验，不能通过重命名绕过权限。所有可用账号都不支持目标时返回 400。
- 修改只影响后续请求；执行中的响应保留开始时的 ID。保存、轮询管理配置不会查询上游。

配置接口见 [控制台接口](CONTROL_API.md)，中间服务配置说明见 [桥接兼容说明](BRIDGE_COMPATIBILITY_ZH.md)。

## SDK 示例

```python
from pathlib import Path
import time
from anthropic import Anthropic

client = Anthropic(
    api_key=Path("/private/client/api-token.txt").read_text().strip(),
    base_url="http://127.0.0.1:18081",
    max_retries=0,
)
model_id = client.models.list().data[0].id
time.sleep(5)
message = client.messages.create(
    model=model_id,
    max_tokens=1024,
    messages=[{"role": "user", "content": "Hello"}],
)
print(message.content)

time.sleep(5)
with client.messages.stream(
    model=model_id,
    max_tokens=1024,
    messages=[{"role": "user", "content": "Hello"}],
) as stream:
    for text in stream.text_stream:
        print(text, end="", flush=True)
```

示例需要 Python `anthropic` 包；文件为客户端本地 API 令牌文件，不是浏览器账号凭证。
示例按默认账号间隔等待 5 秒，已修改配置时请相应调整，并处理 429／Retry-After。

## Messages 请求

`model`、`max_tokens`（1–131072）和非空 `messages` 必填，`stream` 默认为 false。
对话必须以 user 开始、以 user 结束，不支持 assistant 预填充。
`system` 接受字符串或文本块数组；message 的 content 接受字符串或下列内容块。
各项为独立示例，实际使用仍须符合角色及工具配对约束：

```json
{"type":"text","text":"hello"}
```

```json
{"type":"image","source":{"type":"base64","media_type":"image/png","data":"<图片的 Base64>"}}
```

```json
{"type":"document","title":"notes.txt","source":{"type":"text","media_type":"text/plain","data":"文件正文"}}
```

图片支持 PNG、JPEG、GIF 和 WebP；文档支持 Base64 PDF、Base64 UTF-8 纯文本及上述文本来源。
它们可放在 user 内容或 `tool_result.content` 中，按当前账号上传并随对应轮次发送；原始文件字节不写入提示词或网关数据库。
每份文件最多 10 MiB，每次请求媒体内容合计最多 20 MiB、最多 20 个不同上传引用。
完整 HTTP 请求仍受面板“最大请求字节”限制（默认 4 MiB，最高 32 MiB），其中包括 Base64 编码的体积；超限返回 413。
远程图片／文档 URL、官方 Files API 的 `file_id` 和启用的文档 citations 尚不支持，返回明确的 400。
Chat／Responses 仍采用文本输入适配。Token 估算按文本封装计算，尚不代表附件的实际模型用量。

```json
{"type":"tool_use","id":"toolu_example","name":"lookup","input":{"key":"x"}}
```

```json
{"type":"tool_result","tool_use_id":"toolu_example","content":"result","is_error":false}
```

工具调用属于 assistant，工具结果属于随后的 user；每个调用必须有一个对应结果，结果块在普通用户文本之前。
工具结果也可以是文本、图片或文档块组成的数组。工具参数及结果中的语义数据保持原样。
工具声明需包含 name、description（可选）及对象类型的 `input_schema`，支持本地 JSON Schema 引用，
不加载远程或文件引用。`tool_choice` 支持 auto、none、any、tool（指定名称）及 `disable_parallel_tool_use`。
在 Messages 中，`tool_choice:none` 会关闭本轮所有工具（包括移除原生工具描述符）。`any`、`tool` 和
`disable_parallel_tool_use` 只约束客户端工具；它们不保证原生工具按顺序调用。自定义客户端工具由客户端执行；
调用结果放入后续 user 的 `tool_result`。模型生成的参数不合法或违背强制客户端工具选择时返回错误，不伪造结果。

客户端工具走网页真实函数／结果通道，工具名在上游使用独立别名。工具等待期间会占用该账号槽位；结果须在请求超时与 600 秒的较小值内回传（且整条上游流不超过 840 秒）。等待过期或服务重启后，旧状态不可恢复，返回 409 并需重开会话。请求或结果不匹配也返回 409，但不会消费仍有效的 pending 或回执，正确请求仍可重试。网关不会重放旧工具任务。工具结果 sidechannel 只发送客户端返回的结果，不附加网关策略或指令。

`tool_choice` 按每个 HTTP 请求分别校验。首轮 `any` 或 `tool` 只约束发出工具调用的响应；客户端用 `auto` 回传纯结果时，可继续同一上游流。后续请求若改为 `any`、`tool` 或 `none`，增加 user 文本或追加 system 指令，则使用完整已验证历史启动新的上游生成；`none` 的新回合不提供工具。工具结果含 PDF 时也会走完整历史和独立文件上传，不通过 sidechannel 发送 PDF。实测 PDF 的 sidechannel 请求返回 HTTP 400，因此该场景不能按普通文本结果同流续接。

Claude Code 会在工具结果请求中追加 system 内容，因此即使工具结果本身没有附带文本，也会触发完整历史的新生成；网关会先停止旧响应。若 fresh 请求在上游生成开始前失败，客户端可在 `min(request_timeout, 600 秒)` 内，用相同令牌、完整历史（含工具结果）、上下文和工具选择精确重试。重试回执只保留 keyed digest，不保存结果正文，最多 512 条；若停止旧响应失败，重试仍须使用原账号并先完成停止。生成开始后若中断，旧结果重试返回 409；回执过期或服务重启后状态不可恢复，旧结果返回 409 并清理。请求与仍有效回执不匹配也返回 409，但保留回执供正确请求重试。网关不会重放客户端工具。Claude Code 的每次 fresh 会增加响应耗时和账号请求次数；本轮冻结源码实测见[综测结果](TEST_SUMMARY.md)。

### Claude 网页原生工具（Claud2Api 扩展）

Messages 可在 Claude 网页会话中启用以下远端工具组：网页搜索与读取 `web_search`、Artifacts `artifacts`、
代码与文件沙箱 `repl`，以及网页信息与交互卡片 `widgets`。省略 `claud2api_tools` 时采用运行配置中已开启的
工具组；传入数组可按请求选择子集，空数组 `[]` 表示关闭。请求了管理员已关闭的组会在分配账号前返回 400。
除 `tool_choice:none` 会关闭全部工具外，标准 `tools`／`tool_choice` 不会选择原生工具组；请用
`claud2api_tools` 选择它们。`any`、`tool` 和 `disable_parallel_tool_use` 仍只约束客户端工具调用。
`widgets` 包含当前网页端的 37 种组件，包括询问用户、信息卡片、授权提示和排程展示。它们作为网页组件数据原样返回；
网关不提供组件的用户回复回传流程，不会替用户授予授权、连接 MCP 或执行排程操作。具体组件仍由上游账号能力决定。
这些描述符不是账号可用性清单，不能据此宣称全部网页工具均可用；当前实测范围和未覆盖项见[综测结果](TEST_SUMMARY.md)。

```json
{
  "model": "claude-sonnet-5-5",
  "max_tokens": 1024,
  "claud2api_tools": ["web_search"],
  "claud2api_native_output": "extensions",
  "messages": [{"role":"user","content":"搜索 Claude 官方网页并概述结果。"}]
}
```

工具在 Claude 远端环境中执行，Claud2Api 不会在网关主机运行模型给出的命令。默认
`claud2api_native_output` 为 `standard`：原生工具结果、引用和生成文件下载链接以普通文本块返回；客户端工具仍使用标准
`tool_use`／`tool_result`。设置为 `extensions` 才返回结构化扩展。JSON 中每个原生块包装为 `claud2api_native`，
其 `block` 保留上游 `tool_use` 或 `tool_result`；SSE 保留原生 start／delta／stop 生命周期和原始块。
工具错误保留 `is_error`；上游授权键及 MCP 服务端地址等控制元数据会移除。

使用 `extensions` 时，客户端必须把返回的 assistant `content` 数组原样放回完整历史，不要另造 `tool_result`；
下一次请求会校验并保留这些块。启用了原生工具组时，输出模式和所选工具组会参与 Messages 上下文指纹；切换任一项都会新建上游会话。
保持模式一致并继续发送完整历史。期望只接收标准 Messages 内容块的客户端应使用默认 `standard`。
此选项只适用于 Messages；Chat／Responses 保持原有文本输入和顶层扩展格式。

`extensions` 模式下，引用也使用 `claud2api_native` 扩展块：`block.type` 为 `citation`，其 `block.citation` 包含 `text_block_index`、`start_index`、
`end_index`、`cited_text` 和可用的来源字段。偏移按原始文本块的 Unicode 字符计数。`standard` 模式会把引用作为来源文本返回。
来源对象是网页会话的元数据，
与官方 API 的加密引用格式不同。官方日期版本的 API 服务端工具块仍不支持；请使用 `claud2api_tools`。
自定义 MCP 连接器、连接器授权执行和排程执行不在此扩展范围内；实际可用工具由上游账号权限决定。

项目知识搜索可通过 `claud2api_project_uuid` 绑定一个已知项目。该值必须是规范的小写 UUID；网关不会列出或读取账号的全部私有项目，
上游账号仍须有该项目的访问权限。提供 UUID 且管理员已开启项目知识时，省略 `claud2api_tools` 会在默认四组之外加入 `project_knowledge`；
也可在数组中明确选择它。没有 UUID 时显式请求 `project_knowledge` 返回 400。管理员关闭项目知识后，任何项目 UUID 都会被拒绝，
即使 `claud2api_tools` 是空数组。项目 ID 和工具组参与会话指纹；Messages 配置变化会建立新的上游会话，调用方应继续发送完整支持历史。

```json
{
  "claud2api_project_uuid": "<替换为有权访问的规范小写项目 UUID>",
  "claud2api_tools": ["project_knowledge"],
  "claud2api_native_output": "extensions"
}
```

Messages 使用 `extensions` 时，以及 Chat／Responses 返回生成文件时，原生结果中的 `local_resource` 或
`tool_result.structured_content` 会带有 `claud2api_download_url`。Messages 默认 `standard` 模式会把文件名和下载路径作为文本返回。使用链接通过
`GET /v1/native-files/{opaque-handle}` 下载，必须带生成链接时使用的同一凭证（API 令牌或网关密钥）。
链接经过加密并绑定令牌所有者，最多有效 1 小时；下载上限 32 MiB，响应强制作为附件下载。链接失效前服务会保留对应的上游会话。

Claude Code 兼容：顶层 `system` 开头符合已知格式的 `x-anthropic-billing-header` 归因行，
会在上下文哈希、网页提示词投影和本地输入估算前移除。支持独立的首个文本块，或合并文本块／字符串
的首行；其余指令逐字保留。未知格式、内部块、message 角色指令和工具数据保持原样。
具体规则见[会话说明](STICKY_SESSIONS_ZH.md)。

兼容处理：messages 任意位置的 system / developer 文本指令会按出现顺序合并在顶层 system 后，
仍须至少保留一个 user 对话轮次。tool / function / model 等角色不会被猜测转换。
若只在已完成历史之后追加 system／developer 指令，且旧历史及旧指令逐字匹配，可继续使用同一
网页会话；新增指令仍发送给模型。编辑、删除或插入已完成历史中的指令，或修改顶层 system／
模型／工具，仍按原规则重建。
角色错误包含原始消息索引及固定允许的 role_kind，不回显任意未知角色或正文。
assistant 历史中的 thinking / redacted_thinking 块会丢弃；仅含这些块的 assistant 条目省略，
不保留原生思考状态。其他受支持的内容块保持顺序。服务端工具及其他未知块仍返回 400，
诊断仅含允许的 block_kind 和位置，不回显任意未知类型值。

下列不支持的可选控制字段接受后静默忽略：thinking、output_config、temperature、top_p、top_k、
stop_sequences、metadata、cache_control、工具 strict 注解及其他未知可选字段。
受支持的会话 metadata 可先在本地用于路由，不发送到上游。
这不表示实现了对应的原生思考、结构化生成、采样、停止条件或缓存能力。
工具 input 内同名键属于业务数据，不会被剥离。无效 JSON、必填字段及工具引用仍会报错。

## 会话与缓存

Messages 默认采用最后成功一轮后 1 小时到期的粘性会话。客户端继续发送完整历史，
返回的不透明 `X-C2A-Session-ID` 可在后续请求头中复用，也接受 `X-Claud2Api-Session-ID`。
没有显式标识时，仅在完整历史前缀匹配且无歧义时续接。
不同 API 令牌的会话指纹、历史匹配及旧接口 conversation_id 相互隔离；跨令牌使用不透明句柄返回 403。
环境网关密钥拥有独立的原有会话空间。

同账号可以绑定多个会话，请求严格串行；到期后移除本地绑定并排队清理远端。
明确的生成前额度拒绝可迁移到其他账号；部分输出、结果不确定或访问错误不会重放。
两轮之间主动停用绑定账号时，下一轮也可改选账号；访问暂停仍须检查，不会被绕过。
上下文管理可生成内部摘要并保留近期完整轮次；摘要不进入 API 响应正文。

模拟 1h 缓存统计在新安装中默认开启，已有显式设置保持原值。
usage 为本地字符估算，模拟缓存字段不等于真实缓存命中、官方账单或实际省额度。
响应头 `X-Claud2Api-Cache: simulated-1h` 标记模拟展示；可在运行配置中关闭。
详情见 [会话](STICKY_SESSIONS_ZH.md)、[上下文管理](CONTEXT_MANAGEMENT_ZH.md)、[模拟缓存](SIMULATED_CACHE_ZH.md)。

控制台可按 API 令牌查看用量曲线、占比和模拟 USD 费用。金额使用每轮记录的原生模型单价、
缓存开关、创建／读取和输出估算量，不受之后切换开关影响；未定价模型与缺少信息的旧记录单列。
这不修改公开 API usage 字段，也不产生真实 API 账单。详见 [用量与模拟费用](USAGE_ANALYTICS.md)。

## 响应与 SSE

Messages JSON 包含 id、type、role、content、model、stop_reason、stop_sequence、usage。
客户端工具调用时 stop_reason 为 tool_use；原生网页工具在同一轮远端执行。默认 `standard` 模式把原生结果、引用及下载链接作为文本块返回；
`extensions` 模式返回结构化扩展块。
SSE 使用 `event: <type>` 与 `data: <JSON>`，包括
message_start、content_block_start / delta / stop、message_delta、message_stop，等待期间可发送 ping。
客户端工具参数增量为 input_json_delta，在完整参数校验后发送。`extensions` 模式下原生工具使用 `claud2api_native_delta`，
其增量封装上游原始 delta；对应 `content_block_stop` 附带完整 `claud2api_block`，客户端应以它替换该块。`standard` 模式不发送原生扩展块。
中断或错误使用 error 终止，不伪装成功。

`X-Request-ID` 关联脱敏日志；日志保留客户端原始 model 参数和令牌记录 ID，不保存完整令牌、聊天正文或摘要。
`X-Claud2Api-Limitations` 声明用量估算、max_tokens 建议值、网页客户端工具通道、原生网页工具扩展及不转发控制字段等限制。

## 调度与错误

每个账号最大并发 1，全局上限不能提高此限制。生产默认账号请求开始间隔至少 5 秒、每分钟最多 6 次；
过早请求返回 429 和 Retry-After，不转发上游。独立部署或浏览器不会共享本进程内的账号锁。
模型列表查询也占用账号请求调度。有效 Retry-After 会影响冷却；无有效时间的额度拒绝默认保守冷却 5 小时。
账号认证、访问挑战等失败会暂停账号，需人工检查后在控制台恢复；重启不会清除暂停或冷却。

```json
{"type":"error","error":{"type":"invalid_request_error","message":"explanation"},"request_id":"req_example"}
```

| 状态码 | 含义 |
|---|---|
| 400 | 输入不合法、未公开模型或账号不支持目标 |
| 401 | API 令牌无效、过期或删除 |
| 403 | 会话不属于当前令牌 |
| 404 | 未实现接口或不存在的旧接口会话 |
| 413 | 请求体过大 |
| 429 | 令牌、账号、全局并发或请求频率限制 |
| 502 | 上游账号、传输或协议异常 |
| 503 | 账号暂停或状态不可用 |
| 504 / 529 | 超时 / 上游过载 |

不转发上游原始错误正文，不自动重试业务生成。流式 HTTP 200 仅表示响应头开始，须检查终止事件。
`max_tokens` 不保证精确限制网页模型输出；服务仍限制响应大小、持续时间及并发。
浏览器配置不保证与真实浏览器等效，也不保证免于账号限制。

## 其他接口

兼容提供 `POST /v1/chat/completions`、`POST /v1/responses`、`GET /v1/models` 和
`DELETE /v1/conversations/{conversation_id}`。Chat 使用 messages 文本，Responses 使用 input 文本或有序文本消息及可选 instructions。
字符串和支持的文本块会保留原角色；兼容接口不接受客户端工具定义及工具历史，但可通过 `claud2api_tools` 请求网页原生工具。
Chat 和 Responses 返回 `claud2api_native`、`claud2api_citations` 扩展字段；Chat SSE 和 Responses SSE 另有
`claud2api.native_start`、`claud2api.native_delta`、`claud2api.native_stop` 与 `claud2api.citation` 扩展事件。
Responses 只在来源文本能唯一匹配时生成标准 `url_citation` 标注。`claud2api_native_output` 只作用于 Messages；Chat／Responses 保持原有文本与顶层扩展格式。
兼容接口的输入保持文本结构；虽然响应包含原生工具扩展，后续请求不能回传这些结构块。需要保留原生调用与结果历史时请使用 Messages。
均可 stream。用量按实际桥接提示和输出统一进行字符估算，合计等于输入加输出；Chat 流可用
`stream_options.include_usage` 在成功结束时获取独立用量帧。Chat 流失败返回结构化错误，不把错误塞进回答或以成功 stop 结束。
Responses 使用带序号的命名 SSE 事件，以 `response.completed` 或 `response.failed` 终止。
兼容接口可用 conversation_id 复用本进程内会话映射，删除仅限当前令牌拥有的映射，重启后此映射不保留，也不共享 Messages 的自动压缩和额度迁移。
此扩展把每次提交作为后续消息追加，因此设置 conversation_id 时只发送新增消息。
同一 `conversation_id` 必须保持相同的 `claud2api_tools` 和 `claud2api_project_uuid`；更改任一项会在访问上游前返回 409。
保留原配置继续请求，或用新的 `conversation_id` 开始会话。
每轮发送完整历史的客户端应省略该字段；文本兼容接口不对历史去重。
Messages 的持久粘性会话不依赖旧接口的删除路径。
未实现 count_tokens、批量任务及其他官方 API 功能，未知路径返回结构化 404。
