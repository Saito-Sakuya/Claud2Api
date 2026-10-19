<p align="center">
  <img src="ui/icon.svg" alt="Claud2Api Logo" width="112" height="112" />
</p>

<h1 align="center">Claud2Api</h1>

<p align="center"><strong>v0.1.0-alpha.6</strong></p>

<p align="center">
  <a href="DEPLOY.md"><img src="docs/badges/linux-amd64.svg" alt="支持系统：Linux AMD64" height="26" /></a>
  <a href="DEPLOY.md#docker-compose"><img src="docs/badges/docker-compose.svg" alt="部署方式：Docker Compose" height="26" /></a>
</p>

中文 · [English](README_EN.md) · [文档索引](DOCS_ZH.md)

Claud2Api 提供兼容 Messages API 的服务及私有管理控制台，支持账号管理、浏览器配置、
账号代理、模型配置、令牌管理、会话管理、用量估算与消息状态日志。
Messages 还可调用 Claude 网页端的搜索与读取、Artifacts、远端代码／文件工具、信息卡片及指定项目知识；具体能力取决于账号权限。

## 安装

推荐在 Linux AMD64 VPS 上使用 Docker Compose。控制台通过 SSH 隧道访问。

取得源码后，进入项目目录，准备配置文件：

```sh
cd Claud2Api
sudo install -d -m 700 /srv/claud2api-private
sudo install -d -m 700 -o 65532 -g 65532 /srv/claud2api-state
sudo python3 scripts/generate_key.py /srv/claud2api-private/gateway-key.txt
sudo python3 scripts/generate_key.py /srv/claud2api-private/admin-key.txt
sudo touch /srv/claud2api-private/accounts.txt
sudo chmod 600 /srv/claud2api-private/*.txt
sudo chown 65532:65532 /srv/claud2api-private/*.txt
```

创建 `/srv/claud2api-private/settings.env`，内容如下：

```dotenv
CLAUD2API_STATE_DIR=/srv/claud2api-state
GATEWAY_API_KEY_FILE=/srv/claud2api-private/gateway-key.txt
ADMIN_API_KEY_FILE=/srv/claud2api-private/admin-key.txt
CLAUDE_ACCOUNTS_FILE=/srv/claud2api-private/accounts.txt
GATEWAY_PORT=18081
CONTROL_PORT=18082
SIMULATED_CACHE_ENABLED=true
CLAUDE_TIMEZONE=Asia/Taipei
CLAUDE_LOCALE=zh-CN
```

保护配置并启动：

```sh
sudo chmod 600 /srv/claud2api-private/settings.env
sudo docker compose --env-file /srv/claud2api-private/settings.env pull
sudo docker compose --env-file /srv/claud2api-private/settings.env up -d --no-build
curl http://127.0.0.1:18081/health
```

账号文件可留空，启动后从控制台导入。也可以每行填写一个本人有权使用的 Session Key
或完整 Cookie。请勿将账号凭证、代理密码或密钥提交到 Git。

原生安装及更多启动参数见 [安装说明](DEPLOY.md)。

在线版本检查、升级与回退见 [版本管理](UPDATES_ZH.md)。镜像标签和下载文件用途见[发行文件说明](RELEASES_ZH.md)。

## 管理控制台

在本机建立 SSH 隧道：

```sh
ssh -N -L 18082:127.0.0.1:18082 your-vps
```

打开 `http://127.0.0.1:18082/`，使用 `admin-key.txt` 内的独立管理密钥登录。

- **账号池**：支持 `账号名:Session Key` 批量导入、编辑名称及每账号独立 5h 本轮／上轮用量；以紧凑 Token 数和折合 $ 展示。空闲时选择 Chrome / Firefox 版本及独立代理，也可多选统一分配代理。
- **模型配置**：按账号获取完整上游模型列表，自动将全部新模型补入草稿；保留已有别名、修改和停用项，支持选择真实模型，保存后生效。原生模式直接开放账号返回的全部模型。
  支持下游 ID、显示名称、别名映射和启停；响应 `model` 使用所选下游 ID。
