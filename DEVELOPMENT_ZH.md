# 开发者文档

中文 · [English](DEVELOPMENT.md) · [文档索引](DOCS_ZH.md)

本文对应仓库中已实现的 **v0.1.0-alpha.4**。仅需部署或调用服务时，先阅读
[安装说明](DEPLOY.md)和[公开 API](API_ZH.md)。接口细节由各专题文档维护；本文说明
模块职责、状态归属、关键约束，以及如何安全地扩展和验证实现。

## 1. 范围与当前边界

Claud2Api 是带嵌入式管理控制台的 Rust HTTP 服务。主要接口为 Anthropic 格式的
Messages 文本／客户端工具桥接，并提供网页原生工具扩展。上游是网页会话服务，因此接受某个 API 字段，
不代表实现了该字段对应的原生 API 能力。

- Messages 支持 JSON／SSE、经过校验的客户端工具、网页原生工具扩展、持久粘性会话、额度迁移及本地上下文压缩。
- Chat Completions 和 Responses 使用独立的文本输入兼容路径，可请求网页原生工具扩展；它们不是完整 OpenAI 协议实现，
  也不具备全部 Messages 会话、压缩和迁移行为。
- 模型来自实际执行账号的目录。新增原生 ID 无须加入固定型号白名单；缺少参考价格不阻止生成。
- Token、1h 缓存及 USD 金额均为本地估算，上游剩余额度未知。
- 浏览器配置模拟网络特征，不是完整浏览器，不保证免于访问挑战或账号限制。
- 已验证的发行平台为 Linux AMD64。其他平台须分别完成构建和运行验收后才能宣称支持。

## 2. 仓库结构

| 路径 | 职责 |
|---|---|
| [src/main.rs](src/main.rs) | 守护／工作进程入口、请求头超时、工作进程定时清理与退出 |
| [src/updates/](src/updates/) | 固定入口、版本检查、下载校验、工作进程切换与备份恢复 |
| [src/lib.rs](src/lib.rs) | 模块导出，以及从 Cargo 派生的产品版本 |
| [src/config.rs](src/config.rs) | 启动配置、账号凭证及经过校验的运行参数 |
| [src/web.rs](src/web.rs) | 公开认证、响应生命周期、Messages JSON／SSE |
| [src/protocol.rs](src/protocol.rs) | 输入投影、对话／工具校验、会话指纹与工具帧解码 |
| [src/native_tools.rs](src/native_tools.rs) | 网页工具组、权限开关与描述符 |
| [src/native_stream.rs](src/native_stream.rs) | 原生工具生命周期、结构化结果及引用解码 |
| [src/native_files.rs](src/native_files.rs) | 同令牌、限时的生成文件下载 |
| [src/models.rs](src/models.rs) | 下游模型 ID、精确别名及原生目标解析 |
| [src/tokens.rs](src/tokens.rs) | 下游令牌、摘要及令牌接入控制 |
| [src/guard.rs](src/guard.rs) | 取消、截止时间、账号池及单账号租约 |
| [src/engine.rs](src/engine.rs) | 会话预留与归属、迁移、摘要及远端清理 |
| [src/turn.rs](src/turn.rs) | 单次生成尝试、上下文投影及成功状态提交 |
| [src/transport.rs](src/transport.rs) | 上游 HTTP、模型观测及 SSE 读取 |
| [src/browser.rs](src/browser.rs) | 浏览器配置与账号代理校验 |
| [src/store.rs](src/store.rs) | 加密快照、SQLite 事务、迁移及持久记录 |
| [src/observability.rs](src/observability.rs) | 请求跨度、终态及每次尝试的 Token 计数 |
| [src/usage.rs](src/usage.rs) | 用量估算、冻结参考费率和统计时间窗 |
| [src/control.rs](src/control.rs) | 管理员及受限 Bot 信息接口 |
| [src/compat.rs](src/compat.rs) | 独立 Chat／Responses／旧会话删除兼容接口 |
| [src/error.rs](src/error.rs) | 脱敏的结构化错误 |
| [Dockerfile](Dockerfile) | 从源码构建镜像 |
| [Dockerfile.release](Dockerfile.release) | 将已验证的 Release 二进制装入发行镜像 |
| [.github/workflows/release.yml](.github/workflows/release.yml) | 校验版本、发布 GHCR 镜像及完整 Release 资产 |
| [scripts/package_release.py](scripts/package_release.py) | 二进制、更新清单、许可归档及校验和 |
| [scripts/container_acceptance.py](scripts/container_acceptance.py) | 使用合成密钥和临时状态验收最终镜像 |
| [ui/](ui/) | 嵌入式页面、应用状态、图表、统一下拉框及 Logo |
| [tests/](tests/) | 协议、配置及模拟上游集成回归 |
| [scripts/](scripts/) | 密钥生成、许可清单、发行打包与升级验收 |
| [LICENSES/dependencies/index.json](LICENSES/dependencies/index.json) | 锁定依赖来源、许可文件及校验和 |

