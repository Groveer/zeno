# 渐进式披露模式

SKILL.md 充当概述，根据需要将 Agent 指向详细材料，就像入门指南中的目录一样。

## 实用指导

- 将 SKILL.md 正文保持在 **500 行以下** 以获得最佳性能
- 接近此限制时将内容分割为单独的文件
- 使用下面的模式有效地组织说明、代码和资源

## 完整的技能目录结构

```
pdf/
├── SKILL.md          # 主要说明（触发时加载）
├── FORMS.md          # 表单填充指南（根据需要加载）
├── reference.md      # API 参考（根据需要加载）
├── examples.md       # 使用示例（根据需要加载）
└── scripts/
    ├── analyze_form.py   # 实用脚本（执行，不加载）
    ├── fill_form.py      # 表单填充脚本
    └── validate.py       # 验证脚本
```

## 目录用途区分

### references/ — 只读参考文档

**用途**：提供指导、规范、检查清单、API 文档

**特点**：
- Agent 只读，不会修改
- 不包含需要用户填写的占位符
- 通常是说明性内容

**适合内容**：
- API 参考文档
- 检查清单 (checklist.md)
- 最佳实践指南
- 配置说明
- 故障排查指南

### templates/ — 复制型模板文件

**用途**：用户/Agent 复制并填写具体值

**特点**：
- 包含占位符（如 `YYYY-MM-DD`、`[domain]`、`<value>`）
- 使用时需要复制并替换占位符
- 通常是结构性内容

**适合内容**：
- 配置文件模板
- 页面模板（如 entity page template）
- 文档模板（如 SCHEMA.md template）
- 代码骨架

### scripts/ — 可执行脚本

**用途**：执行具体操作

**特点**：
- 可直接运行
- 包含错误处理
- 接受参数或环境变量

### 示例对比

**错误的组织**（模板放在 references/）：
```
references/
├── lint-checks.md       # ✓ 参考文档
└── page-templates.md    # ✗ 这是模板，不应在此
```

**正确的组织**：
```
references/
└── lint-checks.md       # 只读参考文档
templates/
├── schema-template.md   # 复制型模板
├── index-template.md    # 复制型模板
└── page-templates.md    # 复制型模板
scripts/
├── push-changes.sh      # 可执行脚本
└── sync-from-remote.sh  # 可执行脚本
```

## 模式 1：高级指南与参考

```yaml
---
name: pdf-processing
description: 从 PDF 文件中提取文本和表格、填充表单和合并文档。在处理 PDF 文件或用户提及 PDF、表单或文档提取时使用。
---
```

```markdown
# PDF 处理

## 快速开始

使用 pdfplumber 提取文本：

```python
import pdfplumber
with pdfplumber.open("file.pdf") as pdf:
    text = pdf.pages[0].extract_text()
```

## 高级功能

**表单填充**：完整指南请参阅 [FORMS.md](FORMS.md)
**API 参考**：所有方法请参阅 [REFERENCE.md](REFERENCE.md)
**示例**：常见模式请参阅 [EXAMPLES.md](EXAMPLES.md)
```

Agent 仅在需要时加载 FORMS.md、REFERENCE.md 或 EXAMPLES.md。

## 模式 2：特定领域的组织

对于具有多个领域的技能，按领域组织内容以避免加载无关上下文。当用户询问销售指标时，Agent 只需要读取与销售相关的架构，而不是财务或营销数据。这保持 token 使用低且上下文集中。

```
bigquery-skill/
├── SKILL.md（概述和导航）
└── reference/
    ├── finance.md（收入、账单指标）
    ├── sales.md（机会、管道）
    ├── product.md（API 使用、功能）
    └── marketing.md（活动、归因）
```

SKILL.md 内容：

```markdown
# BigQuery 数据分析

## 可用数据集

**财务**：收入、ARR、账单 → 参阅 [reference/finance.md](reference/finance.md)
**销售**：机会、管道、账户 → 参阅 [reference/sales.md](reference/sales.md)
**产品**：API 使用、功能、采用 → 参阅 [reference/product.md](reference/product.md)
**营销**：活动、归因、电子邮件 → 参阅 [reference/marketing.md](reference/marketing.md)

## 快速搜索

使用 grep 查找特定指标：

```bash
grep -i "revenue" reference/finance.md
grep -i "pipeline" reference/sales.md
grep -i "api usage" reference/product.md
```
```

## 模式 3：条件详情

显示基本内容，链接到高级内容：

```markdown
# DOCX 处理

## 创建文档

使用 docx-js 创建新文档。参阅 [DOCX-JS.md](DOCX-JS.md)。

## 编辑文档

对于简单编辑，直接修改 XML。

**对于跟踪更改**：参阅 [REDLINING.md](REDLINING.md)
**对于 OOXML 详情**：参阅 [OOXML.md](OOXML.md)
```

Agent 仅在用户需要这些功能时读取 REDLINING.md 或 OOXML.md。

## 避免深层嵌套的参考

当从其他参考文件引用文件时，Agent 可能会部分读取文件。遇到嵌套参考时，Agent 可能会使用 `head -100` 之类的命令来预览内容，而不是读取整个文件，导致信息不完整。

**保持参考距离 SKILL.md 一级。**所有参考文件应直接从 SKILL.md 链接，以确保 Agent 在需要时读取完整文件。

### 坏的例子：太深

```markdown
# SKILL.md
参阅 [advanced.md](advanced.md)...

# advanced.md
参阅 [details.md](details.md)...

# details.md
这是实际信息...
```

### 好的例子：一级深

```markdown
# SKILL.md

**基本用法**：[SKILL.md 中的说明]
**高级功能**：参阅 [advanced.md](advanced.md)
**API 参考**：参阅 [reference.md](reference.md)
**示例**：参阅 [examples.md](examples.md)
```

## 使用目录结构化较长的参考文件

对于超过 100 行的参考文件，在顶部包含目录。这确保 Agent 即使在部分读取时也能看到可用信息的完整范围。

```markdown
# API 参考

## 目录
- 身份验证和设置
- 核心方法（创建、读取、更新、删除）
- 高级功能（批量操作、webhook）
- 错误处理模式
- 代码示例

## 身份验证和设置
...

## 核心方法
...
```

Agent 可以读取完整文件或根据需要跳转到特定部分。
