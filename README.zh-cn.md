[中文版](README.zh-cn.md)

# 赞歌

[![许可证：Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![模型](https://img.shields.io/badge/model-Hy--MT2-orange)
![平台](https://img.shields.io/badge/platform-Linux-lightgrey)

Hy-MT2 是一款实用的 Rust 命令行工具：具备分词器感知的分段功能、分段级缓存复用机制、管道安全的令牌流处理能力、Markdown 感知的文档翻译功能、批量翻译功能、命令输出翻译功能，以及可热重载的配置系统。

`hymt` 是为那些需要处理真实终端界面及 Markdown 工作流程的翻译任务而设计的，而不仅仅是处理零散的文本字符串。它会将进度信息输出到 `stderr`，翻译内容则输出到 `stdout`，还会记录历史数据以帮助估算完成时间；当模型表现与历史预期相差较大时，它还能自动记录时间偏差问题。

## 为何选择hymt

- 通过一个命令即可翻译位置文本、标准输入内容或文件。  
- 基于Hy-MT2分词器对长文本进行分段，而非盲目分割。  
- 在片段级别重用缓存翻译结果，从而使重复内容几乎能瞬间完成翻译。
- 默认以流式方式处理分词结果，确保`| less`、`| bat`和`| tee`等操作保持响应迅速。  
- 以保留结构的方式分割 Markdown；混合语言限制见下文。
- 可批量处理整个目录树，在写入前预览缓存状态及预计完成时间。  
- 可用`hymt exec`封装任意shell命令，或查看已翻译的`man`和`info`页面。  
- 可调出之前的翻译结果，并查看包含处理效率统计信息的翻译历史记录。  
- 使用`hymt translate-doc`可保持双语Markdown文档的同步。

## 安装

### 安装

```bash
just install
```

二进制默认启用 `telegram` Cargo feature。若不需要 Telegram Bot API 依赖：

```bash
just install-no-telegram
```

文档化并经过测试的安装路径是 Linux x86_64 上的 Rust workspace。

## 配置端点

首次使用时，`hymt`会创建`~/.config/hymt/config.toml`文件。典型的配置如下：

```toml
[endpoint]
url = "http://100.78.159.38:8401/v1"
api_key = ""
model = ""
# 选择一个受支持且经过测试的 Hy-MT2 profile；未覆盖的端点使用 "generic"。
profile = "hy_mt2_7b"

[translation]
# `llama-server -c` 是服务总上下文。两个示例服务的每请求上限都约为
# 8,192 tokens：quality 为 24,576 / 3 槽，throughput 为 65,536 / 8 槽。
# 此处填写每槽上限，而不是服务级 `-c` 总量。
context_window = 8192
max_output_tokens = 4096
max_source_tokens_per_segment = 1024
concurrency = 8 # 使用 hy-mt2-quality.service 时设为 3
stream = true
config_version = 1
timeout = 600
# first_chunk_priority = false
# debug_chunk_timing = false

[inference]
# 客户端会发送这些 OpenAI 兼容请求字段。它们与示例服务配置一致；
# 显式覆盖时请保持客户端和端点配置一致。
temperature = 0.7
top_p = 0.6
top_k = 20
repetition_penalty = 1.05

[completeness]
zh_to_en_min_ratio = 0.3
en_to_zh_min_ratio = 0.3
min_paragraph_ratio = 0.5
max_retries = 2
# 默认 false：顶层 text/file/stdin 在写出最佳尝试后，若仍有片段耗尽完整性重试则非零退出。
# 设为 true（或传 --warn-only-completeness）则仅告警并以 0 退出。
warn_only = false

[timing]
divergence_threshold = 2.0
```

该配置支持热加载，但 `[endpoint].profile` 会在启动时固定（参见[模型 Profile](#模型-profileendpointprofile)）。长时间运行的工作流无需重启即可应用其他更改。

## 模型 Profile（`[endpoint].profile`）

请为 Hy-MT2 端点显式设置 `[endpoint].profile`。可识别的值及覆盖范围如下：

| 值 | 覆盖范围 |
|---|---|
| `hy_mt2_1_8b` | 经过测试的 Hy-MT2 1.8B profile，具有固定的上游分词器来源和 profile 生成默认值。 |
| `hy_mt2_7b` | 经过测试的 Hy-MT2 7B profile，具有固定的上游分词器来源和 profile 生成默认值。 |
| `hy_mt2_30b_a3b` | 经过测试的 Hy-MT2 30B-A3B profile，具有固定的上游分词器来源和 profile 生成默认值。 |
| `generic`（或省略） | 未建档 profile 模式：没有经过测试的 Hy-MT2 分词器或生成默认值覆盖。 |

Profile 会在**进程启动时固定**。其他配置仍然支持热加载；运行中修改磁盘上的 `[endpoint].profile` 会被忽略，必须重启 `hymt` 才会使用其他 profile。分段缓存键和翻译历史记录会保留规范的 profile ID，因此结果不会在 profile 之间共享。

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

可用 `--concurrency N` 覆盖本次运行的并发（覆盖 `[translation].concurrency`）。使用 `--debug-chunk-timing`（或 `HYMT_DEBUG_CHUNK_TIMING=1`）在 stderr 打印各 chunk 的 queue/request/first-token/complete 时序，便于诊断多段流式停顿。

### 混合语言文档：当前限制

Rust 主翻译路径尚未使用段落级语言分析来跳过已经是目标语言的段落。对于文本、`batch` 和 `translate-doc`，不要依赖自动保留目标语言段落：可翻译文本片段仍会发送给模型。Markdown 感知分段仍有助于保留结构，但不保证按混合语言过滤。当前没有旧版检测器兼容安装路径，也没有针对这项未完成行为的 CLI 强制/禁用开关。

## 智能分段与缓存重用

`hymt`会根据Hy-MT2分词器及选定的提示模板来规划每次翻译。当前每个翻译后的片段按以下内容缓存：

- 片段内容哈希值
- 目标语言
- 模板类型
- 模板选项

片段缓存键目前**不**包含端点/模型身份、量化或后端构建、分词器版本或推理采样设置。因此，修改这些设置后仍可能复用旧推理配置生成的条目；`config_version`仅记录任务历史，并不参与片段缓存键。要自动隔离这些配置，仍需实现推理指纹。

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
- 完整性校验是一组快速的截断/结构启发式检查：最小字符比例、段落保留率和 Markdown 标题保留。它能标记可能的截断或结构丢失，不能证明翻译在语义上正确。
- 失败片段最多按`[completeness].max_retries`重试；普通、流式、批量和`translate-doc`的片段完整性校验都使用同一个值。重试耗尽后，`hymt`仍会写出最佳尝试结果，并在 stderr 打印`completeness_degraded_segments=…`。顶层 text/file/stdin 命令（包括流式形式）随后以非零状态退出，便于脚本检测降级结果；可传`--warn-only-completeness`或设置`[completeness].warn_only = true`以保持仅告警且退出码为 0。`batch`、`translate-doc`与`exec`默认同样报告该 stderr 标记，但不会因降级片段而使整个任务失败。
- 源片段还会受扩展/上下文预算以及`[translation].max_source_tokens_per_segment`限制（默认`1024`，`0`表示禁用）。

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

该仓库在[`services/`](services)目录下包含两个互斥的 systemd 用户服务示例。`-c` 是服务总上下文池，`--parallel` 会将其分配给并发请求：

| 服务 | 模型量化 | KV 缓存 | 总上下文 | 并行槽位 | 每槽约上下文 |
|---|---|---|---:|---:|---:|
| `hy-mt2-quality.service` | Q6_K | Q8 (`q8_0`) | 24,576 | 3 | 8,192 |
| `hy-mt2-throughput.service` | Q4_K_M | Q4 (`q4_0`) | 65,536 | 8 | 8,192 |

两者都仅绑定到 Tailscale 接口（`100.78.159.38:8401`），而不绑定到`0.0.0.0`。它们使用 CUDA `llama-server`：quality 单元指向持久化的本地构建，throughput 示例固定使用 mise 的 `llama-cpp/9294-cuda` 构建。绝对可执行文件和模型路径依赖具体主机；替换后端必须支持所示 `llama-server` 的上下文、并行槽位和 KV 缓存参数。

两个服务都设置了 `--temp 0.7`、`--top-k 20`、`--top-p 0.6` 和 `--repeat-penalty 1.05`。两个单元都未显式设置 llama.cpp 的 `min-p` 或重复历史长度，因此采用后端默认值。Rust 客户端目前会发送对应的 `[inference]` 请求字段；修改它们时请明确操作，并让客户端与服务配置保持一致。

## 架构

- `crates/hymt-core`：可热重载的 TOML 配置、提示模板、CJK 语言工具和完整性启发式检查。
- `crates/hymt-segment`：Hy-MT2 分词器集成，以及分层和 Markdown 感知的分段。
- `crates/hymt-client`：异步 OpenAI 兼容 HTTP 客户端、重试、并发限制和 SSE 流式传输。
- `crates/hymt-cache`：SQLite 片段/exec 缓存、任务历史、回溯和 ETA 统计。
- `crates/hymt-translate`：翻译编排、完整性重试、批量/文档工作流和已翻译文档。
- `crates/hymt-cli`：Clap `hymt` 二进制、命令分发、面向 shell 的行为和可选 Telegram 子命令。

## 开发

先安装仓库钩子，然后运行本地质量检查：

```bash
just install-hooks
just pre-commit
```

Lefthook 会在提交前运行 `just pre-commit`，并提供 README 翻译同步。GitHub Actions CI 会在 pull request 和推送到 `main` 时运行，覆盖格式化、Clippy、workspace 测试/检查、无默认 feature 的 CLI 检查、shell 检查、服务单元验证和 TOML 解析。
