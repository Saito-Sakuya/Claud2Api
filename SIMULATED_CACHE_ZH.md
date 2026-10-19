# 模拟 1 小时缓存统计

中文 · [English](SIMULATED_CACHE.md) · [文档索引](DOCS_ZH.md)

`SIMULATED_CACHE_ENABLED=true` 在 Messages 响应中启用本地模拟展示。新安装默认 **true**，
已有保存设置保持原值。计数基于经过验证的粘性历史，既不观测上游 KV 缓存，也不证明额度、性能或金额节省。

启用后响应含 `X-Claud2Api-Cache: simulated-1h` 和 `cache-simulated` 限制标记。
JSON 与 SSE 的 `message_start.message.usage` 使用同样字段：

```json
{
  "input_tokens": 0,
  "cache_read_input_tokens": 800,
  "cache_creation_input_tokens": 200,
  "cache_creation": {
    "ephemeral_5m_input_tokens": 0,
    "ephemeral_1h_input_tokens": 200
  }
}
```

示例将 1000 个逻辑输入估算 Token 分为 800 个模拟读取和 200 个模拟 1h 创建，不重复计入。
值为 0 的顶层可选字段可能省略。首轮、新建、重建或过期会话读取为 0，创建量为本轮估算输入。
经验证的续聊最多读取已保存的逻辑前缀估算，新增部分模拟创建。

只有成功完成一轮才更新前缀估算。展示缓存独立滑动 1 小时，底层粘性会话过期或重建时也会移除。
缓存状态仅含估算和哈希，不含提示词正文；失败或中断不能建立有效缓存，失效历史不能产生命中。
客户端缓存提示接受后丢弃，不转发为上游原生控制。

输出 Token 同样为估算。下游应用可展示这些值，但不能作为上游真实计量或金钱节省的证明。
网关不会在助手正文中插入模拟警告。

主动上下文切换也会将模拟读取重置为 0，重新创建 1h 前缀。展示计数描述逻辑输入，与保守物理
上下文账本和摘要统计分开；二者都不能揭示真实剩余额度或原生缓存节省。