控制台使用原生 DOM JavaScript，无须安装独立前端依赖或执行打包。Rust 通过 `include_str!`
嵌入页面；修改 UI 后需重新构建二进制或镜像。调整接口时须保留持久状态使用的加密域和指纹格式。

## 3. 本地开发

### 工具链

Cargo 声明最低 Rust 版本为 **1.98**；发行验收使用 **1.99.1**。仅有清单声明，不能证明
所有较低版本均已验收。原生传输依赖还需要 C／C++ 工具链、CMake、Perl、pkg-config 和
libclang。Dockerfile 与 CI 定义了固定的 Linux 构建基础镜像。

```sh
sudo apt-get update
sudo apt-get install -y build-essential cmake perl pkg-config libclang-dev
rustup component add rustfmt clippy
cargo build --locked
```

保留 `Cargo.lock` 和固定 Git 修订；版本号相同但源码不同的依赖不能视为等价替换。
Windows 开发者可使用 Linux Docker 环境或 Linux 开发环境完成本文所述发行流程。

### 启动隔离的空实例

在仓库根目录的独立 POSIX shell 中执行以下命令。它在仓库外创建新密钥和状态目录，不导入
真实账号，适用于控制台开发。生成请求还需要另行配置模拟上游或明确获准使用的账号。

```sh
DEV_PRIVATE="$(mktemp -d "${TMPDIR:-/tmp}/claud2api-dev.XXXXXX")"
chmod 700 "$DEV_PRIVATE"
python3 scripts/generate_key.py "$DEV_PRIVATE/gateway-key.txt"
python3 scripts/generate_key.py "$DEV_PRIVATE/admin-key.txt"
touch "$DEV_PRIVATE/accounts.txt"
chmod 600 "$DEV_PRIVATE/accounts.txt"
mkdir -m 700 "$DEV_PRIVATE/state"
unset CLAUDE_SESSION_FILE CLAUDE_COOKIE CLAUDE_SESSION_KEY STICKY_SESSION_STATE_FILE BOT_READ_KEY BOT_READ_KEY_FILE
export GATEWAY_API_KEY_FILE="$DEV_PRIVATE/gateway-key.txt"
export ADMIN_API_KEY_FILE="$DEV_PRIVATE/admin-key.txt"
export CLAUDE_ACCOUNTS_FILE="$DEV_PRIVATE/accounts.txt"
export CLAUD2API_DATA_DIR="$DEV_PRIVATE/state"
export BIND_HOST=127.0.0.1 PORT=18081 CONTROL_BIND=127.0.0.1:18082
cargo run --locked
```

打开 `http://127.0.0.1:18082/`，使用刚生成的管理密钥登录。另一终端执行
`curl http://127.0.0.1:18081/health` 可检查本地状态健康。空账号池仍可能健康；此接口
不能证明上游已登录或模型可用。真实账号、数据库、截图和测试报告应位于发行源码之外，
开发实例不要连接生产状态目录。

## 4. 请求流程与资源归属

```mermaid
flowchart LR
  A[公开接口认证] --> B[令牌并发接入]
  B --> C[输入及模型投影]
  C --> D[会话预留]
  D --> E[账号租约及频率限制]
  E --> F[账号模型目录校验]
  F --> G[上下文投影及上游生成]
  G --> H[输出校验及持久提交]
  H --> I[JSON 或 SSE 响应体]
```

