# 内容指南

## 避免时间敏感信息

不要包含会过时的信息。

### 坏的例子：时间敏感（会变成错误）

```markdown
如果您在 2025 年 8 月之前执行此操作，请使用旧 API。
2025 年 8 月之后，使用新 API。
```

### 好的例子（使用"旧模式"部分）

```markdown
## 当前方法

使用 v2 API 端点：`api.example.com/v2/messages`

## 旧模式

<details>
<summary>旧版 v1 API（已于 2025-08 弃用）</summary>

v1 API 使用：`api.example.com/v1/messages`

此端点不再受支持。
</details>
```

旧模式部分提供历史背景，而不会使主要内容混乱。

## 使用一致的术语

选择一个术语并在整个技能中使用它：

### 好的 - 一致

- 始终"API 端点"
- 始终"字段"
- 始终"提取"

### 坏的 - 不一致

- 混合"API 端点"、"URL"、"API 路由"、"路径"
- 混合"字段"、"框"、"元素"、"控件"
- 混合"提取"、"拉取"、"获取"、"检索"

一致性帮助 Agent 理解和遵循说明。

## 路径规范

始终在文件路径中使用正斜杠，即使在 Windows 上：

- **好的**：`scripts/helper.py`、`reference/guide.md`
- **避免**：`scripts\helper.py`、`reference\guide.md`

Unix 风格的路径在所有平台上都有效，而 Windows 风格的路径在 Unix 系统上会导致错误。

## 文件命名要有描述性

使用表示内容的名称：

- **好的**：`form_validation_rules.md`
- **不好的**：`doc2.md`

## 组织以便发现

按域或功能构建目录：

- **好的**：`reference/finance.md`、`reference/sales.md`
- **不好的**：`docs/file1.md`、`docs/file2.md`

## 明确执行意图

- "运行 analyze_form.py 以提取字段"（执行）
- "参见 analyze_form.py 了解提取算法"（作为参考读取）

## 不假设包已安装

### 不好的示例：假设安装

"使用 pdf 库处理文件。"

### 好的示例：明确说明依赖关系

```markdown
安装所需包：`pip install pypdf`

然后使用它：

```python
from pypdf import PdfReader
reader = PdfReader("file.pdf")
```
```
