# ExecPlan

## 2026-10-07 N1 正式迁移 + Windows 远端 ASR + 自动笔记三阶段总计划

### 当前重新核验结论
- 本轮目标不是重新发明 ClassFlow，而是把 2026-09-20 已经完成的大部分代码真正收口到长期生产形态：**N1 负责轻量调度与缓存，Windows RTX4060 负责重型 ASR**。
- `ClassFlow main` 已包含 `remote_capswriter` provider、`audio.m4a` 检查点、原 AAC 无损 remux、`ffprobe` 校验、成功后尽早删除 MP4、失败保留 M4A，以及远端异步 job 协议与测试。此前计划里“待实现”的 Phase 1 核心实际上已经落进 main。
- Windows 生产 CapsWriter 已完成全阶段抢占式调度；ClassFlow/微信/批量文件统一保持低优先级 `source=file`，生产 CapsWriter 核心本阶段视为稳定内核。
- Windows `CapsWriterGateway` Rust 服务已在 `127.0.0.1:6017` 常驻并通过真实文件转写，但目前只有同步 `POST /v1/transcribe`。ClassFlow 已实现异步 `POST /v1/transcriptions -> GET status -> GET result`，所以优先给**现有 Rust Gateway 增加异步 job 兼容层**，不启用旧 Python Headless API 分支。
- N1 实机根分区约 `6.4 GiB`，已用约 `2.1 GiB`、可用约 `4.3 GiB`；内存约 `1.8 GiB`。N1 不长期保存 WAV，也不加载 Qwen/CapsWriter 模型。
- N1 已完成单 Mihomo 化：系统 Mihomo 同时承担 Meta TUN、规则处理、FRP 的 6000 listener 与 Cloudflare 的 18080 listener；`s-ui` 与旧 `tmu-ws-backup` 已停用。N100 故意保留旧双 Mihomo + S-UI 结构作为救援机。
- 四条管理 SSH 已于 2026-10-07 实测连通：`campus-box-frp`、`campus-box-cf`、`n1-campus-frp`、`n1-campus-cf`。
- N1 IPv6/TUN 已恢复正常；同一天津医大校内限制页实测 IPv4 仍被判校外、IPv6 可正常访问，因此 N1 的 TMU listener 继续保留目标校内域名强制 IPv6。
- N100 当前 IPv6 到目标 literal address 的数据面可通，但 `curl -6` 经主机名出现解析异常，所以不能记为“IPv6 端到端完全正常”。
- Wi-Fi：N1 最近 24 小时未见 deauth/auth timeout；N100 今晚仍出现 `DEAUTH_LEAVING`、关联超时和 AP 切换。两台 RSSI 都很强，旧结论继续成立：主要故障边界在校园 AP/控制器漫游/状态机，不是单纯信号弱。
- N1 的 `classflow-backend.service` 当前仍 `inactive + disabled`；生产仍在 N100。

### 总体架构
```text
SmartClass
   │  每 10 分钟自动发现（Phase 2）
   ▼
N1 / ClassFlow
   ├─ SQLite 可重试任务队列
   ├─ source.mp4 -> 原 AAC 无损 remux -> audio.m4a
   ├─ ffprobe 成功后立即删 MP4
   └─ audio.m4a 作为可重放 ASR checkpoint
               │ HTTPS / Cloudflare Tunnel
               ▼
Windows / Rust CapsWriterGateway
   ├─ 异步 job API
   └─ 统一文件型入口
               ▼
CapsWriter Server :6016
   └─ source=file / P1，mic/P0 可全阶段抢占
```

### Phase 1：N1 正式迁移 + Windows 远端 ASR
1. 给 `CapsWriterGateway` 增加 ClassFlow 已有 provider 期待的异步接口：
   - `POST /v1/transcriptions`：raw body + `X-Filename`，返回 job id；
   - `GET /v1/transcriptions/{id}`：`queued/running/completed/failed`；
   - `GET /v1/transcriptions/{id}/result`：最终结构化结果；
   - 保留 `/v1/transcribe`，不破坏 WeChatCLI/本机脚本。
2. Windows Cloudflare Tunnel 稳定 hostname -> `127.0.0.1:6017`；生产公网入口由 Cloudflare Access Service Token 保护。Gateway Bearer 能力保留为可选附加层，但当前 `asr.zjhstudio.com` 不要求 N1 再分发第二份静态 secret。
3. N1 用短 M4A 走公网入口做真实提交/轮询/取结果，并验证中途断网后 N1 checkpoint 仍可重放。
4. 把“远端暂不可达/笔记本离线”建模成可重试等待，而不是丢任务；重试必须复用 `audio.m4a`，禁止重新下载视频。
5. 前端显示“等待远端 ASR / 上传中 / 已提交 / 转写中 / 重试等待 / 完成 / 失败”。
6. ARM64 构建并部署 N1；迁移当前生产 SQLite/backend.env；切换前确保没有不可恢复的 running task。
7. N1 真课端到端验收后停 N100 ClassFlow 为救援状态；两台不同时消费同一生产任务库。
8. 增加 eMMC 水位与孤儿缓存清理：保留任务引用的失败 M4A，清理无引用孤儿 MP4/M4A，避免 8GB eMMC 被异常任务填满。

#### Phase 1 验收
- Windows 在线时，一条真课可完整 `发现/入队 -> 下载 -> M4A -> 远端 ASR -> artifacts -> cleanup`。
- Windows 离线/合盖/断网时，任务不丢、M4A 不丢、不重复下载 MP4；恢复后可重试完成。
- 本机 mic 使用 CapsWriter 时，远端 ClassFlow file 仍是后台任务并能被抢占。
- N1 重启后未完成任务可从 SQLite + M4A checkpoint 恢复。

### Phase 2：SmartClass 全自动发现
- 已有：Rust Collector、分页、Cover -> `VGA.mp4`、10 分钟轮询、历史回扫、登录失效退避、时间格式去重均已实现；2026-09-22 已修复重启后的重复回灌。
- 当前尾巴不是 Collector 没写，而是登录态/浏览器桥无人值守化；生产 Collector 当前故意关闭。
- 目标：N1 本机独立持久 Chrome/Chromium profile + 会话桥，不再依赖 Windows 反向 SSH；每 10 分钟自动发现新课，登录失效只进入明确退避状态。
- userscript/SmartClass Downloader 保留为调试与救援入口，不再是日常必经步骤。

#### Phase 2 验收
- 连续一个正常上课日无需人工点下载/提交，新课自动进入 ClassFlow。
- N1/后端重启不回灌历史任务，同一 NewID/时间片只有一个任务身份。
- 登录失效/恢复可观测，不把 Cookie/token 明文写入仓库或 env。

### Phase 3：Windows 自动维护课程 Audio/Notes 笔记仓库
```text
<学期笔记根目录>/
  <课程名>/
    Audio/<日期-课次>.md
    Notes/<日期-课次>.md
```
1. Windows 轻量 daemon 定期读取 ClassFlow 已完成任务/课程产物，只拉新增或变化内容。
2. ASR 文本稳定命名写入 `Audio/`，用本地 manifest 保证幂等。
3. 对尚无笔记的 Audio，使用现有 `chatgpt-archive actor send` Phase 4 能力提交到 ChatGPT 网页端“录音转笔记” Project。
4. turn 完成后拉取最终笔记写入 `Notes/`；异常终止、Chrome 重启、steering 后都必须可恢复且不重复生成。
5. 每批成功后 Git add/commit/push；失败保留队列和源 Audio，下次继续。
6. Phase 3 不阻塞 N1 主链；笔记 daemon 离线时，课程下载/ASR checkpoint/任务状态仍自洽。

### 本轮立即执行顺序（2026-10-07）
1. 已完成：恢复关键历史聊天并对齐最后结论。
2. 已完成：核验 N1/N100 网络、IPv6、Wi-Fi、磁盘、四条 SSH 与 ClassFlow 服务现状。
3. 已完成：确认 `remote_capswriter + M4A checkpoint` 已存在 main，不重复实现。
4. 已完成：Windows Rust `CapsWriterGateway` 已收口为通用 sync + async Transcription API；幂等键、job 资源、兼容 `/result`、能力查询与真实 6016 冒烟均已完成。
5. 已完成：`asr.zjhstudio.com`、Cloudflare Tunnel、Access application 已配置；N1 真实 103176-byte M4A 已完成 N1 -> Cloudflare -> 6017 -> 6016 -> N1 的 queued -> completed 跨公网闭环。
6. 正在做：补 ClassFlow“远端临时离线 = 可重试等待”与前端状态语义，确保笔记本合盖/断网不会把任务永久打成失败，也不会占死 worker。
7. 待执行：ARM64 构建、部署 N1、迁移生产 SQLite并真课切流。
8. Phase 1 稳定后再启用 N1 SmartClass Collector；Phase 3 最后实施，避免同时引入三个新故障域。

## 2026-09-22 SmartClass 时间格式去重修复 ExecPlan

### 背景
- 2026-09-20 N100 重启后，SmartClass Collector 首次执行 21 天 backfill。
- 历史任务中的课时常为 `HH:MM`，Collector 新回传值为 `HH:MM:SS`。
- 当前 `segment_key` 直接把原始 `start_time/end_time` 字符串参与哈希，导致 `08:00` 与 `08:00:00` 被视为不同片段，重复任务进入本地 ASR 单槽队列。
- 2026-09-22 已在线清理 15 条确认重复任务、对应 R2 重复单节产物，并重建受影响课程聚合产物；SmartClass Collector 暂时关闭，真实 2026-09-21 新任务已恢复执行。

### 目标
1. 让 `HH:MM` 与零秒的 `HH:MM:SS` 在片段身份上等价。
2. Collector 入库前统一标准化时间展示，避免同一数据库继续出现两套时间格式。
3. 为重复去重场景增加回归测试，覆盖 `08:00` / `08:00:00`。
4. 不在本轮自动重新开启 SmartClass Collector；Bridge 登录态问题留到后续单独处理。

### 实施步骤
1. 在课程身份构造层增加时间规范化函数，并让 `build_segment_key()` 使用规范化后的起止时间。
2. SmartClass `video_to_intake_item()` 将解析出的时间规范为稳定格式后再生成 `IntakeItem`。
3. 增加单元测试：同一课程、NewID、URL 下，`HH:MM` 与 `HH:MM:SS` 生成相同 `segment_key`；Collector 标准时间输出符合预期。
4. 执行 `cargo fmt --check`、`cargo test`、`cargo clippy --all-targets --all-features -- -D warnings`。
5. 更新 `PROGRESS.md`，记录线上清理结果和代码修复状态。

### 验收标准
- `08:00` 与 `08:00:00` 不再生成两个不同任务身份。
- SmartClass 新任务写入后时间格式稳定。
- 既有后端测试全部通过，新回归测试通过。
- 当前生产 N100 继续处理 2026-09-21 新任务，Collector 保持关闭，避免在 Bridge 登录态修复前再次回扫。

