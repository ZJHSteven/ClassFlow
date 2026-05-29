/*!
ASR provider 统一接口。

接口刻意保持很窄：

1. 输入只接收一个 `AsrInput`。
2. 输出只返回 `NormalizedTranscript`。
3. provider 内部是否需要上传、提交任务、调用 Python、读取 JSON，都对 worker 透明。
*/

use async_trait::async_trait;

use crate::{error::AppResult, models::NormalizedTranscript};

use super::types::AsrInput;

#[async_trait]
pub trait AsrProvider: Send + Sync {
    async fn transcribe_audio(&self, input: AsrInput) -> AppResult<NormalizedTranscript>;
}
