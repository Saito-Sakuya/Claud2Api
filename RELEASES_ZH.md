# 发行文件说明

中文 · [English](RELEASES.md) · [文档索引](DOCS_ZH.md)

当前版本：**v0.1.0-alpha.6**。支持 Linux AMD64；以下版本均为 Alpha 预发行版。

## 版本区别

- **alpha4**：Chat／Responses 文本兼容、持久会话、管理面板及容器打包修复。
- **alpha5**：在 alpha4 基础上增加网页原生工具、引用、生成文件下载和指定项目知识检索。项目检索已做模拟测试，尚未用真实项目验证。

- **alpha6**：每轮明确当前可用的原生工具名称，区分客户端与网页工具，纠正历史回复中“没有搜索工具”的误导。

## Docker 镜像

```sh
docker pull ghcr.io/saito-sakuya/claud2api:0.1.0-alpha.6
```

alpha4 的固定标签为 `0.1.0-alpha.4`，alpha5 为 `0.1.0-alpha.5`，alpha6 为 `0.1.0-alpha.6`，也接受相同版本的 `v` 前缀标签。
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