### 当前状态（2026-09-22）
- 已完成：线上 SQLite 已做一致性备份并通过 `PRAGMA quick_check`。
- 已完成：确认并清理 15 条同 NewID 的重复 backfill 任务；其中 8 条已完成重复任务对应的 R2 单节 Markdown/JSON 已删除，5 个受影响课程的 manifest/merged 总稿已用剩余正常任务重建。
- 已完成：生产 `CLASSFLOW_SMARTCLASS_COLLECTOR_ENABLED=false`，Bridge 登录态问题留待后续单独处理。
- 已完成：`normalize_segment_time()`、规范化 `segment_key`、SmartClass intake 时间标准化、旧数据库 `NewID` 兼容去重均已实现。
- 已完成：新增 `HH:MM` / `HH:MM:SS` 等价和旧 `segment_key` 兼容回归测试。
- 已完成：`cargo fmt --check`、`cargo check`、`cargo test`、`cargo clippy --all-targets --all-features -- -D warnings` 全部通过；单元测试 27 项、API 集成测试 11 项均通过。
- 已部署：`2026-09-22 15:38 CST` 在确认 2026-09-21 的 5 条真实任务全部完成、当前无 pending/running 任务后完成生产部署。Windows/GitHub/N100 代码已统一到 `2d7edc1`；N100 Linux 侧 27 个单元测试 + 11 个 API 集成测试通过，release 构建成功，服务重启后健康检查正常，Collector 继续保持关闭。

## 2026-05-29 本地 ASR 接入 ExecPlan

### 目标
- 给 `ClassFlow` 后端增加“可切换的 ASR provider”抽象，避免继续把 DashScope 逻辑硬编码在 `worker.rs + pipeline.rs` 主流程中。
- 在保持现有 DashScope 转写链路可用的前提下，新增一个 `local_qwen` provider，把“音频文件 -> 文本”的能力切到本地 Python 脚本。
- 本地 provider 第一阶段只做“单次脚本模式”，即 Rust 为每次任务启动一次 Python 脚本、等待 JSON 结果返回；先不把“常驻 sidecar + 空闲自动关闭 + 前端状态展示”混进同一轮。
- 先用你已经上传到服务器的 `CapsWriter-Offline` 目录复用代码和模型，但明确不把这份 Windows 打包产物直接当成 Linux 运行时；Linux 侧必须单独补 Python 依赖和原生库。

### 已确认事实
- 当前 `ClassFlow` 后端还是“下载视频 -> 抽音频 -> 上传百炼 -> 提交百炼 -> 轮询百炼 -> 产物入库”的单体流程，DashScope 逻辑尚未抽成独立 provider。
- `CapsWriter-Offline` 里的 Qwen3 引擎确实存在文件级转写入口：`core/server/engines/qwen_asr_gguf/inference/asr.py` 的 `QwenASREngine.transcribe(audio_file=...)` 可以直接按音频路径返回转写结果。
- 你上传到 Linux 机上的目录本质上是 Windows 打包产物：里面大量是 `.dll / .pyd`，当前没有可直接给 Linux 使用的 `libggml.so / libllama.so`。因此这份目录可以复用 Python 源码和模型文件，但不能原样直接运行。
- 当前模型目录真实结构与最初计划书里的示例值有差异，必须以服务器现状为准：
  - `models/Qwen3-ASR/Qwen3-ASR-1.7B`
  - `models/Qwen3-ForcedAligner/Qwen3-ForcedAligner-0.6B`
  - 主模型文件名是 `qwen3_asr_encoder_frontend.onnx`、`qwen3_asr_encoder_backend.onnx`、`qwen3_asr_llm.gguf`
  - 对齐器文件名是 `qwen3_aligner_encoder_frontend.int4.onnx`、`qwen3_aligner_encoder_backend.int4.onnx`、`qwen3_aligner_llm.q5_k.gguf`

### 实现范围
1. 先完成 Linux 侧独立冒烟：
   - 准备 Python venv。
   - 安装最小依赖。
   - 补齐 Linux 版 llama/onnx 所需运行环境。
   - 用单次脚本对一段短音频做真实转写。
2. 在后端新增 `asr/` 模块，抽出统一 provider 接口与公共输入输出结构。
3. 保留 DashScope provider，迁移现有云端转写逻辑，确保旧模式行为不变。
4. 新增 `local_qwen` provider，通过 Python 单次脚本执行本地文件转写并返回标准 JSON。
5. 增加本地 ASR 的配置、结构化日志、错误分类与串行控制。
6. 补齐 Rust 单元测试、集成测试，以及 Python 脚本最小自检。

### 本轮明确不做
- 不写 WebSocket headless client。
- 不拉起完整 CapsWriter GUI / 完整 server 作为主链路。
- 不在本轮先做常驻 HTTP sidecar。
- 不做自动 fallback 到 DashScope。
- 不做前端 provider 热切换。
- 不做多模型统一抽象。

### 执行步骤
1. 已完成：核对 `ClassFlow` 当前 Rust 后端链路，确认 DashScope 逻辑仍写死在主流程，必须先做 provider 抽象。
2. 已完成：核对 `CapsWriter-Offline` 当前目录结构，确认 Qwen3 引擎有文件级 `transcribe(audio_file)` 入口。
3. 已完成：确认服务器上的 `CapsWriter-Offline` 是 Windows 打包目录，不能原样在 Linux 上直接跑；计划已改为“复用源码 + 模型，Linux 单独准备运行时”。
4. 进行中：把本轮 ExecPlan 和项目状态快照写入 `PLANS.md` 与 `PROGRESS.md`，形成执行基线。
5. 已完成：已在当前 Ubuntu 机器上补齐 `cmake / ninja / python3-venv`，从 `llama.cpp` `b7798` 源码构建出 Linux 版 `libggml*.so / libllama.so`，并接入 `CapsWriter` 的 `qwen_asr_gguf/inference/bin/`。
6. 已完成：已创建 `/home/zjhsteven/CapsWriter-Offline/.venv`，补齐最小 Python 依赖，并完成真实本地冒烟：
   - 导入 `QwenASREngine` 成功。
   - 模型加载成功。
   - 真实音频 `嗯，帮我干一个小事情啊！.mp3` 转写成功，输出文本：`嗯，帮我干一个小事情啊。`
   - 实测本机 CPU 模式下该样本一次转写总耗时约 `32.6s`，其中 `prefill` 占比最高。
7. 已完成：已确认 `CapsWriter` 源码里存在一个必须规避的坑：`QwenASREngine.transcribe()` 默认把 `duration=0.0` 传给 `load_audio()`，会被底层音频读取实现解释为“读取 0 帧”，从而出现“模型加载成功但输出空文本”的假象。后续 ClassFlow 自己的 sidecar 脚本必须显式传 `duration=None` 或自行调用 `load_audio() + asr()`，不能直接照搬默认调用。
8. 已完成：已实现 `tools/local_asr/local_qwen_asr_once.py`，把 Linux 冒烟链路固化成可被 Rust 调用的标准 JSON 脚本；并已用真实中文音频验证脚本输出成功。
9. 已完成：已新增后端 `asr/` 模块与 `DashScopeAsrProvider / LocalQwenAsrProvider` 骨架，并把 provider 配置接入 `AppState`。
10. 已完成：`worker.rs` 已接入 `CLASSFLOW_ASR_PROVIDER` 分支：
   - `dashscope` 继续走原有云端上传/轮询逻辑；
   - `local_qwen` 走本地 provider，跳过云端上传，直接对 `audio.wav` 做本地转写后继续原有产物入库流程。
11. 已完成：后端验证已通过 `cargo check`、`cargo test`、`cargo clippy -D warnings`；本地 Python sidecar 脚本也已对真实音频成功输出标准 JSON。
12. 已完成：已安装并登录 `gh`，确认机器可访问 GitHub，并确认账号 `ZJHSteven` 名下已有 fork：`ZJHSteven/CapsWriter-Offline`。
13. 已完成：已在 `/home/zjhsteven/CapsWriter-Offline/CapsWriter-Offline-src` clone 一份干净源码仓库，远端已接成：
   - `origin -> https://github.com/ZJHSteven/CapsWriter-Offline.git`
   - `upstream -> https://github.com/HaujetZhao/CapsWriter-Offline.git`
   这样后续 Linux 适配改动与上游同步不再混在当前“运行时目录”里。
14. 已完成：已把真实后端环境 `/etc/classflow/backend.env` 切到 `CLASSFLOW_ASR_PROVIDER=local_qwen`，并显式写入当前机器的 `CLASSFLOW_LOCAL_ASR_*` 路径；系统级 `classflow-backend.service` 已重启成功。
15. 已完成：真实课程片段 `task_id=9b7c24df-fc5d-43c6-9a9b-62b99b8674af` 端到端冒烟已经成功。
   - 同源历史 DashScope 任务：`90a154be-9ee1-48fc-9e4b-449d629d18ee`
   - 两者 `mp4_url` 完全相同，都是 `2026-05-29 13:30-14:15` 的同一节课，不是新的音频样本。
   - 本地任务事件日志已确认“跳过云端音频上传阶段”并“开始执行本地 Qwen3 ASR 转写”。
   - 本地任务成功产出 `segment.md / segment.json / course.md`，说明后续 ClassFlow 产物链路也已接通。
16. 已完成：已记录这条真实任务的关键性能数据：
   - 音频时长：`45` 分钟（`2700s`）
   - 本地任务总耗时：约 `7963.78s`（`2h 12m 44s`）
   - 其中本地推理耗时：约 `7835.59s`
   - 本地推理 RTF：约 `2.90`
   - 端到端总 RTF：约 `2.95`
   - 同源 DashScope 历史任务总耗时：约 `350.32s`（`5m 50s`）
17. 已完成：已确认当前实现不是“常驻 sidecar + 空闲回收”。
   - 现在的实现仍然是“每个任务启动一次 `local_qwen_asr_once.py`，任务结束后进程退出”。
   - `CLASSFLOW_LOCAL_ASR_CONCURRENCY=1` 会让本地转写串行排队。
   - `CLASSFLOW_LOCAL_ASR_IDLE_SHUTDOWN_SECS` 目前只有配置项，没有接入真实生命周期逻辑，因此不会实现“空闲 N 分钟后再关”。
18. 待执行：下一轮如果要优化真实可用性，优先级应放在“常驻 sidecar + 模型复用 + 空闲回收”，否则连续任务场景下会重复加载模型，并且 N100 上单节课耗时过长。
19. 已完成：补充 `CLASSFLOW_LOCAL_ASR_THREADS / CLASSFLOW_LOCAL_ASR_THREADS_BATCH` 运行时参数，并在 N100 上完成 2 核/4 核对比测试；结论是 4 线程相对默认策略只有约 `1.4%` 的总耗时改善，不足以作为当前阶段的主要优化手段。
20. 已完成：已关闭本地 ASR 的 `Force Aligner`，当前输出改为“纯文本 + 空时间戳”的轻量模式；Python 冒烟与 Rust 回归测试均已通过。
21. 待执行：下一轮优先排查 `encode / prefill` 两段耗时占比，并评估是否应通过更小模型、缩减上下文、GPU 路线、或更换硬件继续提升吞吐。

## 2026-05-31 本地 ASR 常驻复用与队列验证 ExecPlan