修改此链路时应同时阅读 `web::messages`、`Engine::reserve_owned`、`Engine::run`、
`turn::attempt` 和 `Store::mutate`。会话归属通过 active ticket 标识代际，防止旧请求的
取消任务覆盖新预留。上游读取、取消及响应传输都是生命周期的一部分，返回响应头不代表完成。

| 资源 | 必须覆盖的生命周期 |
|---|---|
| 令牌并发名额 | 直至响应体完成或丢弃，包括未读取的 JSON／SSE |
| 账号租约 | 上游工作、读取器终止及有界清理结束 |
| 会话所有者 | 成功提交或取消清理结束，提交时校验 active ticket |
| 选定模型／配置 | 为当前接入请求捕获；后续修改不得重命名执行中响应 |

所有公开兼容接口共用认证和账号调度。环境网关密钥具有独立会话空间，不额外设置令牌并发
信号量，但仍受账号与账号池限制。

## 5. 调度、取消与失败处理

每个账号复用一个 `Semaphore(1)`。同一 Session Key 的不同 Cookie 写法归为同一账号。
全局或令牌并发设置不能突破此上限。一个账号可绑定多个会话，空闲绑定不占请求名额。
普通请求遇忙返回 429，不会为寻找空闲账号而迁移已经绑定的会话。

默认请求开始间隔为 5 秒、滚动一分钟最多 6 次。模型查询和远端清理同样使用账号接入限制。
暂停、冷却与频率状态跨重启保留；其他实例或交互式浏览器不会共享本进程的账号锁。

`Scope::wait` 同时响应取消和截止时间。丢弃响应体会取消工作。释放账号前必须停止并等待
读取器；对结果不确定的远端会话保留清理责任，不能假装请求从未发送。清理器每 15 秒运行。
退出时有界停止监听器和清理器，并刷新状态日志。

| 情形 | 要求 |
|---|---|
| 输入非法、模型未公开或不可用 | 生成前返回 400 |
| API 令牌无效、过期或删除 | 返回 401，不请求上游 |
| 使用其他令牌的不透明会话句柄 | 返回 403，不复用历史 |
| revision 过期或账号忙时修改传输 | 返回 409，保留现有配置 |
| 并发或请求频率超限 | 返回 429 及有界重试信息 |
| 明确的生成前额度拒绝 | 设置冷却；Messages 可尝试迁移 |
| 账号认证失败或访问挑战 | 暂停账号，等待人工检查 |
| 部分输出、传输失败或生成结果不确定 | 不自动重放 |
| 快照持久化异常 | 阻止新的账号工作，不覆盖状态 |

遵守有效 `Retry-After`。缺少重置时间时默认 18000 秒仅是保守的本地冷却，不证明上游真的在
5 小时后重置。整个账号池仍可能不可用，不应承诺永不中断的生成。

## 6. 会话、迁移与上下文

Messages 将持久逻辑会话身份、当前物理网页会话／父消息，以及可选受保护摘要检查点分开维护。
详见[粘性会话](STICKY_SESSIONS_ZH.md)和[上下文管理](CONTEXT_MANAGEMENT_ZH.md)。

- 显式会话标识优先；没有标识时，只有唯一且精确匹配的已完成历史前缀才能续接。
  system 文本或首条用户内容不足以识别会话；不同令牌的会话空间始终隔离。
- 客户端继续发送完整的受支持历史。验证成功的续聊只向上游发送新增轮次。
  成功提交才刷新默认 3600 秒的会话有效期，失败预留不续期；活动工作不会被清理器过期删除。
- 更换模型、system、工具或修改已验证前缀可能重建物理会话。旧会话由原账号排队删除，
  不得跨账号复用网页会话 ID 或父消息 UUID。
- 明确的生成前额度拒绝可在另一个合格账号上创建新物理会话。优先使用有效检查点及未覆盖历史，
  否则使用完整受支持历史重建。每轮每账号最多尝试一次，目标成功后才替换原绑定。
- 管理员在两轮之间停用绑定账号时，下一轮也可改选合格账号；已接入工作正常排空。
  访问暂停不同于主动停用，不能通过迁移绕过人工检查。
- 迁移目标忙时可在请求截止时间内等待。流式等待可发送 ping，不伪造助手文本；断连取消等待。
  部分输出和结果不确定的生成不能进入此重放路径。
