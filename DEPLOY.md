# 安装与运行

中文 · [English](DEPLOY_EN.md) · [文档索引](DOCS_ZH.md)

## Docker Compose

按 [README](README.md) 准备配置后执行：

```sh
docker compose --env-file /srv/claud2api-private/settings.env pull
docker compose --env-file /srv/claud2api-private/settings.env up -d --no-build
docker compose --env-file /srv/claud2api-private/settings.env logs --tail=100
```

Compose 面向 Linux，使用 host 网络并将 API / 控制台绑定到回环地址。
默认 API 端口 18081、控制台 18082。容器用户为 UID/GID 65532，状态目录需归该用户所有
且权限为 0700，密钥及账号文件需让该用户可读并使用 0600。容器默认限制内存 256 MiB；
账号较多或请求较大时应依据实际负载调整。

公网 API 仅反代 `/health`、`/v1/*` 到 `127.0.0.1:18081`。控制台端口只用于 SSH 隧道。
反向代理若运行在其他容器，请按主机网络拓扑设置地址，勿把代理容器自身回环地址作为服务地址。

默认拉取已发布的 Linux AMD64 镜像，无需在服务器编译。设置 `CLAUD2API_IMAGE` 可固定其他发行版本；例如 `ghcr.io/saito-sakuya/claud2api:0.1.0-alpha.4`。需要从源码构建时改用 `docker compose --env-file /srv/claud2api-private/settings.env up -d --build`。

发行文件与校验方法见[发行说明](RELEASES_ZH.md)。

## 原生构建

需要 Rust 1.99.1 或兼容版本，以及 C/C++ 工具链、CMake、Perl、pkg-config、libclang。
固定依赖版本不能替换为同版本号的其他源码。

```sh
sudo apt-get install build-essential cmake perl pkg-config libclang-dev
cargo build --locked --release
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

配置环境变量后运行 `target/release/claud2api`。原生运行时的 `PORT` 默认为 8080，
`BIND_HOST` 默认为 `127.0.0.1`，`CONTROL_BIND` 默认为 `127.0.0.1:18082`。

## 启动配置

下表为程序单独运行时的默认值。密钥的 `*_FILE` 设置优先于同名无后缀环境变量，文件最多 1 MiB；
密钥须为单行，去除首尾空白后至少 32 字节，三个角色的密钥必须互不相同。

| 变量 | 默认值／要求 |
|---|---|
| `GATEWAY_API_KEY` / `GATEWAY_API_KEY_FILE` | 必填，公开 API 认证及状态加密根密钥 |
| `ADMIN_API_KEY` / `ADMIN_API_KEY_FILE` | 必填，独立管理密钥 |
| `BOT_READ_KEY` / `BOT_READ_KEY_FILE` | 可选，只读 info／usage／events |
| `BIND_HOST` | `127.0.0.1`，必须为 IP 地址 |
| `PORT` | `8080`，非零端口 |
| `CONTROL_BIND` | `127.0.0.1:18082`，必须为回环 IP 及非零端口 |
| `CLAUD2API_DATA_DIR` | `/var/lib/claud2api`，必须为绝对路径 |
| `CLAUDE_ACCOUNTS_FILE` | `accounts.txt`，最多 1 MiB，空行和 `#` 注释忽略；明确指定时文件必须存在 |
| `CLAUDE_SESSION_FILE` | 可选单行凭证文件；设置后优先于账号列表和 Cookie／Session 环境变量 |
| `CLAUDE_COOKIE` / `CLAUDE_SESSION_KEY` | 可追加的环境账号凭证；Cookie 必须包含或配合提供 sessionKey |
| `STICKY_SESSION_STATE_FILE` | 可选既有 v1 JSON 导入文件，必须为存在的绝对路径 |
| `BROWSER_PROFILE` | `chrome_146_windows`，须为登记的浏览器配置 |
| `CLAUDE_BASE_URL` | `https://claude.ai`，HTTP(S)，禁止 URL 认证、query、fragment；测试可使用本地模拟地址 |
| `CLAUDE_LOCALE` | `zh-CN`，1–64 个 ASCII 字母、数字或 `-` |
| `CLAUDE_TIMEZONE` | `Asia/Singapore`，有效 IANA 时区 |
| `DEFAULT_MODEL` | `claude-sonnet-5`，旧文本兼容接口的缺省模型；Messages 仍须填写 model |
| `CLAUDE_THINKING` | 旧兼容路径默认自动；`none`、`disabled` 或 `{"type":"disabled"}` 关闭该旧标记 |
| `CLAUDE_EFFORT` / `CLAUDE_CODE_EFFORT_LEVEL` | 前者优先，默认 `medium`；接受 low／medium／high／xhigh／max，其余回退 medium |

账号按 Session Key 去重，上限 512 个；账号凭证不能与服务密钥相同。环境账号用于导入，不会把未再列出的
持久账号自动删除。运行时控制面可继续管理已导入账号。

### 可保存的运行参数

这些环境变量提供新状态的初始值；已保存的运行配置继续生效。布尔值使用 `true` 或 `false`。
非数字、无效布尔值或超出范围会导致启动失败；运行中可从控制台修改相应设置。