### 目标
- 把当前 `local_qwen` 从“每任务启动一次单次脚本”升级为“常驻 sidecar + 模型复用 + 空闲回收”。
- 保持本地 ASR 全局串行，确保连续任务会排队，不会出现“第一个跑完、第二个卡死不动”的队列异常。
- 用可接受时长的短音频片段做多任务冒烟，验证：
  - 同一个 sidecar 进程被连续复用；
  - 空闲超时前不会反复卸载再重载模型；
  - 队列能按顺序推进多条任务。
- 补充性能研判：
  - 查官方 `CapsWriter-Offline` README / 一手资料里对理想 RTF、硬件前提、Qwen3-ASR 路线的描述。
  - 判断当前 `N100 + CPU` 的 `RTF≈2.9` 更像硬件瓶颈，还是代码有额外等待/阻塞。
  - 评估“真正同步并行多音频”是否现实，并把结论放入停车场。

### 当前判断
- 当前 Rust 侧 `LocalQwenAsrProvider` 虽然有 `Semaphore(1)`，但只是限制“同一时刻只允许一个本地转写进入关键区”；它并没有常驻模型进程。
- 当前 `CLASSFLOW_LOCAL_ASR_IDLE_SHUTDOWN_SECS` 只是配置项，没有接到真实生命周期逻辑。
- 当前实现如果连续来三节 45 分钟课程，会是：
  - 全局串行；
  - 每节课重新拉起一次 Python 进程并重载模型；
  - 性能上明显浪费。

### 执行步骤
1. 已完成：确认现状代码确实还是“单次脚本模式”，不是常驻 sidecar。
2. 已完成：已实现 Python 常驻 sidecar：
   - 启动时加载模型；
   - 提供 `health / transcribe / shutdown` 接口；
   - 在同一进程内复用模型。
3. 已完成：Rust 侧已接入 sidecar 生命周期管理：
   - 懒启动；
   - 健康检查；
   - 串行转写；
   - 空闲定时关闭；
   - 后端重启后 sidecar 不会残留在新服务进程之外。
4. 已完成：后端自动化验证通过：
   - `cargo test --manifest-path apps/backend/Cargo.toml`
   - `cargo clippy --manifest-path apps/backend/Cargo.toml --all-targets --all-features -- -D warnings`
   同时顺手修复了一个原有的 `SSE + SQLite` 行映射脆弱点：把 `SELECT *` 改成显式列清单，避免测试库列顺序/缺列导致 `SqliteRow` 索引越界。
5. 已完成：短音频多任务冒烟已通过。使用 3 条 10 秒样本任务验证到：
   - 后端只拉起 1 个 sidecar 进程；
   - 3 条任务在同一 sidecar 内串行完成；
   - sidecar 在 15 秒空闲阈值后收到 `shutdown requested` 并成功退出。
6. 已完成：已查到当前 CPU 利用率偏低的一个上游关键原因：
   - `qwen_asr_gguf/inference/llama.py` 默认把 `n_threads` 设成 `cpu_count // 2`
   - 在 `Intel N100 / 4C` 上，解码线程默认只吃 `2` 核
   - 这与我们观察到“CPU 没完全打满”一致。
7. 已完成：已记录“真并行多音频”的初步可行性判断：
   - 现阶段不建议直接承诺。
   - 上游导出的 Qwen3-ASR 代码里明确写了 `audio encoder do not support batch inference to keep precision`。
   - 当前更现实、收益更高的路线是：先把 sidecar 常驻复用、线程数调优、必要时切 GPU 或更小模型。
8. 待执行：如果下一轮继续优化吞吐，优先顺序建议为：
   - 给本地 sidecar 增加可配置 `n_threads / n_threads_batch`
   - 在当前机器上重新测 `RTF`
   - 再评估是否要深改上游做“真正批内并行”

### 验收标准
- `local_qwen` 连续任务不会每条都重新加载模型。
- 超过空闲阈值后，sidecar 会释放内存并退出。
- 多条短任务顺序执行成功，队列不死锁、不丢任务。
- 文档里明确记录：当前是“常驻 sidecar + 串行队列”，不是“真并行多音频”。

### 验收标准
- `CLASSFLOW_ASR_PROVIDER=dashscope` 时，现有测试与行为不回退。
- `CLASSFLOW_ASR_PROVIDER=local_qwen` 时，一条课程任务能完成“抽音频 -> 本地转写 -> 入库 -> 生成后续产物”。
- 本地转写失败时不自动切回云端，而是明确失败并给出日志。
- 本地 provider 全局串行执行，避免多任务同时压本地模型。
- 当前轮次至少完成“单次脚本模式”真实可用；常驻 sidecar 留到下一轮，前提是不影响现阶段稳定替换云端 ASR。

## 目标
- 在 `ClassFlow` 仓库中实现第一阶段完整方案：Rust 后端、React + Cloudflare Worker 前端、以及与 `smartclass-downloader` 的联动改造。

## 执行步骤
1. 已完成：初始化主仓库基础文件，建立 `PLANS.md`、`PROGRESS.md`、`.gitignore` 等执行基线。
2. 已完成：搭建 Rust 后端工作区与核心模块，打通配置、数据模型、API、SQLite 持久层、任务执行骨架。
3. 已完成：为后端补全单元测试、集成测试与 mock 流程测试。
4. 已完成：搭建 React + Cloudflare Worker 前端，完成任务台、课程库、代理 API，并通过 `lint / test / build`。
5. 已完成：改造 `smartclass-downloader`，增加 `Gopeed / ClassFlow` 双模式投递，并补上 `node:test` 脚本测试。
6. 已完成：已补齐部署文档、systemd 示例、cloudflared 示例，并完成当前环境下可自动执行的最终验收测试。
7. 已完成：已在真实环境中启动 `systemd --user` 后端服务、跑通 DashScope 真转写冒烟、发布 `workers.dev` 前端，并验证公网 API 与课程产物读取。
8. 已完成：已移除前端定时轮询，改为“手动刷新 + 页面重新聚焦同步”；并把 Worker 后端地址切换到 `classflow-backend.zjhstudio.com`。
9. 已完成：统一整理后端 / Worker / userscript 的鉴权说明，完成 userscript“走 Worker 时可不手填 token”的改造，并完成共享 token 旋转与重新部署验证。
10. 已完成：在不重启当前 release 后端的前提下，完成前端安全改动：总稿 404 误导修复、下载按钮补齐、交互反馈增强，并重新发布 Worker；后端下载/上传并发与进度采集改造留待后续启用。
11. 已完成：后端已补齐下载/上传鲁棒性主线：`aria2c` 下载、下载重试与低速退出、上传与转写并发拆分、DashScope 请求重试、上传重试、上传/转写检查点持久化，以及“失败后从已完成阶段继续”的 API 集成测试。
12. 已完成：补齐“用户肉眼可感知”的交互与可观测性闭环，包括前端按钮/切换动效显著化、长错误文本自动换行、受控下载反馈，以及后端下载/上传阶段进度与速率暴露；同时把 `aria2c` 低速退出改成“默认关闭、显式配置才启用”。
13. 已完成：按上线前最终口径收口前端与部署：已彻底移除 `useReducedMotion` 降级逻辑、确保“任务运行中的下载/上传进度与速率”能在前端持续可见，并已同步部署前后端。
14. 已完成：修复后端下载进度采集的时序错误，避免 `aria2c` 进度只在进程退出后才一次性写库；真实任务现已验证“下载中持续更新进度/速率”与“切到上传阶段后继续更新进度/速率”。
15. 已完成：把任务台从“运行中 1.2 秒短轮询”改成“后端 SSE 推送任务摘要 + 前端手动刷新详情兜底”，优先压缩 Worker 请求次数，并保留速率/进度可见性；后端 `cargo check / test`、前端 `lint / test / build` 均已通过。

## 当前决策
- 后端使用 `axum + tokio + sqlx(sqlite) + reqwest + aria2c`。
- 前端使用 Cloudflare 官方 React Workers 脚手架方向，浏览器只访问 Worker，不直接访问后端。
- 课程归组键固定为 `学期 + 日期 + 课程名 + 老师`。
- 第一版只报告已收片段数，不启发式推断课程理论节数。
- 后端公网出口使用 `classflow-backend.zjhstudio.com` 固定域名；Quick Tunnel 仅保留为应急调试手段。
- 前端默认关闭自动轮询，优先降低闪烁与 Worker 请求次数。
- 新一轮改造改为“Worker 绑定 R2，后端通过 Worker 私有接口读写最终产物”，而不是后端直连 R2。（原因：用户明确要求不暴露 R2 外链，且最终产物以文本/JSON 为主，适合由 Worker 做统一下载出口。）
- 成功任务的本地媒体临时文件在产物成功入库后立即删除；失败任务本地工作目录保留 `7` 天，若之后重试成功则立即删除。（原因：既要节省本地空间，也要给失败任务保留复查与重试窗口。）
- 长期保留的课程总稿、manifest、原始结果统一放在 R2；本地尽量不保留长期产物。（原因：避免校园机磁盘逐渐被历史产物占满，同时让前端所有下载都走 Worker。）
- 任务与课程界面需要提供明确的 hover / tap / 选中动画、任务放弃删除入口，以及“凡仍存在于 R2 的产物，都能在前端直接下载”。（原因：当前可点击反馈弱、下载入口不全，已经影响实际使用。）
- 下载链路必须切换到 `aria2c`，并显式落地：断点续传、自动重试、连接超时、低速判定、下载失败分类。（原因：当前 `reqwest + response.bytes()` 已经在真实环境里频繁出现下载中断，不能继续当成可交付实现。）
- 上传链路至少要补齐：请求超时、自动重试、上传并发与转写并发拆分、阶段级断点续跑；不能再只靠“整任务重试”。（原因：校园网弱上行环境下，上传失败是必然要面对的主路径，而不是边角异常。）
- 新一轮前端交互要以“可感知反馈优先”收口：即使用户系统偏好减少动态效果，也要保留轻量 hover / press / 渐变反馈，而不是把按钮完全退化成近乎静态文本。（原因：当前实现把 `useReducedMotion()` 直接用作总开关，导致部分环境里几乎看不到任何交互反馈。）
- `aria2c` 的低速退出默认值不再强制启用；只有显式配置正数阈值时才启用“低于某速率自动失败”。（原因：校园网/教学网场景天然存在长时间低速，默认强杀会把本可完成的慢下载误判成失败。）
- 前端最终交付不再保留 `reduced motion` 条件分支，统一输出明确的 hover / tap / 切页动画。（原因：本项目是操作台，不是内容站；这里更重要的是操作确认感，而不是因环境偏好把交互反馈整体削弱。）
- “进度条与速率”验收标准以“任务运行中后端阶段可见”为准，而不是浏览器下载产物时可见。（原因：用户明确关心的是后端任务正在下载视频、正在上传音频时的实时状态。）
- 下载进度链路必须采用“子进程运行中并发读 stdout/stderr 并立刻写库”的实现，不能先 `wait()` 再统一解析输出。（原因：后一种写法会让数据库在任务执行期间长期停留在空值，前端轮询看不到任何实时进度。）
- 任务台的实时状态同步优先采用 `SSE`，不继续依赖高频 HTTP 轮询；任务详情日志保留“选中加载 + 手动刷新”模式。（原因：用户明确要求降低 Cloudflare Worker 按请求计费消耗，而本场景只需要后端单向推送进度，不需要 WebSocket 的双向能力。）