- **令牌管理**：新增、编辑、删除 API 令牌，调整名称、并发数和有效期时保留原密钥；完整令牌只在创建时显示一次。
- **粘性会话**：查看状态并清理空闲会话，默认有效期为最后成功请求后 1 小时。
- **运行配置**：修改会话、请求及调度参数，逐组开关网页原生工具；模拟 1h 缓存展示默认开启。
- **版本管理**：启动及每 5 分钟检查更新，新版以橙黄色发光提示；支持兼容版本一键升级、回退最近两个已发布版本，保留数据并自动重新加载面板。首次发行没有历史版本时不显示回退选项。
- **消息日志**：以紧凑双行显示输入／输出 Token 与模拟缓存读取／写入（`1h`）；普通输入扣除缓存部分，悬停查看精确计数。查看上下游 JSON / SSE 状态及令牌名称；按全部、错误、正常或账号筛选，未知与未完成输出单独标记。
- **运行概览**：按令牌查看用量曲线、占比和模拟 USD 费用；默认最近 24h，可选择今天、7d、本月、上月或运行以来。
  曲线支持 24 个小时点或 7 个日点。登录成功后默认每 20 秒刷新，可调整刷新间隔。

代理 URL 支持 `http://`、`https://`、`socks5://`、`socks5h://` 及相应认证信息。
修改浏览器或代理不会主动清除已有会话及有效缓存。运行中的账号需等待请求结束再修改。
管理控制台应仅通过 SSH 访问，禁止配置公网反向代理。

模拟费用按记录当时的模型单价、输入／输出、1h 缓存创建与读取量计算。缓存关闭后新增请求按普通输入估价，
历史金额不变；首次创建缓存可能增加模拟费用。无法确认价格或旧记录缺少信息时会显示未计价，
用量与费用均非官方账单。口径与例子见 [用量与模拟费用](USAGE_ANALYTICS.md)。

## API 使用

Base URL 为服务根地址，例如 `http://127.0.0.1:18081`。客户端使用 `gateway-key.txt`
内的密钥，或控制台创建的 API 令牌；不能使用管理密钥或账号凭证。

```python
from pathlib import Path
import time
from anthropic import Anthropic

client = Anthropic(
    base_url="http://127.0.0.1:18081",
    api_key=Path("/private/gateway-key.txt").read_text().strip(),
    max_retries=0,
)
models = client.models.list()
model_id = models.data[0].id
time.sleep(5)
message = client.messages.create(
    model=model_id,
    max_tokens=1024,
    messages=[{"role": "user", "content": "Hello"}],
)
print(message.content)
```

示例需要 Python `anthropic` 包。模型查询也占用账号调度，示例按默认间隔等待 5 秒；
若已修改间隔，请相应调整，并处理 429／Retry-After。

默认使用账号实际支持的原生模型名，例如 `claude-haiku-5-5`、`claude-sonnet-5-5`。
需要在中间服务中单独区分本服务时，可使用 `c2a-claude-haiku-5-5`、`c2a-claude-sonnet-5-5`；
两者调用相同真实模型，权限与额度相同。启用自定义模型配置后，请使用 `/v1/models` 中展示的 ID。
`c2a-` 是模型名前缀，与 `c2a_tok_` 令牌前缀不同。

JSON、流式响应、工具调用及模型名用法见 [中文 API 文档](API_ZH.md) / [English](API.md)，
管理及只读信息接口见 [控制台接口](CONTROL_API.md)。

## 免责声明

Claud2Api 是独立项目，非 Anthropic 官方产品，与 Claude / Anthropic 无隶属、合作或背书关系。
Claude 名称及相关商标属于其权利人。使用者须拥有相应账号、数据及服务的使用授权，并遵守
适用法律及服务条款。本项目不保证账号安全、服务可用性、额度或真实缓存节省；页面统计仅供参考。
本版本处于 Alpha 阶段，使用前请备份必要数据。

## 许可证

[MIT](LICENSE)。第三方组件适用各自许可证，见 [第三方许可](THIRD_PARTY_ZH.md)。

## 文档

所有指南见[双语文档索引](DOCS_ZH.md)，开发与审阅请从[开发者文档](DEVELOPMENT_ZH.md)开始。
