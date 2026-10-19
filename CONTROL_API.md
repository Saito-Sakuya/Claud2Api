# 控制台接口

中文 · [English](CONTROL_API_EN.md) · [文档索引](DOCS_ZH.md)

仅在独立回环监听器提供 `/control/v1/*`，使用 `Authorization: Bearer <管理密钥>`。
公开 API 密钥不能访问管理接口。返回包含 `schema_version:1`。

## 只读信息

| 接口 | 内容 |
|---|---|
| `GET /info` | 版本、健康、账号容量、会话数、浏览器配置列表 |
| `GET /usage?since=<Unix秒>` | 默认最近 5 小时的本地用量估算 |
| `GET /events?after=<游标>&limit=100` | 状态变更与操作记录 |

可选 `BOT_READ_KEY_FILE` 仅能读取以上三个接口，供外部通知程序使用。
上游剩余额度始终未知。失败、断连及其他服务对账号的使用可能无法完整计量。

since 被限制在最近 31 天至当前时刻；`/usage` 最多读取最近 10000 条匹配记录，须检查 truncated。
`/events` 返回 cursor 和 events（seq／at／kind／data），外部程序保存 cursor 作为下一次 after。
limit 默认 100，限制在 1–200；没有内置主动推送。

## 管理操作

以下路径均以 `/control/v1` 为前缀。

| 方法 / 路径 | 用途 |
|---|---|
| `GET /accounts` | 账号状态、浏览器配置、脱敏代理设置 |
| `POST /accounts/import` | 批量导入 1–512 个账号，按 Session Key 去重 |
| `PATCH /accounts/{id}` | 编辑名称、启停、浏览器配置及代理 |
| `PATCH /accounts/proxy` | 为选中账号原子分配同一代理或直接连接 |
| `POST /accounts/{id}/resume` | 人工检查后解除访问暂停，不清除冷却 |
| `GET /sessions` | 本地会话 ID、账号、模型、到期及活动状态 |
| `DELETE /sessions/{id}` | 清理空闲会话；活动会话返回 409 |
| `GET /settings` | 读取配置及 revision |
| `PUT /settings` | 提交完整运行配置 |
| `GET /logs` | 查询消息状态日志，管理密钥专用 |
| `GET /models` | 读取模型配置 |
| `PUT /models` | 更新模型列表、名称及别名映射 |
| `POST /models/upstream` | 手动获取所选账号的原生目录及默认配置模板 |
| `GET /tokens` | 查看令牌元信息及当前并发占用 |
| `GET /analytics` | 管理员专用令牌用量、趋势及模拟费用 |
| `POST /tokens` | 创建 API 令牌，仅此次返回完整令牌 |
| `PATCH /tokens/{id}` | 编辑名称、并发及有效期，保留原密钥 |
| `DELETE /tokens/{id}` | 删除令牌，拒绝新请求 |

账号、设置、模型、令牌及会话写操作需 `expected_revision`，版本过期返回 409；下文的目录观测除外。控制台后台刷新不会覆盖未保存的表单；
若其他操作改变了配置版本，请重新读取并审核后保存。

账号导入正文：

```json
{"expected_revision":1,"accounts":[{"credential":"<凭证>","label":"账号名称","browser_profile":"firefox_151_windows","proxy_url":"socks5h://<host>:1080"}]}
```

账号 PATCH 可包含 `enabled`、`label`、`browser_profile`、`proxy_url`。
省略 `proxy_url` 保留原设置，空字符串清除代理。代理认证信息只写入，不回读。
账号忙碌时修改浏览器或代理返回 409；启停不强制中断正在执行的请求。

## 运行配置

`GET /settings` 返回 revision、完整 settings 和 effective_for。先读取现有值，再修改需要的字段；
`PUT /settings` 需提交完整 settings，而不是部分补丁。例如以下为默认配置示例：

```json
{
  "expected_revision": 1,
  "settings": {
    "session_ttl_seconds": 3600,
    "request_timeout_seconds": 300,
    "max_request_bytes": 4194304,
    "simulated_cache": true,
    "compaction_enabled": true,
    "context_budget": 128000,
    "compaction_trigger_percent": 75,
    "keep_recent_rounds": 6,
    "account_min_interval_seconds": 5,
    "account_requests_per_minute": 6,
    "rate_limit_min_seconds": 60,
    "rate_limit_fallback_seconds": 18000,
    "overload_seconds": 15,
    "native_web_search": true,
    "native_artifacts": true,
    "native_repl": true,
    "native_widgets": true,
    "native_project_knowledge": true
  }
}
```

