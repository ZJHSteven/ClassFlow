/*!
ASR provider 抽象层。

这一层的职责是把“音频文件 -> 标准化转写结果”从下载/抽音频主流程里拆出来。

为什么现在要单独分层：

1. 旧实现把 DashScope 上传、提交、轮询全部写死在 `worker.rs + pipeline.rs` 中。
2. 这会让“切换到本地 ASR”变成大面积重写，而不是单点替换。
3. 新层只关心“给我一个音频文件，我返回 `NormalizedTranscript`”，这样云端和本地都能挂进来。
*/

pub mod dashscope;
pub mod local_qwen;
pub mod provider;
pub mod remote_capswriter;
pub mod types;

pub use dashscope::DashScopeAsrProvider;
pub use local_qwen::LocalQwenAsrProvider;
pub use provider::AsrProvider;
pub use remote_capswriter::RemoteCapsWriterAsrProvider;
pub use types::{AsrInput, AsrProviderKind};
