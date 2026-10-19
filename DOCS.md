# Documentation index

[中文](DOCS_ZH.md) · English

The source version is **v0.1.0-alpha.7**. See the [changelog](CHANGELOG_EN.md) and [test summary (Chinese)](TEST_SUMMARY.md); GitHub Releases show publication status. Every maintained project guide has a
complete Chinese/English counterpart. Existing default filenames remain valid;
use the language links below or at the top of each guide.

## Reading paths

- **Install/use:** README → installation → public API.
- **Operate:** control API → sticky sessions/context → usage/cache.
- **Develop/review:** developer guide → relevant contract → source/tests → release checklist.
- **Integrate an intermediary:** public API → bridge model IDs → compatibility controls.

## Documents

| Topic | 中文 | English | Contents |
|---|---|---|---|
| Release files | [发行文件说明](RELEASES_ZH.md) | [Release files](RELEASES.md) | alpha4–alpha7, Docker tags and downloadable assets |
| Managed updates | [版本管理](UPDATES_ZH.md) | [Version manager](UPDATES.md) | Checking for updates, upgrading and rolling back |
| Overview | [README](README.md) | [README](README_EN.md) | Installation, console usage, API entry and disclaimer |
| Developer guide | [开发者文档](DEVELOPMENT_ZH.md) | [Developer guide](DEVELOPMENT.md) | Architecture, resource/state ownership, extension, testing and releases |
| Installation | [安装与运行](DEPLOY.md) | [Installation](DEPLOY_EN.md) | Native/Compose setup, complete environment reference and backup |
| Public API | [API 文档](API_ZH.md) | [API contract](API.md) | Authentication, models, Messages/tools/SSE and limitations |
| Test summary | [简要综测表](TEST_SUMMARY.md) | [Test summary (Chinese)](TEST_SUMMARY.md) | Tool checks, token usage and regression results |
| Control API | [控制台接口](CONTROL_API.md) | [Control API](CONTROL_API_EN.md) | Accounts, models, tokens, settings, logs, analytics and Bot reads |
| Bridge model IDs | [桥接模型 ID](BRIDGE_COMPATIBILITY_ZH.md) | [Bridge model IDs](BRIDGE_COMPATIBILITY.md) | Native, c2a- and configured model naming |
| Compatibility controls | [参数兼容](SUB2API_COMPATIBILITY_ZH.md) | [Compatibility controls](SUB2API_COMPATIBILITY.md) | Accepted/discarded optional fields and preserved semantic data |
| Sticky sessions | [粘性会话](STICKY_SESSIONS_ZH.md) | [Sticky sessions](STICKY_SESSIONS.md) | Identity, expiry, account binding, quota migration and persistence |
| Context | [上下文管理](CONTEXT_MANAGEMENT_ZH.md) | [Context management](CONTEXT_MANAGEMENT.md) | Summary checkpoints, tool-safe cuts and transactional rotation |
| Simulated cache | [模拟缓存](SIMULATED_CACHE_ZH.md) | [Simulated cache](SIMULATED_CACHE.md) | One-hour display categories, lifecycle and limitations |
| Usage/cost | [用量与费用](USAGE_ANALYTICS.md) | [Usage/cost](USAGE_ANALYTICS_EN.md) | Token owners, time windows, frozen quotes and examples |
| Changelog | [更新记录](CHANGELOG.md) | [Changelog](CHANGELOG_EN.md) | Release changes |
| Third-party licenses | [第三方许可](THIRD_PARTY_ZH.md) | [Third-party licenses](THIRD_PARTY.md) | Dependency inventory and notice handling |
| Index | [文档索引](DOCS_ZH.md) | [Documentation index](DOCS.md) | Reading paths and bilingual navigation |

## Documentation contract

Document implemented behavior and distinguish it from estimates or future work.
Match fields, defaults, units, scopes, error codes and examples across languages;
change both sides together. Keep reusable contracts in their dedicated reference
instead of copying incomplete variants into README. Add new guides to this table
and verify local file/anchor links and code-block syntax.

The root [MIT license](LICENSE), original dependency notices and the machine-readable
inventory retain their original content. The bilingual third-party guide explains
them without replacing or translating away the legal notices. The developer guide
explains internals; README remains focused on installation and usage.