| 变量 | 默认值 | 范围／含义 |
|---|---:|---|
| `STICKY_SESSION_TTL_SECONDS` | 3600 | 60–604800 秒，成功一轮后的空闲有效期 |
| `REQUEST_TIMEOUT_SECONDS` | 300 | 1–900 秒 |
| `MAX_REQUEST_BYTES` | 4194304 | 1024–33554432 字节 |
| `SIMULATED_CACHE_ENABLED` | true | 模拟 1h 缓存展示 |
| `CONTEXT_COMPACTION_ENABLED` | true | 主动上下文压缩 |
| `CONTEXT_BUDGET_TOKENS` | 128000 | 4096–1000000，本地保守预算单位 |
| `CONTEXT_COMPACTION_TRIGGER_PERCENT` | 75 | 10–90 |
| `CONTEXT_KEEP_RECENT_ROUNDS` | 6 | 1–32 个完整文本／工具单元 |
| `NATIVE_WEB_SEARCH_ENABLED` | true | 开放网页搜索与读取工具组 |
| `NATIVE_ARTIFACTS_ENABLED` | true | 开放 Artifacts 工具组 |
| `NATIVE_REPL_ENABLED` | true | 开放远端代码与文件工具组 |
| `NATIVE_WIDGETS_ENABLED` | true | 开放网页端 37 种信息与交互组件 |
| `NATIVE_PROJECT_KNOWLEDGE_ENABLED` | true | 允许请求绑定的项目知识搜索；请求仍须提供项目 UUID |
| `ACCOUNT_MIN_INTERVAL_SECONDS` | 5 | 1–300 秒 |
| `ACCOUNT_REQUESTS_PER_MINUTE` | 6 | 1–60 次／滚动分钟 |
| `ACCOUNT_RATE_LIMIT_COOLDOWN_SECONDS` | 60 | 60–86400 秒，限额冷却下限 |
| `ACCOUNT_RATE_LIMIT_FALLBACK_SECONDS` | 18000 | 60–604800 秒，缺少有效 Retry-After 时使用 |
| `ACCOUNT_OVERLOAD_COOLDOWN_SECONDS` | 15 | 1–900 秒 |

### Compose 与程序默认值的区别

Compose 使用 `GATEWAY_PORT=18081`、`CONTROL_PORT=18082`，时区默认 `Asia/Taipei`，并固定两个监听器
为回环地址。`CLAUD2API_STATE_DIR` 必须指定宿主机受保护状态目录，映射到容器内 `/var/lib/claud2api`。
`CLAUD2API_UID`／`CLAUD2API_GID` 默认均为 65532；更改后须同步文件所有权。

`settings.env` 用于 Compose 变量插值，不会自动把任意变量传入容器。除原 Compose 明确声明的变量外，
需在 `compose.override.yaml` 的 environment 中配置，例如：

```yaml
services:
  claud2api:
    environment:
      BROWSER_PROFILE: firefox_151_windows
      ACCOUNT_RATE_LIMIT_FALLBACK_SECONDS: "18000"
      NATIVE_WEB_SEARCH_ENABLED: "true"
      NATIVE_ARTIFACTS_ENABLED: "true"
      NATIVE_REPL_ENABLED: "true"
      NATIVE_WIDGETS_ENABLED: "true"
      NATIVE_PROJECT_KNOWLEDGE_ENABLED: "true"
```

原生工具开关也可直接在控制台逐组修改；Compose 未默认传入上述环境变量。
它们只作为新状态的初始值，已保存的运行配置优先。

显式加载覆盖文件后启动：

```sh
docker compose --env-file /srv/claud2api-private/settings.env \
  -f docker-compose.yml -f compose.override.yaml up -d --build
```

可选 Bot 密钥也需要额外挂载受保护文件，并将 `BOT_READ_KEY_FILE` 设为其容器内路径。
仅在宿主机 env 文件写入该变量不会完成挂载。验证基础配置可用
`docker compose --env-file /srv/claud2api-private/settings.env config --quiet`，避免输出完整配置；
使用覆盖文件时再加入对应的两个 `-f` 参数。

## 浏览器与代理

浏览器和代理可在控制台逐账号设置。Chrome 146–152、Firefox 144–151 均为 Windows 配置。
代理留空即直接连接，不自动沿用系统代理。`socks5` 在本地解析目标域名，`socks5h` 由代理解析。
同一账号只应由一个实例使用；其他进程或浏览器不共享本服务的账号调度。

## 更新与备份

兼容版本的在线升级及回退使用 [版本管理](UPDATES_ZH.md)。状态挂载必须允许执行，
并预留三个程序、两份数据库备份和一个下载文件的空间，保留原密钥及配置。守护进程
不会随工作进程更新；协议、运行时或数据格式变化需要替换完整镜像。

替换完整镜像前停止该实例，备份整个状态目录、账号文件和对应网关密钥。读取原有状态需要保留
原网关密钥，直接换密钥会导致解密失败。保存过的运行配置优先于新的默认值。
同一状态目录只允许一个写入实例。兼容版本回退保留最新数据；只有灾难恢复或已验证的
数据迁移恢复才使用停机备份，必须核对备份后的新请求，不能覆盖运行中的实例。

状态读写失败时健康检查会报告异常并停止接受新的账号工作。账号访问失败需检查后在控制台
人工恢复，重启不会清除账号暂停或额度冷却。停用账号的远端清理会保留至重新启用后执行。

统计日历按 `CLAUDE_TIMEZONE` 计算，值须为有效 IANA 时区，例如 `Asia/Taipei`。
概览默认最近 24h；新记录按当时的缓存配置和价格保存模拟费用，历史记录不会追溯重算。

原生开发、测试、模块职责及发行检查见[开发者文档](DEVELOPMENT_ZH.md)。

### 在线更新配置

| 变量 | 默认值 | 用途 |
|---|---|---|
| `CLAUD2API_UPDATE_REPOSITORY` | `Saito-Sakuya/Claud2Api` | 信任的公开发布仓库 |
| `CLAUD2API_UPDATE_DISABLED` | `false` | 精确 true 停用在线检查与切换 |
