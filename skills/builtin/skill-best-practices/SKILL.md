---
name: skill-best-practices
description: AI Agent Skills 编写最佳实践指南。在创建或修改任何 skill 时必须参考。包含核心原则、结构规范、命名约定、渐进式披露模式、工作流设计、内容指南、常见模式、评估迭代策略和检查清单。
version: 1.0.0
source: https://platform.claude.com/docs/zh-CN/agents-and-tools/agent-skills/best-practices
---

# Skill 编写最佳实践

> **重要**：此 skill 在创建或修改任何 skill 时必须参考。详细内容拆分为多个参考文件，按需加载。

## 概述

好的 skill 应该简洁、结构良好，并通过真实使用进行测试。本指南提供实用的编写决策，帮助您编写 Agent 能够有效发现和使用的 skill。

## 核心原则速览

1. **简洁是关键**：上下文窗口是公共资源，只添加 Agent 不知道的信息
2. **匹配自由度**：根据任务脆弱性调整说明详细程度
3. **多模型测试**：在不同能力层级的模型上测试（快速/平衡/强推理）

详见 [references/core-principles.md](references/core-principles.md)

## Skill 结构

### YAML 前置事项

必须包含：
- `name`：最多 64 字符，仅小写字母、数字、连字符
- `description`：最多 1024 字符，第三人称描述功能和触发场景

详见 [references/structure.md](references/structure.md)

## 渐进式披露模式

SKILL.md 作为目录，保持在 500 行以内，详细内容拆分为独立文件。

详见 [references/progressive-disclosure.md](references/progressive-disclosure.md)

## 工作流和反馈循环

- 清单模式：复杂任务提供可复制清单
- 验证循环：运行验证器 → 修复错误 → 重复
- 计划-验证-执行：破坏性操作前先生成计划文件

详见 [references/workflows.md](references/workflows.md)

## 内容指南

- 避免时间敏感信息
- 使用一致术语
- 路径使用正斜杠

详见 [references/content-guidelines.md](references/content-guidelines.md)

## 常见模式

- 模板模式
- 示例模式
- 条件工作流模式

详见 [references/patterns.md](references/patterns.md)

## 评估和迭代

- 评估驱动开发：先创建 3 个测试场景
- Agent 协作迭代：设计 Agent 设计 → 测试 Agent 测试 → 反馈优化

详见 [references/evaluation.md](references/evaluation.md)

## 反模式

- 避免 Windows 风格路径
- 避免提供太多选项
- 避免假设工具已安装

详见 [references/anti-patterns.md](references/anti-patterns.md)

## 可执行代码的 Skill

- 解决问题，不要推卸给 Agent
- 提供实用脚本
- 创建可验证的中间输出

详见 [references/executable-skills.md](references/executable-skills.md)

## MCP 工具参考

使用完全限定名称 `ServerName:tool_name`

详见 [references/mcp-tools.md](references/mcp-tools.md)

## 有效 Skill 检查清单

详见 [references/checklist.md](references/checklist.md)

## 创建 Skill 时的强制流程

1. **阅读此 skill**：加载并理解所有参考文件
2. **规划结构**：确定是否需要拆分为多个文件（>500 行）
3. **编写前置事项**：name 和 description 必须符合规范
4. **遵循核心原则**：简洁、匹配自由度、渐进式披露
5. **添加工作流**：复杂任务使用清单和验证循环
6. **自检**：使用 checklist.md 验证

## 修改 Skill 时的强制流程

1. **阅读此 skill**：加载相关参考文件
2. **评估变更影响**：是否影响 description、结构、自由度
3. **保持一致性**：术语、格式、风格
4. **更新相关文件**：确保引用文件同步更新
5. **自检**：使用 checklist.md 验证
