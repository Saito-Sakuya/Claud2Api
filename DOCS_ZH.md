# 文档索引

中文 · [English](DOCS.md)

本文档组对应 **v0.1.0-alpha.4**。每份维护中的项目指南均有完整中英文对应版本。
原有默认文件名保持有效，可从下表或文档顶部切换语言。

## 阅读顺序

- **安装使用**：README → 安装与运行 → 公开 API。
- **日常运维**：控制台接口 → 粘性会话／上下文 → 用量／缓存。
- **开发审阅**：开发者文档 → 对应契约 → 源码与测试 → 发行检查。
- **中间服务集成**：公开 API → 桥接模型 ID → 参数兼容处理。

## 文档列表

| 主题 | 中文 | English | 主要内容 |
|---|---|---|---|
| 在线更新 | [版本管理](UPDATES_ZH.md) | [Version manager](UPDATES.md) | 检查新版、一键更新与回退 |
| 项目入口 | [README](README.md) | [README](README_EN.md) | 安装、控制台使用、API 入口及免责声明 |
| 开发者文档 | [开发者文档](DEVELOPMENT_ZH.md) | [Developer guide](DEVELOPMENT.md) | 架构、资源与状态归属、扩展、测试及发行 |
| 安装 | [安装与运行](DEPLOY.md) | [Installation](DEPLOY_EN.md) | 原生／Compose、完整环境变量及备份 |
| 公开 API | [API 文档](API_ZH.md) | [API contract](API.md) | 认证、模型、Messages／工具／SSE 及限制 |
| 控制接口 | [控制台接口](CONTROL_API.md) | [Control API](CONTROL_API_EN.md) | 账号、模型、令牌、设置、日志、统计及 Bot 读取 |
| 桥接模型 ID | [桥接模型 ID](BRIDGE_COMPATIBILITY_ZH.md) | [Bridge model IDs](BRIDGE_COMPATIBILITY.md) | 原生名、c2a- 及配置模型名 |
| 参数兼容 | [参数兼容](SUB2API_COMPATIBILITY_ZH.md) | [Compatibility controls](SUB2API_COMPATIBILITY.md) | 接受后丢弃的可选字段与保留的语义数据 |
| 粘性会话 | [粘性会话](STICKY_SESSIONS_ZH.md) | [Sticky sessions](STICKY_SESSIONS.md) | 身份、过期、账号绑定、额度迁移与持久化 |
| 上下文 | [上下文管理](CONTEXT_MANAGEMENT_ZH.md) | [Context management](CONTEXT_MANAGEMENT.md) | 摘要检查点、工具安全切点及事务化切换 |
| 模拟缓存 | [模拟缓存](SIMULATED_CACHE_ZH.md) | [Simulated cache](SIMULATED_CACHE.md) | 1h 展示类别、生命周期及限制 |
| 用量／费用 | [用量与费用](USAGE_ANALYTICS.md) | [Usage/cost](USAGE_ANALYTICS_EN.md) | 令牌归属、时间窗、冻结计价及示例 |
| 更新记录 | [更新记录](CHANGELOG.md) | [Changelog](CHANGELOG_EN.md) | 发行变更 |
| 第三方许可 | [第三方许可](THIRD_PARTY_ZH.md) | [Third-party licenses](THIRD_PARTY.md) | 依赖清单及许可声明维护 |
| 索引 | [文档索引](DOCS_ZH.md) | [Documentation index](DOCS.md) | 阅读顺序及双语导航 |

## 文档维护约定

只记录已实现行为，并区分估算与后续计划。两种语言的字段、默认值、单位、权限、错误码和示例须一致，
变更时同步维护。可复用契约放在相应专题，避免在 README 中复制不完整版本。新增指南应加入本表，
并验证本地文件／锚点链接和代码块语法。

根目录 [MIT 许可证](LICENSE)、依赖许可原文及机器可读清单保持原样，双语第三方说明不替代或改写这些
法律声明。实现细节放在开发者文档，README 继续专注安装和使用。
