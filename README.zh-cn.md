[中文版](README.zh-cn.md)

# 赞歌

![Python](https://img.shields.io/badge/python-3.11%2B-blue)
![许可证](https://img.shields.io/badge/license-Apache--2.0-green)
![模型](https://img.shields.io/badge/model-Hy--MT2-orange)
![平台](https://img.shields.io/badge/platform-Linux%20%7C%20Termux-lightgrey)

Hy-MT2是一款实用的命令行工具：具备分词器感知的分段功能、分段级缓存复用机制、管道安全的令牌流处理能力、多语言文档处理功能、批量翻译功能、命令输出翻译功能，以及可热重载的配置系统。

`hymt` 是为那些需要处理真实终端界面及 Markdown 工作流程的翻译任务而设计的，而不仅仅是处理零散的文本字符串。它会将进度信息输出到 `stderr`，翻译内容则输出到 `stdout`，还会记录历史数据以帮助估算完成时间；当模型表现与历史预期相差较大时，它还能自动记录时间偏差问题。

## 为何选择hymt

- 通过一个命令即可翻译位置文本、标准输入内容或文件。  
- 基于Hy-MT2分词器对长文本进行分段，而非盲目分割。  
- 在段落级别重用缓存翻译结果，从而使重复出现的段落几乎能瞬间完成翻译。  
- 默认以流式方式处理分词结果，确保`| less`、`| bat`和`| tee`等操作保持响应迅速。  
- 能识别混合语言的Markdown文档，仅翻译非目标语言的段落，同时保留代码块格式。  
- 可批量处理整个目录树，在写入前预览缓存状态及预计完成时间。  
- 可用`hymt exec`封装任意shell命令，或查看已翻译的`man`和`info`页面。  
- 可调出之前的翻译结果，并查看包含处理效率统计信息的翻译历史记录。  
- 使用`hymt translate-doc`可保持双语Markdown文档的同步。

## 安装

### 使用 `uv` 快速安装

```bash
uv tool install .
```

如果您希望在本地可编辑的安装版本中获得可选的语言检测功能：

```bash
uv pip install --system -e ".[detect]"
mise reshim
```

这样你就得到了：

- `langdetect`，用于混合语言的局部翻译。

### Termux / 安卓

Android版本会跳过Rust的`tokenizers`依赖，因此`hymt`会自动采用近似的方式计算词元数量。翻译功能依然可用，只是分词精度不如基于Linux词法分析器的方案。

```bash
uv pip install --system -e ".[detect]"
mise reshim
```

## 配置端点

首次使用时，`hymt`会创建`~/.config/hymt/config.toml`文件。典型的配置如下：

```toml
[endpoint]
url = "http://100.78.159.38:8401/v1"
api_key = ""
model = ""

[translation]
context_window = 65536
max_output_tokens = 4096
concurrency = 8
stream = true
config_version = 1
timeout = 600

[completeness]
zh_to_en_min_ratio = 0.3
en_to_zh_min_ratio = 0.3
min_paragraph_ratio = 0.5
max_retries = 2

[timing]
divergence_threshold = 2.0
```

该配置支持热加载。长时间运行的工作流无需重启即可应用更改。

## 快速开始

### 翻译文本、标准输入或文件内容

```bash
hymt "Hello world" -t zh
printf 'Release notes go here.\n' | hymt -t ja
hymt -f CHANGELOG.md -t fr -o CHANGELOG.fr.md
```

### 保持适合流媒体播放

流式传输默认是启用的。这意味着你可以继续使用常规的shell管道：

```bash
hymt -f article.md -t zh | less
hymt -f notes.txt -t ja | bat -l markdown
hymt -f report.md -t zh | tee report.zh.preview.md
```

如果需要完全缓冲的响应，请使用`--no-stream`。

### 混合语言的文档依然可读

当安装了可选的语言检测功能后，`hymt`会保留已为目标语言的段落，翻译其余部分，并始终保留代码块。

这使得它适用于：

- 双语README文件  
- 包含复制过来的shell输出的设计说明  
- 混合英文代码与中文注释的API文档

## 智能分段与缓存重用

`hymt`会根据Hy-MT2分词器及选定的提示模板来规划每次翻译。每个翻译后的片段都会被缓存：

- 段落内容哈希值
- 目标语言
- 模板类型
- 模板选项

这可以实现：

- 运行中断后快速重试  
- 仅少数段落更改时几乎即时重新生成  
- 在常规翻译、批量翻译、文档翻译以及已翻译的手册页面中重复使用缓存

进度始终以相同格式输出在`stderr`中：

```text
[done/total] XX.XX% | elapsed Xm Ys | eta Xm Ys | NN.NN tok/s
```

## 翻译Markdown文档

`translate-doc` 是用于双语 Markdown 树的结构化处理命令。

```bash
hymt translate-doc README.md
hymt translate-doc README.md -t ja
hymt translate-doc README.md -t zh -o README.zh-cn.md
hymt translate-doc docs/ --recursive
```

行为：

- 默认目标语言为`zh`，Markdown输出文件会自动命名为`.zh-cn.md`。
- 当使用`--output-dir`参数时，目录模式会翻译Markdown文件并保留相对路径。
- 完整性校验会检查已翻译片段的最小字符比例、段落保留率和 Markdown 标题保留情况。
- 失败片段最多按`[completeness].max_retries`重试；重试耗尽后，`hymt`会告警并继续使用最佳尝试。

## 批量翻译目录树

当您希望对 `.md` 和 `.txt` 文件采用先预览的工作流程时，请使用 `batch`：

```bash
hymt batch docs -t zh
hymt batch docs -t zh --write --yes
hymt batch docs -t zh --write --output-dir translated-docs
```

批量预览报告：

- 已选择与已跳过的文件
- 每个文件的缓存状态：`已满`、`部分`或`无`
- 缓存的段数
- 每个文件的预计完成时间
- 总体预计完成时间

## 翻译命令输出和手册内容

### 包装终端命令

```bash
hymt exec -- cargo test
hymt exec -- git status
hymt exec precache --recursive
```

`hymt exec` 会保留原始命令的输出，然后在其后添加翻译后的输出。它对于不熟悉的命令行界面、构建失败以及过长的帮助文本非常有用。

### 阅读已翻译的 `man` 和 `info` 文档

```bash
hymt man git-rebase
hymt man --original git-rebase
hymt info coreutils
hymt info --refresh bash
```

## 回忆、历史记录与预计到达时间估算

翻译历史记录存储在 `~/.local/share/hymt/history.db` 的 SQLite 数据库中。

常用命令：

```bash
hymt history
hymt history --stats
hymt recall
hymt recall --list
hymt estimate 10000 -l zh
```

历史力量：

- 最新输出回溯
- 处理量统计信息
- 中位数/百分位数预计完成时间估算
- 反映实际token处理量的进度条

## 时间差异自动提交流程

在完成交互式翻译后，`hymt`会将实际运行时间与历史估算值进行比较。当运行时间偏差超过`[timing].divergence_threshold`时，它会提示用户提交一个包含相关信息的GitHub问题：

- 令牌数量
- 分段数量
- 吞吐量统计
- 配置版本
- 模型元数据

这样就能更轻松地追踪服务器设置、并发处理能力或提示行为方面的退化问题。

## 通过Tailscale远程使用Hy-MT2

该仓库在[`services/`](services)目录下包含两个systemd用户服务示例：

- `hy-mt2-quality.service`：Q6_K，单个槽位，Q8 KV，16K上下文容量
- `hy-mt2-throughput.service`：Q4_K_M，8个槽位，Q4 KV，64K上下文容量

两者都仅绑定到Tailscale接口（`100.78.159.38:8401`），而不绑定到`0.0.0.0`。当您希望通过Tailnet使用远程的Hy-MT2主机时，应将`endpoint.url`设置为该地址。

## 架构

- `src/hymt/config.py`：可热重载的TOML配置文件  
- `src/hymt/segment.py`：基于分词器的字符计数与分词功能  
- `src/hymt/client.py`：具备重试机制的异步OpenAI兼容翻译客户端  
- `src/hymt/translate.py`：核心翻译流程，包括缓存查询、流式处理、进度显示及时间记录功能  
- `src/hymt/history.py`：基于SQLite的任务历史记录、召回率及预计完成时间统计  
- `src/hymt/batch.py`：目录规划、缓存预览及批量写入功能  
- `src/hymt/doc_translate.py`：专注于Markdown文档的翻译与预览工作流  
- `src/hymt/docs.py`：已翻译的`man`和`info`文档  
- `src/hymt/exec_wrapper.py`：命令封装工具及翻译后的运行结果输出  
- `src/hymt/cli.py`：基于Click框架的命令行接口入口

## 开发

使用以下命令运行完整的本地质量检查：

```bash
env JUST_TEMPDIR=$PWD/.git/just-tmp just pre-commit
```

如果您希望在自动化过程中实现README的双语同步，请查看`doc-translate-sync`脚本以及`lefthook.yml`中的`post-commit`钩子。
