# 可执行代码的 Skill

以下部分重点关注包含可执行脚本的技能。如果您的技能仅使用 markdown 说明，请跳到检查清单。

## 解决，不要推卸

编写技能脚本时，处理错误条件而不是推卸给 Agent。

### 好的例子：显式处理错误

```python
def process_file(path):
    """处理文件，如果不存在则创建它。"""
    try:
        with open(path) as f:
            return f.read()
    except FileNotFoundError:
        # 创建具有默认内容的文件而不是失败
        print(f"未找到文件 {path}，正在创建默认文件")
        with open(path, "w") as f:
            f.write("")
        return ""
    except PermissionError:
        # 提供替代方案而不是失败
        print(f"无法访问 {path}，使用默认值")
        return ""
```

### 坏的例子：推卸给 Agent

```python
def process_file(path):
    # 只是失败并让 Agent 弄清楚
    return open(path).read()
```

### 配置参数

配置参数也应该是合理的和文档化的，以避免"巫术常数"（Ousterhout 定律）。如果您不知道正确的值，Agent 如何确定它？

**好的例子：自文档化：**

```python
# HTTP 请求通常在 30 秒内完成
# 更长的超时时间考虑了慢速连接
REQUEST_TIMEOUT = 30

# 三次重试平衡了可靠性与速度
# 大多数间歇性故障在第二次重试时解决
MAX_RETRIES = 3
```

**坏的例子：魔法数字：**

```python
TIMEOUT = 47  # 为什么是 47？
RETRIES = 5   # 为什么是 5？
```

## 提供实用脚本

即使 Agent 可以编写脚本，预制脚本也有优势：

**实用脚本的优势：**

- 比生成的代码更可靠
- 节省 token（无需在上下文中包含代码）
- 节省时间（无需代码生成）
- 确保跨使用的一致性

### 重要区别

在说明中明确说明 Agent 是否应该：

- **执行脚本**（最常见）："运行 analyze_form.py 来提取字段"
- **作为参考读取**（对于复杂逻辑）："参阅 analyze_form.py 了解字段提取算法"

对于大多数实用脚本，执行是首选，因为它更可靠和高效。

### 例子

```markdown
## 实用脚本

**analyze_form.py**：从 PDF 中提取所有表单字段

```bash
python scripts/analyze_form.py input.pdf > fields.json
```

输出格式：
```json
{
  "field_name": {"type": "text", "x": 100, "y": 200},
  "signature": {"type": "sig", "x": 150, "y": 500}
}
```

**validate_boxes.py**：检查重叠的边界框

```bash
python scripts/validate_boxes.py fields.json
# 返回："OK"或列出冲突
```

**fill_form.py**：将字段值应用到 PDF

```bash
python scripts/fill_form.py input.pdf fields.json output.pdf
```
```

## 使用视觉分析

当输入可以呈现为图像时，让 Agent 分析它们：

```markdown
## 表单布局分析

1. 将 PDF 转换为图像：
   ```bash
   python scripts/pdf_to_images.py form.pdf
   ```

2. 分析每个页面图像以识别表单字段
3. Agent 可以直观地看到字段位置和类型
```

在此示例中，您需要编写 pdf_to_images.py 脚本。

Agent 的视觉功能有助于理解布局和结构。

## 创建可验证的中间输出

当 Agent 执行复杂的开放式任务时，它可能会犯错误。"计划-验证-执行"模式通过让 Agent 首先以结构化格式创建计划，然后在执行前用脚本验证该计划，从而及早捕获错误。

### 示例

想象要求 Agent 根据电子表格更新 PDF 中的 50 个表单字段。如果没有验证，Agent 可能会引用不存在的字段、创建冲突的值、遗漏必需字段或错误地应用更新。

**解决方案**：使用工作流模式（PDF 表单填充），但添加一个中间 changes.json 文件，在应用更改前进行验证。工作流变为：分析 → 创建计划文件 → 验证计划 → 执行 → 验证。

### 为什么此模式有效

- **及早捕获错误**：验证在应用更改前发现问题
- **机器可验证**：脚本提供客观验证
- **可逆的规划**：Agent 可以迭代计划而不触及原始文件
- **清晰的调试**：错误消息指向具体问题

### 何时使用

批量操作、破坏性更改、复杂验证规则、高风险操作。

### 实现提示

使用具体错误消息使验证脚本详细，例如"字段 'signature_date' 未找到。可用字段：customer_name, order_total, signature_date_signed"，以帮助 Agent 修复问题。

## 运行时环境

Agent 的运行时环境取决于具体平台，通常支持文件系统访问、shell 命令执行和代码执行功能。在编写可执行脚本时，请确认目标平台的包安装和网络访问能力。

### 这如何影响您的创作

- **元数据预加载**：在启动时，所有技能 YAML 前置内容中的名称和描述被加载到系统提示中
- **按需读取文件**：Agent 在需要时使用 bash 读取工具从文件系统访问 SKILL.md 和其他文件
- **高效执行脚本**：实用脚本可以通过 bash 执行，而无需将其完整内容加载到上下文中。只有脚本的输出消耗 token
- **大文件无上下文惩罚**：参考文件、数据或文档在实际读取前不消耗上下文 token
- **文件路径很重要**：Agent 像导航文件系统一样导航您的技能目录。使用正斜杠（reference/guide.md），而不是反斜杠
- **文件命名要有描述性**：使用表示内容的名称：form_validation_rules.md，而不是 doc2.md
- **组织以便发现**：按域或功能构建目录
- **捆绑全面的资源**：包括完整的 API 文档、广泛的示例、大型数据集；在访问前没有上下文惩罚
- **优先使用脚本进行确定性操作**：编写 validate_form.py 而不是要求 Agent 生成验证代码
- **明确执行意图**："运行 analyze_form.py 以提取字段"（执行）/"参见 analyze_form.py 了解提取算法"（作为参考读取）
- **测试文件访问模式**：通过使用真实请求进行测试来验证 Agent 可以导航您的目录结构