- 压缩保留原始 system／工具定义、当前输入及近期完整文本／工具单元。默认达到保守物理预算
  128000 单位的 75% 时考虑压缩，保留最近 6 个完整单元；工具调用与结果不能跨切点分开。
- 内部摘要也是一次受账号调度的生成。必须验证严格 JSON、大小、前缀 HMAC 及实际缩减后才保留
  检查点。摘要有损且消耗额度；无效摘要保留原状态并退避，不能出现在业务响应中。

Messages 持久会话与 Chat／Responses 的进程内 `conversation_id` 映射不同。旧接口映射按
令牌隔离，在线升级及回退时通过内存交接；普通重启或强制崩溃后不保留，
也不走 Messages 的压缩或透明额度迁移流程。

## 7. 持久化与凭证边界

`Store::mutate` 先复制快照并执行受校验变更，再序列化加密，在一个 SQLite 事务内提交 vault
及可选审计事件，成功后才替换内存快照。必须保持这个顺序，不能在写入失败后让内存继续宣称
未落盘的配置或会话已生效。

快照使用带认证的 AES-256-GCM，密钥由网关密钥派生，使用随机 nonce 和版本化域／AAD。
SQLite 还包含事件、用量和日志元数据表，**整个数据库并非全库加密**。这些元数据不得存储凭证、
提示词、工具参数／结果或摘要正文。加密快照确实包含账号凭证、代理秘密和有界交接摘要，
因此备份仍属于敏感资料。

五个网页原生工具开关单独写入 `native_tool_settings` 表，不加入旧版无法解析的新字段，保持 alpha4
仍可读取加密快照以便兼容回退。没有已保存开关值时按默认开启处理；新值与快照在同一 SQLite 事务中提交。

Linux 状态目录使用 0700，数据库与锁文件使用 0600；存储启用单实例文件锁和 SQLite WAL／FULL。
停止实例后，将完整状态目录与对应网关密钥一起备份。直接修改该密钥会导致解密失败；撤销普通
API 令牌无须轮换网关密钥。

保留经过认证的旧格式解密和可选一次性 version1 JSON 导入。恢复时退役结果不确定的会话并保留
清理责任。损坏状态或错误密钥须拒绝启动，不能用空库替换无法读取的数据库，也不能让新旧实例
同时使用同一批真实账号。

## 8. 模型、工具与上游传输

模型解析先匹配精确公开 ID，再进行一次 `c2a-` 归一化。精确停用项必须拒绝，不能回退到原生别名。
解析原生目标后，按实际执行账号验证权限。自定义模式下，各种响应均保留当前请求捕获的下游 ID。
`/v1/models` 代表单个所选账号，不是整个账号池权限的并集。详见[控制接口](CONTROL_API.md)。

管理员主动获取上游目录时，使用现有账号锁、浏览器配置及代理，最长等待 8 秒。配置轮询仅读本地。
获取仅保存观测，不修改模型映射或其 revision；控制台将缺少的模型加入未保存草稿，保存是独立操作。

客户端工具通过带随机分隔符的文本协议表达，并按声明的 schema 校验。Messages 还可按管理员设置和请求配置开放
网页搜索／读取、远端 Artifacts、代码／文件沙箱、网页端 37 种信息与交互组件，以及指定项目知识搜索。
项目知识须绑定一个规范 UUID，不能枚举账号私有项目。原生工具在 Claude 远端执行；网关不在本机执行命令，也不伪造结果。
`claud2api_native` 内容块必须经协议校验并在完整历史中原样往返；原生调用与结果须在同一 assistant 轮配对。
控制台配置、项目 UUID 和请求选择进入会话指纹，内部摘要关闭原生工具。远端文件只通过同令牌所有者、加密且限时的
`claud2api_download_url` 读取，下载限 32 MiB；对应上游会话会保留到链接过期。工具 input 中的 `thinking` 属于业务数据，不能递归删除。
禁止加载远程／文件 schema；新增内容块必须明确实现协议，不能笼统强转。

浏览器配置必须对应 `wreq`／`wreq-util` 实际支持的组合：Chrome Windows 146–152、Firefox Windows
144–151。更换配置要求账号空闲，并保留账号身份、原信号量、会话／检查点／缓存状态和 revision 校验。
只有固定依赖确实支持时才能扩充版本。