## 2026-04-22 网络健康闸门与百炼 task_id 检查点 ExecPlan

### 目标
- 给 Rust 后端增加“网络健康闸门”：当外部网络处于 Wi-Fi/AP 漫游、断联、代理链路短时不可达等波动期时，暂停新的外部网络操作，等待探针确认网络恢复后再继续。
- 让网络波动期间的失败不消耗普通业务重试次数，避免任务因为临时断网被标记为真正失败。
- 给百炼录音文件 REST 异步转写增加 `task_id` 检查点：提交成功后立即落库，断网恢复后优先继续轮询同一个百炼任务，降低重复提交与重复计费风险。
- 同时处理 `task_id` 可能过期的问题：只在安全窗口内复用；如果百炼明确返回不存在/过期，则清理旧 `task_id`，用仍有效的 `oss://` 上传检查点重新提交。

### 执行步骤
1. 已完成：梳理现有 `PipelineIo`、`Repository`、`WorkerArtifactStore`、任务表可选列迁移和 API 测试夹具，确定最小改动面。
2. 已完成：新增网络健康模块，维护 `Healthy / Unhealthy / Recovering` 状态、外部探针、恢复等待与网络错误分类。
3. 已完成：把真实 DashScope 上传/提交/轮询/结果下载，以及 Worker/R2 产物写入读取删除纳入网络健康闸门；网络不健康时等待恢复，不把等待期算作普通重试失败。
4. 已完成：为任务表增加 `dashscope_task_id` 与保存时间字段，提交百炼任务成功后立即保存，重试转写阶段时优先复用。
5. 已完成：实现 `task_id` 复用窗口和失效兜底：旧 `task_id` 超过安全窗口或百炼返回不存在/过期时清理并重新提交，不让任务永久卡住。
6. 已完成：补充后端测试，覆盖网络闸门暂停/恢复、`task_id` 复用、`task_id` 过期后重新提交、以及既有产物失败后转写检查点续跑不回退。
7. 已完成：运行后端格式化、编译、测试、Clippy，更新 `PROGRESS.md` 并提交。

### 当前状态
- 已完成：已确认当前转写链路是百炼录音文件 REST 异步任务，不是实时 WebSocket；本轮已从“只保存 `oss://` 上传检查点和最终 `transcript_json`”推进为“提交百炼后立即保存 `task_id`”。
- 已决策：`task_id` 只作为短期恢复检查点，不能作为永久事实；过期或百炼侧已删除时必须能清理并重新提交。
- 已决策：网络健康闸门以“恢复后继续”而不是“立刻判任务失败”为主；普通业务错误、配置错误、鉴权错误、内容类转写失败不进入网络等待。
- 已完成：验证已通过 `cargo fmt --check --manifest-path apps/backend/Cargo.toml`、`cargo check --manifest-path apps/backend/Cargo.toml`、`cargo test --manifest-path apps/backend/Cargo.toml`、`cargo clippy --manifest-path apps/backend/Cargo.toml --all-targets --all-features -- -D warnings`。

## 2026-04-22 写入产物偶发失败只读排查 ExecPlan

### 目标
- 在不修改业务代码的前提下，排查今天线上任务在 `storing_artifacts` / “写入产物”阶段偶发失败、随后重试成功的原因。
- 对比今天失败样本与 2026-04-09、2026-04-13 两轮历史写产物故障，判断是否仍是同一类 Worker/R2/Access/代理链路问题，还是新的阶段超时、请求重试耗尽、课程产物重建冲突或 R2 写入抖动。
- 通过日志、数据库、真实探针和自动化测试给出证据，不做业务代码修复。

### 执行步骤
1. 已完成：读取 systemd 后端日志、SQLite 任务表与事件日志，筛出今天所有失败后成功的任务，记录任务 ID、课程、阶段、错误文本、失败时间与重试成功时间。
2. 已完成：审查 `WorkerArtifactStore`、`sync_course_artifacts` 与 Worker 私有产物接口的现有实现，确认写入阶段实际会连续写哪些对象、每个对象是否有重试、哪些失败会中断整任务。
3. 已完成：复盘相关 git 历史，找出此前“写入产物”链路的改动点，并对照今天错误是否命中同一边界。
4. 已完成：做临时探针压测，验证后端到 Worker 私有产物接口的 `PUT/DELETE` 延迟、状态码、Access 头、代理路径稳定性；探针对象使用临时路径，不改业务数据。
5. 已完成：运行现有后端与 Worker 测试，确认当前代码层面的产物写入、Access 重定向诊断、公开产物读取测试仍全绿。
6. 已完成：汇总根因判断、复现概率、直接证据与下一步修复建议；本轮未修改业务代码。

### 当前状态
- 已完成：本轮确认 2026-04-21 晚间共有 4 个任务在 `storing_artifacts` 阶段失败后重试成功，失败集中在北京时间 21:31 到 21:45，错误为后端写 Worker 私有产物接口时 `请求超时` / `请求发送失败`。
- 已完成：同一时间段还出现了多条 DashScope 临时 OSS 上传 `error sending request` 重试日志，说明当时不是单个 R2 对象、Access 鉴权或产物路径错误，而是本机外网出口 / 代理链路抖动影响多个外部服务。
- 已完成：当前以真实后端进程环境、普通用户身份对 `workers.dev` 与自定义前端域名各做 10 次临时 `PUT` 探针，均返回 `204`；当前链路已恢复，未复现持续性 Worker/R2 故障。
- 已完成：验证已通过 `cargo check --manifest-path apps/backend/Cargo.toml`、`cargo test --manifest-path apps/backend/Cargo.toml`、`cargo clippy --manifest-path apps/backend/Cargo.toml --all-targets --all-features -- -D warnings`、`npm test`、`npm run lint`、`npm run build`。

## 2026-04-22 上传带宽与 mihomo 链路对照排查 ExecPlan

### 目标
- 在不修改业务代码的前提下，继续排查 2026-04-21 晚间写入产物失败是否由“OSS 上传 + Worker 写入同时抢占 3~5 Mbps 上行”触发。
- 对照同一时间窗口的 `mihomo` 日志，判断 Worker / OSS 请求当时是走代理还是直连，以及是否存在 `i/o timeout`、连接失败、节点切换等证据。
- 讨论后续治理方向：应用内并发/带宽整形、系统级 QoS / 限速、代理规则调整，先形成判断，不改代码。

### 执行步骤
1. 已完成：从 SQLite 事件日志和 systemd 日志还原北京时间 2026-04-21 21:20 到 21:50 的任务阶段时间线，标出 OSS 上传开始/成功/重试、Worker 写入开始/失败/重试。
2. 已完成：结合当前配置 `CLASSFLOW_TASK_WORKER_COUNT`、`CLASSFLOW_UPLOAD_CONCURRENCY`、WorkerArtifactStore 实现，估算当时最多同时存在多少条 OSS 上传与 Worker 写入请求。
3. 已完成：估算失败样本里 Worker 产物正文/JSON 的体积量级，与 OSS 音频上传体积量级对比，判断 Worker 写入是否足以打满 3~5 Mbps 上行。
4. 已完成：读取 `mihomo.service` 在同一时间窗口的 journal 日志，检索 `classflow-web`、`workers.dev`、`dashscope`、`oss-cn-beijing`、`timeout`、`DIRECT`、代理组命中等线索。
5. 已完成：汇总两个假设的证据权重，并给出下一步可选保护措施，不修改业务代码。

### 当前状态
- 已完成：失败窗口里 OSS 上传走 `DIRECT`，Worker 写入走 `Kuromis[香港 01]`；WorkerArtifactStore 没有独立全局并发信号量，只受 `TASK_WORKER_COUNT=4` 间接限制，而 OSS 上传由 `CLASSFLOW_UPLOAD_CONCURRENCY=2` 限制。
- 已完成：失败对象 `segments/*.json` 体积约 `80KB~137KB`，单节 Markdown 约 `9KB~15KB`，而 45 分钟 `16kHz mono pcm_s16le` WAV 理论约 `86MB`；Worker 产物本体不大，不太像“请求体 30 秒内传不完”，更像弱上行/链路重连时小请求握手或响应被拖死。
- 已完成：北京时间 21:29:18 到 21:29:24，`wlp3s0` 发生 AP 漫游/重连、两次认证超时、`mihomo` TUN 默认接口变化、`cloudflared` QUIC `timeout: no recent network activity`；这与第一批 Worker 写入失败高度重合。
- 已完成：21:39、21:41、21:46、21:54 附近还继续出现 Wi-Fi AP 切换/掉 carrier，说明失败窗口不是一次孤立请求错误，而是校园 Wi-Fi 链路不稳定。
- 已完成：`speedtest-tracker` 配置为 `*/30 * * * *`，21:30 确有 `cli.speedtest.net` 连接；但 speedtest 数据库显示该次测速 `failed`，错误为 `ConfigurationError`，主要证据不是测速持续打满带宽，而是它也在网络最乱的窗口触发了一次外部连接。
- 结论：两个假设里，“带宽/并发竞争”是放大因素，“Wi-Fi 漫游/重连 + 代理/DIRECT 混合链路被打断”是更强的直接证据。后续保护措施应优先考虑：固定/优化无线连接、避开半小时 speedtest、给全局上行做系统级整形，另可补应用内 Worker 写入并发/退避保护。

## 2026-04-22 speedtest、校园网认证与 AP 漫游对齐排查 ExecPlan

### 目标
- 在不修改业务代码的前提下，查清 `speedtest-tracker` 是从哪里启动、何时配置、日志与数据放在哪里，并判断是否应先暂停该后台测速任务。
- 对齐北京时间 2026-04-21 晚间故障窗口里的 `NetworkManager`、内核 Wi-Fi、`mihomo`、`cloudflared`、`campus-login` 与 ClassFlow 后端日志，判断“Worker 写入失败”是否可归因到无线网卡 AP 漫游/重连。
- 区分三种可能：无线 AP 漫游导致底层断联、校园网认证掉线/脚本重登导致断联、或脚本周期太粗导致恢复等待时间被放大。

### 执行步骤
1. 已完成：定位 `speedtest-tracker` 的 Docker Compose、容器、数据库、调度表达式、日志输出与创建时间，并停止容器但保留数据。
2. 已完成：读取 `campus-login.service`、`campus-login.timer` 与认证脚本，确认真实执行周期、探测目标、日志输出方式，以及是否会改 IP / 重新认证 / 重启网络。
3. 已完成：按秒级时间线抽取 2026-04-21 21:20 到 21:55 的 NetworkManager、内核 `wlp3s0`、`mihomo`、`cloudflared`、`campus-login`、ClassFlow 后端日志。
4. 已完成：检查 `nmcli` 当前 Wi-Fi 连接配置、BSSID、漫游/自动连接策略、DHCP 记录与 IP 变化记录，判断 AP 切换更像“AP 踢下线”还是客户端主动漫游。
5. 已完成：给出本轮证据权重、是否能排除“强小上行”为主因、以及后续运维治理建议；本轮不改业务代码。

