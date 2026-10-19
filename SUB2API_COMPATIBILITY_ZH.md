# 参数兼容处理

中文 · [English](SUB2API_COMPATIBILITY.md) · [文档索引](DOCS_ZH.md)

在受支持的请求边界，可选 thinking、output_config、采样参数、停止序列、metadata／缓存提示和
严格生成注解接受后丢弃，不进入上游参数或提示词。文本、客户端工具、工具参数／结果和受支持的
tool_choice 仍须校验。

受支持的会话 metadata 可供本地粘性路由使用。未知内容类型仍报错，诊断仅使用固定允许的类型和路径。
assistant 历史中的 thinking／redacted_thinking 块可丢弃，保留其余受支持文本及工具数据。
这不表示具备原生思考或缓存能力。

详见 [API](API_ZH.md)、[粘性会话](STICKY_SESSIONS_ZH.md)及可配置的[模拟展示](SIMULATED_CACHE_ZH.md)。