每账号代理覆盖上游请求与响应。先禁用环境代理，再应用明确配置的 HTTP／HTTPS／SOCKS5／SOCKS5H
地址。校验错误不能回显凭证，回读只提供脱敏概要。`socks5` 本地解析，`socks5h` 代理解析。
上游 API 重定向或访问挑战不能作为自动登录恢复处理。

## 9. 用量、模拟缓存与日志

以下口径必须区分：

| 计量 | 依据与用途 |
|---|---|
| 逻辑 API 用量 | 完整逻辑输入／输出的字符估算 |
| 物理上下文 | 依据 UTF-8 字节的保守估算，用于压缩决策 |
| 上游日志用量 | 每次尝试实际发送的 prompt 与收到的输出片段 |
| 模拟缓存 | 本地逻辑前缀拆分，不观测上游 KV 缓存 |
| 模拟费用 | 每条用量记录保存的原生模型费率与类别快照 |

模拟缓存和粘性会话虽默认均为 1 小时，但属于独立字段。缓存成功时延长 `cache_until`；只有开启模拟、
前缀匹配且未过期的续聊才能读取。重建、迁移或压缩后重新模拟创建。不能将完整输入和缓存类别重复计入。
开关只影响新记录，不改写历史价格。详见[模拟缓存](SIMULATED_CACHE_ZH.md)和[用量说明](USAGE_ANALYTICS.md)。

状态日志保留请求模型、令牌归属／名称、账号、方向、阶段、状态、字节／分块，以及可选 Token 估算。
下游输入已包含缓存创建／读取；只有响应体成功完整结束才发布最终输出。上游按各次尝试分别统计 prompt
及收到的 text／thinking／partial-JSON 片段。失败且未知的输出为 `null`，正常结束且为空才可为 0；
部分输出必须明确标记未完成。

状态日志最多保留 10000 条／7 天；用量账本属于另一张表，不按此窗口截断。
独立 Bot `/usage` 汇总仍有 10000 条上限并返回 truncated；完整账本统计使用 `/analytics`。统计采用配置的 IANA 日历
边界与冻结的整数纳美元金额。未知价格或旧归属保持未知／未计价，不能解释为免费或重新分配给现有用户。
日志不存聊天、凭证或摘要正文。队列溢出与日志故障必须可观测，不应仅为保住日志而破坏无关生成。

## 10. 控制台与 Bot 集成

独立控制监听器只能绑定回环地址。管理密钥和可选 Bot 密钥与公开 API 凭证分离。写入需
`expected_revision`；主动目录观测不修改配置 revision，因而是例外。409 时保留用户草稿并重新审核，
不能覆盖其他操作的修改。代理凭证只写入；API 令牌明文只在创建响应中出现一次。

`ui/app.js` 管理登录／退出、页面、串行轮询和未保存表单；`ui/charts.js` 管理图表；
`ui/selects.js` 管理统一且可访问的下拉弹层。登录后默认每 20 秒刷新。保留 abort／epoch 防护、
未保存草稿、一次性令牌展示、中英数字统一字体和纯深色样式。用户数据避免写入原始 `innerHTML`。
弹层必须保持在视口内，动态控件移除后也要移除观察器。

后续通知服务可用 Bot 只读密钥轮询 `/control/v1/info`、`/usage`、`/events`。此密钥不能读取账号管理、
日志、analytics、模型配置或令牌。游标持久化及去重由外部程序完成。当前没有内置推送 Bot，也不保证
Webhook 投递／重试；集成时仍应通过 SSH 或本机访问控制端口。

## 11. 验证方法