### 当前状态
- 已完成：`speedtest-tracker` 不是 systemd 单元，而是 `/home/zjhsteven/speedtest-tracker/docker-compose.yml` 启动的 Docker 容器，镜像为 `lscr.io/linuxserver/speedtest-tracker:latest`，创建于 `2026-03-06 19:10:19 +0800`，配置 `restart: unless-stopped` 与 `SPEEDTEST_SCHEDULE="*/30 * * * *"`。
- 已完成：`speedtest-tracker` 数据与日志位于 `/home/zjhsteven/speedtest-tracker/config`，其中 SQLite 为 `config/database.sqlite`，应用日志目录为 `config/log`；容器已执行 `docker stop speedtest-tracker`，当前为 `Exited (0)`，未删除数据。
- 已完成：`speedtest-tracker` 在北京时间 `2026-04-21 21:30` 触发过 `cli.speedtest.net:443` 连接，但数据库记录显示 `21:00 / 21:30 / 22:00` 三次都是 `failed`，错误为 `Configuration - Could not retrieve or read configuration (ConfigurationError)`，不像持续测速流量打满上行。
- 已完成：`campus-login.timer` 实际为开机 2 分钟后首次执行，之后按 `OnUnitActiveSec=10min` 周期运行，不是 5 分钟；故障窗口里的 `21:22 / 21:32 / 21:42 / 21:52` 均只是检测到百度可访问后退出，没有执行门户登录 POST。
- 已完成：`campus-login.sh` 不会主动重启网卡或改 IP；它只检查当前 SSID、访问百度、必要时向校园网门户发登录请求。故障窗口未触发登录请求，因此“校园网认证脚本导致断网”证据弱。
- 已完成：`21:29:18` 日志明确为 `wlp3s0` 从当前 AP `06:05:88:4b:c8:99` 断开并尝试认证 `06:05:88:4b:bb:55`，随后又尝试 `06:05:88:4b:cc:41`，两次认证超时后在 `21:29:22` 回到 `06:05:88:4b:c8:99`；同一秒 `cloudflared` 报 `timeout: no recent network activity`，`mihomo` 报 TUN 默认接口变化。
- 已完成：`21:39:37`、`21:41:07~21:41:08`、`21:46:15`、`21:54:55~21:54:58` 还有多次 AP 切换或 deauth/reassoc。DHCP 每次都很快拿回同一个 `100.65.37.205`，没有看到 IP 变化或等待门户认证的证据。
- 结论：这批失败的主因更应理解为无线网卡/AP 漫游重连造成的底层瞬断，`mihomo`、`cloudflared`、Worker 写入和 OSS 请求只是同一底层断联的不同表现；小上行和并发可能放大脆弱性，但不是本轮最强主因。

## 2026-04-13 任务台 SSE 请求风暴修复 ExecPlan

### 目标
- 修复任务台进入页面后持续显示“正在加载任务列表...”并反复请求 `/api/v1/tasks` 的前端状态循环。
- 保持任务状态刷新以 SSE 为主：服务端有任务变化时推送任务摘要快照，前端不再因为普通状态更新重复触发阻塞式 HTTP 列表加载。
- 保留 HTTP 作为必要兜底：首屏 SSE 不可用或超时时才请求任务列表；重试、删除、下载、详情按需查看仍继续走 HTTP。
- 增加回归测试，明确断言任务台稳定后不会持续创建新的普通任务列表请求，也不会重复重建 SSE 连接。

### 执行步骤
1. 已完成：定位到 `TaskPanel` 中 `tasks -> loadTaskDetail -> loadTasks -> useEffect` 的依赖链会导致列表更新后重新触发阻塞加载。
2. 已完成：重构 `TaskPanel` 的任务列表状态读取方式，用 `ref` 保存最新任务摘要，避免详情缓存判断依赖 `tasks` 状态本身。
3. 已完成：让任务页首屏优先等待 SSE 首帧；只有 SSE 不可用、解析失败或首帧超时才退回一次 HTTP 列表加载。
4. 已完成：调整手动刷新/回到前台同步逻辑，避免在 SSE 已连接时无意义调用 `/api/v1/tasks`；按钮改为“重连任务流”。
5. 已完成：补充前端测试，覆盖稳定首屏不会持续重复请求任务列表、SSE 首帧可填充任务列表、SSE 失败可退回 HTTP。
6. 已完成：前端验证已通过 `npm run lint`、`npm test`（19 项）、`npm run build`；本轮只改前端 SSE 消费与测试，未改后端 SSE 契约。
7. 已完成：前端 Worker 已重新构建并通过 `npx wrangler deploy` 发布，当前版本号为 `1659f30b-44ae-4842-90d5-f5c3010ff388`；匿名线上探针返回 `302` 到 Cloudflare Access 登录页，符合当前入口受 Access 保护的预期。

## 2026-04-13 临时 OSS 重试与 HGP404 写产物排查 ExecPlan

### 目标
- 修复“历史上传检查点只看数据库 `uploaded_source_url`，不考虑 DashScope 临时 `oss://` 文件已过期”的重试缺陷，避免失败任务多天后重试仍跳过上传并继续失败。
- 排查最近毛泽东思想课程三节课在 `storing_artifacts` 阶段失败、日志出现 `HGP404` 的真实原因，区分是 Worker 私有产物接口、Cloudflare Access / 代理链路、R2 对象路径，还是后端写入策略问题。
- 补齐自动化测试，覆盖“临时 OSS 过期后必须重新上传”和“写产物 404 错误信息必须足够可诊断”这两条回归边界。
- 如代码修复通过验证，再构建并重启系统级后端服务，必要时对失败任务执行受控重试验证。

### 执行步骤
1. 已完成：审查 `worker.rs`、`pipeline.rs`、`artifacts.rs`、SQLite 仓储与现有 API 测试，定位上传检查点、转写检查点、产物写入错误传播的当前实现。
2. 已完成：读取真实数据库与系统级后端日志，按课程名、日期、阶段、错误文本筛出“医学人文英语”和最近三节“毛泽东思想”失败任务，确认失败发生时间、任务 ID、检查点字段和最新错误。
3. 已完成：修改重试策略：如果失败阶段在转写及之前，且保存的是 `oss://` 临时 URL，就不能跨多天盲跳上传；超过 47 小时安全复用窗口或缺少保存时间时会重新上传音频。
4. 已完成：增强 Worker 产物写入 404 / Access 重定向的错误上下文，包含产物路径、HTTP 状态、响应体、请求 URL 与 `Location`，避免只显示无法定位来源的短码。
5. 已完成：新增后端测试，覆盖临时 OSS 过期重试、未过期重试仍可复用上传检查点、产物写入遇到 Cloudflare Access 重定向时不自动跟随到空体 404。
6. 已完成：后端格式化、编译、测试、Clippy、release 构建、系统级 `classflow-backend.service` 重启与真实任务重试验收均已执行。

### 当前状态
- 已完成：医学人文综合英语失败任务 `498d9a22-0968-4929-89ed-b8336c8d1999` 的旧 `oss://` 上传路径来自 `2026-04-04`，在 `2026-04-13` 重试时百炼返回 `BadRequest.ResourceNotExist`，根因是临时 OSS 对象已过期但后端仍跳过上传。
- 已完成：三条毛泽东思想失败任务 `bbcd053e-e4a5-4de9-880b-09a8f2c74a77`、`c81a5111-e0b3-4a22-9a5a-8b39404f1fb0`、`fa6c16b3-7125-4fae-8ac8-78f119cd7d99` 都已有转写检查点，失败点在写 Worker 私有产物；真实探针证明后端请求先被 Cloudflare Access 拦成 `302`，旧 `reqwest` 自动跟随重定向后最终表现为空体 `404`。
- 已完成：本地后端验证已通过 `cargo fmt --check --manifest-path apps/backend/Cargo.toml`、`cargo check --manifest-path apps/backend/Cargo.toml`、`cargo test --manifest-path apps/backend/Cargo.toml`、`cargo clippy --manifest-path apps/backend/Cargo.toml --all-targets --all-features -- -D warnings`。
- 已完成：真实系统级后端环境文件已补齐产物代理访问 Worker 所需的 Cloudflare Access Service Token 变量，release 后端已重新构建并重启，健康检查返回 `200 OK`，后端私有产物接口 `PUT + DELETE` 探针均返回 `204`。
- 已完成：三条“毛泽东思想”真实失败任务已重试成功并转为 `succeeded`。
- 已完成：医学人文综合英语任务已按新策略重新上传音频并提交百炼，已不再报旧的 `BadRequest.ResourceNotExist`；当前剩余失败为百炼内容类结果 `SUCCESS_WITH_NO_VALID_FRAGMENT`，与旧的 `ASR_RESPONSE_HAVE_NO_WORDS` 同类，说明它已经越过临时 OSS 过期问题，剩下要判断原始音频是否无有效可识别语音。

## 2026-03-25 本轮 ExecPlan

### 目标
- 收紧前端布局密度，确保主操作按钮在常见笔记本浏览器视口内更容易一次看全。
- 为任务台、课程库的左侧列表和右侧详情/日志区域补上明确高度上限与内部滚动，避免页面整体被历史数据无限拉长。
- 调整任务与课程排序口径，改为“日期越新越靠前”，优先展示最近课程。
- 优化课程总稿读取链路，尽量减少“预览一次、下载一次就重复命中多层 Worker”的额外消耗。

### 执行步骤
1. 审查任务台、课程库、下载链路、Worker 代理与仓储排序代码，明确哪些请求是当前重复发生的。
2. 调整前端页面骨架与样式：压缩字号、间距、卡片内边距、表格行高，并为左右面板设定更稳定的视口高度。
3. 为任务台左侧列表、课程库左侧列表、任务详情日志区、错误提示区、课程总稿预览区增加内部滚动与长度上限，确保页面主按钮不被长内容挤出首屏。
4. 调整任务列表与课程列表排序规则，统一改为按课程日期倒序展示，并保持同日内排序稳定。
5. 优化课程总稿读取与下载逻辑，优先复用已拿到的总稿内容，减少重复请求；同时评估并收敛 Worker 产物读取是否可以避免“Worker 再请求自己”的链路。
6. 更新或新增前端/后端测试，覆盖排序、滚动容器关键结构、课程总稿复用逻辑与现有 SSE 行为不回退。
7. 运行 `npm run lint`、`npm test`、`npm run build`，并补跑相关后端测试；完成后更新 `PROGRESS.md` 并提交。

### 当前状态
- 已完成：步骤 1 到 7 均已执行完毕。
- 已完成：前端布局已收紧，左右面板改为固定高度配合内部滚动，长日志/长报错/长课程列表不再把整页无限撑长。
- 已完成：课程总稿链路已优化为“公开 Worker 直读 R2 + 前端预览复用到下载”，减少重复 Worker 命中。
- 已完成：任务列表与课程列表均已改成最新日期优先，相关前后端测试、前端构建与后端测试均已通过。

