/*!
ASR 层公共类型定义。

这里集中放“provider 输入”和“provider 选择枚举”，避免把字符串字面量散落在配置层、
worker 层和 provider 实现层。
*/

use std::path::PathBuf;

use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrProviderKind {
    DashScope,
    LocalQwen,
    RemoteCapsWriter,
}

impl AsrProviderKind {
    /// 将 provider 枚举转换为稳定字符串，方便日志与事件输出。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DashScope => "dashscope",
            Self::LocalQwen => "local_qwen",
            Self::RemoteCapsWriter => "remote_capswriter",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AsrInput {
    /// 待转写音频文件路径。
    pub audio_path: PathBuf,
    /// 当前 ClassFlow 任务 ID，用于日志与临时文件命名。
    pub task_id: String,
    /// 语言参数，当前本地 provider 会转换为 CapsWriter 可识别的语言名称。
    pub language: Option<String>,
    /// 附加上下文，后续可用于给本地模型提供提示。
    pub context: Option<String>,
    /// 远端异步 ASR 的已保存 job id；为空表示需要新建 job。
    pub resume_job_id: Option<String>,
    /// provider 向 worker 汇报阶段变化的轻量通道。
    ///
    /// 本地 / DashScope provider 可以忽略；remote_capswriter 用它把“上传中 / 已提交 /
    /// queued / running / job 失效”及时同步回任务数据库和前端。
    pub progress: Option<UnboundedSender<AsrProgressEvent>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsrProgressEvent {
    Uploading,
    /// 远端文件上传的实时进度。
    ///
    /// 这里故意复用和下载阶段相同的四个指标，worker 可以直接写入 tasks 现有的
    /// `transferred_bytes / total_bytes / rate_bytes_per_sec / eta_seconds` 字段，
    /// 不需要为了“上传进度条”再扩一套数据库 schema。
    UploadProgress {
        transferred_bytes: u64,
        total_bytes: u64,
        rate_bytes_per_sec: u64,
        eta_seconds: Option<u64>,
    },
    Submitted {
        job_id: String,
    },
    Queued {
        job_id: String,
    },
    Running {
        job_id: String,
    },
    JobInvalidated {
        job_id: String,
        reason: String,
    },
}

impl AsrInput {
    pub fn report(&self, event: AsrProgressEvent) {
        if let Some(progress) = &self.progress {
            let _ = progress.send(event);
        }
    }
}
