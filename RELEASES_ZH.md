# 发行文件说明

中文 · [English](RELEASES.md) · [文档索引](DOCS_ZH.md)

当前源码版本：**v0.1.0-alpha.7**。支持 Linux AMD64；实际可下载版本以 GitHub Release 为准，以下均为 Alpha 预发行版。

## 版本区别

- **alpha4**：Chat／Responses 文本兼容、持久会话、管理面板及容器打包修复。
- **alpha5**：在 alpha4 基础上增加网页原生工具、引用、生成文件下载和指定项目知识检索。项目检索已做模拟测试，尚未用真实项目验证。

- **alpha6**：每轮明确当前可用的原生工具名称，区分客户端与网页工具，纠正历史回复中“没有搜索工具”的误导。

- **alpha6.1**：恢复中性工具声明；增加同发行线补丁自动更新和面板开关，功能版本仍需确认；发布复用已验证的 main 构建。
- **alpha7**：补齐图片／PDF／TXT 与真实客户端工具通道，修复原生结果兼容、续接与失败重试；211 项回归和真实客户端测试通过，具体范围见[综测结果](TEST_SUMMARY.md)。从 alpha6.1 升级需手动确认，先结束正在等待结果的工具任务。

## Docker 镜像

```sh
docker pull ghcr.io/saito-sakuya/claud2api:0.1.0-alpha.7
```

alpha7 镜像在该版本 Release 发布后提供；发布前可使用 `0.1.0-alpha.6.1`，或从源码构建。
历史固定标签为 `0.1.0-alpha.4`、`0.1.0-alpha.5`、`0.1.0-alpha.6`、`0.1.0-alpha.6.1`，也提供相同版本的 `v` 前缀标签。
请固定具体版本；安装配置按 [README](README.md)。镜像直接使用同一 Release 的已校验二进制。

## 下载文件

| 文件 | 用途 |
|---|---|
| `claud2api-linux-amd64` | 原生可执行程序，也供面板在线更新使用；普通 Docker 用户无需手动下载 |
| `claud2api-update.json` | 版本、平台、数据格式及程序 SHA-256，供更新器校验 |
| `SHA256SUMS` | 手动运行 `sha256sum -c SHA256SUMS` 校验下载文件 |
| `claud2api-licenses.tar.gz` | 本项目及随附依赖的许可证和第三方说明 |
| GitHub 自动提供的 Source code | 对应标签的源码，不是可执行程序或 Docker 镜像 |

手动安装时解压许可归档并与程序一起保留。更新与回退见[版本管理](UPDATES_ZH.md)，保留原数据目录和密钥。
