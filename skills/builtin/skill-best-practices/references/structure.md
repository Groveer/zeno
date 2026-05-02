# Skill 结构与命名规范

## YAML 前置事项

SKILL.md 前置事项需要两个字段：

### name

- 最多 64 字符
- 只能包含小写字母、数字和连字符
- 不能包含 XML 标签
- 不能包含保留字："anthropic"、"agent"

### description

- 必须非空
- 最多 1024 字符
- 不能包含 XML 标签
- 应该描述技能的功能以及何时使用它

## 命名约定

使用一致的命名模式使技能更容易引用和讨论。考虑对技能名称使用**动名词形式**（动词 + -ing），因为这清楚地描述了技能提供的活动或能力。

请记住，name 字段只能使用小写字母、数字和连字符。

### 好的命名示例（动名词形式）

```
processing-pdfs
analyzing-spreadsheets
managing-databases
testing-code
writing-documentation
```

### 可接受的替代方案

- **名词短语**：pdf-processing、spreadsheet-analysis
- **面向行动**：process-pdfs、analyze-spreadsheets

### 避免

- **模糊的名称**：helper、utils、tools
- **过于通用**：documents、data、files
- **保留字**：anthropic-helper、agent-tools
- **技能集合中的不一致模式**

一致的命名使得以下操作更容易：

- 在文档和对话中引用技能
- 一目了然地理解技能的功能
- 组织和搜索多个技能
- 维护专业、统一的技能库

## 编写有效的描述

description 字段启用技能发现，应该包括技能的功能和何时使用它。

### 始终用第三人称写作

描述被注入到系统提示中，不一致的视角可能会导致发现问题。

- **好的**："处理 Excel 文件并生成报告"
- **避免**："我可以帮助您处理 Excel 文件"
- **避免**："您可以使用它来处理 Excel 文件"

### 具体并包括关键术语

包括技能的功能和何时使用它的具体触发器/上下文。

每个技能恰好有一个描述字段。描述对于技能选择至关重要：Agent 使用它从可用技能列表中选择正确的技能。您的描述必须提供足够的细节，使 Agent 知道何时选择此技能，而 SKILL.md 的其余部分提供实现细节。

### 有效的示例

**PDF 处理技能：**

```yaml
description: 从 PDF 文件中提取文本和表格、填充表单、合并文档。在处理 PDF 文件或用户提及 PDF、表单或文档提取时使用。
```

**Excel 分析技能：**

```yaml
description: 分析 Excel 电子表格、创建数据透视表、生成图表。在分析 Excel 文件、电子表格、表格数据或 .xlsx 文件时使用。
```

**Git 提交助手技能：**

```yaml
description: 通过分析 git diff 生成描述性提交消息。当用户要求帮助编写提交消息或审查暂存更改时使用。
```

### 避免模糊的描述

```yaml
# 避免
description: 帮助处理文档
description: 处理数据
description: 对文件进行各种操作
```