## 2026-03-27 DashScope 转写模型修正 ExecPlan

### 目标
- 核对阿里云百炼录音文件转写当前官方模型名，确认 `fun-asr` 与 `fun-asr-mtl` 的适用差异。
- 在不破坏现有上传凭证、异步转写、任务轮询链路的前提下，把后端默认模型从旧值调整为更符合当前免费额度方案的模型名。
- 把模型配置入口明确暴露到环境变量与部署文档，避免后续再靠源码猜默认值。
- 通过自动化测试与真实 DashScope API 冒烟共同确认改动有效。

### 执行步骤
1. 审查后端配置、上传凭证请求、转写任务提交、测试夹具与部署文档，定位模型名的所有读写入口。
2. 查阅阿里云官方文档，确认录音文件转写可用模型名，以及临时 `oss://` 上传与模型名必须一致的约束。
3. 用当前环境的真实 `DASHSCOPE_API_KEY` 做最小化 API 冒烟，至少验证 `getPolicy` 能接受目标模型名；若条件允许，再补一次最小音频上传与异步转写提交验证。
4. 修改后端默认配置、示例环境变量、测试默认值与部署说明，把模型名暴露为可直接修改的显式配置项。
5. 运行后端格式化、静态检查与测试；若真实 API 冒烟可执行，则再次验证改后的目标模型链路。
6. 更新 `PROGRESS.md` 记录本轮结论、关键决策与部署注意事项，并提交。

### 当前状态
- 已完成：已定位当前默认模型为 `fun-asr`，且上传凭证与转写提交共用同一配置值。
- 已完成：已核对阿里云官方文档，并完成真实 API 冒烟，确认 `fun-asr` 与 `fun-asr-mtl` 当前都可正常提交并成功转写官方示例音频。
- 已完成：已把默认模型调整为 `fun-asr-mtl`，并把配置入口在示例环境变量与部署文档中进一步写清楚。
- 已完成：后端验证已通过 `cargo fmt --check`、`cargo check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test`。

## 2026-03-28 课程总稿下载命名微调 ExecPlan

### 目标
- 把前端“下载课程总稿”的默认文件名改成你既有的使用习惯：`月.日-课程名-老师.md`。
- 保持课程原始数据里的完整日期不变，只调整浏览器下载时展示给用户的文件名。
- 确认这次改动只影响前端课程级下载，不误伤后端存储键、课程归组键与已有产物路径。

### 执行步骤
1. 审查课程库面板里课程总稿与课程清单的文件名拼接逻辑，确认当前规则与受影响按钮范围。
2. 把下载文件名改为“日期只保留月.日，顺序为日期-课程名-老师”，并为缺失或异常日期准备安全兜底，避免生成空文件名。
3. 新增或更新前端测试，覆盖“课程总稿下载命名符合旧规则”以及“已预览总稿时下载仍不重复请求”的现有行为不回退。
4. 运行前端 `npm test`、`npm run lint`、`npm run build` 完整验证。
5. 更新 `PROGRESS.md` 记录本轮命名规则变更、测试结果与重新部署状态，并完成前端重新部署。

### 当前状态
- 已完成：已把课程总稿下载文件名改为 `月.日-课程名-老师.md`，并保持后端课程日期、课程归组键、产物路径不变。
- 已完成：前端新增 `courseDownloadFilename` 命名工具与对应单元测试，覆盖标准日期、兼容日期、非法字符清洗、缺省兜底命名。
- 已完成：前端验证已通过 `npm test`（15 项测试）、`npm run lint`、`npm run build`。
- 已完成：Cloudflare Worker 已重新部署到 `https://classflow-web.zhangjiahe0830.workers.dev`，版本号 `81191b71-ac36-4b71-ba66-5b2591d49fc4`；线上抽样验收的首页与 `/api/v1/health` 均返回正常。

## 2026-04-03 系统级托管与 Access 收口 ExecPlan

### 目标
- 把当前依赖 `systemd --user` 的后端常驻方式迁移为真正的系统级 `systemd` 服务，彻底消除“SSH / VS Code 会话断开后后端消失”的风险。
- 迁移时继续沿用现有真实数据目录与环境变量文件，避免改动数据库、缓存、临时目录与产物目录造成额外风险。
- 梳理当前前端、Worker、后端 Tunnel、自定义域名与 `workers.dev` 的访问关系，为后续接入 Cloudflare Access Service Token 做准备。
- 把当前机器上的真实部署路径、systemd 单元位置、数据目录位置与 Access 配置步骤补进文档，方便后续维护与自动化测试。

### 执行步骤
1. 审查当前用户级 `classflow-backend.service`、真实 `backend.env`、数据库路径、临时目录、产物目录与 `cloudflared` 状态，确认迁移边界。
2. 在不迁移数据目录的前提下，生成并安装系统级 `classflow-backend.service`，让它以当前账号 `zjhsteven` 身份运行，但由系统级 `systemd` 在开机阶段托管。
3. 停用用户级 `classflow-backend.service`，启动系统级服务，并验证本机 `127.0.0.1:8787`、Tunnel 外网健康检查与当前进程归属。
4. 梳理仓库与现网中所有依赖 `workers.dev`、后端 Tunnel 域名、`BACKEND_BASE_URL`、`BACKEND_TOKEN` 的位置，明确后续 Access 收口时必须同步修改的点。
5. 更新 `docs/deployment.md`、`PLANS.md`、`PROGRESS.md`，记录真实部署路径、目录清单、系统级托管方案，以及 Cloudflare Access / Service Token 的配置步骤。
6. 运行必要验证命令，确认迁移后服务可持续运行；完成后提交本轮修改。

### 当前状态
- 已完成：已确认当前线上后端此前实际运行于 `~/.config/systemd/user/classflow-backend.service`，并且用户未开启 `linger`，这正是“VS Code 断开后后端跟着消失”的直接原因。
- 已完成：已确认真实环境变量文件原始来源位于 `/home/zjhsteven/.config/classflow/backend.env`，并已复制到系统级路径 `/etc/classflow/backend.env`；真实数据库位于 `/home/zjhsteven/.local/state/classflow/data/classflow.db`，临时目录位于 `/home/zjhsteven/.local/state/classflow/tmp`，本地产物目录位于 `/home/zjhsteven/.local/state/classflow/artifacts`。
- 已完成：已确认 `cloudflared.service` 当前由系统级 `systemd` 正常托管并已启用。
- 已完成：系统级 `/etc/systemd/system/classflow-backend.service` 已安装并 `enable --now`，当前以后端二进制 `/home/zjhsteven/ClassFlow/target/release/backend` 运行于 `/system.slice/classflow-backend.service`。
- 已完成：旧的用户级 `classflow-backend.service` 已 `disable --now`，当前 `http://127.0.0.1:8787/api/v1/health` 与 `https://classflow-backend.zjhstudio.com/api/v1/health` 验证均通过。
- 已完成：已在 `docs/deployment.md` 中补充当前校园机的真实部署路径、系统级托管方式、以及 Cloudflare Access / Service Token 的配置步骤。

## 2026-04-09 Worker 回源与前端卡顿排查 ExecPlan

### 目标
- 先验证 `classflow-web.zhangjiahe0830.workers.dev` 走代理节点是否是当前 `storing_artifacts` 失败的直接诱因。
- 在线上环境最小化改动 `mihomo` 规则，把该域名单独切到 `DIRECT`，然后立刻重试当前 `11` 个失败任务，确认任务是否能从“只差最后写产物”恢复成功。
- 若代理切换后故障仍存在，再继续收缩到后端 `WorkerArtifactStore` 传输策略与前端首屏 / 详情面板卡顿问题。
- 同步记录“系统级 service 但仍按 `uid=1000` 进入 TUN”的现网事实，避免后续继续把问题归因到错误方向。

### 执行步骤
1. 审查并备份当前 `/etc/mihomo/config.yaml`，确认 `tun.include-uid`、`redir-host`、规则顺序与 `classflow-web.zhangjiahe0830.workers.dev` 当前命中策略组。
2. 在 `rules:` 顶部附近新增对 `classflow-web.zhangjiahe0830.workers.dev` 的精确 `DIRECT` 规则，重载 `mihomo`，并确认新连接命中 `DIRECT` 而非 `Kuromis`。
3. 统计当前失败任务列表，在线上后端逐个调用重试接口，观察是否仍卡死在 `storing_artifacts`，并同步检查 `mihomo` / `journalctl` / 数据库状态。
4. 如果重试成功，继续补做后端最小健壮性修复：Worker 产物请求的超时、重试、错误展开与 URL 双斜杠清理。
5. 独立排查前端体验问题：首屏 `502`、`SSE` 首连易断、详情切换慢、课程面板串行请求重；先用现网探针复现，再决定是先改前端请求编排还是先改后端查询结构。
6. 更新 `PROGRESS.md` 记录本轮验证结果与最终结论，并按仓库要求提交。

