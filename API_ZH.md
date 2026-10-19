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
{"type":"tool_use","id":"toolu_example","name":"lookup","input":{"key":"x"}}
```

```json
{"type":"tool_result","tool_use_id":"toolu_example","content":"result","is_error":false}
```

工具调用属于 assistant，工具结果属于随后的 user；每个调用必须有一个对应结果，结果块在普通用户文本之前。
工具结果也可以是文本块数组。工具参数及结果中的语义数据保持原样。
工具声明需包含 name、description（可选）及对象类型的 `input_schema`，支持本地 JSON Schema 引用，
不加载远程或文件引用。`tool_choice` 支持 auto、none、any、tool（指定名称）及 `disable_parallel_tool_use`。
本服务不执行工具；客户端执行后，携带完整历史及 user 的 tool_result 继续请求。
模型生成的工具参数不合法或违背强制工具选择时返回错误，不伪造工具结果。

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
不保留原生思考状态。其他文本、工具块保持顺序。图片、文档、服务端工具及其他未知块仍返回 400，
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
工具调用时 stop_reason 为 tool_use。SSE 使用 `event: <type>` 与 `data: <JSON>`，包括
message_start、content_block_start / delta / stop、message_delta、message_stop，等待期间可发送 ping。
工具参数增量为 input_json_delta，在完整参数校验后发送。中断或错误使用 error 终止，不伪装成功。

`X-Request-ID` 关联脱敏日志；日志保留客户端原始 model 参数和令牌记录 ID，不保存完整令牌、聊天正文或摘要。
`X-Claud2Api-Limitations` 声明用量估算、max_tokens 建议值、文本工具桥接及不转发控制字段等限制。

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
字符串和支持的文本块会保留原角色；非文本块和未支持的工具调用会被拒绝，不再默认启用网页原生工具。
均可 stream。用量按实际桥接提示和输出统一进行字符估算，合计等于输入加输出；Chat 流可用
`stream_options.include_usage` 在成功结束时获取独立用量帧。Chat 流失败返回结构化错误，不把错误塞进回答或以成功 stop 结束。
Responses 使用带序号的命名 SSE 事件，以 `response.completed` 或 `response.failed` 终止。
兼容接口可用 conversation_id 复用本进程内会话映射，删除仅限当前令牌拥有的映射，重启后此映射不保留，也不共享 Messages 的自动压缩和额度迁移。
此扩展把每次提交作为后续消息追加，因此设置 conversation_id 时只发送新增消息。
每轮发送完整历史的客户端应省略该字段；文本兼容接口不对历史去重。
Messages 的持久粘性会话不依赖旧接口的删除路径。
未实现 count_tokens、批量任务及其他官方 API 功能，未知路径返回结构化 404。
