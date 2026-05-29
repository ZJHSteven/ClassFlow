/*!
ASR 层公共类型定义。

这里集中放“provider 输入”和“provider 选择枚举”，避免把字符串字面量散落在配置层、
worker 层和 provider 实现层。
*/

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrProviderKind {
    DashScope,
    LocalQwen,
}

impl AsrProviderKind {
    /// 将 provider 枚举转换为稳定字符串，方便日志与事件输出。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DashScope => "dashscope",
            Self::LocalQwen => "local_qwen",
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
}