### 当前状态
- 已完成：已确认 `classflow-backend.service` 虽由系统级 `systemd` 托管，但服务实际用户仍是 `zjhsteven`，而 `/etc/mihomo/config.yaml` 使用 `tun.include-uid: [1000]`，因此后端网络流量仍会进入 TUN，不会因“system 级托管”自动绕过代理。
- 已完成：已确认当前 `mihomo` DNS 模式为 `redir-host`，并非 `fake-ip`，因此现阶段不再把 `fake-ip` 作为主嫌疑。
- 已完成：已从 `mihomo` 日志中抓到 `2026-04-04 23:02:25 +0800`、`23:09:51 +0800`、`23:20:10 +0800` 对 `classflow-web.zhangjiahe0830.workers.dev:443` 的 `dial Kuromis ... i/o timeout` 直接证据。
- 已完成：已复现前端层面的一个近似问题：并发触发 `tasks` / `courses` / `tasks/stream` 时，普通列表请求常可返回 `200`，但 `SSE` 首连会间歇性在 TLS 层失败，这与“首屏首次加载常报 502、刷新后恢复”的现象一致。
- 已完成：已再次核对线上真实环境变量文件，确认 `/etc/classflow/backend.env` 仍保留旧值 `CLASSFLOW_DASHSCOPE_MODEL=fun-asr`，这与仓库里已切到 `fun-asr-mtl` 的默认值不一致，必须在本轮一并纠正，避免继续按旧模型计费。
- 已完成：已把线上 `/etc/classflow/backend.env` 的 `CLASSFLOW_DASHSCOPE_MODEL` 改为 `fun-asr-mtl`，并在无运行任务窗口重启系统级后端；随后已验证真实进程环境变量确实变为 `CLASSFLOW_DASHSCOPE_MODEL=fun-asr-mtl`。
- 已完成：已实测 `classflow-web.zhangjiahe0830.workers.dev -> DIRECT` 在这台机子上不可用；`curl` 直接打 Worker 会触发 TLS 失败，而 `mihomo` 记录到 `dial tcp 199.96.63.163:443: i/o timeout`。
- 已完成：已临时把 `classflow-web.zhangjiahe0830.workers.dev` 切到 `Exflux` 做对照实验；代表任务 `3a0da4ba-12c3-4b3e-8df3-21b5e179848f` 以及其余 `8` 条 `storing_artifacts` 失败任务均已重试成功，说明“最后一步写 Worker 产物失败”与链路选择高度相关。
- 已完成：按用户要求，线上 `mihomo` 现已把 `classflow-web.zhangjiahe0830.workers.dev` 的精确规则改回 `Kuromis` 主组，不再继续使用 `Exflux` 作为长期规则。
- 已完成：仓库代码已补强 `WorkerArtifactStore`：新增独立连接超时/总超时/有限重试/更完整错误上下文，并新增 URL 规范化与临时 `5xx` 自动重试测试；`cargo test -p backend` 已全绿。
- 已完成：已量化前端慢点来源。任务台首屏默认会并发触发 `GET /api/v1/tasks` 与 `GET /api/v1/tasks/stream`，随后再补 `GET /api/v1/tasks/{id}`；课程库则是 `GET /api/v1/courses` 之后串行触发 `GET /api/v1/courses/{key}` 与 `GET /api/v1/courses/{key}/artifacts/course.md`。
- 已完成：已定位“点任务详情特别慢”的硬根因之一：后端原先把 `transcript_json` / `transcript_text` 整包内联进 `/api/v1/tasks/{id}`，代表任务详情从后端返回时高达 `238466` 字节，其中 `transcript_json` 单独就占 `206697` 字节，而前端其实完全没用这两项。
- 已完成：已把任务详情接口改为“前端瘦身响应 + task.json 下载仍保留完整快照”，并重新部署系统级后端。重详情样本现已从 `238466 B / 2.18s` 降到 `5795 B / 0.52s`。
- 已完成：已对前端 `TaskPanel` 做两项直接缓解修复并重新部署 Worker：首次阻塞加载增加短重试，且首屏拿到首个列表成功前不再抢先建立 SSE；另外新增按 `updated_at` 命中的任务详情本地缓存，减少来回切换任务时的重复等待。
- 正在做：保留 `3` 条 `uploading_audio` 失败任务与 `1` 条 `ASR_RESPONSE_HAVE_NO_WORDS` 失败任务作为线上样本，继续把“DashScope 上传慢 / 易失败”和“前端首屏 502 是否已被缓解、SSE 首连是否仍偶发失败”作为下一阶段排查重点。

## 2026-04-09 Cloudflare Access 收口复核 ExecPlan

### 目标
- 把当前 `ClassFlow` 实际对公网暴露的前端、Worker 默认域名、后端 Tunnel 域名逐一核实清楚，避免只盯着自定义域名而漏掉旁路入口。
- 用“仓库代码 + 现网探针”双重证据确认：哪些入口已经被 Cloudflare Access 挡住，哪些入口仍能匿名访问，哪些入口虽然需要应用层 Bearer Token，但最外层仍未接入 Access。
- 输出一套可直接照着执行的收口顺序，优先先堵住真正能匿名读业务数据的入口，再处理后端 Tunnel 域名与自动化 Token。
- 将本轮结论固化到仓库记忆，避免后续再次误判“真正裸奔的到底是哪一个域名”。

### 执行步骤
1. 审查仓库内 `wrangler.toml`、Worker 代理代码、后端鉴权中间件与部署文档，定位所有公开访问入口与鉴权边界。
2. 对当前已知公网入口执行最小化在线探针，请求首页、健康检查与受保护业务 API，确认真实返回是 `200`、`401` 还是 `302 Access`。
3. 结合 Cloudflare 官方文档，核对 Access Service Token 的创建方式、请求头格式、`workers_dev = false` 与 Preview URL 的默认行为。
4. 整理“当前真实暴露面 -> 风险等级 -> 建议收口动作 -> 收口前置条件”的矩阵，明确先后顺序。
5. 更新 `PROGRESS.md` / `PLANS.md` 记录本轮事实与下一步操作建议，并提交。

### 当前状态
- 已完成：已确认仓库与文档中当前明确出现的公网入口至少有 `classflow.zjhstudio.com`、`classflow-web.zhangjiahe0830.workers.dev`、`classflow-backend.zjhstudio.com` 三个。
- 已完成：现网探针已确认 `https://classflow.zjhstudio.com/` 与 `https://classflow.zjhstudio.com/api/v1/tasks` 当前都会被 Cloudflare Access 重定向到登录页。
- 已完成：现网探针已确认 `https://classflow-backend.zjhstudio.com/api/v1/health` 当前可匿名访问，而 `https://classflow-backend.zjhstudio.com/api/v1/tasks` 在未带 Bearer Token 时会返回 `401`，说明它最外层尚未接入 Access，当前主要依赖应用层 `CLASSFLOW_BEARER_TOKEN`。
- 已完成：现网探针已确认 `https://classflow-web.zhangjiahe0830.workers.dev/api/v1/tasks` 当前可匿名返回完整任务列表；这说明真正的高风险公开入口是 `workers.dev` 默认域名，因为 Worker 会自动补上后端 Bearer Token。
- 已完成：已确认当前仓库的 [wrangler.toml](/home/zjhsteven/ClassFlow/apps/web/wrangler.toml) 里尚未显式声明 `workers_dev = false`，因此后续如果只在面板里手关 `workers.dev`，再次 `wrangler deploy` 时存在被重新打开的风险。
- 已完成：已通过 `npx wrangler versions list` 与 `npx wrangler deployments list` 确认当前 Worker 仍处于持续部署状态，需要把配置文件作为唯一事实源一并收口。
- 正在做：整理最终对外说明与落地步骤，明确“先封 `workers.dev`，再让 Worker 带 Access Service Token 回源后端 Tunnel”的安全收口方案。

## 2026-04-09 Access Service Token 接入与验证 ExecPlan

### 目标
- 把 Cloudflare Access Service Token 正确接入到 Worker 回源链路，确保后端 Tunnel 域名在接入 Access 后，前端代理仍能正常工作。
- 明确回答“前端浏览器、Worker、smartclass 脚本”这三类调用方各自是否需要携带 Service Token，避免把自动化凭证错误地下发到浏览器或脚本侧。
- 通过本地单元测试、现网 `curl` 探针与 Worker 重新部署，验证新链路已经可用。
- 同步更新部署文档、进度记录与计划状态，确保后续关停 `workers.dev` 时不会再丢上下文。

### 执行步骤
1. 查阅 Cloudflare 官方文档，核对 Service Token 的请求头格式、Worker 回源 Access 受保护源站的推荐做法，以及 `workers_dev` / `preview_urls` 的配置项。
2. 审查当前 Worker 代理与测试，设计最小改动：为后端回源请求增加可选的 `CF-Access-Client-Id` / `CF-Access-Client-Secret` 透传能力，但不把凭证暴露给浏览器。
3. 修改 Worker 代码、测试与示例环境变量；同时更新 README / 部署文档，明确哪些调用方需要凭证、哪些不需要。
4. 运行前端 `npm test`、`npm run lint`、`npm run build`，确认改动没有打断现有代理、静态资源与 R2 产物逻辑。
5. 使用真实 Service Token 对现网受 Access 保护入口做最小化 `curl` 验证；若回源配置已就绪，再重新部署 Worker 并做线上探针。
6. 更新 `PROGRESS.md` 记录结论与注意事项，并按仓库要求提交。

### 当前状态
- 已完成：已用 Context7 与 Cloudflare 官方文档核对 Service Token 的标准请求头为 `CF-Access-Client-Id` 与 `CF-Access-Client-Secret`。
- 已完成：已确认当前前端浏览器访问 `classflow.zjhstudio.com` 走的是“人类登录 Access”路径，不应把 Service Token 发到浏览器端。
- 已完成：已确认当前仓库中的 Worker 代理尚未向后端回源请求附带 Access Service Token，因此一旦后端 Tunnel 域名也被 Access 保护，现有 Worker 会直接失效。
- 已完成：已修改 Worker 代理与测试，并补充“浏览器 / Worker / smartclass 脚本”三类调用方的使用边界说明；前端本地验证已通过 `npm test`（17 项）、`npm run lint`、`npm run build`。
- 已完成：已把新的 `CF_ACCESS_CLIENT_ID` / `CF_ACCESS_CLIENT_SECRET` 写入 Worker secret，并重新部署到 `https://classflow-web.zhangjiahe0830.workers.dev`，当前版本号 `fe03c7e2-42ff-4a06-9e6d-93996c9db66c`。
- 已完成：现网探针已确认 `classflow-web.zhangjiahe0830.workers.dev` 现在也会被 Cloudflare Access 重定向到登录页，说明 `workers.dev` 入口已不再匿名裸奔。
- 已完成：带真实 Service Token 访问 `classflow.zjhstudio.com` 与 `classflow-web.zhangjiahe0830.workers.dev` 仍返回 `302`，且返回元数据里 `service_token_status=false`；这说明 Token 虽已创建，但尚未被挂到对应 Access 应用的 `Service Auth` 策略中，或挂到了错误应用。

## 2026-09-19 SmartClass 登录态过夜保活实验 ExecPlan

### 目标
- 用当前 Chrome 中已经由用户本人登录的 SmartClass 会话做一晚上的低频保活实验，不复制、不落盘任何 Cookie 值或认证 token。
- 每 10 分钟只访问一次正常的 `GET /UserSpace/CourseList.aspx` 作为心跳，再调用页面本身使用的 `GetCurrentUser` 接口判断认证是否仍有效。
- 记录 `PowerCreatorCMSAuthenticationID`、`OnlineUser`、`platform_token` 的名称、会话属性与过期时间元数据，但绝不记录 Cookie 值。
- 实验默认持续 12 小时，并临时阻止 Windows 系统睡眠（允许屏幕关闭），避免“机器睡着”把会话超时实验污染掉。

### 判定方法
1. 若连续 12 小时 `authenticated=true`，说明“10 分钟一次正常课程列表请求”至少能把当前登录态稳定维持过夜；后续再单独做无心跳对照，区分它究竟是保活成功还是登录本来就足够长。
2. 若在固定时间突然从 `authenticated=true` 变为 `false`，而浏览器与网络仍正常，则优先怀疑服务端存在绝对登录寿命或其他不可由心跳续期的限制。
3. 若同时出现浏览器不可连接、网络错误或长时间日志空档，则不能把该次失败直接归因于 SmartClass 会话机制。

### 实现
- 新增 `tools/smartclass/session_keepalive_probe.ps1`。
- 日志写入仓库根目录的 `tmp/`（已被 `.gitignore` 忽略），使用 JSONL 逐次落盘；实验结束另写 summary JSON。
- 先用单次模式做烟雾测试，再以隐藏 PowerShell 进程启动 12 小时正式实验。

## 2026-09-20 N1 远端 CapsWriter ASR + 压缩音频检查点 ExecPlan

