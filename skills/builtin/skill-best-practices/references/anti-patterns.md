# 反模式

## 避免 Windows 风格的路径
正斜杠在所有平台有效，反斜杠在 Unix 系统上会导致错误。详见 content-guidelines.md 的路径规范部分。

## 避免提供太多选项
除非必要，否则不要呈现多种方法：

**坏的例子：太多选择**（令人困惑）：
"您可以使用 pypdf、或 pdfplumber、或 PyMuPDF、或 pdf2image、或..."

**好的例子：提供默认值**（带有逃生舱口）：
"使用 pdfplumber 进行文本提取：

```python
import pdfplumber
```

对于需要 OCR 的扫描 PDF，改用 pdf2image 和 pytesseract。"

## 避免假设工具已安装
不要假设依赖可用，应明确说明安装步骤和用法。详见 content-guidelines.md。

## 避免时间敏感信息
不要包含会过时的信息，旧版内容应放入"旧模式"部分隔离。详见 content-guidelines.md。

## 避免模糊的描述
描述必须具体且包含触发场景，否则 Agent 无法正确匹配技能。详见 structure.md 的描述编写部分。

## 避免深层嵌套的参考
参考文件深度超过一级时，Agent 可能只部分读取导致信息丢失。详见 progressive-disclosure.md。

## 避免巫术常数（硬编码数字）
代码中不要出现没有解释的魔法数字。所有值都应该有理由或注释。

## 避免推卸错误处理
脚本应显式处理错误，而不是让 Agent 去弄清楚。