允许范围见[可保存的运行参数](DEPLOY.md#可保存的运行参数)，未知 settings 字段会被拒绝。
修改用于后续请求，执行中的请求保留已捕获限制；保存后的值跨重启保留。
示例 revision 只是占位值，应使用 GET 返回的当前值，不能以示例覆盖未经审核的现有设置。
五个 `native_*` 开关默认开启，可在设置页逐组关闭；旧状态文件缺少这些字段时按开启迁移。
公开请求可用 `claud2api_tools` 再选择子集，不能覆盖管理员关闭的组。项目知识搜索还要求请求提供
`claud2api_project_uuid`；未指定项目 UUID 时，它不会加入默认工具组。


导入的 `credential` 接受原始 Session Key、完整 Cookie 或 `账号名:Session Key`。
仅非 Cookie 行按第一个半角冒号拆分；显式 `label` 优先，重复项不覆盖原账号。
名称可单独修改，无需等待生成结束。`PATCH /accounts/proxy` 接受 `expected_revision`、
1–512 个不重复的 `account_ids` 和 `proxy_url`（空字符串表示全部直接连接）。
先验证所有账号并取得原有并发锁，再一次性保存；任一忙碌、缺失、版本冲突或无效代理时
整批失败。浏览器配置和会话保持，代理认证只写入、不回读。

`GET /accounts` 每项新增 `usage_cycles`：`current`、`previous`、
`duration_seconds:18000`、`estimated:true` 及统计时间。每轮含 `start`、不包含的右边界
`end`、`fallback_records` 和与分析接口同口径的 `totals`。每账号首次生成起算 5h；
空闲过期后 `current:null`，保留上一实际使用轮，下一次生成重新起算，不填补空闲轮。
包含内部摘要及已记录失败；失败消耗可能未知。旧账本缺少开始时间时按完成时间推算，
`fallback_records` 明示条数。读取完整账本，不截取最近 10000 条，超过 8 秒返回 504。
这是本地累计估算，不能视为官方额度，其他客户端使用及未记录失败不在其内。

`PATCH /tokens/{id}` 接受完整可编辑元信息：`expected_revision`、`label`、
`max_concurrency`、`expires_at`（null 为永不过期，否则须为未来 Unix 秒）。
保留原密钥、ID、创建时间及会话；仅影响后续接入。降低并发时已有请求正常结束，
占用可暂时高于新上限，降到上限以下后才接收新请求。编辑不再次返回完整密钥。
上述管理写入仅管理员可用，旧 revision 返回 409；取消并重新打开编辑以读取最新配置。

## 模型配置

```json
{"expected_revision":1,"models":{"enabled":true,"entries":[{"id":"work-haiku","display_name":"工作模型","upstream_model":"claude-haiku-5-5","enabled":true}]}}
```

GET 返回 `schema_version`、`revision` 和同结构 `models`。默认 enabled=false，沿用原生模型列表。
还返回 `upstream_catalog`（account_id、fetched_at、models 原生 ID 数组）及 `default_models`（同 entries 结构），
无已获取记录时为 null／空数组。此 GET 仅读取本地上次观测，不会访问上游。
最多 256 条，ID 唯一、1–150 个 ASCII 字母、数字或 `-_.:`；显示名称为 1–100 字符，不能包含控制字符。
真实模型必须为原生名称，不能以 `c2a-` 开头，不进行链式映射。下游 `c2a-` 前缀保留给 `claude-*`。
保存时只校验配置，不访问上游。空的启用列表表示不公开任何模型。

自定义模式下 `/v1/models` 返回启用条目与本次所选账号实际目录的交集，不聚合整个账号池。
列表 ID 用于请求，display_name 为展示名；所有生成响应中的 model 返回所选 ID。
实际调用始终验证目标权限。配置变更不改变已开始响应的 model。
原生与 `c2a-` 用途详见 [中文 API 文档](API_ZH.md#模型名称与模型列表)。

`POST /models/upstream` 正文为 `{"account_id":null}` 或指定已有账号 ID，不需要 expected_revision。
只在管理员主动点击时查询，复用账号并发 1、代理、浏览器特征、暂停／冷却／频率限制；不进行消息生成。
未指定时自动选择可用空闲账号，指定账号不存在返回 404，忙碌／冷却返回 429，暂停返回 503。
上游访问失败沿用原有暂停规则；失败不覆盖上次成功目录。请求最长 8 秒。
成功返回 schema_version、当前 revision、upstream_catalog、default_models，并持久保存观测及安全审计；
不修改映射、启停或全局配置 revision。默认条目使用同一原生 ID 作为 ID、显示名和真实目标。
控制台在手动获取成功后，将完整目录中尚未配置的模型全部补入草稿（按 ID 去重并默认启用），
保留已有名称、映射及停用状态，保存后生效。也支持再次补入或逐项加入及逐行上游下拉选择。
目录和调用路径不限制为某几个型号；新模型仍须由实际执行账号的实时目录确认支持。
原先为空且未开启的列表会展示默认草稿供修改，修改条目自动开启自定义模式，保存才改变服务行为。
取消自定义模式仍可保存回原生目录。浏览器中的未保存草稿不会被重新获取或轮询覆盖。

## API 令牌

创建正文：

```json
{"expected_revision":2,"label":"工作端","max_concurrency":2,"expires_at":1893456000}
```

`label` 为非空、最多 80 字符；`max_concurrency` 为 1–512；`expires_at` 为未来 Unix 秒，
null 或省略表示永不过期。最多保留 512 条记录，包括到期记录；可删除不用的令牌。
返回 `schema_version`、`revision`、`id`、`token`；完整 `c2a_tok_...` 只在此响应中出现，不能再次读取。

GET 返回 `tokens` 数组，各项包括 id、label、max_concurrency、created_at、expires_at、expired、active_requests，
不返回明文、摘要或可恢复令牌。`environment_key_enabled:true` 表示原环境网关密钥仍有效，不能在此删除，
不受新增令牌的单独并发限制；更换此密钥涉及数据加密，不应直接改值代替撤销令牌。
DELETE 正文为 `{"expected_revision":3}`，返回新 revision 和 deleted=true。

令牌限制覆盖认证后的请求及完整 JSON / SSE 响应，超出返回 429。账号与全局限制仍然有效。
删除、到期拒绝后续请求，已经接入的请求正常排空。不同令牌的会话空间隔离。
新令牌仅用于公开 API，不具备控制接口或 Bot 权限；Bot 也不能读取模型配置、令牌与日志。

## 消息状态日志

```text
GET /control/v1/logs?level=all&account_id=<账号ID>&limit=100&before=<游标>
```

`level` 为 `all`、`error`、`normal`，省略账号表示全部账号。`before` 用于加载更早记录，limit 默认 100，限制在 1–200。
返回 `records`、`next_before`、`has_more`、保留限制、healthy 及 `dropped` 队列溢出计数。
日志记录请求 ID、账号、请求模型 `model`、令牌记录 ID `token_id`、请求时令牌名称 `token_name`、上游/下游方向、JSON/SSE、阶段、状态码、耗时、字节及分块数。
名称随请求保存，删除令牌不删除历史名称；环境网关密钥单列。旧日志缺失名称时不推测归属。
`model` 保留客户端提交的模型 ID（包括 `c2a-` 前缀），不包含其他请求正文。
无模型参数的请求或旧日志显示为 `—`；不会推测、回填历史模型。
保留最多 10000 条、最长 7 天；异常退出后未完成记录标为中断。
流式 HTTP 200 仅表示响应头已返回，终态错误仍会归入错误分类。
日志不保存聊天正文、内部摘要、Cookie 或代理密码。
最终下游记录的 `native_tools` 仅含允许的工具名称与 `started`／`completed`／`error` 状态；不含工具 ID、参数、结果正文或 URL。
`native_tools_complete: true` 表示已完整解码，此时 `[]` 确认本轮没有原生调用；`false` 表示列表可能不完整或已截断（最多 128 次调用）。旧记录或未知情况的两个字段均为 `null`。

生成记录还包含 `usage`，旧日志及非生成操作为 `null`，不会回填或当作零：

```json
{"input_tokens":1200,"output_tokens":80,"cache_creation_input_tokens":200,"cache_read_input_tokens":1000,"estimated":true,"complete":true,"basis":"logical_request"}
```

- 下游 `basis=logical_request`：输入为完整逻辑输入，**已包含**模拟缓存创建和读取；输出与本地用量账本同源。
  Messages 日志输入等于响应 usage 的普通输入、缓存创建、缓存读取三者之和，输出等于响应 output_tokens。
  兼容 Chat/Responses 接口仍沿用已有响应格式，日志采用本地账本估算，不依赖旧响应中的占位 usage。
- 上游 `basis=sent_prompt`：各次生成分别估算本次发送的 prompt 和已收到的 text、thinking、partial_json 输出片段，
  不含网页已有历史；内部压缩和重试独立计数，不复制下游总量。按累计 Unicode 字符数除以 4 向上取整，不随流式分块次数增加。
- `complete=false` 表示尚未完成或失败。上游收到的部分输出仍可显示，但不代表完整输出；未收到输出的失败为 null，
  正常结束且确实为空才是 0。下游仅在响应体完整结束且状态成功时发布最终输出；断连、失败的输出为 null。
- 缓存关闭或尚未确认成功时，两个缓存字段为 null。Token 为本地估算，`estimated=true`，不是平台的官方计量或实际账单。

控制台在请求模型之后合并展示 Token 用量：上方为输入、输出，下方为已确认的模拟缓存读取、写入及 `1h` 标记。
有缓存明细时，普通输入等于输入总量减去缓存创建和读取，避免重复展示；按每条历史记录显示，不随当前缓存开关重算。
大数使用 K/M 等紧凑格式，悬停可查看精确计数及输入总量；未知、待完成和已接收的部分输出仍单独标记。

## 令牌用量与模拟金额

`GET /control/v1/analytics?range=24h&granularity=hour` 仅允许管理密钥；Bot 的原 `/usage` 接口保持不变。

`range`：today、24h（默认）、7d、month、last_month、all。日历范围按 `CLAUDE_TIMEZONE` 的 IANA 时区计算，
上月右边界为本月起点（不包含）。all 覆盖现有持久账本，重启不清零。
`granularity`：hour（默认，最近 24h 分为 24 个小时段）、day（最近 7d 分为 7 个 24h 段）。
趋势窗口独立于统计范围，返回 `trend_window.buckets` 边界，空段补零；当前段包含当前秒。

返回 schema_version、range、granularity、timezone、window、trend_window、totals、tokens、series、by_account，
以及 estimated、currency=USD、price_source、price_date、ledger_since、truncated=false。
tokens 项为 id、name、totals；series 项为 id、name、points（每项同 Totals）。
最多展示 12 个令牌曲线，余者合并为一条；超过 2048 个历史令牌时分组合并，`grouped_historical_tokens` 标记，合计仍完整。

Totals 包括 records、requests（业务记录）、internal_requests、successful_requests、unmetered_requests、legacy_records、
priced_requests、unpriced_requests、cache_enabled_requests；input_estimate、output_estimate、physical_estimate、
uncached_input、cache_creation_simulated、cache_read_simulated；total_nano_usd、baseline_nano_usd、
input_nano_usd、output_nano_usd、cache_write_nano_usd、cache_read_nano_usd。
priced_uncached_input、priced_output、priced_cache_creation、priced_cache_read 是费用表对应的已计价 Token 子集，避免把未知价格的 Token 与金额小计混算。
金额整数单位为 10⁻⁹ 美元，展示除以 10⁹。未计价和消耗未知记录不可解释为免费。
未知模型不猜价，旧记录不补计价。请求被账号接入前拒绝、断连或内部失败的消耗可能未计量；以日志诊断状态。
查询不截为最近 10000 条，8 秒超时返回 504，不返回伪完整总额。不会查询上游。
计价来源及示例见 [用量说明](USAGE_ANALYTICS.md)。

## 版本管理

仅管理员可用：`GET /updates`、`POST /updates/check`、`POST /updates/apply`、`POST /updates/policy`。
Bot 及公开 API 凭证无权访问。检查及应用返回 202；应用须提交 GET 返回的
`expected_version`、`target_version` 及 `catalog_revision`。版本或目录过期、
非提供的目标及并发切换返回 409，格式错误返回 400。GET 包含 `current_version`、
`supervisor_version`、`repository`、`enabled`、`checking`、检查时间、`check_error`、
`update_available`、`latest`、最多两个 `rollback_versions`、`catalog_revision`、
`busy`、`job` 及下载进度。失败任务恢复成功时可返回 `recovered:true`。
支持自动补丁的守护进程另返回 `auto_patch_supported:true`、`auto_patch`（`enabled`、
`revision`、`blocked_versions`）、`patch_update` 及 `job.automatic`。`patch_update` 与
`latest` 可不同：前者为当前发行线补丁，后者可能是需要确认的功能版本。
修改策略提交 `{"expected_revision":1,"auto_patch":false}`，使用 `auto_patch.revision`，
成功返回 200；策略过期或正在切换返回 409。失败或中断的安装停止自动重试，手动更新仍可重试。
手动回退会持久关闭自动补丁。旧守护进程缺少这些字段，面板提示升级基础程序。
非支持平台独立运行返回 `enabled:false`；守护入口绝不暴露内部 `/_runtime/*` 操作。
发布与数据规则见 [版本管理](UPDATES_ZH.md)。