### 目标
- N1 只承担 ClassFlow 下载、压缩音频检查点、任务状态、课程产物与远端 ASR 调度，不在 2GB RAM / 8GB eMMC 上加载 Qwen/CapsWriter 模型。
- Windows RTX4060 作为远端 CapsWriter ASR 节点；ClassFlow 通过 HTTP 上传音频文件，所有请求固定保持 CapsWriter `source=file` 后台优先级，继续服从既有 `mic > file` 抢占调度。
- 把当前 `source.mp4 -> audio.wav -> ASR -> 最终统一 cleanup` 改为 eMMC 友好的阶段生命周期：`source.mp4 -> 原 AAC remux 到 audio.m4a -> 校验 -> 立即删 MP4 -> 远端 ASR -> 全任务成功后删 M4A`。
- M4A 在任务失败时保留，作为可以反复重发 ASR 的长期检查点；N1 不生成长期 WAV/FLAC。

### 已确认事实
- 本地 `main` 在重新 `git fetch origin --prune` 后为 `ahead 4 / behind 0`；`campusbox-snapshot-20260914` 没有任何 main 尚未包含的提交，所以当前本地 main 是 N100 抢救版本的超集。
- 历史实测 8 个 SmartClass 45 分钟片段的音轨均为 `AAC / 8kHz / mono / ~47.6kbps`，原始音轨约 `15~16MiB/45min`；因此最终方案是原 AAC/M4A remux，而不是 WAV/FLAC。
- 当前 ClassFlow 代码仍固定生成 `audio.wav`（16kHz/mono/PCM s16le），只有整任务成功才删除 work_dir；失败时 MP4 + WAV 会一起长期残留，尚未实现上述新生命周期。
- 当前 `AsrProvider` 抽象已存在，但 `local_qwen` provider 会在 ClassFlow 同机启动 Python sidecar，并把本地文件路径发给 `127.0.0.1`；这不适用于 N1 -> Windows 跨机 ASR。
- CapsWriter `zjh/mic-preempt-headless-api` 开发分支已经实现 Headless HTTP API：原始 body 上传、Bearer Token、异步 job/status/result、临时文件清理；内部继续走 FileTranscriber/WS，所有 API 任务天然是 `source=file`。
- CapsWriterGateway CLI 也已完成并通过 7 项测试，但 HTTP API 当前更适合 N1，因为它直接解决跨机器文件上传与异步任务状态。
- ARM64 交叉构建链已经验证可行：便携 Zig + `cargo-zigbuild` 可产出约 22.7MB 的 `aarch64-unknown-linux-gnu` backend；最终部署必须在本轮代码改完后重新构建。

### 设计冻结
1. 新增 `remote_capswriter` ASR provider，不复用/伪装 `local_qwen`。
2. provider 请求协议：
   - `POST {base}/v1/transcriptions`，raw body 流式上传音频，`X-Filename`，可选 `Authorization: Bearer ...`；
   - 接收 `202` + job id；
   - 轮询 `GET /v1/transcriptions/{id}`，直到 completed/failed；
   - completed 后读取 `GET /v1/transcriptions/{id}/result`；
   - 把 CapsWriter `text/text_accu/tokens/timestamps/duration` 映射到 `NormalizedTranscript`。
3. provider 只接收音频 checkpoint 文件；Windows Headless API 内部负责 FFmpeg 解码为 CapsWriter 需要的 PCM，N1 不生成 PCM/WAV。
4. 下载阶段 checkpoint 优先级：
   - `audio.m4a` 已存在且非空：直接跳过 MP4 下载和 remux；
   - 否则 `source.mp4` 已完整：直接 remux；
   - 否则下载 MP4。
5. remux 使用 `ffmpeg -map 0:a:0 -vn -c:a copy`，目标 M4A；成功后再用 `ffprobe` 验证存在可读音轨，然后删除 `source.mp4`。
6. remux/验证失败时绝不删除 MP4；不静默转码成 WAV/FLAC。
7. ASR 或后续产物阶段失败时保留 `audio.m4a`；只有整个任务成功、课程产物同步完成后才清理 work_dir。
8. `dashscope` 暂时保留为备用 provider；N1 生产配置切到 `remote_capswriter`。`local_qwen` 兼容代码暂不删除，避免扩大本轮回归面。

### 实施步骤
1. 扩展 `AppConfig` / `AsrProviderKind` / provider 选择，增加远端 CapsWriter base URL、Bearer Token、上传超时、轮询间隔与总超时。
2. 新增 `asr/remote_capswriter.rs`，使用 reqwest + ReaderStream 流式上传，不一次性读入内存；实现 job 状态轮询、错误映射与结果归一化。
3. 将 `PipelineIo::extract_audio` 重构为压缩音频 checkpoint 提取，并增加必要的文件删除/音频验证能力；RealPipeline 用 ffmpeg remux + ffprobe。
4. 重构 worker 文件名与恢复逻辑为 `audio.m4a`，成功 remux/验证后尽快删除 MP4。
5. 扩展 API/backend 测试：覆盖 M4A checkpoint 命中跳过下载、remux 后删除 MP4、remux 失败保留 MP4、远端 CapsWriter 上传/轮询/结果/失败行为。
6. 更新 `env.example`、部署说明与 `PROGRESS.md`；运行 `cargo fmt --check`、`cargo check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test`。
7. 使用已验证的 `cargo-zigbuild` 重新构建 ARM64 release，部署到 N1；恢复 SQLite/backend.env 后再做端到端真实任务烟测。

### 与 N100/N1 并行验证的网络边界
- Cloudflare：同一 tunnel token 是同一 Tunnel UUID 的 replica 凭据，不是“账号通用 token”；N100+N1 同 token 可以同时在线，但会作为同一 tunnel 的 replicas，不能保证请求落到哪一台，因而不适合确定性 A/B。
- FRP：认证 token 可以复用；但当前 N100/N1 配置复制了相同 proxy name + remotePort，且没有 loadBalancer group。两台原样同时上线会发生代理注册/端口冲突；若要 HA 必须显式 loadBalancer group，若要 A/B 则使用独立 proxy 名和 remotePort。
- 并行一周的目标拓扑是四个可选客户端节点：`n100-frp / n100-cf / n1-frp / n1-cf`。FRP 分配独立远端端口；Cloudflare 为两台主机使用不同 Tunnel UUID/hostname，避免 replica 随机落点和 SSH host-key 混用。

## 2026-09-20 SmartClass 自动课程采集 Collector ExecPlan

### 目标
- 把当前已经验证稳定的 SmartClass 登录态保活逻辑正式接入 ClassFlow，使后端能每 10 分钟自动发现新录播并复用现有 intake/任务流水线处理。
- 不新增第二套课程数据库；以现有 SQLite `tasks.segment_key UNIQUE` + `INSERT OR IGNORE` 作为唯一 overlap/去重真相源。
- 首次启用时支持向历史时间窗口回扫，补齐最近约两周未进入 ClassFlow 的课程；后续轮询只增量发现新课。
- 标准 SmartClass VOD 优先由课程列表返回的 `Cover` 路径推导同目录 `VGA.mp4`，不需要逐个打开播放页；异常/CloudVod 条目保留为后续 fallback，不做暴力枚举。

### 已确定的设计
1. Collector 作为 Rust 后端内部长期任务运行，与 worker 共用同一 `Repository` 和 `AppState`。
2. 默认轮询间隔 10 分钟，可用环境变量关闭或调整；一次轮询只做一次课程列表查询，不加载图片/视频资源。
3. 历史回扫采用“时间窗口 + overlap 停止”而不是新建课程表：从当前日期向过去查询、正规化为现有 `IntakeItem`、调用现有 `create_batch_with_tasks()`，由 `segment_key` 唯一约束负责幂等。
4. 认证凭据不写入 ClassFlow 数据库；Collector 的会话提供方式与运行位置解耦，优先使用受限的本地会话桥接/浏览器 profile，而不是把 HttpOnly Cookie 明文复制到配置文件。
5. 登录失效时只记录并退避，不高频重试；恢复登录后自动继续扫描。

### 实施步骤
1. 从当前登录页面核对 `GetMyVideoList` 的真实参数、分页字段和返回结构，确认是否需要逐页还是仅日期窗口。
2. 扩展 `AppConfig`：Collector 开关、轮询间隔、历史回扫天数、SmartClass 基础地址、会话桥接地址/超时等。
3. 新增 `smartclass/` 模块：API DTO、列表拉取、Cover -> VOD URL 正规化、日期/教师/课程字段映射。
4. 新增 repository 批量 existence/overlap 辅助接口（若现有 intake 返回信息不足），但不建立新的课程持久化表。
5. 在 `app.rs` 启动 Collector background loop，复用 `create_batch_with_tasks()` 入队；保证 Collector 异常不会拖垮 HTTP 服务。
6. 增加单元/集成测试：分页/窗口、重复条目、历史补录、登录失效、502、空列表、异常 Cover、标准 VOD 推导。
7. 跑 `cargo fmt --check`、`cargo check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test`。
8. 核对 N100/N1 的最新部署状态；先在 N100 测试环境用当前有效登录态跑一次真实列表抓取与补录，再决定正式迁移到 N1。

### 登录寿命实验改进
- 同一服务端 Session 的多个 10/20/30/40 分钟探针不能视为独立并行实验，因为任一探针可能刷新同一个服务端会话。
- 真并行实验必须使用彼此独立登录得到的服务端 Session/profile；后续实验工具应支持多个独立 Chrome profile。
- 在只有一个登录态时，优先使用指数退避寻找 idle-timeout 量级：10 -> 20 -> 40 -> 80 -> 160 分钟；每轮成功后再加倍，失败后在上下界之间二分，比逐个固定间隔跑 24 小时更高效。

### 当前实施状态（2026-09-20 晚）
- 已完成：确认 `GetMyVideoList` 使用 `PageSize / PageNumber` 分页，响应包含 `Data[] + TotalCount`，官方页面默认 `PageSize=12`。
- 已完成：Rust Collector 已接入现有 SQLite/intake/queue，不新增课程表；首次回扫 21 天，后续每 10 分钟回看最近 2 天。
- 已完成：标准 `CloudVod=false` 录播使用 `Cover -> 同目录 VGA.mp4`；`CloudVod=true` 暂跳过，不猜测目录、不枚举相邻 ScheduleID。
- 已完成：Windows 浏览器桥通过页面官方 `CreateGetMyVideosRequest()` 发请求，Rust 不生成 `csrkToken`，桥也不导出 Cookie/token。
- 已完成：本地 `cargo fmt/check/test/clippy` 全绿；N100 同步后 `cargo check/test` 全绿（24 个单元测试 + 11 个 API 集成测试）。
- 已完成：N100 已 fast-forward 到最新 `main`，构建 release，并在无 pending/running 任务时启用 Collector；当前轮询间隔 600 秒。
- 已完成：临时反向 SSH 已把 N100 `127.0.0.1:8790` 映射到 Windows 浏览器桥，跨机 `/health` 与视频列表请求链路已实际验证。
- 当前阻塞：原 SmartClass 登录态在 12 小时保活实验结束后的下午/晚间已自然失效；Collector 当前正确进入“未授权后等待下一轮”状态，不误入队。
- 后续：用户重新正常登录 SmartClass 后，当前临时链路会自动在下一轮开始 21 天补录；最终无人值守形态改为 N100/N1 本机独立 Chromium profile + 本机桥，去除对 Windows 与反向 SSH 的依赖。