在仓库根目录、上述 Linux 工具链中运行：

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked --release
node --check ui/app.js
node --check ui/charts.js
node --check ui/selects.js
node --check ui/versions.js
python3 scripts/update_acceptance.py --artifacts /tmp/claud2api-update-acceptance --build
```

Node 只用于这些语法检查，服务运行不依赖 Node。测试数量以当前 Cargo 报告为准。
兼容接口按输入、输出、错误和用量需求验证，不再与旧输出快照逐字比较。

| 测试范围 | 关键断言 |
|---|---|
| [tests/protocol.rs](tests/protocol.rs) | 状态指纹、结构化封装、system 角色、工具／schema 引用、分片 SSE／工具帧及摘要安全切点 |
| [tests/config.rs](tests/config.rs) | 非法 locale、控制绑定和导入路径拒绝启动 |
| [tests/browser.rs](tests/browser.rs) | 精确浏览器目录及代理校验 |
| [tests/gateway.rs](tests/gateway.rs) | 账号／令牌生命周期、JSON／SSE、模型与目录、取消、迁移／压缩、重启状态、日志及统计 |
| [src/updates/](src/updates/) | SemVer、发布资产、条件请求／退避、兼容性、数据库锁、备份恢复及安装历史 |
| [scripts/update_acceptance.py](scripts/update_acceptance.py) | 真实工作进程升级／回退、进行中的 SSE、Messages／Chat 绑定与数据保留、失败候选及重启 |

失败、截断、额度、时序和取消测试使用本地模拟上游，无须真实账号。UI 修改还需在桌面和窄屏浏览器验证：
登录、轮询、草稿保留、令牌保密、连接恢复、下拉键盘交互、空图表及无横向溢出。

发行时构建实际 Dockerfile，并对最终镜像使用模拟上游验收：非 root／只读运行、回环管理隔离、
严格 SDK JSON／SSE、缓存开启／读取／关闭、Token 日志口径、令牌权限和强制重启持久化。
仓库目前没有覆盖全部 SDK／浏览器验收的一键命令，应另行记录实际脚本、镜像 ID 和结果。
真实上游测试须另获授权并使用隔离账号；生产账号、其他网关和客户端不作为测试 fixture。

## 12. 扩展检查表

| 修改 | 需要检查的位置与行为 |
|---|---|
| 新公开字段／内容块 | `protocol.rs` 投影校验、`web.rs` 序列化、工具语义、脱敏错误、JSON／SSE 及双语 API |
| 新模型／别名行为 | `models.rs`、实际账号目录、六类响应格式、精确停用项优先级 |
| 新浏览器配置 | 固定依赖支持、`browser.rs` 映射、UA／Client Hints／TLS 一致性、忙碌拒绝及状态连续性 |
| 新持久字段 | Serde 默认值、加载校验、事务顺序、旧 vault／导入、重启及损坏处理 |
| 新价格条目 | 明确原生 ID 和带日期来源、`usage.rs`、分档／缓存计算及未知价格情形 |
| 新控制操作 | 权限、revision、校验、脱敏审计、UI 草稿／竞态及 Bot 隔离 |
| 新日志字段 | 请求／尝试归属、隐私、旧值为 null 的兼容、筛选及响应终态语义 |

为一项契约保留单一实现归属，在各接口复用。对可观测行为及失败边界增加回归测试；不能因为接受了
可选字段，就悄悄扩大对外能力声明。

## 13. 发行与维护

1. 同步更新 Cargo 版本／锁文件本包条目、UI、README、更新记录及 Compose 镜像标签。
   运行时版本从 Cargo 派生。
2. 完成与修改相关的测试和最终镜像验收，记录平台、工具链、镜像／二进制哈希及尚未验证的限制。
3. 修改依赖后执行 `cargo metadata --locked --format-version 1`，在能够读取依赖源码路径的环境中
   用 `scripts/license_inventory.py` 重新生成许可清单，并审核许可文件与校验和变化。
4. 检查候选源码树和 Git 状态，排除凭证、状态、构建产物、截图和私有报告，保留 MIT 及必要第三方声明。
   分支／PR CI 验证并构建产物；推送与版本匹配的标签会触发发布工作流，校验草稿资产后正式发布。
   两个工作流均不部署生产。
5. 兼容更新与回退使用[版本管理](UPDATES_ZH.md)，保留当前数据。完整镜像／数据格式迁移须另行准备
   停机状态备份、匹配的密钥及已验证的恢复方案。不能清空冷却、丢弃无法读取的状态，或将陈旧备份
   覆盖到新请求之上，使更新看似成功。
6. 同步双语文档与[文档索引](DOCS_ZH.md)，保持已有文件名及链接有效。交付审阅的修改应在约定的
   审阅／提交步骤完成后再提交。

## 14. 按边界排查故障

| 现象 | 优先检查的证据 |
|---|---|
| 启动失败 | 脱敏启动错误、密钥分离、绝对状态／导入路径、文件锁、数据库及原生依赖 |
| 客户端返回 400 | 请求模型和安全输入诊断，区分可丢弃控制字段与非法语义数据 |
| 账号看似空闲但返回 429 | 令牌／全局接入、账号频率／冷却及实际绑定会话 |
| 重启后仍暂停 | 持久访问暂停；人工检查后明确 resume，重启不等于恢复 |
| SSE HTTP 200 后失败 | SSE 终止事件及关联的上下游请求 ID |
| Token 数量不同 | 完整逻辑输入与每次 prompt 的区别、缓存是否已包含及 complete／partial／null |
| 历史金额异常变化 | 冻结计价／开关、查询时区／窗口及是否误做追溯重算 |
| 表单回退或冲突 | 未保存草稿和共享 revision，避免再发不同步写入 |

仅收集允许的元数据与可复现模拟输入。问题报告不能附带真实密钥、Cookie、原始 HAR 或私有聊天正文。

## 在线版本管理

`src/updates/` 持有发布目录、完整性校验、单写入工作进程和 SQLite 备份恢复；
`ui/versions.js` 提供版本弹窗及保留内存登录的重新载入。使用步骤见
[更新指南](UPDATES_ZH.md)，以下说明开发、发布与恢复细节。变更此路径需运行集成升级验收，
并核对原有协议回归。

### 版本检查

后端启动时及每 300 秒检查公开 GitHub Releases。已连接的面板每 30 秒读取后端更新状态，
任务进行中轮询进度。所有面板共用一个后端目录及检查预算，不会因面板数量增加 GitHub
轮询次数。手动检查最小间隔 30 秒，使用 ETag 避免重复传输。网络失败、尚无发行版或
限流不影响正常服务；按响应头将重试延后 5 分钟至一天。不会向 GitHub 发送 GitHub
访问令牌、网关密钥或管理密钥。

同一发行线补丁默认自动安装：`0.1.0-alpha.6 → 0.1.0-alpha.6.1`，或正式版
`1.2.3 → 1.2.4`。跨 alpha/beta/rc 轮次、跨正式版 minor/major 需管理员确认。
有更高功能版本时，仍独立查找当前线补丁。发布者须只把兼容修复归入补丁编号。
策略及未完成安装记录保存在 `updates/policy.json`，切换前先写记录，成功后移除；
失败或中断后不会自动反复尝试该版本，之后的新补丁仍可安装。手动回退持久关闭自动补丁。
成功安装更高版本后，清理已经不再属于当前自动候选范围的失败记录。
管理员可在面板修改开关或手动重试；策略使用独立 `revision`，不改业务设置版本。

### 运行方式与数据连续性

Linux AMD64 程序默认启动固定监听公开及控制端口的守护进程，单个工作进程持有 SQLite
和上游会话，面板资源随工作进程内嵌。更新使用经过校验的 Linux 程序替换工作进程，
不需要 shell、Git 工作目录、容器控制接口或 root。下载与等待空闲窗口时正常处理请求。
进行中的 HTTP/SSE 响应持续使用原工作进程，直到完成或断开；30 分钟仍无空闲窗口则
本次更新失败，现有流量继续运行。

空闲窗口内，守护进程在内存导出兼容接口会话绑定，停止旧写入者，创建一致 SQLite
备份，启动候选版本并验证健康，导入绑定后提交所选版本。账号、名称、代理、令牌密钥
及摘要、已保存设置、粘性会话、检查点、模拟缓存、冷却、用量、事件及消息日志继续
使用原状态和加密密钥。正常回退保留当前数据，不会把用量回滚到旧发行版的备份。
候选启动失败时恢复本次切换前的备份并重启原版本；持久事务日志处理切换中断，
守护进程退出时工作进程通过标准输入 EOF 结束，避免残留写入者。强制杀进程或主机
崩溃仍可能像普通重启一样丢失临时兼容接口绑定；Messages 持久会话保存在 SQLite。

停止、备份及启动期间，新请求会短暂排队，不保证零延迟。最多容纳 256 个代理响应，
等候最多 60 秒后返回 503，避免无限等待。完整状态目录必须可写、允许执行程序
（不能挂载 `noexec`），并归服务用户所有。预留三个安装版本、两份完整数据库备份及
一个下载暂存文件的空间；更新程序包上限 128 MiB。容器根目录只读及 `/tmp` 的
`noexec` 设置仍可保留。

### 发布源与兼容范围

默认源为 `Saito-Sakuya/Claud2Api`，可选启动配置：

| 变量 | 默认值 | 用途 |
|---|---|---|
| `CLAUD2API_UPDATE_REPOSITORY` | `Saito-Sakuya/Claud2Api` | 信任的公开 owner/repo；修改会改变程序代码供应方 |
| `CLAUD2API_UPDATE_DISABLED` | `false` | 精确设为 `true` 时停用检查及在线版本切换 |

Compose 转发这两个变量，不支持私有仓库。只提供有效 SemVer 标签、已发布且具备完整
校验资产的版本。正式版不升级到预发行版；预发行安装可更新到更高的预发行版或正式版。
回退提供最接近当前版本的两个更低已发布版本。已删除、不兼容或无法下载的资产可能
使已有选项在应用时安全失败。目录最多检查 500 项，历史截断时在弹窗提示。

必须包含 `claud2api-linux-amd64` 与 `claud2api-update.json`。清单绑定版本、协议、
运行时 ABI、数据格式、大小及 SHA-256；同时验证 GitHub 资产摘要、下载大小、受信任
HTTPS 跳转域名、ELF 架构及程序自己的构建描述。这些完整性校验信任配置仓库，不是
独立的发布者签名。目前协议为 1，运行时 ABI 为 `linux-amd64-glibc-2.36`，数据格式为
`sqlite-v1/Claud2Api-vault-v1`。拒绝自动迁移数据库格式、ABI 或守护协议。
此类变更需另行验证迁移及备份方案后替换完整容器或基础程序。守护进程仍是初次安装
的版本，弹窗接口同时提供守护版本及当前工作版本。
自动补丁需要新守护进程；旧安装须保留原状态与密钥，更新一次容器或基础程序。
仅热更新工作进程无法给旧守护进程增加策略能力，面板会提示。

所选程序及最近两个安装程序保存在 `updates/versions/`，沿用同一卷重启或重建容器后
仍使用所选版本。安装缓存历史与弹窗中的两个已发布回退选项是独立概念。
重启旧镜像不会覆盖已经保存的选择。手动恢复时先停止实例，备份完整状态及匹配的
密钥和配置，仅移除 `updates/`，再启动预期且兼容的基础镜像。进程运行时不可移除
`control.sqlite3`、其 WAL/SHM 或 `updates/`；正常回退不可用陈旧数据库覆盖已接受请求。

### 构建与发布

main 的 push 工作流执行格式、Clippy、测试及隔离升级验收，再构建 Linux 程序并生成
与版本对应的程序包、散列及来源记录。PR 产物不能用于发布。标签工作流最多等待 30 分钟，
只复用完全相同提交、成功的可信 main run/attempt 产物，验证来源与全部散列后构建并验收
最终容器；不重复 Rust 测试和编译。没有合格产物则拒绝发布。
推送标签必须等于 `v` 加 Cargo 包版本。先建立草稿，上传程序、
清单和 `SHA256SUMS`，核验 GitHub 摘要后才发布。上传失败保留草稿，重试可替换草稿
资产，绝不修改已经发布的版本。后续每次发行均需新版本和标签；仅上传源码不会产生
可升级或回退的发行版，需先发布实际兼容版本。

本地打包，不会发布：

```sh
cargo build --locked --release
python3 scripts/package_release.py --binary target/release/claud2api
```

集成验收使用真实工作进程、合成的本地发行版及本地上游服务，不调用真实提供商且
无需账号凭证：

```sh
python3 scripts/update_acceptance.py --artifacts /tmp/claud2api-update-acceptance --build
```

覆盖进行中的 SSE、继续对话及缓存、账号令牌持久化、两个版本回退、重启版本选择、
损坏候选数据库的恢复、无效资产、权限及限流退避。构建演练入口仅调试版可用，正式
程序拒绝 `CLAUD2API_UPDATE_TEST_API`。
